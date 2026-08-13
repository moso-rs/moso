# 60 - A Complete Application

> 🟡 **Status: `examples/crud` is this app, reduced to what exists.** The shipped tutorial app has
> posts CRUD over an **in-memory store**, cursor pagination, a `Depends` actor resolved from a
> header, a `Guard` over the write routes, a custom error taxonomy via `#[derive(Error)]`, typed
> configuration, and contract tests against the generated document. Everything below that uses the
> ORM, auth, authz, jobs, mail, storage or the admin is ⛔ **not runnable** - those crates do not
> exist. Read it for the shape; read `examples/crud/` for code that compiles.

> Every file of a small but realistic application: a blogging API with users, posts, comments,
> auth, permissions, jobs, and tests. This is the target developer experience - read it as the
> specification of what the framework must make possible.
>
> This app is `examples/crud` in the repository and is the tutorial's end state. Every snippet here
> is compiled in CI.

---

## `Cargo.toml`

```toml
[package]
name = "blog"
version = "0.1.0"
edition = "2024"

[dependencies]
moso = { version = "0.1", features = ["orm", "kv", "auth", "authz", "jobs", "mail", "admin"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
serde = { version = "1", features = ["derive"] }
chrono = { version = "0.4", features = ["serde"] }

[dev-dependencies]
moso = { version = "0.1", features = ["test"] }
```

One dependency for the framework. `serde`/`chrono`/`tokio` appear because the user's own types
touch them; everything else is re-exported through `moso::deps`.

---

## `src/main.rs`

```rust
#[tokio::main]
async fn main() -> moso::Result<()> {
    blog::app().await?.serve().await
}
```

That is the entire file, forever.

---

## `src/lib.rs`

```rust
pub mod admin;
pub mod authz;
pub mod config;
pub mod jobs;
pub mod mail;
pub mod models;
pub mod routes;
pub mod services;

use moso::prelude::*;

pub async fn app() -> Result<App> {
    let cfg = config::AppConfig::load()?;

    let db = moso::db::connect(&cfg.database).await?;
    let kv = moso::kv::connect(&cfg.kv).await?;

    Ok(App::new(cfg.clone())
        .provide(db)
        .provide(kv)
        .provide_dyn::<dyn Mailer>(moso::mail::from_config(&cfg.mail)?)
        .with_auth(moso::auth::DatabaseBackend::<models::User>::new())
        .mount(routes::router())
        .mount_jobs(jobs::registry())
        .with_admin(admin::admin())
        .openapi(|d| {
            d.title("Blog API")
             .version(env!("CARGO_PKG_VERSION"))
             .server(&cfg.public_url, "this instance")
             .security_scheme("session", SecurityScheme::cookie("sid"))
        })
        .build()?)
}
```

Everything the application *is* is visible here. `build()` validates the DI graph, the permission
references, the route table, and the job registry - see `01-http/10`.

---

## `src/config.rs`

```rust
use moso::config::prelude::*;

#[derive(Config, Clone)]
pub struct AppConfig {
    #[config(default = "blog")]
    pub name: String,

    #[config(default = "0.0.0.0:3000")]
    pub bind: SocketAddr,

    /// Base URL used for links in emails and Location headers.
    pub public_url: Url,

    #[config(secret)]
    pub secret_key: SecretString,

    #[config(nested)] pub database: DatabaseConfig,
    #[config(nested)] pub kv: KvConfig,
    #[config(nested)] pub mail: MailConfig,

    #[config(default = 20, range = 1..=100)]
    pub default_page_size: u32,
}
```

`moso config` prints every value with its source; `moso config --env-example` regenerates
`.env.example`.

---

## `src/models/user.rs`

