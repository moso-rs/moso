---
title: Relations
description: Declare belongs-to, has-many, has-one and many-to-many relations, load them in a bounded number of statements, and join when you need to filter by the other side.
order: 15
status: shipped
---

A relation is one `Related<..>` field on an entity plus one attribute. `#[derive(Entity)]` turns
that into three things: a constant you name in a query (`Post::AUTHOR`), an accessor you read
afterwards (`post.author()`), and a foreign key that reaches the migration generator as a real
constraint.

The accessor cannot issue a statement. `Related::get` takes `&self`, is not `async`, and has no
executor in scope, so an N+1 is not something you can write by accident: the compiler will not let
you. Loading is always explicit and always bounded. `.with(Post::AUTHOR)` costs exactly one extra
statement whether the page has ten rows or ten thousand, and a nested preload costs one more per
level, never one per row.

Everything here needs the `orm` cargo feature on the `moso` facade, which is off by default because
it pulls in a database driver.

## Declaring a relation

Two entities, one relation, both directions:

```rust title="src/models.rs"
use moso::db::prelude::*;
use moso::Entity;

/// Someone who writes posts.
#[derive(Entity, Debug, Clone)]
#[entity(table = "authors")]
pub struct Author {
    /// The primary key.
    #[entity(pk)]
    pub id: i64,
    /// Login identity; one row per address.
    #[entity(unique)]
    pub email: String,
    /// Everything this author wrote.
    #[entity(has_many = Post, fk = "author_id")]
    pub posts: Related<Vec<Post>>,
}

/// One post, by one author.
#[derive(Entity, Debug, Clone)]
#[entity(table = "posts")]
pub struct Post {
    /// The primary key.
    #[entity(pk)]
    pub id: i64,
    /// The headline.
    pub title: String,
    /// How many people read it.
    pub views: i64,
    /// Whose post this is.
    pub author_id: i64,
    /// Who wrote it.
    #[entity(belongs_to = Author, fk = "author_id")]
    pub author: Related<Author>,
}
```

`author_id` is a declared scalar field, and that is not optional. A `belongs_to` foreign key has to
be a real field on the same struct, because the preloader reads the value out of the parent in order
to batch on it. The derive refuses a `fk` that names a column with no field behind it.

The derive emits, per relation: a constant named by the upper-snake of the field
(`Author::POSTS`, `Post::AUTHOR`), and an accessor named exactly like the field
(`author.posts()`, `post.author()`).

## The four kinds

| Kind | Field type | Foreign key lives on | One statement loads it with |
| --- | --- | --- | --- |
| `belongs_to` | `Related<T>` or `Related<Option<T>>` | this table | `WHERE target.pk = ANY(keys)` |
| `has_many` | `Related<Vec<T>>` | the other table | `WHERE target.fk = ANY(keys)` |
| `has_one` | `Related<Option<T>>` | the other table | `WHERE target.fk = ANY(keys)` |
| `many_to_many` | `Related<Vec<T>>` | a join table | one query against the join table |

Two of the four are inferred from the field type when you write no relation attribute at all:
`Related<Vec<T>>` becomes a `has_many`, and `Related<T>` or `Related<Option<T>>` becomes a
`belongs_to`. `has_one` and `many_to_many` are never inferred and must be written out. In
particular, `Related<Option<T>>` with no attribute is a **nullable `belongs_to`**, not a `has_one`.

### The attribute keys

