---
title: Permissions and roles
description: Declare a typed permission registry, build roles from it, resolve an actor per request, and guard endpoints with the requires attribute.
order: 27
status: shipped
---

[Authentication](./authentication.md) answers who is calling. Authorization answers what they may do,
and Moso answers it in `moso-authz`, a crate you add yourself. It works in two layers. A *capability*
check asks whether the caller holds a permission at all (`posts.publish`), needs no database row, and
guards an endpoint or a whole router subtree. A *resource* check asks whether this caller may publish
*this* post, loads the row, and runs a policy you wrote in ordinary Rust.

This page is layer one: the permission registry, roles, scopes, the actor, `#[requires]`, and what a
refusal looks like on the wire. Layer two is [policies and query scoping](./policies.md). The two
share the actor and the audit trail, so read this one first.

> [!IMPORTANT]
> The runtime is real and tested, and `moso check --authz` runs the two library functions that make
> deny-by-default provable, reading `lints.missing_authz` out of `moso.toml` while it does it. The
> check is one you run rather than one boot runs for you: a permission named by a mistyped **string**
> compiles and boots, and `moso check --authz` in CI (or the test in
> [reading the declarations back](#marking-an-endpoint-public)) is what turns it into an error before
> it reaches a request. Prefer the enum path, where a typo is a compile error.

## Add the crate

Two dependencies. The facade carries the macros, the runtime types come from `moso-authz` directly.

```toml title="Cargo.toml"
[dependencies]
moso = { path = "/absolute/path/to/moso/crates/moso", features = ["authz"] }
moso-authz = { path = "/absolute/path/to/moso/crates/moso-authz" }
```

Path dependencies because nothing is published yet; see [installation](../start/installation.md).

Three costs, all visible before your first build rather than after it:

- `authz` is off by default and implies `orm`, so turning it on pulls a database driver into your
  build whether or not you use the ORM.
- There is no `moso::authz` module. Unlike the ORM (`moso::db`) the facade re-exports these types
  only under a hidden module the macros target, so your code names `moso_authz::` for every type. The
  four macros (`permissions!`, `roles!`, `#[requires]`, `#[public]`) come from `moso`.
- `moso-authz` declares no Cargo features of its own. Nothing inside it is optional.

Without the `authz` feature the macros do not compile, and the failure is a path resolution error
inside the generated code rather than a readable diagnostic.

## The smallest working example

Four pieces: a registry, roles built from it, a source that turns a request into an actor, and a
guarded endpoint.

```rust title="src/authz.rs"
moso::permissions! {
    /// Posts
    posts.read      = "View posts",
    posts.create    = "Create posts",
    posts.publish   = "Publish posts",

    /// Administration
    admin.access    = "Access the admin panel",
}

moso::roles! {
    /// Read-only access.
    Viewer = [posts.read],
    /// Writes and edits posts.
    Editor = Viewer + [posts.create],
    /// Runs the organisation.
    Admin  = Editor + [posts.publish, admin.access],
}
```

```rust title="src/routes/posts.rs"
/// Create a post.
#[moso::requires(Perm::PostsCreate)]
#[moso::endpoint]
pub async fn create() -> moso::Result<moso::response::Created<PostOut>> {
    Ok(moso::response::Created::at("/posts/1", PostOut { id: 1 }))
}
```

```rust title="src/main.rs"
App::new(config)
    .provide_dyn::<dyn ActorSource<Role>>(Arc::new(SessionActor))
    .provide_dyn::<dyn PermissionSource>(Arc::new(ActorPermissions::<Role>::new()))
    .mount(routes::router())
    .build()
```

`SessionActor` is yours and is [written below](#the-one-provider-you-write). With those four pieces a
caller without `posts.create` gets a 403 before the handler body runs, the operation's OpenAPI entry
gains a documented 403 naming the permission, and the denial is written to an audit trail even though
you registered no sink.

## Declaring the permission registry

`permissions!` takes a comma-separated list of `group.name = "description"` entries and generates a
`#[repr(u16)] pub enum Perm` in the module that invoked it. Exactly one dot per entry, and the
description is required, because it is the text that ends up in the OpenAPI 403 and in an admin role
editor.

```rust title="src/authz.rs"
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

The variant name is the entry in UpperCamel: `posts.read` becomes `Perm::PostsRead`. Doc comments
group the list for a human reader; they do not affect what is generated.

| Generated item | What it gives you |
| --- | --- |
| `Perm::PostsPublish` | The variant. `#[repr(u16)]`, discriminant equal to its index. |
| `Perm::ALL` | Every permission, in declaration order. |
| `Perm::NAMES` / `Perm::DESCRIPTIONS` | The wire names and descriptions as `&'static [&'static str]`. |
| `Perm::as_str()` | `"posts.publish"`. A `const fn`. |
| `Perm::description()` / `Perm::group()` | `"Publish posts"` and `"posts"`. Both `const fn`. |
| `Perm::parse(name)` | `Option<Perm>`, for a database column or an API key scope list. |
| `impl Display for Perm` | Prints the wire name. |
| `impl Permission for Perm` | Including `FINGERPRINT`, a hash of the names in order. |

The macro refuses, at compile time, an empty registry, a duplicate name (pointing at both
declarations), an entry with no dot or more than one, a description that is not a string literal, and
more than 256 entries.

### The cap is 256, and it is not negotiable

A permission's index is its bit in a `PermSet`, which is a fixed `[u64; 4]`. That makes it `Copy`,
32 bytes, and a check four bitwise ANDs. Raising the cap would widen every set in every request, so
it is a constant rather than a parameter. An application that wants more than 256 permissions is
usually modelling rows as capabilities; those belong in a [policy](./policies.md), not in the
registry.

> [!WARNING]
> Declaration order is bit order. Reordering the list changes what a stored `PermSet` means. Add new
> permissions at the end. Across a process boundary the registry fingerprint catches the mistake, but
> inside one database it will not.

### Working with sets

`PermSet<P>` is the type every check reads. It is `Copy`, so it is passed by value everywhere.

| Method | What it does |
| --- | --- |
| `PermSet::of([Perm::A, Perm::B])` | A set holding exactly these. |
| `PermSet::all()` / `PermSet::empty()` | Every registered permission, or none. |
| `.with(p)` / `.without(p)` | Add or remove one, returning a new set. |
| `.has(p)` | One shift and one AND. |
| `.has_all(other)` / `.has_any(other)` | Superset and intersection tests. |
| `.union` / `.intersection` / `.difference` | Set algebra. `intersection` is the API key ceiling rule. |
| `.len()` / `.is_empty()` | Population count. |
| `.iter()` / `.names()` | In registry order, not insertion order. |
| `PermSet::parse_all(&names)` | `(PermSet, usize)`: the set, plus how many names the registry no longer knows. |
| `.to_bits()` / `PermSet::from_bits(bits)` | Cross a boundary that cannot name `Perm`. |

`PermBits` is the erased form: the four words plus the registry fingerprint. Store that in an API key
row, a cache entry or a token, and `from_bits` refuses bits whose fingerprint does not match rather
than reinterpreting them as a different registry's permissions. The fingerprint is FNV-1a over the
names in order. It catches a mismatched build, not an attacker.

`PermissionRegistry::of::<Perm>()` is the runtime view of the same data: `all()`, `groups()`,
`in_group(g)`, `lookup(name)`, `suggest(name)` (edit distance, so `posts.pubish` suggests
`posts.publish` and `xyzzy` suggests nothing) and `check(&names)`, which returns the errors for every
name that is not registered. `groups()` and `in_group` are here so you can build your own role
editor: Moso is a framework and ships the registry, not an admin panel.

### Reading it from a terminal

```text
$ moso authz permissions --group posts
PERMISSION      GROUP  DESCRIPTION
posts.read      posts  View posts
posts.publish   posts  Publish posts

  ✓ 2 permission(s)                (fingerprint 0x9f2a…)
      a stored PermSet is only meaningful against this fingerprint
```

`moso authz roles` is the same for `roles!`: each role, how many permissions it grants, and which.
A role that grants nothing is called out, because an empty right-hand side and a deleted permission
look identical from the outside and both mean "anyone holding this is refused everywhere".

The fingerprint is on the output for the reason above: bit order is declaration order, so a stored
`PermSet` only means what it meant if the fingerprint still matches. Printing it beside the list is
how you compare a deployed build against a persisted one.

Both commands take `--json`, and both read `fn authz` in your `src/dump.rs`, the same function
`moso check --authz` uses. Until you replace the stub `moso new` writes, they exit 1 naming what to
add rather than printing an empty table.

## Roles

`roles!` builds a `#[repr(u8)] pub enum Role` from the registry. A role is a parent role, a bracketed
list of permissions, or a sum of both.

```rust title="src/authz.rs"
moso::roles! {
    /// Read-only access.
    Viewer  = [posts.read, users.read],
    /// Writes and edits posts.
    Editor  = Viewer + [posts.create, posts.update],
    /// Runs the organisation.
    Admin   = Editor + [posts.publish, posts.delete, users.invite, users.suspend, admin.access],
    /// Owns the organisation.
    Owner   = Admin + [admin.settings],
}
```

Inside the brackets a permission may be written `group.name` or as the variant identifier
(`PostsRead`). The doc comment becomes `Role::Editor.description()`, verbatim, trailing period
included; without one the description falls back to the variant name.

Inheritance is flattened by the macro, so `Role::permissions()` is a `const fn` returning a
`PermSet<Perm>` and resolving a role at runtime is a copy of four words, not a graph walk. The
consequence is that a cycle has no fixed point, so it is a compile error naming the roles and
printing the path. Unknown parents, duplicate roles and an empty registry are compile errors too, and
the cap is 64 static roles.

`RoleSet<R>` is the same idea one level up: a `u64` bitset with `of`, `with`, `without`, `has`,
`union`, `iter`, `names`, `parse_all`, and `permissions()`, which is the union of every held role's
permissions.

## Scopes

Every role grant is held in a scope, from the first line of code, even in an application that only
ever uses `Scope::Global`. Retrofitting scopes later means touching every authorization call site.

```rust
use moso_authz::{Scope, ScopeId};

let acme = Scope::Org(ScopeId::new("acme"));

assert_eq!(acme.as_key(), "org:acme");
assert!(Scope::Global.covers(&acme));
assert!(!acme.covers(&Scope::Global));
```

`Scope` is `Global`, `Org(id)`, `Project(id)` or `Custom { kind, id }`. `ScopeId` is a string, built
from a slug, from an `i64`, or from a `moso_schema::Id<E>`, because tenants are identified by a UUID
in one application and a bigint in the next.

`covers` is deliberately shallow: `Global` covers everything, and nothing else covers anything but
itself. Whether an org admin is also a project admin is your decision, and building that in here
would make every application that disagrees insecure by default.

## Roles that live in a database

`RoleAssignment<R>` is a grant as a row: subject, role wire name, scope, `granted_at`, an optional
`expires_at`, plus `is_active(now)` and `resolve()`. The role is stored as its wire name so a grant
for a role you later delete still deserialises instead of failing the whole query.

A `RoleSource<R>` is where grants come from:

```rust
pub trait RoleSource<R: Role>: Send + Sync + 'static {
    fn roles_for<'a>(&'a self, subject: &'a ActorId, scope: &'a Scope)
        -> BoxFuture<'a, moso_authz::Result<RoleSet<R>>>;

    fn extra_permissions<'a>(&'a self, subject: &'a ActorId, scope: &'a Scope)
        -> BoxFuture<'a, moso_authz::Result<PermSet<R::Perm>>>;

    fn assignments<'a>(&'a self, subject: &'a ActorId)
        -> BoxFuture<'a, moso_authz::Result<Vec<RoleAssignment<R>>>>;
}
```

Only `roles_for` has no default. `extra_permissions` is how customer-defined roles work: a role your
customer built in your admin UI is not in the `roles!` enum, so you return the permissions it grants
directly and the two compose by union. `assignments` is for a "who has access to what" screen.

`MemoryRoleSource<R>` ships for tests, fixtures, seeded development databases and single-tenant
deployments configured from a file:

```rust
let acme = Scope::Org(ScopeId::new("acme"));
let source = MemoryRoleSource::<Role>::new()
    .granting(RoleAssignment::new(
        ActorId::new("usr_1"),
        Role::Viewer,
        Scope::Global,
    ))
    .granting(RoleAssignment::new(
        ActorId::new("usr_1"),
        Role::Admin,
        acme.clone(),
    ));

let alice = ActorId::new("usr_1");

// Globally: only the global grant.
assert_eq!(
    source.roles_for(&alice, &Scope::Global).await.expect("roles"),
    RoleSet::of([Role::Viewer]),
);
// In `acme`: the global one applies here too, plus the scoped one.
assert_eq!(
    source.roles_for(&alice, &acme).await.expect("roles"),
    RoleSet::of([Role::Viewer, Role::Admin]),
);
```

`roles_for` filters on the subject, on whether the grant's scope covers the requested one, and on
`is_active(Utc::now())`, so an expired grant disappears without a background job.

`extra_permissions` matches the scope the same way, with `Scope::covers`, so a direct grant reaches
exactly the scopes a role grant in the same scope would. A custom source should do the same: a source
that matches one way for roles and another way for direct grants grants a customer-defined role's
permissions in scopes the role itself does not reach.

There is no caching layer. If resolving roles is a query per request and that matters, cache inside
your own source; nothing in `moso-authz` touches [the KV store](./cache.md).

## The actor

`Actor<R>` is who is asking. It is built once per request and holds the resolved permission set, so
every check afterwards is a bitwise operation.

```rust
use moso_authz::{Actor, ActorId, ActorKind, RoleSet, Scope};

let alice = Actor::new(
    ActorId::new("usr_1"),
    ActorKind::User,
    Scope::Global,
    RoleSet::of([Role::Editor]),
);

assert!(alice.has(Perm::PostsCreate));
assert!(!alice.has(Perm::PostsPublish));
```

| Method | What it is for |
| --- | --- |
| `Actor::new(id, kind, scope, roles)` | Resolves `roles.permissions()` once. |
| `Actor::anonymous()` | No roles, no permissions, `Scope::Global`. |
| `.with_permissions(extra)` | Union in a grant that came from outside any static role. |
| `.capped_at(ceiling)` | Intersect with a credential's own scopes. |
| `.in_scope(scope, roles)` | Re-resolve for another tenant, keeping the ceiling. |
| `.has` / `.has_all` / `.has_any` | The questions. Constant time. |
| `.is(role)` | A role check. Prefer `has`: permissions survive a reorganisation of roles. |
| `.ceiling()` | The cap, kept so an explain trace can show it. |
| `.identity()` | An `ActorIdentity` (the id, the kind and the scope), which is what an audit record needs without naming `R`. `into_parts()` unpacks it. |

`ActorKind` is `Anonymous`, `User`, `ApiKey`, `Service` or `Job`, with `is_authenticated()` and
`as_str()`.

`Job` is the kind a background job runs under, and the enqueuer's identity travels with it. Enqueue
inside `moso_jobs::actor::scope(actor.identity().to_wire(), …)` (request middleware that resolved an
`Actor` is the natural place) and the identity rides on the queued row; a worker restores it, so
`ctx.actor_identity()` in the job body hands back the string, `ActorIdentity::from_wire` decodes it,
and `detached_ctx_for(&identity)` builds a `PolicyCtx` that runs the job *as* whoever scheduled it.
Only the identity crosses (id, kind, scope), never a credential or a resolved permission set, so a
job that needs to know what the subject may do *now* re-resolves it and a revoked permission is
already gone. `moso-jobs` keeps the string opaque and takes no dependency on this crate. See
[background jobs](./jobs.md).

### Capping an actor at an API key's scopes

An API key must never grant more than the user behind it holds. `capped_at` intersects, which makes
that structural rather than a rule somebody has to remember:

```rust
let key_scopes = PermSet::of([Perm::PostsRead, Perm::AdminSettings]);
let alice = Actor::new(
    ActorId::new("usr_1"),
    ActorKind::ApiKey,
    Scope::Global,
    RoleSet::of([Role::Editor]),
)
.capped_at(key_scopes);

assert!(alice.has(Perm::PostsRead));
assert!(
    !alice.has(Perm::AdminSettings),
    "the key listed a scope the user does not hold",
);
assert!(!alice.has(Perm::PostsUpdate), "the key does not carry it");
assert_eq!(alice.ceiling(), Some(key_scopes));
```

The cap is sticky. A later `with_permissions` cannot lift it, and two caps in a chain take the
tighter one:

```rust
let alice = Actor::new(
    ActorId::new("usr_1"),
    ActorKind::ApiKey,
    Scope::Global,
    RoleSet::of([Role::Owner]),
)
.capped_at(PermSet::of([Perm::PostsRead]))
.with_permissions(PermSet::of([Perm::AdminSettings]));

assert!(alice.has(Perm::PostsRead));
assert!(!alice.has(Perm::AdminSettings));
```

`moso-auth` stores an API key's scopes as `Vec<String>` and knows nothing about this crate, so the
bridge on this side is `PermSet::parse_all(&record.scopes)` followed by `capped_at`. See
[JWT and API keys](./jwt-and-api-keys.md).

## The one provider you write

`ActorSource<R>` is the seam between "who is logged in" and "what may they do". It is the only trait
you must implement, and it is the reason this crate does not depend on `moso-auth`: a service
authorised by an API key, a job running as a service principal and an operator on the command line
all need authorization without a login form.

```rust title="src/authz/source.rs"
use moso::{BoxFuture, RequestCtx};
use moso_authz::{Actor, ActorId, ActorKind, ActorSource, RoleSet, Scope};

use crate::authz::Role;

/// Turns a request into who is asking.
pub struct SessionActor;

impl ActorSource<Role> for SessionActor {
    fn actor<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, moso::Result<Actor<Role>>> {
        Box::pin(async move {
            let Some(id) = ctx.headers().get("x-actor").and_then(|v| v.to_str().ok()) else {
                return Ok(Actor::anonymous());
            };
            Ok(Actor::new(
                ActorId::new(id),
                ActorKind::User,
                Scope::Global,
                RoleSet::of([Role::Editor]),
            ))
        })
    }
}
```

A real one reads the session or API key, loads the subject's assignments for the scope the request is
in, calls `with_permissions` for customer-defined roles and `capped_at` when the credential is a key.

> [!NOTE]
> An absent credential is not an error. It is `Actor::anonymous()`, and the refusal belongs to the
> permission check, which produces a 403. If you want a 401 for a missing credential, return one from
> your source: `actor` returns `moso::Result`, and a 401 or a 503 from it propagates unchanged.

Register two providers at boot:

```rust title="src/main.rs"
App::new(config)
    .provide_dyn::<dyn ActorSource<Role>>(Arc::new(SessionActor))
    .provide_dyn::<dyn PermissionSource>(Arc::new(ActorPermissions::<Role>::new()))
```

`ActorPermissions` takes no arguments because it resolves the actor through the request cache rather
than through a source it holds. That is what makes a handler with both `#[requires]` and an
`Authorized<..>` parameter resolve its actor once.

`Actor<Role>` is a `Dependency`, so a handler reads it with `Depends<Actor<Role>>` and the missing
provider is a boot error rather than a first-request 500.

## Requiring a permission on an endpoint

`#[requires]` goes **above** `#[endpoint]`. Rust expands the outermost attribute first, and
`#[endpoint]` builds its extraction glue from the signature it sees, so a parameter added afterwards
would never be passed and the check would never run. Both attributes detect the wrong order and fail
the build with the corrected order printed.

```rust title="src/routes/posts.rs"
/// Create a post.
#[moso::requires(Perm::PostsCreate)]
#[moso::endpoint]
pub async fn create() -> moso::Result<moso::response::Created<PostOut>> {
    Ok(moso::response::Created::at("/posts/1", PostOut { id: 1 }))
}

/// Suspend a user, and record that somebody did.
#[moso::requires(Perm::UsersSuspend, audit)]
#[moso::endpoint]
pub async fn suspend() -> moso::Result<moso::response::NoContent> {
    Ok(moso::response::NoContent)
}

/// Read a post, for anybody who can read *or* administer.
#[moso::requires(any(Perm::PostsRead, Perm::AdminAccess))]
#[moso::endpoint]
pub async fn show() -> moso::Result<moso::extract::Json<PostOut>> {
    Ok(moso::extract::Json(PostOut { id: 1 }))
}

/// Publish a post, named by its wire name rather than its variant.
#[moso::requires("posts.publish")]
#[moso::endpoint]
pub async fn publish() -> moso::Result<moso::response::NoContent> {
    Ok(moso::response::NoContent)
}

/// Liveness. Deliberately open.
#[moso::public]
#[moso::endpoint]
pub async fn healthz() -> moso::Result<moso::response::NoContent> {
    Ok(moso::response::NoContent)
}
```

| Form | Meaning |
| --- | --- |
| `Perm::PostsCreate` | One permission by enum path. The permission type is inferred from the path. |
| `Perm::A, Perm::B` | All of them. A bare list means `all`. |
| `any(a, b)` | At least one of them. |
| `all(a, b)` | Accepted for symmetry with `any`. Same as a bare list. |
| `"posts.publish"` | By wire name. Not compile-checked; see the caveat below. |
| `audit` | A bare word. Writes an audit entry on the allow path as well as the deny path. |

The attribute generates a hidden unit type carrying the declaration as associated constants and
prepends a `Required<..>` parameter to your handler. Prepending matters: the *last* parameter is the
one `#[endpoint]` treats as the body extractor, so appending would break every handler that reads a
body. You never name `Required` yourself.

> [!WARNING]
> `any` and `all` set one mode for the whole attribute, not per group.
> `#[requires(Perm::A, any(Perm::B, Perm::C))]` means "any one of A, B or C", not "A and (B or C)".
> There is no nested boolean structure. Write two checks, or put the logic in a
> [policy](./policies.md).

The string form is checked against the registry when the operation is described, and the result is
recorded in the OpenAPI document with a "did you mean". Because nothing runs that check
automatically today, a typo reaches the request path, where it fails closed with a 500 rather than a
403. Prefer the enum path, where a typo is a compile error.

## Guarding a subtree

The runtime form of the same check is `Requires`, which is a `moso_core::Guard`:

```rust title="src/routes/mod.rs"
let admin = Router::new()
    .get("/users", moso::ep!(list_users))
    .post("/users/{id}/suspend", moso::ep!(suspend))
    .guard(Requires::new(PermSet::of([Perm::AdminAccess])));

let router = Router::new()
    .get("/posts", moso::ep!(list_posts))
    .nest("/admin", admin);
```

`.any()` switches the mode, `.audited()` forces an audit entry on allows. Unlike a plain layer, a
guard contributes to the OpenAPI document, so every operation it protects gains the same documented
403 that `#[requires]` produces.

Two things to know. `guard` applies to every route registered on that router *so far*, so it goes
after the routes it protects. And `Guard` has no provider requirement, so a missing
`dyn PermissionSource` surfaces on the first request through the subtree instead of at boot. The
per-endpoint `#[requires]` form does have that boot check.

> [!IMPORTANT]
> `Requires::new(PermSet::empty())` **refuses everybody**, under either mode, including an actor
> holding every permission there is. The mathematical reading of "holds all of nothing" is that
> everybody satisfies it, which would turn a set built from a filter that came back empty into an
> open door; deny-by-default is the framework's posture, so the empty requirement is the one nobody
> can satisfy. The guard says so once at `warn` when it is built, `Requires::is_vacuous()` reports
> it, and the operation's documented 403 explains it. Use `#[public]` to declare a route open on
> purpose.

## Marking an endpoint public

Deny by default is only provable if "nothing declared" is distinguishable from "declared open". That
is what `#[public]` is for: it records the declaration and clears the document-level security
requirement for the operation.

Declarations accumulate in the operation's `x-moso-authz` OpenAPI extension as a deduplicated array,
so an endpoint with both a `#[requires]` and an `Authorized<..>` parameter declares two entries. You
read them back with library functions:

| Function | Returns |
| --- | --- |
| `read_declarations(spec)` | Every `AuthzDeclaration` on one operation spec. |
| `declarations_of(operation)` | The same, from an assembled document's operation. |
| `undeclared_operations(document)` | `(method, path, source)` for every operation that declared nothing. |
| `document_problems(document)` | Every mistyped permission across the document, located by operation. |
| `boot_problems(spec)` | The same for one operation. |

`AuthzDeclaration` is `Public`, `Permissions { names, all }`, `Policy { action, resource }` or
`Unknown { names }`, where `Unknown` carries each bad name with its suggestion and is the only
variant for which `is_problem()` is true.

`moso check --authz` is what calls the last two for you:

```text
$ moso check --authz
warning[missing_authz]: `POST /posts` declares no authorization
  --> src/routes/posts.rs:41
   = note: deny by default is only provable if every operation says which it is
   = help: add `#[requires(Perm::..)]`, take an `Authorized<..>` parameter, or mark it `#[public]`

error[unknown_permission]: `POST /posts`: unknown permission `posts.pubish`
  --> POST /posts
   = note: a permission named by a string is checked against the registry at boot
   = help: did you mean `posts.publish`?
```

`missing_authz` is a warning and `unknown_permission` is an error, which is the split
`40-cli.md` specifies: one is a policy your team decides, the other cannot ever succeed at runtime.
Both levels are settable: this is the `[lints]` table that used to be read by nothing:

```toml title="moso.toml"
[lints]
missing_authz = "deny"
```

The command exits 1 when anything at `deny` fires, so it gates CI. `--strict` promotes the warnings
too, and `--json` carries every finding with its lint name, level, location, note and help.

Nothing runs it for you, so it is a line in your pipeline. If you would rather have it as a test
(which is the form that also runs under `cargo test`), the same two functions make one:

```rust title="tests/authz.rs"
#[test]
fn every_operation_declares_its_authorization() {
    let app = my_app::build().expect("the application builds");
    let document = app.openapi();

    let undeclared = moso_authz::undeclared_operations(document);
    assert!(
        undeclared.is_empty(),
        "every endpoint is either guarded or marked `#[public]`: {undeclared:#?}",
    );

    let problems = moso_authz::document_problems(document);
    assert!(problems.is_empty(), "{problems:#?}");
}
```

The third element of an `undeclared_operations` tuple is the `file:line` under the `x-moso-source`
extension. `#[endpoint]` does not write it (it is the only thing that knows where a handler lives,
and it does not record that yet), so until it does, write it yourself from a describing hook:

```rust
use moso_authz::{mark_source, source};

// `source!()` expands to `concat!(file!(), ":", line!())` where you write it.
mark_source(operation, source!());
```

`mark_source` keeps the first location written, so a location `#[endpoint]` records later cannot be
overwritten by a vaguer one. `source_of(spec)` and `source_at(operation)` read it back. An operation
nobody located reports `None` rather than a guess.

## What a refusal looks like

Every authorization failure is one of four kinds, and each becomes a different HTTP response.

| Error | Status | Body |
| --- | --- | --- |
| `Denied` | 403 | The reason in a non-production profile, a fixed sentence otherwise. Always logged at `info` on the `moso::authz` target. |
| `NotFound` | 404 | `"Post not found"`. Produced before the policy runs. |
| `UnknownPermission` | 500 | A note saying it should have been caught at boot. The request is refused. |
| `Unavailable` | 503 | The role store could not be reached, marked retryable. |

A denial is an RFC 9457 problem document like every other Moso error. In a development profile:

```json
{
  "type": "https://moso.rs/errors/forbidden",
  "title": "Forbidden",
  "status": 403,
  "detail": "required permissions on posts.publish denied: missing all of posts.publish",
  "instance": "/posts/42/publish",
  "request_id": "01J8XG7K3RQZ4B0N2Y6M9C5V1T"
}
```

In production the `detail` is replaced, unconditionally, by
`"You do not have permission to perform this action."`. The reason is written for whoever debugs it,
and "not the author" tells the caller who the author is.

> [!WARNING]
> The split is `profile != Production`, not `profile == Development`. A staging profile that is not
> literally production sends policy reasons to its callers. If that is not what you want, run staging
> under the production profile and configure it differently. See
> [configuration](./configuration.md).

A denial is not degraded. If the store your roles come from is unreachable, the request gets a 503
marked retryable, never an empty permission set: turning a cache outage into a site-wide lockout is
worse than a retry, and turning it into "all permissions" is unthinkable.

## What ends up in the OpenAPI document

Every requirement writes itself into the document, because a check that can return a 403 without the
document saying so makes the document wrong. Each guarded operation gains:

- a `security` requirement naming the scheme `moso_auth`. The name is fixed; *defining* the scheme is
  your job, through `moso-auth` or by hand, because this crate does not know how you authenticate.
- a 401, described as no credentials presented or credentials that identify nobody.
- a 403 listing each required permission with its human description, prefixed by "all of" or "at
  least one of".
- a 503 describing the unreachable role store and saying that it is never degraded.

`#[public]` clears the document-level security requirement for its operation. See
[OpenAPI](./openapi.md) for how the document is assembled.

## Auditing

Denials are audited with no wiring at all. Absent a registered sink the crate falls back to a tracing
sink on the `moso::authz::audit` target, because an authorization layer whose audit is opt-in is an
authorization layer with no audit. Choosing a sink, the record's exact fields, the database table and
the retention story are on [policies and query scoping](./policies.md#auditing-decisions), because
that is where most of the interesting decisions get made.

### Attributing capability denials

The audit entry for a `#[requires]` denial reads the actor from a request extension, because
`#[requires]` names the permission enum and never the role enum and so cannot resolve an `Actor<R>`
itself. One line puts one there:

```rust title="src/routes/mod.rs"
let router = Router::new()
    .get("/posts", moso::ep!(list))
    .post("/posts/{id}/publish", moso::ep!(publish))
    .layer(moso_authz::actor_layer::<Role>());   // ← every route registered above
```

`actor_layer` resolves the actor through your `ActorSource` before the request context exists (which
is when it has to happen, because the context snapshots the extensions) and publishes three things:
an `ActorIdentity` for the audit trail, the caller's `IpAddr` (through `ClientIp`, so
`http.trusted_proxies` decides whether `X-Forwarded-For` counts), and the whole `Actor<Role>`. That
last one is why the layer costs nothing: `Depends<Actor<Role>>` reads it back instead of asking the
source again, so an attributed *and* authorised request still resolves its actor once.

Three things it deliberately does not do. It never changes a status code: a source that returns a 401
or a 503 is logged at `debug` and the request goes on, so a `#[public]` endpoint behind the layer
still answers, and the error reaches the caller from the extractor that actually needed an actor. It
does not refuse an absent credential: that is `Actor::anonymous()`, which is what those entries
record anyway. And like every `Router::layer` call it applies to the routes registered *before* it,
so it goes last.

Without it, capability denials record an anonymous actor in the global scope. Entries from an
`Authorized<..>` parameter are attributed correctly either way, because that extractor has the actor
in hand.

## Failure modes

| Symptom | Cause |
| --- | --- |
| Boot error naming `dyn ActorSource<Role>` | No actor source registered. `Actor<Role>` declares it as a provider requirement. |
| First request through a guarded subtree 500s about `PermissionSource` | `Requires` as a router guard has no boot check. Register `ActorPermissions::<Role>::new()`. |
| 500 saying two registries are in one process | The `PermissionSource` was built against a different `permissions!` invocation from the `Perm` in `#[requires]`. An application has exactly one registry. |
| 500 naming permissions the application does not declare | A mistyped wire-name string in `#[requires("...")]`. Fails closed. |
| `#[requires]` never runs | It was written below `#[endpoint]`. Both attributes catch this now and print the corrected order. |
| Nobody is allowed, including an administrator | `Requires::new` was given an empty set. An empty requirement refuses; there is a `warn` line from where it was built. |
| Audit entries name `anonymous` | No `actor_layer` on that router. `Authorized<..>` entries are unaffected. |
| A stored permission set means the wrong thing | The registry was reordered. Bit order is declaration order. |
| Roles resolve globally but not in a tenant | `Scope::covers` is not hierarchical. Only `Global` covers other scopes. |

## See also

- [Policies and query scoping](./policies.md) for resource checks, filtering a list to what the
  caller may see, explain traces, redaction and the audit trail.
- [Authentication](./authentication.md) for the crate that produces the sessions and keys your
  `ActorSource` reads.
- [JWT and API keys](./jwt-and-api-keys.md) for where an API key's scopes come from.
- [Dependency injection](./dependency-injection.md) for `provide_dyn`, `Depends` and boot validation.
- [Errors](./errors.md) for the problem document every refusal is rendered as.
- [Security](./security.md) for the rest of the defaults.