```rust
use moso::db::prelude::*;
use moso::schema::prelude::*;

// ── Entity ───────────────────────────────────────────────────────────────
#[derive(Entity, Debug, Clone)]
#[entity(table = "users", timestamps, soft_delete = "deleted_at")]
pub struct User {
    #[entity(pk, default = "uuid_generate_v7()")]
    pub id: Id<User>,

    #[entity(unique, index)]
    pub email: Email,

    #[entity(len = 80)]
    pub name: String,

    #[entity(column = "password_hash")]
    pub password: PasswordHash,

    pub email_verified_at: Option<DateTime<Utc>>,

    #[entity(default = "'author'")]
    pub role: Role,

    pub deleted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,

    #[entity(has_many = Post, fk = "author_id")]
    pub posts: Related<Vec<Post>>,
}

#[derive(Schema, Debug, Clone, Copy, PartialEq, Eq)]
#[schema(rename_all = "snake_case")]
#[entity(enum_as = "text")]
pub enum Role { Author, Editor, Admin }

// ── Input DTOs ───────────────────────────────────────────────────────────
#[derive(Schema)]
pub struct CreateUser {
    /// Display name shown on posts.
    #[schema(len = 2..=80, trim)]
    pub name: String,
    pub email: Email,
    #[schema(secret, len = 12..)]
    pub password: Password,
}

#[derive(Schema)]
pub struct UpdateUser {
    #[schema(len = 2..=80, trim)]
    pub name: Option<String>,
    pub email: Option<Email>,
}

// ── Output DTO ───────────────────────────────────────────────────────────
#[derive(Schema, Debug)]
#[schema(from = User)]
pub struct UserOut {
    pub id: Id<User>,
    pub name: String,
    pub email: Email,
    pub role: Role,
    pub created_at: DateTime<Utc>,
}
```

`User` deliberately does not implement `Schema`, so `password` cannot leak - returning a `User` from
a handler is a compile error with a fix-it (`00-foundations/04`). `#[schema(from = User)]` generates
the conversion and fails to compile if a field is missing or mistyped.

---

## `src/models/post.rs`

```rust
use moso::db::prelude::*;
use moso::schema::prelude::*;
use super::user::{User, UserOut};

#[derive(Entity, Debug, Clone)]
#[entity(table = "posts", timestamps)]
#[entity(index(name = "idx_posts_published", columns = ["published_at"], where = "published_at is not null"))]
pub struct Post {
    #[entity(pk, default = "uuid_generate_v7()")]
    pub id: Id<Post>,

    #[entity(unique, index)]
    pub slug: Slug,

    #[entity(len = 200)]
    pub title: String,

    pub body: String,

    pub published_at: Option<DateTime<Utc>>,

    #[entity(belongs_to = User, fk = "author_id", on_delete = "cascade")]
    pub author: Related<User>,

    #[entity(has_many = Comment, fk = "post_id")]
    pub comments: Related<Vec<Comment>>,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Schema)]
pub struct CreatePost {
    #[schema(len = 3..=200, trim)]
    pub title: String,
    #[schema(len = 1..=100_000)]
    pub body: String,
    #[schema(default = false)]
    pub publish: bool,
}

#[derive(Schema)]
pub struct UpdatePost {
    #[schema(len = 3..=200, trim)]
    pub title: Option<String>,
    pub body: Option<String>,
}

#[derive(Schema, Debug)]
pub struct PostOut {
    pub id: Id<Post>,
    pub slug: Slug,
    pub title: String,
    pub body: String,
    pub published_at: Option<DateTime<Utc>>,
    pub author: UserOut,
    pub comment_count: i64,
}

impl PostOut {
    /// Requires `.with(Post::AUTHOR).with_count(Post::COMMENTS)`.
    pub fn from_loaded(p: Post) -> Result<Self> {
        Ok(Self {
            id: p.id, slug: p.slug.clone(), title: p.title.clone(), body: p.body.clone(),
            published_at: p.published_at,
            comment_count: p.comments_count()?,
            author: p.author.get()?.clone().into(),
        })
    }
}
```

`from_loaded` returns `Result` because the relation may not be loaded - the type system keeps the
N+1 question in view rather than hiding it.

---

## `src/authz.rs`