| Key | Value | What it does | Default |
| --- | --- | --- | --- |
| `belongs_to = T` | entity type | the foreign key is on this table | inferred from `Related<T>` |
| `has_many = T` | entity type | the foreign key is on `T`'s table, many rows | inferred from `Related<Vec<T>>` |
| `has_one = T` | entity type | the foreign key is on `T`'s table, at most one row | never inferred |
| `many_to_many = T` | entity type | through a join table | never inferred |
| `belongs_to_any(..)` | see below | a polymorphic target | none |
| `fk = "col"` | string | the foreign-key column | `belongs_to`: `<field>_id`. `has_many` and `has_one`: `<this entity snake_case>_id` |
| `through = "table"` | string | the join table for a `many_to_many` | `<this entity snake>_<plural target snake>`, so `Post` to `Tag` is `post_tags` |
| `left = "col"` | string | join-table column pointing back here | `<this entity snake>_id` |
| `right = "col"` | string | join-table column pointing at the target | `<target snake>_id` |
| `on_delete = "..."` | `cascade`, `restrict`, `set_null`, `set_default`, `no_action` | the FK referential action the migration emits | none |
| `on_update = "..."` | the same set | the FK referential action | none |
| `self_ref` | flag | the target is this entity, so the join gets an alias | off |
| `count_of = "relation"` | string, on a plain `Option<i64>` field | holds a row count filled by `.with_count(..)` | none |

Every foreign key, join table and join column is validated as an SQL identifier at expansion time,
so a typo is a compile error and nothing user-supplied is ever interpolated into SQL.

## Reading a relation

The accessor returns `Result<&Payload, NotLoaded>`. It is not `async`, it takes no executor, and it
does not exist in a form that could take one:

```rust title="tests/relations.rs"
#[test]
fn n2_an_unloaded_relation_returns_not_loaded_without_a_database() {
    let post = Post {
        id: 1,
        title: "Hello".to_owned(),
        views: 0,
        author_id: 1,
        author: Related::NotLoaded,
    };

    // No `Db` is in scope in this test at all: if the accessor could query, it
    // could not compile, let alone pass.
    let error = post.author().expect_err("nothing was preloaded");
    let text = error.to_string();
    assert!(
        text.contains("Post::author"),
        "the error names the relation: {text}"
    );
    assert!(
        text.contains(".with(Post::AUTHOR)"),
        "and carries a paste-able fix: {text}"
    );
}
```

`NotLoaded` prints the entity, the field, the constant to add, and the on-demand alternative:

```text
relation `Post::comments` was not loaded
  the query that produced this `Post` did not include `.with(Post::COMMENTS)`
  add it:   Post::query().with(Post::COMMENTS)
  or load on demand for a single row:   post.load(Post::COMMENTS, &db).await?
```

### Serialising a relation

`Related<T>` is `Serialize`. An unloaded relation should not appear in a response body as `null` or
`[]`, because both of those are lies: the client cannot tell "there are no comments" from "nobody
asked for the comments". Pair it with `skip_serializing_if` and the field disappears instead:

```rust
use moso_orm::Related;
use serde::Serialize;

#[derive(Serialize)]
struct PostDto {
    title: &'static str,
    #[serde(skip_serializing_if = "Related::is_not_loaded")]
    comments: Related<Vec<i32>>,
}

let dto = PostDto { title: "hi", comments: Related::NotLoaded };
assert_eq!(serde_json::to_string(&dto).unwrap(), r#"{"title":"hi"}"#);
```

### The `Related<T>` surface

`Related<T>` is a two-variant enum, `NotLoaded` and `Loaded(T)`, defaulting to `NotLoaded`.

| Method | Returns | Notes |
| --- | --- | --- |
| `get()` | `Result<&T, NotLoaded>` | what the generated accessor calls |
| `get_named(entity, field, constant)` | `Result<&T, NotLoaded>` | the same, with a message that names all three |
| `get_mut()` | `Result<&mut T, NotLoaded>` | |
| `into_inner()` | `Result<T, NotLoaded>` | |
| `is_loaded()` / `is_not_loaded()` | `bool` | `const` |
| `as_option()` / `into_option()` | `Option<&T>` / `Option<T>` | for when "unknown" and "empty" genuinely do not differ |
| `load(value)` | `()` | fills it in by hand, for a fixture or a test |
| `map(f)` | `Related<U>` | |

## Eager loading

`.with(RELATION)` on a query loads the related rows after the parents decode. One statement per
preload node, for any number of parents:

