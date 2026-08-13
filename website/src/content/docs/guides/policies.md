---
title: Policies and query scoping
description: Write resource policies in plain Rust, load an authorised row with one extractor, filter lists in SQL, redact fields, and read the explain trace.
order: 28
status: shipped
---

A permission says the caller may publish posts. A policy says whether they may publish *this* post.
[Permissions and roles](./permissions.md) covers the first question; this page covers the second, and
the two features that make it useful in a real application: filtering a list query so a caller only
ever loads rows they may see, and explaining a refusal afterwards.

Policies are ordinary Rust. There is no DSL to learn, no string-matching rule engine, and no
configuration file. A policy is one `impl` block with one method that returns a `Decision`, which
means it is testable, debuggable, refactorable, and visible to `rust-analyzer`. The trade is
expressiveness: a rule that a Polar or Casbin policy expresses in one line may take you five. You get
tooling in return.

> [!IMPORTANT]
> The runtime is real and tested, and `moso authz explain` gives the trace an offline entry point,
> which is how the question usually arrives, since "why can't Alice publish" turns up as a ticket
> rather than as a request you can re-issue with an extra header. It reads a `fn authz` in your
> `src/dump.rs` and refuses in the production profile, for the same reason the header does. See
> [asking for a trace offline](#asking-for-a-trace-offline). The
> [agreement harness](#proving-the-two-policies-agree) is the test worth writing first.

## Actions are types

An action is a unit struct with a wire name. `actions!` declares them, and the `for Role;` header
binds the group to your role enum:

```rust title="src/authz.rs"
moso_authz::actions! {
    for Role;
    /// Listing posts.
    Read = "read",
    /// Making a draft public.
    Publish = "publish",
}
```

Each entry becomes a `pub struct` deriving `Clone, Copy, Debug, Default, PartialEq, Eq` with an
`impl Action` carrying `NAME`. The header additionally emits `impl HasRole`, which is how the role
type travels: `Authorized<Publish, Post>` already has three type parameters and none of them names
the role, so the role rides on the action, the one parameter every authorization question already
mentions.

The header is only required for actions used with the `Authorized` extractor. The imperative
`Actor::can` and the query-scoping `authorized_for` do not need it.

## Writing a policy

```rust title="src/authz/post.rs"
use moso_authz::{Actor, Decision, Policy, PolicyCtx};

use crate::authz::{Perm, Publish, Role};
use crate::models::Post;

impl Policy<Publish, Post> for Actor<Role> {
    async fn allows(&self, _: Publish, post: &Post, _ctx: &PolicyCtx) -> Decision {
        if self.has(Perm::PostsPublish) && post.author_id == self.id().as_str() {
            return Decision::allow("author");
        }
        if self.has(Perm::AdminAccess) {
            return Decision::allow("admin override");
        }
        Decision::deny("not the author and not an admin")
    }
}
```

Four things in that block are load bearing.

**The impl target is `Actor<Role>`, concrete, not `Actor<R>`.** A generic impl is rejected by the
orphan rule, because `R` would be an uncovered parameter in a foreign type. This is also why actions
are types local to your crate: `Publish` is what makes the impl legal.

**`allows` returns `Decision`, never `Result<Decision>`.** A policy cannot fail. By the time it runs,
everything it needs is in hand, and an unreachable store is the actor source's problem. Keeping the
return type infallible is what keeps "denied" and "broken" from being the same value.

**The reason is not optional.** Both constructors take one. It is the string that shows up in a
development 403, in the audit trail and in the explain block, and it is what makes a decision
answerable six months later.

**`PolicyCtx` is the request around the decision**: `actor()`, `scope()`, `request_id()`,
`development()` and `explain()`. You rarely read it, but it is there when a rule depends on the
request rather than the row.

## Loading an authorised row

`Authorized<A, R>` is one extractor that reads the path id, loads the row with a single `SELECT`,
runs the policy and hands you the row.

```rust title="src/routes/posts.rs"
#[endpoint]
async fn publish(post: Authorized<Publish, Post>) -> Result<Redacted<PostOut>> {
    let (post, decision) = post.into_parts();
    Ok(Redacted::new(PostOut::from(post), decision))
}
```

In order, per request, the extractor:

1. reads the profile and locates the identifier for the audit record,
2. resolves `Actor<Role>` through the request cache, so a handler that also has a `#[requires]`
   resolves its actor once,
3. loads the row. An absent row is a 404 immediately, before the policy runs,
4. builds a `PolicyCtx` for this request,
5. runs `allows`, inside a `debug_span!` on the `moso::authz` target,
6. writes an audit entry naming `Post#42`: denials always, allows only when the audit configuration
   asks for them,
7. hands you the row, or refuses.

`Authorized` is not a tuple struct, so `Authorized(post)` destructuring does not work. The resource
comes out through one of these:

| Method | Gives you |
| --- | --- |
| `*post` / `post.title` | `Deref<Target = R>`, for read-only access. |
| `.into_inner()` | The resource, dropping the decision. |
| `.into_parts()` | `(R, Decision)`. |
| `.decision()` | The decision, borrowed. |
| `.map(f)` | `Authorized<A, T, S>`: convert to a DTO, keep the decision. |
| `.into_redacted()` | `Redacted<R>` in one call, when the handler returns the entity itself. |

### Choosing what gets loaded

The third type parameter is the resource source, and it defaults to `FromPathId`.

| Source | What it does |
| --- | --- |
| `FromPathId` | Reads `{id}`, parses it as `R::Pk`, loads by primary key. The default. |
| `FromPath<N>` | Reads `{N::NAME}`. Declare the name with `path_name!`. |
| `Masked<S>` | Wraps any source and turns a *denial* into a 404. |
| your own `impl ResourceSource<R>` | Anything else: a header, a composite key, a row loaded through a service. |

```rust title="src/routes/posts.rs"
use moso_authz::{Authorized, FromPath, FromPathId, Masked};

moso_authz::path_name!(
    /// The `{post_id}` segment, for `Authorized<_, _, FromPath<PostId>>`.
    PostId = "post_id"
);

/// Reads `{id}`. These two are the same type.
type ByPathId = Authorized<Publish, Post>;
type Explicit = Authorized<Publish, Post, FromPathId>;

/// Reads `{post_id}`.
type ByNamedSegment = Authorized<Publish, Post, FromPath<PostId>>;

/// A denial becomes a 404.
type Hidden = Authorized<Publish, Post, Masked<FromPathId>>;
```

Note that `path_name!` separates entries with a **semicolon**, not a comma. The parameter name has to
travel on a marker type because `&'static str` is not a valid const generic on stable Rust, so
`FromPath<"post_id">` is not writable today.

A custom source implements `RESOURCE`, `describe`, `load` and optionally `locate`:

```rust
pub trait ResourceSource<R>: Send + Sync + 'static {
    const RESOURCE: &'static str;
    const MASK_NOT_FOUND: bool = false;

    fn describe(op: &mut OperationBuilder);
    fn load<'a>(parts: &'a mut http::request::Parts, ctx: &'a RequestCtx)
        -> BoxFuture<'a, moso::Result<Option<R>>>;
    fn locate(parts: &http::request::Parts, ctx: &RequestCtx) -> Option<String>;
}
```

`load` returns `Ok(None)` for an absent resource rather than an error, and the extractor turns that
into a 404 built from `RESOURCE`, before the policy runs. `MASK_NOT_FOUND` goes the other way: it
rewrites a *denial* as the same 404, which is what `Masked<S>` flips. `locate` is the identifier that
ends up in the audit record as `Post#42`; the default is `None`, which records the type and not the
row.

The three ways loading fails are worth knowing, because two of them are not 404s. A route with no
such path parameter is a 500 whose help text names the parameter and suggests `path_name!`. A
parameter that will not parse as `R::Pk` is a 400 with a field error at `/path/{name}`, deliberately
not a 404, so a typo is not hidden. Only a missing row is a 404.

### 404 before 403, and when to invert it

The row is loaded before the policy runs, so an unauthorised caller can tell an existing id from a
missing one. That is the right default: the alternative, a 403 on an id that does not exist, confirms
which ids do.

`Masked<S>` inverts it for the cases where existence itself is the secret, an invoice number that
increments, or a document whose presence implies a deal:

```rust title="src/routes/invoices.rs"
/// Show one invoice.
#[endpoint]
async fn show(invoice: Authorized<Read, Invoice, Masked<FromPathId>>) -> Result<Json<InvoiceOut>> {
    Ok(Json(InvoiceOut::from(invoice.into_inner())))
}
```

It is a type rather than a builder method on purpose: the choice changes the endpoint's documented
responses, so it belongs in the signature that documents the endpoint. Accept the trade, though. Your
callers can no longer tell a typo from a permission problem, and that is a real support cost.

### Asking without an extractor

Inside a job, a CLI, a test or a handler that already has the row, call the policy directly:

```rust
// `?` converts a `moso_authz::Error::Denied` into the 403 problem document.
let decision = actor.can(Publish, &post).await.into_result("publish", "Post#42")?;
```

`can` builds a detached context. `can_with(action, &resource, &ctx)` takes one you already have and
adds the tracing span. `detached_ctx()` is the same context `can` builds, exposed so a job can run a
policy exactly the way a handler does. `Decision::into_result(action, resource)` turns a denial into
a `moso_authz::Error::Denied` carrying the reason, and keeps the obligations on the allow path.

One caveat about that `?`. The blanket `From<moso_authz::Error>` conversion cannot see the profile,
so it withholds the reason unconditionally: a hand-rolled check gives the caller the generic denial
even in development. When you want the development reason, call
`error.into_response(development)` yourself with the profile from `ctx.state().profile()`. The
`Authorized` extractor already does.

## Scoping queries

This is the feature that separates an authorization layer from a decorator. A policy answers a
question about one row. A list endpoint has no row yet, and filtering after loading is both slower
and, more importantly, wrong: the page total counts what the table holds, not what the caller may
see.

`ScopedPolicy` contributes a `WHERE` clause instead:

```rust title="src/authz/post.rs"
use moso::db::Select;
use moso_authz::ScopedPolicy;

impl ScopedPolicy<Read, Post> for Actor<Role> {
    fn scope_query(&self, query: Select<Post>) -> Select<Post> {
        if self.has(Perm::AdminAccess) {
            return query;
        }
        query.filter(Post::PUBLISHED.eq(true) | Post::AUTHOR_ID.eq(self.id().as_str()))
    }
}
```

Apply it at the call site:

```rust title="src/routes/posts.rs"
#[endpoint]
async fn list(
    Depends(actor): Depends<Actor<Role>>,
    Inject(db): Inject<Db>,
) -> Result<Json<Vec<PostOut>>> {
    let posts = Select::<Post>::new()
        .authorized_for::<Read>(&actor)
        .order_by(Post::ID.asc())
        .fetch_all(&*db)
        .await?;

    Ok(Json(posts.into_iter().map(PostOut::from).collect()))
}
```

`authorized_for` comes from the `AuthorizedQuery` trait, which is blanket implemented for every
`Select<E>`, so import `moso_authz::AuthorizedQuery` (or its prelude) to see the method.

Three properties are worth stating precisely.

**It is shape stable.** `Select<E>` goes in and `Select<E>` comes out, so authorization composes with
`filter`, `order_by` and `paginate` in any order, and adding it to an existing query changes nothing
else about that query.

**It is synchronous.** `scope_query` runs while the statement is being built. An `await` there would
mean a query per query. If your rule needs data the actor does not already carry, load it in your
`ActorSource` and hang it off the actor's permissions.

**It costs nothing extra.** The filter is a `WHERE` clause, so an authorised `fetch_all` is still one
statement rather than a wide read followed by a `retain` in Rust. And because the count is computed
from the filtered query, pagination totals are honest:

```rust
let page = Select::<Post>::new()
    .authorized_for::<Read>(&alice)
    .order_by(Post::ID.asc())
    .paginate_offset(1, 2)
    .fetch(db)
    .await
    .expect("the page loads");

assert_eq!(page.items.len(), 2, "the first page holds two rows");
assert_eq!(
    page.total,
    Some(5),
    "the total counts what Alice may see, not what the table holds",
);
```

Six rows in the table, five Alice may read. Filtering after loading gives you the same five rows and
a total of six, and the bug only shows up in the page count.

`authorized_for_if::<Read>(condition, &actor)` applies the same filter conditionally, for an endpoint
that has a legitimate unfiltered mode.

> [!WARNING]
> `scope_query` takes `Select<R>`, which is `Select<R, ()>`. A multi-tenant entity produces
> `Select<E, NeedsTenant>`, so you must call `.scoped(tenant)` **before** `authorized_for`. That is
> deliberate: an authorization filter on an unscoped query is a filter across every tenant's rows,
> and making the ordering a compile error is better than making it a review comment. See
> [multi-tenancy](./multi-tenancy.md).

## Proving the two policies agree

`ScopedPolicy` and `Policy` are two hand-written impls of two traits, in two different languages
(one Rust `if` chain, one `WHERE` clause), and nothing in the type system relates them. When they
drift, one of two things happens, and the first is an incident:

| Drift | What it is |
| --- | --- |
| The filter admits a row the policy denies | a **data leak**: a list endpoint hands over rows the detail endpoint 403s on |
| The filter hides a row the policy allows | a missing row, reported as a bug nobody can reproduce |

Neither is catchable by reading the two blocks side by side, because the whole point of `scope_query`
is that it does not look like `allows` even when it agrees. So there is a harness, and it runs against
a real database, because a filter is SQL:

```rust title="tests/authz.rs"
#[tokio::test]
async fn the_read_policy_and_its_filter_admit_the_same_rows() {
    let db = test_database().await;
    let rows = seed(&db).await;

    moso_authz::testing::assert_policies_agree::<Read, Post, Role>(
        &db,
        &[alice(), bob(), an_admin(), Actor::anonymous()],
        &rows,
    )
    .await;
}
```

For each actor it runs the query `scope_query` builds, runs `allows` over every row you gave it, and
compares the two sets of primary keys. A disagreement panics with the whole report: leaks named as
`LEAKED`, hidden rows as `HIDDEN`, one line each with the actor and the row.

Give it the actor *shapes* your two impls branch on (an owner, a peer, an administrator, nobody),
not every actor in your database. One per branch is what exercises them.

| Item | What it gives you |
| --- | --- |
| `assert_policies_agree::<A, R, Role>(db, actors, rows)` | The assertion. Panics with the report, and panics if it compared nothing. |
| `policy_agreement::<A, R, Role>(db, actors, rows)` | The same, as an `Agreement` you can inspect. |
| `Agreement::holds()` / `leaks()` | Whether they agreed at all, and whether any disagreement is the direction that leaks. |
| `Agreement::comparisons()` | How many (actor, row) pairs were checked. Assert on it, or an empty list passes vacuously. |
| `Agreement::render()` | The report, one disagreement per line. |

The rows must already be in the table: the harness reads through the database so the `WHERE` clause
is the one the database actually ran, and it needs the values in hand to ask the row policy about
them. Skip the test when `DATABASE_URL` is unset, the way every other database test in a Moso
application does.

None of this removes the underlying advice: where the row-level rule is expressible as a predicate,
write the predicate once in `scope_query` and have `allows` call the same helper. The harness is what
tells you when you stopped doing that.

## Redacting a response

A `Decision` can carry obligations: things that must happen if the answer is yes. That turns "may
they read this" into "they may read this with the salary removed", without a second DTO and a second
endpoint.

```rust title="src/authz/post.rs"
impl Policy<Read, Post> for Actor<Role> {
    async fn allows(&self, _: Read, post: &Post, _ctx: &PolicyCtx) -> Decision {
        if post.published {
            // A peer may read it, but not the author's identity.
            let decision = Decision::allow("published");
            if self.has(Perm::AdminAccess) {
                return decision;
            }
            return decision.with_obligation(Obligation::redact("/author_id"));
        }
        if post.author_id == self.id().as_str() || self.has(Perm::AdminAccess) {
            return Decision::allow("author or admin");
        }
        Decision::deny("draft, and not the author")
    }
}
```

Return `Redacted<T>` from the handler and the obligations are applied to the serialised body:

```rust
// A peer may read the post, but not who wrote it or what the reviewer said.
let peer = Decision::allow("published")
    .with_obligation(Obligation::redact("/author_id"))
    .with_obligation(Obligation::mask("/reviewer_note", 5));

let body = Redacted::new(post_out(), peer).to_json().expect("json");

assert_eq!(
    serde_json::to_string_pretty(&body).expect("render"),
    concat!(
        "{\n",
        "  \"id\": 1,\n",
        "  \"title\": \"Alice, published\",\n",
        "  \"reviewer_note\": \"•••••••••twice\"\n",
        "}",
    ),
);
```

| Obligation | Effect on the body |
| --- | --- |
| `Obligation::redact("/author_id")` | Removes the field. |
| `Obligation::mask("/card_number", 4)` | Replaces all but the last four characters with a bullet. |
| `Obligation::Custom { key, value }` | Nothing on the wire. Your code interprets it. |

Note that `author_id` is **gone**, not null. A null still says the field exists and that this caller
was not allowed to see it, which is one bit more than they should have.

The pointers are RFC 6901 JSON Pointers, so they reach into nested objects and arrays, remove array
elements, and honour the `~0` and `~1` escapes. A pointer that matches nothing is a no-op: failing
the request instead would turn a policy refactor into an outage.

Three sharp edges:

- **Masking a non-string changes its JSON type.** The value is rendered first, so `123456789` masked
  with `keep = 4` becomes the string `"•••••6789"`, not a number. `keep` counts `char`s.
- **`Redacted<T>` serialises twice when obligations exist**, because applying a pointer needs a JSON
  tree. A decision with no obligations serialises once, so only responses that actually redact pay.
- **`Custom` is inert on the wire by design.** A serialiser that guessed at "require
  re-authentication" would be inventing behaviour. An unrecognised obligation never causes a
  recognised one to be skipped.

`Redacted::plain(value)` wraps a value with an allow and no obligations, `with_status(code)` sets a
non-200 status, and `Decision::apply_obligations(&mut value)` applies them by hand to any
`serde_json::Value` when you are not returning a `Redacted`.

## The explain trace

A policy can record the checks it made, whether or not anybody asked:

```rust title="src/authz/post.rs"
impl Policy<Publish, Post> for Actor<Role> {
    async fn allows(&self, _: Publish, post: &Post, _ctx: &PolicyCtx) -> Decision {
        let author = post.author_id == self.id().as_str();
        if self.has(Perm::PostsPublish) && author {
            return Decision::allow("author")
                .with_step(TraceStep::new("has(posts.publish)", true))
                .with_step(TraceStep::new("post.author_id == actor.id", true));
        }
        if self.has(Perm::AdminAccess) {
            return Decision::allow("admin override")
                .with_step(TraceStep::new("has(admin.access)", true));
        }
        Decision::deny("not the author and not an admin")
            .with_step(TraceStep::new(
                "has(posts.publish)",
                self.has(Perm::PostsPublish),
            ))
            .with_step(
                TraceStep::new("post.author_id == actor.id", author).with_detail(format!(
                    "author={}, actor={}",
                    post.author_id,
                    self.id()
                )),
            )
            .with_step(TraceStep::new("has(admin.access)", false))
    }
}
```

Recording steps unconditionally is the intended style. `PolicyCtx::explain()` gates whether the trace
is rendered, and a policy sprinkled with `if ctx.explain()` reads worse for no real saving.

### Asking for a trace

Send the header on the request that was refused:

```bash
curl -i -X POST http://localhost:3000/posts/456/publish \
  -H 'X-Moso-Authz-Explain: 1'
```

The value can be `1`, `true`, `yes` or `on`, in any case, with surrounding whitespace. The parser is
generous because this is a debugging affordance typed by hand into a `curl` command. When it is
honoured, the 403's `detail` becomes the decision's reason, a blank line, and the rendered block.

> [!CAUTION]
> The header is honoured only when the profile is not production. `PolicyCtx::for_request` stores
> `explain && development`, and the header parser refuses in production independently, so two places
> agree. An explain trace hands your authorization model to whoever asked for it.

### Asking for a trace offline

```text
$ moso authz explain --actor usr_123 --action publish --resource Post#456

DENY  posts.publish

  actor      usr_123 (alice@example.com)
  roles      Editor (global)
  perms      posts.read, posts.create, posts.update  (from Editor)
  required   posts.publish
  policy     Policy<Publish, Post> for Actor  src/authz/post.rs:14
  reason     "not the author and not an admin"
  trace
    ✓ has(posts.publish)
    ✗ post.author_id == actor.id  author=usr_9, actor=usr_123
```

`--scope <KEY>` evaluates in a scope other than global. `--json` returns the structured
`Explanation` rather than the block.

The block is printed **verbatim**. `Explanation::render` is snapshot tested where it is written, and
a second renderer in the CLI would be a second thing to keep in step whose first divergence nobody
would notice. The CLI's job is to ask, not to lay out.

> [!CAUTION]
> `moso authz explain` refuses in the production profile and says so, exiting 1 unless
> `--allow-production` is passed. This is the same line the header holds, and it is enforced in the
> application rather than in the CLI, because the application is the half that knows its own
> profile, and a check that lived only in the CLI would be a check an older CLI does not have.

The application's half is the `explain` view of `fn authz` in `src/dump.rs`, which builds the
`Explanation` from your actor source, your role source and your policy registry, and answers with
`explanation.render()` under `rendered` plus the structured form. `moso new` writes a stub that
already holds the production refusal. Keep that check ahead of whatever you build the explanation
from, so an incomplete implementation cannot leak a trace it has not finished assembling.

### Reading the block

```text
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

| Row | What it tells you |
| --- | --- |
| The first line | `ALLOW` or `DENY`, then the subject: the first required permission, else the action, else the resource. |
| `actor` | The identifier, and a label if one was attached. |
| `roles` | Every role held, each with its scope. |
| `perms` | The effective permission set. `(from Editor)` appears only when exactly one role is held, because with two it would be a guess. |
| `required` | What the check wanted. |
| `policy` | The impl's signature and its `file:line`. |
| `reason` | The decision's reason, quoted. |
| `trace` | The steps the policy recorded, in order. |

Rows for empty sections are omitted entirely. The `✓` and `✗` prefixes and the `(detail)` suffix are
what the renderer adds; everything else on a trace line is the string your policy wrote, which is why
the example lines end in an arrow and a value.

The rendered block is a contract: its column widths and row order are snapshot tested against the
design document, and one function builds every explanation, so a block from a live 403 and a block
from an offline tool are the same block.

### Naming the policy in the output

The `policy` row comes from a `PolicyRef`, and this crate cannot know where you wrote an `impl`. Build
a registry at boot with the `policy!` macro, which captures the file and line, and register it:

```rust title="src/main.rs"
let policies = moso_authz::policy!(PolicyRegistry::new(), Publish, "Post");
let policies = moso_authz::policy!(policies, Read, "Post");

App::new(config)
    .provide(policies)          // ← the `policy` row in a live explain block
    .provide_dyn::<dyn ActorSource<Role>>(Arc::new(SessionActor))
```

`Authorized<..>` looks for a `PolicyRegistry` in the provider map, hands it to the `PolicyCtx`, and
uses it when it renders an explanation, so the block from a live 403 names the impl and its
`file:line`, exactly as an offline block would. Inside a policy, `ctx.policy_for(action, resource)`
is the same lookup.

An application that registers nothing is not broken: the block is rendered without that row, because
a location this crate invented would be worse than an admitted gap. A block served over HTTP still
has no `required` row and no actor label: those come from a capability check and from your
application respectively.

Building one by hand is unchanged: `registry.lookup(action, resource)` and
`Explanation::by_policy(policy_ref)`.

You can also build an explanation directly from any decision:

```rust
let explanation = actor.explain(&decision)
    .labelled("alice@example.com")
    .with_requirement(required, Some("publish"), Some("Post#456"), None)
    .by_policy(registry.lookup("publish", "Post").expect("registered"));

println!("{}", explanation.render());
```

That is how you get the full block into a support tool, a test, or a log line, and it is what the
`explain` view of `src/dump.rs` calls, so `moso authz explain` and your support tool render the same
thing from the same code.

## Auditing decisions

Every denial is audited whether or not you wire anything. Absent a registered sink the crate falls
back to a tracing sink on the `moso::authz::audit` target, because an authorization layer whose audit
is opt-in is an authorization layer with no audit.

| Sink | Use it for |
| --- | --- |
| `TracingAuditSink` | The default. Your log pipeline is the system of record. |
| `MemoryAuditSink` | Tests. `entries()`, `denials()`, `len()`, `clear()`. |
| `TableAuditSink` | The `moso_authz_audit` database table. |
| `BatchingAuditSink` | Wraps any of the above and writes in batches. |
| your own `impl AuditSink` | Anything else: a queue, a SIEM, an append-only file. |

```rust title="src/main.rs"
let mut audit = AuditConfig::default();
audit.allows = true;

App::new(config)
    .provide_dyn::<dyn AuditSink>(Arc::new(TableAuditSink::new(db.clone())))
    .provide(audit)
```

`AuditConfig` is `#[non_exhaustive]`, so you start from `default()` and assign fields rather than
writing a struct literal.

| Field | Default | Read by |
| --- | --- | --- |
| `denies` | `true` | every check, on the deny path |
| `allows` | `false` | every check, on the allow path |
| `retention_days` | `365` | `AuditConfig::retention_cutoff`, `TableAuditSink::purge_expired`, `spawn_purge` |
| `batch_size` | `1` | `BatchingAuditSink`, where one means write-through |
| `flush_interval` | 5 s | the batching sink's flusher, so a partial batch is not held forever |

An individual endpoint forces an allow entry with `#[requires(Perm::UsersSuspend, audit)]`, which is
what the `forced` argument means:

```rust
use moso_authz::audit::record_if_wanted;
use moso_authz::{ActorId, ActorKind, AuditConfig, AuditRecord, MemoryAuditSink, Scope};

let sink = MemoryAuditSink::new();
let config = AuditConfig::default();
let allow = AuditRecord::allow(
    ActorId::new("usr_1"),
    ActorKind::User,
    Scope::Global,
    "posts.read",
    "viewer",
);

// Allows are off by default and this call site did not ask for one.
record_if_wanted(&sink, &config, allow.clone(), false).await;
assert!(sink.is_empty());

// `#[requires(.., audit)]` is what `forced` means.
record_if_wanted(&sink, &config, allow, true).await;
assert_eq!(sink.len(), 1);
```

### What a record holds

Eleven fields, and the list is deliberately short: `at`, `actor`, `actor_kind`, `scope`, `action`,
`resource`, `outcome`, `reason`, `request_id`, `ip`, `route`. No email, no name, no request body. An
audit log that accumulates personal data is a liability with a retention policy attached, and every
field it does not hold is a field that cannot leak.

Two details follow from that. `route` is the matched route *pattern*, never the raw path, because a
raw path is unbounded and ends up in a metric label. And `reason` is truncated at 200 characters,
because a policy is free to put a row's contents in its reason and the audit trail is exactly where
those contents must not accumulate.

`AuditSink::record` returns nothing. It cannot fail the request, because the request has already been
decided by the time an entry is written, and turning a full audit table into a 500 on every endpoint
is a worse outcome than a logged write failure. If you need the opposite, write the row yourself
inside your own transaction: `AuditEntry::COLUMNS` and `table::insert_sql(backend)` are public for
exactly that.

A sink that cannot write logs at `error` on the `moso::authz::audit` target **and** counts the entry:
`moso_authz::audit::audit_dropped()` is a process-wide count of entries this process failed to write,
published through the process-wide metrics sink (`moso_core::middleware::metrics`) as
`moso_authz_audit_dropped`, so an exporter reads it alongside the other Moso counters. A hand-written
sink calls `count_dropped(n)` on its own failure path so the losses land in one series rather than
two.

A denial is logged at `info`, not `warn`. A denial is a normal outcome of a correct authorization
model, and a log level that says otherwise trains operators to ignore it.

### Writing in batches

One `INSERT` per denial is fine at ten denials a minute and is the reason somebody turns auditing off
at ten thousand. `BatchingAuditSink` wraps any other sink and hands it whole batches:

```rust title="src/main.rs"
let (sink, guard) = BatchingAuditSink::start(
    Arc::new(TableAuditSink::new(db.clone())),
    &audit,
);

App::new(config)
    .provide(audit)
    .provide_dyn::<dyn AuditSink>(sink)
    .lifespan(move |_| async move { Ok(guard) })
    .on_shutdown(|resolver| async move { moso_authz::audit::flush_audit(&resolver).await })
```

`start` reads `batch_size` and `flush_interval` from the configuration and spawns the flusher, so
call it from inside the runtime: `main`, an `on_startup` hook, or a `lifespan` factory. It panics
outside one.

Three properties, and each is a decision worth knowing:

- **Memory is bounded at `batch_size` entries.** The `record` call that fills the buffer takes the
  batch and writes it before returning, so a slow inner sink slows the recording task rather than
  growing a queue behind it.
- **Nothing is lost while the process lives.** An entry leaves the buffer exactly once, into whichever
  writer took it, and `record` cannot fail. The only way to lose one is to exit without flushing.
- **Nothing is held forever.** The flusher writes whatever is buffered every `flush_interval`, so a
  system quiet enough never to fill a batch still gets its entries out.

### Flushing at shutdown

`AuditSink::flush` documents the shutdown drain as its call site, and `moso-core` knows nothing about
this crate, so the connection is one line you write:

```rust title="src/main.rs"
.on_shutdown(|resolver| async move { moso_authz::audit::flush_audit(&resolver).await })
```

`flush_audit` resolves whatever `dyn AuditSink` is registered and flushes it. An application that
registered none is a no-op, because the fallback tracing sink buffers nothing.

The `AuditGuard` from `start` is the second half. `guard.shutdown().await` is the path that is
guaranteed to write: it stops the timer, writes the last partial batch, and waits for it. Dropping
the guard, which is what a `lifespan` does, stops the timer, and then writes the last batch by
blocking the worker when the runtime is multi-threaded, which is the one `App::serve` runs on.
Anywhere else it cannot await at all, so it logs at `error` and counts the entries through
`audit_dropped` rather than claiming they were written. Wire both: the hook is the belt, the guard
is the braces.

### The database table

`TableAuditSink` writes to `moso_authz_audit`. Create it either way:

```rust
// Directly, for a development database or a test.
sink.create_table().await?;

// Or lift the DDL into a reviewable migration.
for statement in moso_authz::table::create_table_sql(moso::db::Backend::Postgres) {
    println!("{statement}");
}
```

`create_table_sql` returns three statements: the table plus indexes on `(actor, at)` and on `(at)`.
PostgreSQL gets `bigserial` and `timestamptz`, other backends get `integer primary key autoincrement`
and `text`. Read rows back with `AuditEntry::select_list()` and `AuditEntry::from_row(&row)`.

### Ageing entries out

`retention_days` is the number, and three calls apply it:

| Call | What it does |
| --- | --- |
| `config.retention_cutoff(Utc::now())` | The timestamp entries older than which have aged out. `None` when `retention_days` is zero, which means keep forever. |
| `sink.purge_expired(&config).await?` | One pass, returning how many rows went. |
| `sink.spawn_purge(config, every)` | The same on a timer, returning a `PurgeTask` that stops when you drop it. |

```rust title="src/main.rs"
let purge = sink.spawn_purge(audit, Duration::from_secs(60 * 60 * 6));

App::new(config)
    .provide_dyn::<dyn AuditSink>(Arc::new(sink))
    .lifespan(move |_| async move { Ok(purge) })
```

The first purge runs one interval in, not at boot, because a deploy loop restarting every minute
would otherwise run a full-table delete every minute. A failed purge is logged and retried on the next
tick, because a database that is briefly unreachable must not switch retention off until somebody
notices. `purge(cutoff)` is still there for a caller with its own idea of a cutoff.

> [!NOTE]
> The audit entry for a `#[requires]` denial reads the actor from a request extension. Add
> `.layer(moso_authz::actor_layer::<Role>())` to the router so something puts one there; see
> [attributing capability denials](./permissions.md#attributing-capability-denials). Entries from an
> `Authorized<..>` parameter are attributed correctly either way.

## What the OpenAPI document says

An `Authorized<A, R, S>` parameter contributes the path parameter, a 404, the shared 401 and 503, a
403 naming the action and the resource and explaining the development and production reason split,
and an `x-moso-authz` declaration recording the action and resource pair. `Redacted<T>` contributes
its documented 200.

Those declarations are what a deny-by-default audit reads back. The functions are on
[permissions and roles](./permissions.md#marking-an-endpoint-public), where the same check runs as a
CI step or a test.

## Failure modes

| Symptom | Cause |
| --- | --- |
| `Actor<Role>` does not implement `Policy<Publish, Post>` | The impl is missing, or it was written generically as `Actor<R>`, which the orphan rule rejects. |
| `Authorized<..>` will not compile | The action has no `HasRole` impl. Add the `for Role;` header to `actions!`. |
| Internal error naming the path parameter | The route has no `{id}` segment. Use `FromPath<N>` with `path_name!` for a differently-named one. |
| 400 with a field error at `/path/{id}` | The path segment does not parse as `R::Pk`. Not hidden as a 404, so a typo is visible. |
| 500 on the first request naming `Db` | `FromPathId` runs a `SELECT` but the `Db` requirement is not in the extractor's boot check, so a missing provider shows up on the first request. |
| A list endpoint returns rows the caller may not see | No `ScopedPolicy` impl, or `authorized_for` is missing from that query. Nothing enforces its presence. |
| A list shows a row the detail endpoint 403s on | `scope_query` and `allows` have drifted. `assert_policies_agree` reports it as `LEAKED`. |
| A row opens by its id but never appears in a list | The same drift, the other way. The harness reports it as `HIDDEN`. |
| A page total counts rows the caller cannot see | Filtering happened after loading rather than in `scope_query`. |
| `scope_query` will not accept the query | The entity is multi-tenant. Call `.scoped(tenant)` first. |
| A redaction did nothing | The pointer does not match. Pointers that match nothing are silent no-ops. |
| Policy reasons appearing in staging responses | The split is `profile != Production`. |
| The explain header is ignored | The profile is production. It is refused there unconditionally. |
| `moso authz explain` exits 1 naming `--allow-production` | The same rule, offline. The binary detected a production profile; pass the flag if this terminal is the right place for the trace. |
| `moso authz explain` says the project does not use `moso-authz` | `fn authz` in `src/dump.rs` is still the stub `moso new` wrote. The CLI sees only what that function answers. |

## See also

- [Permissions and roles](./permissions.md) for the registry, roles, the actor and `#[requires]`.
- [Multi-tenancy](./multi-tenancy.md) for why `scope_query` demands an already-scoped query.
- [Responses](./responses.md) for how `Redacted<T>` fits the response model.
- [Observability](./observability.md) for the `moso::authz` and `moso::authz::audit` targets, and for
  where `moso_authz_audit_dropped` belongs in an exporter.
- [Testing](./testing.md) for `MemoryAuditSink` and `MemoryRoleSource` in a test app.