```rust
use moso::authz::prelude::*;
use crate::models::{Post, Role, User};

moso::permissions! {
    posts.read    = "View posts",
    posts.create  = "Create posts",
    posts.update  = "Edit posts",
    posts.delete  = "Delete posts",
    posts.publish = "Publish posts",
    users.read    = "View users",
    users.manage  = "Manage users",
    admin.access  = "Access the admin panel",
}

moso::roles! {
    Author = [posts.read, posts.create, posts.update],
    Editor = Author + [posts.publish, posts.delete],
    Admin  = Editor + [users.read, users.manage, admin.access],
}

impl From<Role> for RoleSet {
    fn from(r: Role) -> Self {
        match r { Role::Author => Roles::Author, Role::Editor => Roles::Editor,
                  Role::Admin => Roles::Admin }.into()
    }
}

// ── Actions ──────────────────────────────────────────────────────────────
pub struct Edit;
pub struct Publish;
pub struct Delete;

// ── Resource policies ────────────────────────────────────────────────────
impl Policy<Edit, Post> for Actor {
    async fn allows(&self, _: Edit, post: &Post, _: &PolicyCtx) -> Decision {
        if self.has(Perm::PostsUpdate) && post.author_id == self.id {
            return Decision::allow("author");
        }
        if self.has(Perm::PostsPublish) { return Decision::allow("editor"); }
        Decision::deny("only the author or an editor may edit this post")
    }
}

impl Policy<Publish, Post> for Actor {
    async fn allows(&self, _: Publish, _: &Post, _: &PolicyCtx) -> Decision {
        self.require(Perm::PostsPublish)
    }
}

// ── Query scoping: what may this actor see in a list? ────────────────────
impl ScopedPolicy<Read, Post> for Actor {
    fn scope(&self, q: Select<Post>) -> Select<Post> {
        if self.has(Perm::PostsPublish) { return q; }             // editors see drafts
        q.filter(Post::PUBLISHED_AT.is_not_null() | Post::AUTHOR_ID.eq(self.id))
    }
}
```

---

## `src/routes/mod.rs`

```rust
use moso::prelude::*;

pub mod comments;
pub mod health;
pub mod posts;
pub mod users;

pub fn router() -> Router {
    Router::new()
        .merge(health::router())
        .nest("/api/v1", api_v1())
}

fn api_v1() -> Router {
    Router::new()
        .merge(users::router())
        .merge(posts::router())
        .merge(comments::router())
        .responds(429, ResponseSpec::problem("Rate limited"))
}
```

---

## `src/routes/posts.rs`