```rust
// N3: the whole table plus one relation is two statements, whatever the
// row count. This is the assertion, not an inspection of the SQL.
let mark: StatementMark = db.statements().mark();
let posts = Post::query().with(Post::AUTHOR).fetch_all(db).await?;
assert_eq!(posts.len() as i64, AUTHORS * POSTS_EACH);
assert_eq!(
    db.statements().since(mark),
    2,
    "N3: one statement for the posts, one for every author they point at"
);
assert!(
    posts.iter().all(|post| post.author().is_ok()),
    "every row's relation is loaded, from the one extra statement"
);
```

The algorithm is the same for every kind: collect the parents' keys (the primary key, or the
parent's foreign-key column for a `belongs_to`), **deduplicate** them so a hundred posts by ten
authors ask about ten, issue one statement with `WHERE key = ANY($1)`, and group the returned rows
back onto the right parent. Where a dialect has no array binding the same statement renders as
`WHERE key IN (...)` instead.

Zero statements are issued when there is nothing worth asking about: an empty parent set, or one
whose keys are all `NULL`.

### Nested preloads

Preloads nest, and the cost is one statement per level rather than per row:

```rust
// N3, nested: +1 per level, still not per row.
let mark = db.statements().mark();
let authors = Author::query()
    .with(Preload::from(Author::POSTS).with(Post::AUTHOR))
    .fetch_all(db)
    .await?;
assert_eq!(authors.len() as i64, AUTHORS);
assert_eq!(
    db.statements().since(mark),
    3,
    "N3: authors, their posts, and the posts' authors, three statements"
);
```

`Preload::from(..)` is the conversion you need before refining a node: the refinement methods live
on `Preload`, not on the relation constant. `Post::COMMENTS.preload()` is the same thing spelled
through the `Relation` trait.

### Conditional preloads

`.with_opt(Option<..>)` takes a preload that may not be there, and `.join_if` and `.filter_if` do
the same for their halves. None of them change the query's type, so a conditional preload is an
ordinary `if`:

```rust
let mut query = Post::query().filter(Post::VIEWS.gt(100));
if include_author {
    query = query.with(Post::AUTHOR);
}
let posts = query.fetch_all(&db).await?;
```

### Refining a preload

A `Preload` can be filtered, ordered, capped per parent, and nested, all before it runs. Assume here
that `Post` also carries `#[entity(has_many = Comment)] pub comments: Related<Vec<Comment>>`, and
that `Comment` has `approved` and `created_at` columns:

```rust
use moso::db::Preload;

let posts = Post::query()
    .with(
        Preload::from(Post::COMMENTS)
            .filter(Comment::APPROVED.eq(true))
            .order_by(Comment::CREATED_AT.desc())
            .limit_per_parent(3),
    )
    .fetch_all(&db)
    .await?;
```

| Method | Effect |
| --- | --- |
| `filter(predicate)` | adds a `WHERE` clause to the child query |
| `order_by(term)` | orders the children |
| `limit_per_parent(n)` | the newest (or first) `n` children **per parent**, not `n` in total |
| `columns(idents)` | restricts the projection, see the limitation below |
| `with(preload)` | nests another node under this one |
| `counting()` | fetch a count instead of rows |
| `statement_count()` | how many statements this tree will cost, before it runs |

`limit_per_parent` is the interesting one. It renders as a `ROW_NUMBER()` window on a dialect that
has window functions, and as a CTE plus a correlated `count(*)` where it does not. Both forms are
executed against the same fixture in the test suite and asserted to return identical rows in
identical order, which is the only honest way to claim the dialect difference is invisible. Either
way it is still **one** statement.

`statement_count()` is worth asserting on in a test. A three-level preload tree reports `3`, and a
change that quietly adds a round trip fails the assertion rather than the pager.

> [!WARNING]
> `Preload::columns` exists but rejects any list that is not the target entity's full column set,
> with `Error::Unsupported`. `Entity::from_row` decodes positionally, so a narrowed row would shift
> every column after the gap. The design documents describe a column-subset preload producing a
> narrow reference type; that is not built.

### Counting instead of loading

When you want "how many comments" and not the comments, `.with_count(..)` issues one
`SELECT key, count(*) ... GROUP BY key` and sends no child rows over the wire. Declare somewhere to
put the number:

