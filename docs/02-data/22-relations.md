# 22 — Relations & N+1-Safe Loading

> ⛔ **NOT IMPLEMENTED.** This document is design intent only. No crate in the workspace provides
> any of it, nothing references it, and nothing is stubbed. See
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

> The N+1 query is the single most common performance failure in ORM-backed applications. Moso's
> design makes it **structurally impossible to cause by accident** and cheap to fix when intended.

## Declaring relations

```rust
// example
#[derive(Entity)]
pub struct Post {
    #[entity(pk)] pub id: Id<Post>,
    pub title: String,

    #[entity(belongs_to = User, fk = "author_id", on_delete = "cascade")]
    pub author: Related<User>,

    #[entity(has_many = Comment, fk = "post_id")]
    pub comments: Related<Vec<Comment>>,

    #[entity(many_to_many = Tag, through = "post_tags", left = "post_id", right = "tag_id")]
    pub tags: Related<Vec<Tag>>,

    #[entity(has_one = PostStats, fk = "post_id")]
    pub stats: Related<Option<PostStats>>,
}
```

| Kind | Field type | FK lives on | Generated constant |
| --- | --- | --- | --- |
| `belongs_to` | `Related<T>` or `Related<Option<T>>` | this table | `Post::AUTHOR: BelongsTo<Post, User>` |
| `has_many` | `Related<Vec<T>>` | other table | `Post::COMMENTS: HasMany<Post, Comment>` |
| `has_one` | `Related<Option<T>>` | other table | `Post::STATS: HasOne<Post, PostStats>` |
| `many_to_many` | `Related<Vec<T>>` | join table | `Post::TAGS: ManyToMany<Post, Tag>` |

`belongs_to` also generates the scalar FK accessor `Post::AUTHOR_ID: Column<Post, Id<User>>`, so you
can filter on the FK without a join.

Self-referential (`#[entity(belongs_to = Category, fk = "parent_id", self_ref)]`) and polymorphic
(`#[entity(belongs_to_any(types(Post, Comment), type_column = "target_type", id_column = "target_id"))]`)
relations are supported; polymorphic relations produce a generated enum `CommentTargetRef`.

## `Related<T>` — the anti-N+1 type

```rust
// spec
pub enum Related<T> {
    NotLoaded,
    Loaded(T),
}

impl<T> Related<T> {
    /// The primary accessor. Never queries.
    pub fn get(&self) -> Result<&T, NotLoaded>;
    pub fn get_mut(&mut self) -> Result<&mut T, NotLoaded>;
    pub fn into_inner(self) -> Result<T, NotLoaded>;
    pub fn is_loaded(&self) -> bool;
    pub fn as_option(&self) -> Option<&T>;
}
```

`NotLoaded` renders as:

```
error: relation `Post::comments` was not loaded
  the query that produced this `Post` did not include `.with(Post::COMMENTS)`
  add it:   Post::query().with(Post::COMMENTS)
  or load on demand for a single row:   post.load(Post::COMMENTS, &db).await?
```

Serde: `Related::NotLoaded` **skips** the field entirely on serialise (rather than emitting `null`),
so a DTO built from a partially loaded entity never claims "no comments" when it means "unknown."
`#[derive(Schema)] #[schema(from = Post)]` requires an explicitly loaded relation for any field it
maps from — a compile-time nudge toward correctness.

## Preloading

```rust
// example — 2 statements total, regardless of row count
let posts = Post::query()
    .filter(Post::PUBLISHED.eq(true))
    .with(Post::AUTHOR)
    .fetch_all(&db).await?;

for p in &posts {
    println!("{} by {}", p.title, p.author.get()?.name);
}
```

Execution strategy per relation kind:

| Kind | Strategy | Statements |
| --- | --- | --- |
| `belongs_to` / `has_one` | collect distinct FKs → `WHERE id = ANY($1)` | +1 |
| `has_many` | collect parent PKs → `WHERE fk = ANY($1)` | +1 |
| `many_to_many` | one join-table query with both sides → `WHERE left = ANY($1)` | +1 |