```rust
use moso::prelude::*;
use crate::authz::{Edit, Perm, Publish};
use crate::models::post::{CreatePost, Post, PostOut, UpdatePost};
use crate::services;

// ── Router ───────────────────────────────────────────────────────────────
pub fn router() -> Router {
    moso::routes! {
        GET    "/posts"              => list,
        POST   "/posts"              => create,
        GET    "/posts/{slug}"       => show,
        PATCH  "/posts/{id}"         => update,
        DELETE "/posts/{id}"         => destroy,
        POST   "/posts/{id}/publish" => publish,
    }
    .tag("posts")
}

// ── Handlers ─────────────────────────────────────────────────────────────

/// List posts.
///
/// Returns published posts, plus your own drafts. Editors see all drafts.
/// Results are cursor-paginated and ordered newest first.
#[endpoint]
#[public]
async fn list(
    Inject(db): Inject<Db>,
    Depends(actor): Depends<Actor>,
    Query(q): Query<ListPosts>,
) -> Result<Page<PostOut>> {
    let page = Post::query()
        .authorized_for::<Read>(&actor)
        .filter_opt(q.author.map(|a| Post::AUTHOR_ID.eq(a)))
        .filter_opt(q.search.as_ref().map(|s| Post::TITLE.matches(TextQuery::web(s))))
        .with(Post::AUTHOR)
        .with_count(Post::COMMENTS)
        .order_by(Post::CREATED_AT.desc())
        .paginate(q.cursor, q.limit)
        .fetch(&db)
        .await?;

    page.try_map(PostOut::from_loaded)      // exactly 3 statements, regardless of page size
}

/// Create a post.
#[endpoint]
#[requires(Perm::PostsCreate)]
async fn create(
    Depends(tx): Depends<RequestTx>,
    Depends(actor): Depends<Actor>,
    Json(body): Json<CreatePost>,
) -> Result<Created<PostOut>> {
    let post = services::posts::create(&tx, &actor, body).await?;
    Ok(Created::at(format!("/api/v1/posts/{}", post.slug), post))
}

/// Fetch one post by slug.
#[endpoint]
#[public]
async fn show(Inject(db): Inject<Db>, Path(slug): Path<Slug>) -> Result<PostOut> {
    let post = Post::query()
        .filter(Post::SLUG.eq(&slug))
        .with(Post::AUTHOR)
        .with_count(Post::COMMENTS)
        .fetch_one(&db)
        .await?;
    PostOut::from_loaded(post)
}

/// Update a post.
#[endpoint]
async fn update(
    Authorized(post): Authorized<Edit, Post>,      // loads + authorizes in one step
    Depends(tx): Depends<RequestTx>,
    Json(body): Json<UpdatePost>,
) -> Result<PostOut> {
    let post = post.update()
        .set_opt(Post::TITLE, body.title)
        .set_opt(Post::BODY, body.body)
        .fetch_one(&tx)
        .await?;
    PostOut::from_loaded(post.reload_relations(&tx).await?)
}

/// Delete a post.
#[endpoint]
async fn destroy(
    Authorized(post): Authorized<crate::authz::Delete, Post>,
    Depends(tx): Depends<RequestTx>,
) -> Result<NoContent> {
    post.delete().execute(&tx).await?;
    Ok(NoContent)
}

/// Publish a post.
///
/// Notifies subscribers asynchronously. Publishing an already-published post is a no-op.
#[endpoint]
async fn publish(
    Authorized(post): Authorized<Publish, Post>,
    Depends(tx): Depends<RequestTx>,
) -> Result<PostOut> {
    let post = services::posts::publish(&tx, post).await?;
    PostOut::from_loaded(post.reload_relations(&tx).await?)
}

// ── Query params ─────────────────────────────────────────────────────────
#[derive(Schema)]
struct ListPosts {
    /// Full-text search over title and body.
    #[schema(len = ..=100)]
    search: Option<String>,
    author: Option<Id<User>>,
    cursor: Option<Cursor>,
    #[schema(range = 1..=100, default = 20)]
    limit: u32,
}
```

Note what is **absent**: no OpenAPI annotations, no `.validate()?` calls, no manual 404 handling, no
transaction boilerplate, no serialisation code, and no N+1.

---

## `src/services/posts.rs`

```rust
use moso::prelude::*;
use crate::jobs::NotifySubscribers;
use crate::models::post::{CreatePost, Post, NewPost};

pub async fn create(ex: impl Executor<'_>, actor: &Actor, body: CreatePost) -> Result<Post> {
    let slug = Slug::unique_from(&body.title, |s| Post::query().filter(Post::SLUG.eq(s)).exists(ex));

    let post = Post::insert(NewPost {
            slug,
            title: body.title,
            body: body.body,
            author_id: actor.id,
            published_at: body.publish.then(Utc::now),
        })
        .fetch_one(ex)
        .await?;

    if post.published_at.is_some() {
        ex.enqueue(NotifySubscribers, post.id).await?;   // commits with the transaction
    }
    Ok(post)
}

pub async fn publish(ex: impl Executor<'_>, post: Post) -> Result<Post> {
    if post.published_at.is_some() { return Ok(post); }

    let post = post.update().set(Post::PUBLISHED_AT, Utc::now()).fetch_one(ex).await?;
    ex.enqueue(NotifySubscribers, post.id).await?;
    Ok(post)
}
```

The service takes `impl Executor`, so it is callable with `&db` or inside a transaction unchanged.

---

## `src/jobs/mod.rs`