```rust
#[derive(Entity, Debug, Clone)]
#[entity(table = "posts")]
pub struct Post {
    #[entity(pk)]
    pub id: i64,
    pub title: String,
    /// Every comment on this post.
    #[entity(has_many = Comment)]
    pub comments: Related<Vec<Comment>>,
    /// How many there are, from `.with_count(Post::COMMENTS)`.
    #[entity(count_of = "comments")]
    pub comments_count: Option<i64>,
}
```

`post.comments_count()` then returns `Result<i64, NotLoaded>`, with the same error shape as a
relation accessor. The count field is not a column: it is neither selected nor inserted.

One `.with(..)` and one `.with_count(..)` of the same relation write two different fields, so you
can ask for both.

## Loading after the fact

Sometimes the query that produced a row is not the place to decide what to load. Three functions
cover it, all in `moso::db::relation`:

```rust
use moso::db::relation::{load, load_many, LoadRelations};

// One row.
load(&mut post, Post::AUTHOR, &db).await?;

// A batch already in hand: still one statement, not one per row.
load_many(&mut posts, Post::COMMENTS, &db).await?;
```

`LoadRelations` is a blanket trait over every `Entity`, so with it in scope the same two are
methods: `post.load(Post::AUTHOR, &db).await?` and `Post::load_many(&mut posts, Post::COMMENTS,
&db).await?`. `load_many_with(&mut posts, preload, &db)` takes a refined `Preload` instead of a bare
relation.

> [!CAUTION]
> `load` inside a `for` loop is exactly the N+1 the rest of this page exists to prevent. Collect the
> rows first and call `load_many` once. `moso check`'s `n_plus_one` lint reports a `.load(` or
> `.fetch_` inside a loop, at `warn` by default. It is a lexical scan rather than a parse, so it
> finds the shape written plainly and will miss one spelled unusually.

## Joining

A join and a preload are different operations with different methods and no overlap. A join brings
the related entity's columns into scope so you can **filter or order by them**. It does not load
anything. Conflating the two is what produces both the row-multiplying `LIMIT` bug and the surprise
N+1.

```rust
// Filter by the author's column, load nothing extra.
let popular = Post::query()
    .join(Post::AUTHOR)
    .filter(Author::EMAIL.ends_with("@example.com"))
    .order_by(Post::VIEWS.desc())
    .fetch_all(&db)
    .await?;
```

| Method | SQL |
| --- | --- |
| `join(relation)` | `INNER JOIN` |
| `left_join(relation)` | `LEFT JOIN` |
| `right_join(relation)` | `RIGHT JOIN` |
| `full_join(relation)` | `FULL JOIN` |
| `join_with(kind, relation)` | the kind picked at runtime from `JoinKind` |
| `join_if(condition, relation)` | joins only when `condition` |
| `join_opt(Option<relation>)` | joins only when `Some` |

The joined set is not a type parameter, deliberately: with one, `join_if` could not exist, and every
predicate and helper in the expression API would grow a parameter. Instead a `Predicate` records
which entities' columns went into it, `filter` captures the caller's file and line, and the check
happens when the statement is built. Filtering on a column of an entity you never joined is
`Error::Unjoined`, and nothing is sent to the server:

```text
`User` is not joined in this query
  at src/handlers/posts.rs:48
  this expression mentions `users.is_admin`,
  but the query selects from `Post` and joins nothing
  help: add `.join(Post::AUTHOR)` before this filter
  help: or filter on the foreign key: `Post::AUTHOR_ID.eq(..)`
```

> [!NOTE]
> Joining a `has_many` multiplies parent rows: a post with ten comments comes back ten times, and
> any `LIMIT` you set counts the multiplied rows. Add `.distinct()` when you only want the parents.
> This is why preloading is two narrow round trips rather than one wide join.

## Writing through a many-to-many

There is no cascading save of an object graph. Join-table rows are written explicitly, one call at a
time, through `RELATION.on(&owner)`. Assume `Post` carries
`#[entity(many_to_many = Tag)] pub tags: Related<Vec<Tag>>`, which defaults to the join table
`post_tags` with columns `post_id` and `tag_id`:

