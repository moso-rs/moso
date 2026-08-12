# 31 — Authorization

> ⛔ **NOT IMPLEMENTED.** This document is design intent only. No crate in the workspace provides
> any of it, nothing references it, and nothing is stubbed. See
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

> The research is unambiguous: **RBAC/permission systems in Rust are largely roll-your-own, and
> there is no dominant batteries-included authorization layer.** This is the clearest unclaimed
> gap in the ecosystem and the strongest single differentiator available to Moso.

## Design goals

1. **Typed, not stringly.** A permission is a compile-time constant. A typo is a compile error, not
   a silent `false`.
2. **Enumerable.** The full permission set is knowable at boot, so the admin UI can render it, the
   OpenAPI can document it, and an audit can list it.
3. **Two layers, cleanly separated.** Coarse *capability* checks ("may this actor publish posts at
   all") and fine *resource* checks ("may they publish *this* post"). Most systems fail because
   they only model one.
4. **Deny by default, and provably so.** An endpoint with no authorization declaration is flagged.
5. **Explainable.** Every decision can produce a trace of why it was allowed or denied. Debugging
   "why can't this user do X" without that is the recurring pain of every authz system.

## Layer 1 — the permission registry

```rust
// example — src/authz.rs
moso::permissions! {
    /// Posts
    posts.read      = "View posts",
    posts.create    = "Create posts",
    posts.update    = "Edit posts",
    posts.delete    = "Delete posts",
    posts.publish   = "Publish posts",

    /// Users
    users.read      = "View users",
    users.invite    = "Invite users",
    users.suspend   = "Suspend users",

    /// Administration
    admin.access    = "Access the admin panel",
    admin.settings  = "Change organisation settings",
}
```

Generates:

```rust
// generated
#[repr(u16)]
pub enum Perm { PostsRead, PostsCreate, /* … */ AdminSettings }

impl Perm {
    pub const ALL: &'static [Perm] = &[ /* … */ ];
    pub const fn as_str(self) -> &'static str;          // "posts.publish"
    pub const fn description(self) -> &'static str;
    pub const fn group(self) -> &'static str;           // "posts"
    pub fn parse(s: &str) -> Option<Perm>;              // for DB round-trips
}

/// Compact, copyable, O(1) — 8 bytes covers 64 permissions, grows in 8-byte words.
pub struct PermSet(/* bitset */);
```

`PermSet` is a bitset because permission checks happen many times per request and an
`Arc<HashSet<String>>` lookup in a hot loop is how authz becomes a performance problem.

### Roles

```rust
// example
moso::roles! {
    Viewer  = [posts.read, users.read],
    Editor  = Viewer + [posts.create, posts.update],
    Admin   = Editor + [posts.publish, posts.delete, users.invite, users.suspend, admin.access],
    Owner   = Admin + [admin.settings],
}
```

Static roles are a `const PermSet` — resolution is free. Dynamic roles (customer-defined, stored in
a table) are also supported via `RoleSource`; the two compose, and the docs explain when you need
dynamic ones (multi-tenant SaaS with customer-managed roles) versus when static is correct (most
apps).

Scoped roles for multi-tenancy: a user's roles are per-scope
(`RoleAssignment { user, role, scope: Scope }` where `Scope` is `Global | Org(id) | Project(id)`),
which is the model almost every B2B SaaS eventually needs.

## Layer 2 — resource policies

Permissions cannot express "the author may edit their own post." Policies can.

```rust
// example — src/authz/post.rs
pub struct Publish;      // action marker types
pub struct Edit;

impl Policy<Edit, Post> for Actor {
    async fn allows(&self, _: Edit, post: &Post, ctx: &PolicyCtx) -> Decision {
        if self.has(Perm::PostsUpdate) && post.author_id == self.id {
            return Decision::allow("author");
        }
        if self.has(Perm::AdminAccess) {
            return Decision::allow("admin override");
        }
        Decision::deny("not the author and not an admin")
    }
}
```

```rust
// spec
pub trait Policy<A, R>: Send + Sync {
    fn allows(&self, action: A, resource: &R, ctx: &PolicyCtx)
        -> impl Future<Output = Decision> + Send;
}

pub struct Decision { allowed: bool, reason: Cow<'static, str>, obligations: Vec<Obligation> }
impl Decision {
    pub fn allow(reason: impl Into<Cow<'static, str>>) -> Self;
    pub fn deny(reason: impl Into<Cow<'static, str>>) -> Self;
    /// Attach a required side effect, e.g. "redact the `salary` field".
    pub fn with_obligation(self, o: Obligation) -> Self;
}
```

**Obligations** are the feature that turns a boolean into something useful: a policy can allow the
read but require field redaction, and the serialiser honours it. That covers the "managers see
salaries, peers do not" case without a second DTO.

## Using it in handlers

### The declarative form (most common)

```rust
// example
#[endpoint]
#[requires(Perm::PostsCreate)]
async fn create(Inject(db): Inject<Db>, Json(b): Json<CreatePost>) -> Result<Created<PostOut>> { … }
```

`#[requires]` is checked before the handler body runs, contributes `security` + a documented 403 to
the OpenAPI, and the permission is validated against the registry **at boot** (a typo is a boot
error with a "did you mean" suggestion).

### The guard extractor (resource-scoped)

```rust
// example
#[endpoint]
async fn publish(
    Authorized(post): Authorized<Publish, Post>,     // loads the post AND checks the policy
    Inject(db): Inject<Db>,
) -> Result<PostOut> {
    let post = post.update().set(Post::PUBLISHED_AT, now()).fetch_one(&db).await?;
    Ok(post.into())
}
```

`Authorized<A, R>`:
- Reads the resource id from the path (by convention `{id}`, overridable with
  `Authorized<Publish, Post, FromPath<"post_id">>`).
- Loads the resource (404 if absent — **before** the policy check, and the docs explain the
  information-leak trade-off and how to invert it with `.mask_not_found()` when existence itself is
  sensitive).
- Runs the policy (403 with the `Decision` reason in dev; a generic message in production).
- Yields the loaded resource, so there is no second query.

### The imperative form

```rust
// example
let decision = actor.can(Edit, &post).await;
if !decision.allowed() { return Err(Error::forbidden(decision.reason())); }
```

### Query-level filtering (the hard one, and the one that matters)

Checking each row after loading is wrong at scale. Policies can contribute a filter:

```rust
// example
impl ScopedPolicy<Read, Post> for Actor {
    fn scope(&self, q: Select<Post>) -> Select<Post> {
        if self.has(Perm::AdminAccess) { return q; }
        q.filter(Post::PUBLISHED.eq(true) | Post::AUTHOR_ID.eq(self.id))
    }
}

// in the handler
let posts = Post::query().authorized_for::<Read>(&actor).paginate(cursor, 20).fetch(&db).await?;
```

This is the feature that separates a real authorization layer from a decorator. It means list
endpoints are correct *and* fast, and pagination counts are right.

## Deny by default

`moso check` reports every endpoint with no authorization declaration:

```
$ moso check --authz
✗ 3 endpoints have no authorization declaration

  POST /posts                 src/routes/posts.rs:31
  DELETE /posts/{id}          src/routes/posts.rs:78
  GET /admin/stats            src/routes/admin.rs:12

  add `#[requires(..)]`, an `Authorized<..>` parameter, or `#[public]` to declare intent
```

`#[public]` is an explicit annotation meaning "no authorization needed," so the audit distinguishes
"considered and public" from "forgotten." In `moso.toml`, `lints.missing_authz = "deny"` turns this
into a build failure — the recommended setting for anything handling real data.

## Explainability

```
$ moso authz explain --user usr_123 --action publish --resource post:456
DENY  posts.publish

  actor      usr_123 (alice@example.com)
  roles      Editor (global), Viewer (org:acme)
  perms      posts.read, posts.create, posts.update  (from Editor)
  required   posts.publish
  policy     Policy<Publish, Post> for Actor  src/authz/post.rs:14
  reason     "not the author and not an admin"
  trace
    ✓ authenticated
    ✓ resource loaded (post:456, author=usr_999)
    ✗ has(posts.publish) → false
    ✗ post.author_id == actor.id → false
    ✗ has(admin.access) → false
```

Also available at runtime: `X-Moso-Authz-Explain: 1` on a request in `dev` returns the trace in the
403 body. This turns a class of "it just says forbidden" support tickets into self-service.

## Audit log

Every deny, and every allow on a `#[requires(audit)]` permission, writes to `moso_authz_audit`:
actor, action, resource, decision, reason, request id, timestamp, IP. Queryable in the admin,
exportable, with a retention policy. Compliance-driven buyers ask for this and its absence is
frequently what disqualifies a framework in an enterprise evaluation.

## Integration points

| Consumer | Uses |
| --- | --- |
| OpenAPI | permission descriptions in 403 responses; `security` requirements |
| Admin | per-model permissions auto-derived (`posts.read` gates the Posts section) |
| API keys | scoped to a `PermSet`; the key's set intersects the user's |
| Jobs | a job runs as a `Principal`; policies apply identically |
| Admin UI | role editor listing every permission with its description |

## Performance

- `PermSet` check: a bitwise AND, ~1 ns.
- Role resolution: cached per request in the `Actor` dependency; one query per request maximum,
  cached in KV with the user's `auth_hash` as part of the key so a role change invalidates it.
- Policy evaluation: user code; the framework adds a tracing span and nothing else.
- Target: **< 1 µs** of framework overhead per authorization check.

## Comparison to prior art

| | Casbin (rust) | oso (deprecated Rust SDK) | Hand-rolled | **moso-authz** |
| --- | --- | --- | --- | --- |
| Typed permissions | ❌ strings | ⚠️ polar DSL | varies | ✅ compile-time |
| Enumerable at boot | ❌ | ❌ | ❌ | ✅ |
| Resource policies | ⚠️ via matchers | ✅ | ✅ | ✅ |
| Query-level filtering | ❌ | ✅ (data filtering) | rare | ✅ |
| Explain trace | ⚠️ | ✅ | ❌ | ✅ |
| No separate policy language | ✅ | ❌ | ✅ | ✅ |
| Framework-integrated | ❌ | ❌ | — | ✅ |

The one thing we deliberately do not do is invent a policy DSL. Policies are Rust: testable,
debuggable, refactorable, and visible to `rust-analyzer`. Oso's Polar is more expressive; the
docs say so and explain the trade.

## Acceptance criteria (WP-18)

1. `permissions!` produces a typed enum; an unknown permission in `#[requires]` is a boot error
   with a suggestion (test).
2. `moso check --authz` finds every undeclared endpoint; `#[public]` silences it.
3. `Authorized<A, R>` loads once, checks once, and returns the resource — asserted with a query
   counter.
4. `authorized_for::<Read>` produces the expected SQL filter and correct pagination totals.
5. Obligations redact fields in the serialised response (snapshot test).
6. `moso authz explain` output matches the runtime decision for the same inputs.
7. Audit entries are written for every deny, with no PII beyond the actor id and IP.
8. Authorization overhead benchmark < 1 µs per check.