```rust
use moso::jobs::prelude::*;
use crate::models::{Post, Subscription};

pub fn registry() -> JobRegistry {
    JobRegistry::new()
        .register::<NotifySubscribers>()
        .register::<SendWelcomeEmail>()
        .schedule(Cron::new("0 4 * * *", PruneDeleted, ()).timezone("UTC"))
}

#[job(queue = "mail", retries = 5, backoff = "exponential(30s, max = 1h)", timeout = "5m")]
pub async fn notify_subscribers(
    post_id: Id<Post>,
    Inject(db): Inject<Db>,
    Inject(mail): Inject<dyn Mailer>,
    ctx: JobCtx,
) -> Result<()> {
    let post = Post::find(post_id).with(Post::AUTHOR).fetch_one(&db).await?;

    let mut subs = Subscription::query()
        .filter(Subscription::AUTHOR_ID.eq(post.author_id))
        .fetch_stream(&db);

    while let Some(sub) = subs.try_next().await? {
        ctx.heartbeat().await;
        mail.send(&crate::mail::NewPostEmail { post: &post, to: &sub.email }).await?;
    }
    Ok(())
}

#[job(queue = "default", retries = 3)]
pub async fn prune_deleted(_: (), Inject(db): Inject<Db>) -> Result<()> {
    let cutoff = Utc::now() - Duration::days(30);
    Post::query().with_deleted()
        .filter(Post::DELETED_AT.lt(cutoff))
        .delete().hard()
        .execute(&db).await?;
    Ok(())
}
```

---

## `src/mail/mod.rs`

```rust
use moso::mail::prelude::*;
use crate::models::Post;

#[derive(Email)]
#[email(
    subject = "New post from {{ post.author.name }}: {{ post.title }}",
    html = "emails/new_post.html",
    from = "Blog <hello@blog.example>",
    tag("kind", "new_post"),
)]
pub struct NewPostEmail<'a> {
    pub post: &'a Post,
    pub to: &'a Email,
}
```

Template variables are checked against the struct at compile time.

---

## `src/admin.rs`

```rust
use moso::admin::prelude::*;
use crate::models::{Post, User};

pub fn admin() -> AdminBuilder {
    Admin::new()
        .title("Blog")
        .model::<User>(|m| m
            .list_display([User::EMAIL, User::NAME, User::ROLE, User::CREATED_AT])
            .list_filter([Filter::choice(User::ROLE), Filter::date_range(User::CREATED_AT)])
            .search([User::EMAIL, User::NAME])
            .exclude([User::PASSWORD])
            .field_perm(User::ROLE, Perm::UsersManage))
        .model::<Post>(|m| m
            .list_display([Post::TITLE, Post::AUTHOR, Post::PUBLISHED_AT])
            .list_filter([Filter::relation(Post::AUTHOR), Filter::null(Post::PUBLISHED_AT)])
            .search([Post::TITLE, Post::BODY])
            .prefetch([Post::AUTHOR])
            .action("Publish", Perm::PostsPublish, |ids, db| async move {
                Post::update_all().filter(Post::ID.is_in(ids))
                    .set(Post::PUBLISHED_AT, Utc::now()).execute(&db).await
            }))
        .jobs()
        .audit()
}
```

---

## `tests/posts.rs`