```rust
// 6: attach / detach / sync are one statement each and idempotent.
let attachment = Post::TAGS.on(&posts[0]);
for _ in 0..2 {
    let mark = db.statements().mark();
    attachment.attach([1_i64, 2], db).await?;
    assert_eq!(db.statements().since(mark), 1, "attach is one statement");
}

let mark = db.statements().mark();
attachment.detach([1_i64], db).await?;
assert_eq!(db.statements().since(mark), 1, "detach is one statement");

let expected = if db.backend() == Backend::Postgres { 1 } else { 2 };
for _ in 0..2 {
    let mark = db.statements().mark();
    attachment.sync([2_i64, 3], db).await?;
    assert_eq!(db.statements().since(mark), expected, "sync is idempotent");
}
```

| Call | What it does | Statements |
| --- | --- | --- |
| `attach(ids, ex)` | adds links, `ON CONFLICT DO NOTHING` | 1 |
| `detach(ids, ex)` | removes those links | 1 |
| `sync(ids, ex)` | makes the set exactly `ids` | 1 on PostgreSQL, 2 on SQLite |
| `clear(ex)` | removes every link for this owner | 1 |

All four are idempotent. On PostgreSQL `sync` is a single data-modifying CTE, with the delete riding
in the insert's `WITH`:

```sql
WITH "moso_detached" AS (DELETE FROM "post_tags" WHERE "post_tags"."post_id" = $1 AND "post_tags"."tag_id" <> ALL ($2))
INSERT INTO "post_tags" ("post_id", "tag_id") VALUES ($3, $4), ($5, $6) ON CONFLICT ("post_id", "tag_id") DO NOTHING
```

On a backend that needs two statements, run `sync` inside a transaction so that a failure between
them cannot leave the set empty.

Every one of the four has a planning counterpart that returns the statements without running them
(`attach_statement`, `detach_statement`, `sync_statements`, `clear_statement`), for a test or an
audit log.

The generated accessor `post.tags()` returns `Result<&Vec<Tag>, NotLoaded>`, not an attachment
handle. The design documents spell these writes as `post.tags().attach(..)`; the real spelling is
`Post::TAGS.on(&post).attach([...], &tx).await?`. There is also no relation-scoped insert that fills
the foreign key in for you.

## Polymorphic relations

A comment that can hang off a post or another comment is a `belongs_to_any`, with a discriminator
column and an id column:

```rust
#[derive(Entity, Debug, Clone)]
#[entity(table = "comments")]
pub struct Comment {
    #[entity(pk)]
    pub id: i64,
    pub target_type: String,
    pub target_id: i64,
    #[entity(belongs_to_any(types(Post, Tag), type_column = "target_type", id_column = "target_id"))]
    pub target: Related<CommentTargetRef>,
}
```

The derive generates the enum `CommentTargetRef` with one variant per declared type, the constant
`Comment::TARGET`, and the key reader `Comment::TARGET_KEY`. `type_column` defaults to
`"target_type"` and `id_column` to `"target_id"`, so both are omissible when you use those names.

The cost is one statement **per target type actually present in the parent set**, never per row, and
none at all for a declared type that nothing in the batch points at. This is the one exception to
"one node, one statement". A `UNION` would make it one, but it would require the target tables to
have the same shape, which is the entire reason the relation is polymorphic.

## Self-referential relations

Add `self_ref` when the target is the entity itself. The generated join gets an alias, so
`posts JOIN posts` never happens:

```rust
/// The comment this one replies to.
#[entity(belongs_to = Comment, fk = "parent_id", self_ref)]
pub parent: Related<Option<Comment>>,
```

`ManyToMany` has no self-referential form, deliberately: a join table is symmetric already.

## Referential actions

`on_delete` and `on_update` reach `moso-migrate` and become real foreign-key constraints, because
application-level cascades are not atomic with concurrent writers.

```rust
#[entity(has_many = Comment, fk = "post_id", on_delete = "cascade")]
pub comments: Related<Vec<Comment>>,
```