Never a per-row query. Never a cartesian JOIN that duplicates parent rows (the other classic ORM
bug — Rails' `includes` with `references` produces it). The docs explain the trade explicitly: two
round trips beat one row-multiplying join for anything but tiny result sets, and `.join_preload()`
exists for the cases where one round trip genuinely wins.

### Nested and filtered preloads

```rust
// example — 3 statements
let posts = Post::query()
    .with(Post::AUTHOR)
    .with(Post::COMMENTS.with(Comment::AUTHOR))
    .fetch_all(&db).await?;

// filtered, ordered, limited preload — "the 3 newest comments per post"
let posts = Post::query()
    .with(Post::COMMENTS
        .filter(Comment::APPROVED.eq(true))
        .order_by(Comment::CREATED_AT.desc())
        .limit_per_parent(3))          // lateral join / window function under the hood
    .fetch_all(&db).await?;

// preload only some columns
let posts = Post::query()
    .with(Post::AUTHOR.select((User::ID, User::NAME)))   // → Related<UserRef>
    .fetch_all(&db).await?;
```

`limit_per_parent` compiles to a `ROW_NUMBER()` window on Postgres and a correlated subquery on
SQLite; the dialect difference is invisible and tested on both.

### Counting without loading

```rust
// example
let posts = Post::query()
    .with_count(Post::COMMENTS)        // adds `comments_count` to the row
    .fetch_all(&db).await?;
p.comments_count()?;                   // i64, from a scalar subquery — no rows fetched
```

### Loading after the fact

```rust
// example — for one row, or a batch
post.load(Post::COMMENTS, &db).await?;
Post::load_many(&mut posts, Post::COMMENTS, &db).await?;   // still 1 statement for the batch
```

`load_many` exists so the "I already have the rows and now I need the children" case does not
tempt anyone into a loop. `moso check` flags `.load(` inside a `for` loop over entities and
suggests `load_many`.

## Joins for filtering

Preloads fetch related rows; joins filter by them. They are separate operations with separate
methods, which is the distinction Rails conflates and everyone gets wrong.

```rust
// example — filter posts by author attribute; does NOT load the author
let posts = Post::query()
    .join(Post::AUTHOR)
    .filter(User::IS_ADMIN.eq(true))
    .fetch_all(&db).await?;

// both: filter and load
let posts = Post::query()
    .join(Post::AUTHOR)
    .filter(User::IS_ADMIN.eq(true))
    .with(Post::AUTHOR)
    .fetch_all(&db).await?;
```

Joining brings the target entity's columns into scope for `filter`/`order_by`. Referencing a column
whose entity is not joined is a **compile error**:

```
error[E0277]: `User` is not joined in this query
  --> src/routes/posts.rs:18:17
   |
18 |         .filter(User::IS_ADMIN.eq(true))
   |                 ^^^^^^^^^^^^^ column belongs to `User`, but the query selects from `Post`
   |
   = help: add `.join(Post::AUTHOR)` before this filter
   = help: or use the foreign key directly: `Post::AUTHOR_ID.eq(..)`
```

Implementation: `Select<E>` carries a second type parameter defaulted to `()` for the joined set —
`Select<Post, (User,)>`. This is the *one* place we allow the type to change shape, because the
alternative is runtime errors for typos. The joined-set parameter is defaulted and hidden in the
common case, and `moso check` has a rule that no user-facing error should print more than one join
type; if the ergonomics prove bad in the M1 dogfood, we fall back to a runtime check with an equally
good message (this is a **TODO(agent)** decision point — measure with real users before locking).

## Writing through relations

```rust
// example
let post = Post::insert(NewPost { title, author_id: user.id, .. }).fetch_one(&tx).await?;
post.tags().attach([tag_a, tag_b], &tx).await?;      // many-to-many join rows
post.tags().detach([tag_a], &tx).await?;
post.tags().sync([tag_b, tag_c], &tx).await?;        // set exactly this collection

post.comments().insert(NewComment { body, author_id }).fetch_one(&tx).await?;  // fk filled in
```

No cascading saves of object graphs. Writes are explicit statements. This is deliberate: implicit
graph persistence is where ActiveRecord-style ORMs become unpredictable, and Rust users will not
accept it.

## Cascade and referential integrity

`on_delete` / `on_update` are emitted into the migration as real FK constraints
(`cascade`, `restrict`, `set_null`, `no_action`). The database enforces them — Moso does not
simulate cascades in application code, because application-level cascades are not atomic with
concurrent writers.

Soft-deleted parents: `#[entity(soft_delete)]` on the parent makes `on_delete = "cascade"` invalid
(a compile error), because a soft delete does not fire a FK cascade. The suggested fix is a
`soft_cascade` job or a partial index.

## Testing N+1

```rust
// spec — moso-test
let counter = db.count_statements();
let posts = Post::query().with(Post::COMMENTS).fetch_all(&db).await?;
for p in &posts { let _ = p.comments.get()?; }
assert_eq!(counter.total(), 2);
```

`assert_queries!(db, 2, { ... })` is the ergonomic macro form. The reference app's tests assert
statement counts on every list endpoint, and the framework's own test suite does the same. In `dev`,
a request issuing more than `db.n_plus_one_threshold` (default 20) statements logs a warning naming
the repeated statement and the call site — a built-in N+1 detector.

## Acceptance criteria (WP-12)

1. Every relation kind loads with the documented statement count, asserted by
   `assert_queries!` on a fixture of 100 parents × 10 children.
2. Nested preload two levels deep: exactly 3 statements.
3. `limit_per_parent(3)` returns exactly 3 per parent on Postgres and SQLite, with identical
   results.
4. Accessing an unloaded relation returns `NotLoaded` with the documented message; it never
   queries. Verified by running with a closed pool.
5. Filtering on an unjoined entity's column is a compile error (UI test).
6. `sync`/`attach`/`detach` are idempotent and use one statement each.
7. The dev N+1 warning fires on a hand-written loop and names the repeated statement.
8. Serialising a `Related::NotLoaded` field omits it; a snapshot test proves it is not `null`.