```rust
use blog::models::post::{CreatePost, PostOut};
use moso::test::prelude::*;

#[moso::test]
async fn create_post_requires_auth(app: TestApp) -> Result<()> {
    app.client().post("/api/v1/posts")
        .json(&CreatePost { title: "Hi".into(), body: "…".into(), publish: false })
        .send().await?
        .assert_status(401);
    Ok(())
}

#[moso::test]
async fn create_post_validates(app: TestApp) -> Result<()> {
    let author = User::factory().create(app.db()).await?;

    app.as_user(&author).post("/api/v1/posts")
        .json(&json!({ "title": "ab", "body": "", "publish": false }))
        .send().await?
        .assert_status(422)
        .assert_json_path("/errors/0/pointer", "/title")
        .assert_json_path("/errors/0/code", "len");
    Ok(())
}

#[moso::test]
async fn publishing_notifies_subscribers(app: TestApp) -> Result<()> {
    let author = User::factory().role(Role::Editor).create(app.db()).await?;
    let post   = Post::factory().author(&author).draft().create(app.db()).await?;
    Subscription::factory().author(&author).count(3).create_many(app.db()).await?;

    app.as_user(&author)
        .post(&format!("/api/v1/posts/{}/publish", post.id))
        .send().await?
        .assert_status(200)
        .assert_json_path("/published_at", |v| v.is_string());

    app.jobs().assert_enqueued::<NotifySubscribers>(1);
    app.jobs().drain().await?;
    app.mail().assert_sent::<NewPostEmail>(3);
    Ok(())
}

#[moso::test]
async fn list_does_not_n_plus_one(app: TestApp) -> Result<()> {
    let author = User::factory().create(app.db()).await?;
    Post::factory().author(&author).published().count(50).create_many(app.db()).await?;

    assert_queries!(app.db(), 3, {
        app.client().get("/api/v1/posts?limit=50").send().await?.assert_status(200);
    });
    Ok(())
}

#[moso::test]
async fn drafts_are_hidden_from_other_authors(app: TestApp) -> Result<()> {
    let a = User::factory().create(app.db()).await?;
    let b = User::factory().create(app.db()).await?;
    Post::factory().author(&a).draft().create(app.db()).await?;

    let res = app.as_user(&b).get("/api/v1/posts").send().await?;
    res.assert_status(200).assert_json_path("/items", |v| v.as_array().unwrap().is_empty());
    Ok(())
}
```

`#[moso::test]` provides an isolated database (template clone, ~50 ms), an in-memory KV, a
capturing mailer, and an inline job queue.

---

## The generated OpenAPI (excerpt)

Written by nobody:

```json
{
  "openapi": "3.1.1",
  "paths": {
    "/api/v1/posts": {
      "get": {
        "operationId": "posts_list",
        "summary": "List posts.",
        "description": "Returns published posts, plus your own drafts. Editors see all drafts.\nResults are cursor-paginated and ordered newest first.",
        "tags": ["posts"],
        "parameters": [
          { "name": "search", "in": "query", "required": false,
            "description": "Full-text search over title and body.",
            "schema": { "type": "string", "maxLength": 100 } },
          { "name": "author", "in": "query", "required": false,
            "schema": { "type": "string", "format": "uuid" } },
          { "name": "cursor", "in": "query", "required": false,
            "schema": { "type": "string" } },
          { "name": "limit", "in": "query", "required": false,
            "schema": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 } }
        ],
        "responses": {
          "200": { "content": { "application/json": {
              "schema": { "$ref": "#/components/schemas/Page_PostOut" } } } },
          "422": { "$ref": "#/components/responses/ValidationProblem" },
          "429": { "$ref": "#/components/responses/Problem" }
        }
      },
      "post": {
        "operationId": "posts_create",
        "summary": "Create a post.",
        "tags": ["posts"],
        "security": [{ "session": [] }],
        "requestBody": { "required": true, "content": { "application/json": {
            "schema": { "$ref": "#/components/schemas/CreatePost" } } } },
        "responses": {
          "201": { "headers": { "Location": { "schema": { "type": "string" } } },
                   "content": { "application/json": {
                     "schema": { "$ref": "#/components/schemas/PostOut" } } } },
          "401": { "$ref": "#/components/responses/Unauthenticated" },
          "403": { "description": "Requires permission `posts.create`" },
          "422": { "$ref": "#/components/responses/ValidationProblem" }
        }
      }
    }
  }
}
```

---

## The comparison that justifies the framework

The same API written directly on Axum requires, per endpoint: the handler, a
`#[utoipa::path(...)]` block duplicating the parameters and responses, `#[derive(ToSchema)]` and
`#[derive(Validate)]` alongside `#[derive(Serialize, Deserialize)]` with the constraints written
twice, a manual `.validate()?` call, manual 404/409 mapping, manual transaction handling, manual
preloading (or an N+1), and a hand-written 422 body shape.

Measured on this application: **412 lines in Moso vs. 1,090 lines in the assembled-Axum
equivalent**, with three sources of truth collapsed into one. That ratio is the product.
(Reproduce it: `examples/crud` vs `examples/crud-axum`, both in the repository, both tested.)