`on_delete = "cascade"` pointing at a soft-deleted parent is a **compile error**. A soft delete is an
`UPDATE`, so it does not fire a foreign-key cascade, and a declaration that says otherwise is a
promise the database will not keep.

## Catching an N+1 that got through

Everything above makes the common accident unwriteable. These are for the rest.

**The statement counter is always on.** `db.statements()` gives you `mark()` and `since(mark)` for
the number of statements a block issued, at the cost of one relaxed increment per statement. This is
what the assertions on this page use.

**In tests, `assert_queries!` reads the same counter** and prints a numbered list on failure, naming
the repeated statement as an N+1 with the `.with(..)` fix. It takes an exact count, `at most n` for
a budget, and `+ transactions` when you want `begin` and `commit` counted:

```rust title="tests/posts.rs"
use moso_test::{TestDb, assert_queries};

let db = TestDb::acquire().await?;
let orm = db.orm().await?;

let posts = assert_queries!(&db, 2, {
    Post::query().with(Post::AUTHOR).fetch_all(orm).await?
});
```

**A per-request detector.** `relation::detect(detector, future)` installs an `NPlusOne` as a tokio
task-local for the duration of a future, records every statement by fingerprint, and logs a `warn`
on the way out when one repeated more than the threshold:

```rust
use moso_orm::relation::{NPlusOne, detect};
use std::sync::Arc;

async fn handle_a_request() {
    let detector = Arc::new(NPlusOne::new(20));
    detect(Arc::clone(&detector), async { /* run the handler */ }).await;
    assert!(detector.report().is_none());
}
```

`NPlusOne::configured(&config)` builds one at `database.n_plus_one_threshold` (20 by default, 10
under `DatabaseConfig::for_dev`). The fingerprint is a shape and never a value (`"SELECT FROM
comments"`), so a warning cannot leak a bound parameter into a log line. Because it is a task-local
rather than a global, two requests on one runtime cannot pollute each other's counts.

**A free warning with no setup.** Any handle that crosses `n_plus_one_threshold` statements logs
once, on the crossing, suggesting `.with(..)`. Per handle rather than per process, so
`db.request_scoped()` narrows it to one request.

## Failure modes

| What you did | What happens |
| --- | --- |
| Read a relation the query did not load | `NotLoaded`, naming the entity, the field and the constant. No statement |
| Filtered on an unjoined entity's column | `Error::Unjoined` at build time, with your file and line. Nothing reaches the server |
| Declared a relation field that is not `Related<..>` | compile error naming the right shape |
| Gave `fk` a column that is not a field | compile error: the preloader has to read the key out of the parent |
| Used `on_delete = "cascade"` against a soft-deleted parent | compile error |
| Preloaded a subset of columns | `Error::Unsupported`: decoding is positional |
| Used a float, timestamp, interval or array as a relation key | rejected: two values can compare equal in SQL and hash differently here, which would put a child under the wrong parent |
| Called `fetch_stream` on a query with preloads | the preloads are skipped. Batching needs the whole parent set, and pretending otherwise would reintroduce the N+1 |
| Called `fetch_all` with no `.limit(..)` | capped at 10,000 rows with a `warn` naming the entity. Say `.unlimited()` when you mean it |

## What the design documents promise and the code does not have

Four things appear in `docs/02-data/22-relations.md` and are not implemented. If you read the design
documents, do not plan around them:

- **`.join_preload()`**, for the cases where one round trip genuinely wins. No such method exists.
- **Column-subset preloads** producing a narrow reference type. `Preload::columns` rejects subsets.
- **Relation writes as accessor calls** (`post.tags().attach(..)`, `post.comments().insert(..)`).
  Use `Post::TAGS.on(&post)` instead; there is no relation-scoped insert.
- **Refinement directly on a relation constant** (`Post::COMMENTS.filter(..)`). Wrap it first with
  `Preload::from(Post::COMMENTS)`.

## See also

- [Transactions and pooling](./transactions.md), which is where a `sync` on SQLite belongs.
- [Multi-tenancy](./multi-tenancy.md): preloads carry no tenant predicate, only the foreign key.
- [Testing](./testing.md) for `assert_queries!` and `TestDb`.
