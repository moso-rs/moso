//! The posts repository, over Moso's ORM.
//!
//! This module is the seam the tutorial is built around: the handlers speak to
//! it, and it speaks to the database. Everything in it goes through the ORM —
//! [`Post::query`], [`Post::insert`], [`Post::update`], [`Post::delete`] and the
//! keyset [`Select::paginate`](moso::db::Select::paginate) — so there is not one
//! hand-written SQL string above `moso-sql`, and swapping SQLite for PostgreSQL
//! is a URL, not an edit here.
//!
//! # Where the schema comes from
//!
//! A production application runs `moso db migrate` against migrations generated
//! from the entity. An example that must boot with no server and no setup
//! creates its single table inline at boot with [`create_schema`] — the honest
//! shortcut, called out as such. The `DROP` in front of the `CREATE` is what
//! makes a fresh `cargo run` start from an empty table every time.
//!
//! # Errors are values
//!
//! The ORM's own [`moso::db::Error`] is mapped here onto the taxonomy the API
//! speaks: an absent row becomes [`BlogError::PostNotFound`] (a 404), a
//! forged pagination cursor becomes a 422 pointed at `/cursor`, and anything
//! else — a real database fault — becomes an honest 500.

use chrono::Utc;
use moso::db::prelude::*;
use moso::response::Page;
use moso::response::cursor::CursorCodec;
use moso::schema::{Cursor, Id, Slug, ValidationErrors, codes};
// The ORM prelude above re-exports its own `Error`/`Result`; these explicit
// imports shadow them, so the store's own functions speak the API's error type.
use moso::{Error, Result};

use crate::error::BlogError;
use crate::models::post::NewPost;
use crate::models::{CreatePost, Post, UpdatePost};

/// The DDL for the one table this example owns.
///
/// SQLite is dynamically typed, so these declarations are affinities rather than
/// hard constraints; the `moso db make-migration` a real project would run
/// emits the same shape from the entity descriptor, per backend.
const SCHEMA: &[&str] = &[
    "DROP TABLE IF EXISTS posts",
    "CREATE TABLE posts (\
        id blob PRIMARY KEY, \
        slug text NOT NULL UNIQUE, \
        title text NOT NULL, \
        body text NOT NULL, \
        author text NOT NULL, \
        published_at text, \
        created_at text NOT NULL, \
        updated_at text NOT NULL)",
];

/// Create the `posts` table, dropping any previous one.
///
/// Called once at boot, before the store answers a request.
///
/// # Errors
/// Any database failure, as an internal 500 — a table that will not create is
/// not something a client can fix.
pub async fn create_schema(db: &Db) -> Result<()> {
    for statement in SCHEMA {
        RawQuery::new(*statement)
            .execute(db)
            .await
            .map_err(Error::internal)?;
    }
    Ok(())
}

/// Insert a post built from a validated request body.
///
/// The slug is derived from the title and made unique by suffixing, which is the
/// behaviour a blog wants: two posts called "Hello" become `hello` and `hello-2`
/// rather than a 409 the author cannot act on.
///
/// # Errors
/// [`BlogError::UnsluggableTitle`] if the title has nothing sluggable in it, and
/// an internal 500 for a database fault.
pub async fn create(db: &Db, body: CreatePost, author: &str) -> Result<Post> {
    let base = Slug::from_title(&body.title).ok_or(BlogError::UnsluggableTitle)?;
    let slug = unique_slug(db, &base).await?;

    let now = Utc::now();
    Post::insert(NewPost {
        id: Id::new(),
        slug,
        title: body.title,
        body: body.body,
        author: author.to_owned(),
        published_at: body.publish.then_some(now),
        created_at: now,
        updated_at: now,
    })
    .fetch_one(db)
    .await
    .map_err(Error::internal)
}

/// Fetch one post, or a 404.
///
/// # Errors
/// [`BlogError::PostNotFound`], or an internal 500 for a database fault.
pub async fn get(db: &Db, id: Id<Post>) -> Result<Post> {
    Post::find(id)
        .fetch_optional(db)
        .await
        .map_err(Error::internal)?
        .ok_or_else(|| BlogError::post_not_found(id).into())
}

/// Apply a PATCH body to an already-loaded post.
///
/// # Errors
/// [`BlogError::NothingToUpdate`] for an empty body, or an internal 500 for a
/// database fault.
pub async fn update(db: &Db, post: &Post, changes: UpdatePost) -> Result<Post> {
    if changes.is_empty() {
        return Err(BlogError::NothingToUpdate.into());
    }
    post.update()
        .set_opt(Post::TITLE, changes.title)
        .set_opt(Post::BODY, changes.body)
        .set(Post::UPDATED_AT, Utc::now())
        .fetch_one(db)
        .await
        .map_err(Error::internal)
}

/// Publish a post. Publishing twice is a no-op, not an error.
///
/// # Errors
/// [`BlogError::PostNotFound`], or an internal 500 for a database fault.
pub async fn publish(db: &Db, id: Id<Post>) -> Result<Post> {
    let post = get(db, id).await?;
    if post.is_published() {
        return Ok(post);
    }
    let now = Utc::now();
    post.update()
        .set(Post::PUBLISHED_AT, Some(now))
        .set(Post::UPDATED_AT, now)
        .fetch_one(db)
        .await
        .map_err(Error::internal)
}

/// Delete an already-loaded post.
///
/// # Errors
/// An internal 500 for a database fault. A repeated delete is a 404 already,
/// because the handler loads the row first and that load is what 404s.
pub async fn delete(db: &Db, post: &Post) -> Result<()> {
    post.delete().execute(db).await.map_err(Error::internal)?;
    Ok(())
}

/// One page of the newest-first listing, filtered for this actor.
///
/// Cursor pagination, its tiebreaker and its signed opaque token all come from
/// the ORM. The page carries a total (one extra `count(*)`), which is what the
/// listing endpoint reports.
///
/// # Errors
/// A 422 pointed at `/cursor` for a cursor this API did not issue, and an
/// internal 500 for a database fault.
pub async fn list(
    db: &Db,
    filter: &ListFilter,
    cursor: Option<Cursor>,
    limit: u32,
    codec: &CursorCodec,
) -> Result<Page<Post>> {
    let query = Post::query()
        .filter_opt(visibility_predicate(filter))
        .filter_opt(
            filter
                .search
                .as_ref()
                .map(|needle| Post::TITLE.icontains(needle)),
        )
        .order_by(Post::CREATED_AT.desc());

    query
        .paginate(cursor, limit)
        .signed_with(codec.clone())
        .with_total()
        .fetch(db)
        .await
        .map_err(map_page_error)
}

/// Build one candidate slug at a time until one is free.
///
/// The uniqueness check is a real indexed lookup, so this is `n` cheap queries
/// for the `n`th collision — which for a title is almost always zero or one.
async fn unique_slug(db: &Db, base: &Slug) -> Result<Slug> {
    let mut suffix = 1_u32;
    let mut candidate = base.clone();
    loop {
        let taken = Post::query()
            .filter(Post::SLUG.eq(&candidate))
            .count(db)
            .await
            .map_err(Error::internal)?
            > 0;
        if !taken {
            return Ok(candidate);
        }
        suffix += 1;
        candidate = Slug::from_title(&format!("{}-{suffix}", base.as_str()))
            .unwrap_or_else(|| candidate.clone());
    }
}

/// The `WHERE` addition that hides drafts this actor may not see, or `None` when
/// the actor may see everything.
fn visibility_predicate(filter: &ListFilter) -> Option<Predicate> {
    match &filter.drafts {
        DraftVisibility::All => None,
        DraftVisibility::None => Some(Post::PUBLISHED_AT.is_not_null()),
        DraftVisibility::Own(author) => {
            Some(Post::PUBLISHED_AT.is_not_null().or(Post::AUTHOR.eq(author)))
        }
    }
}

/// Turn a pagination failure into the response a client can act on.
///
/// A forged or mismatched cursor is the client's mistake and reads as a 422 at
/// `/cursor`, exactly like a `limit` out of range — one error shape, not two.
/// Anything else is a database fault and an honest 500.
fn map_page_error(error: moso::db::Error) -> Error {
    if matches!(error, moso::db::Error::Cursor(_)) {
        return Error::validation(ValidationErrors::one(
            "/cursor",
            codes::FORMAT,
            "this is not a cursor this API issued; start from the first page",
        ));
    }
    Error::internal(error)
}

/// What a listing request is asking to see.
///
/// A plain value rather than a closure so that it can be built in the handler
/// and turned into a predicate here, with no risk of the two disagreeing about
/// what "matching" means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListFilter {
    /// Case-insensitive substring of the title.
    pub search: Option<String>,
    /// Whether drafts are included, and whose.
    pub drafts: DraftVisibility,
}

/// Which drafts a listing may show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DraftVisibility {
    /// Published posts only. What an anonymous reader sees.
    #[default]
    None,
    /// Published posts, plus drafts written by this author.
    Own(String),
    /// Everything. What an editor sees when they ask for it.
    All,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An isolated on-disk SQLite database of this test's own, with the schema
    /// applied. A file rather than `:memory:`, because a pooled in-memory
    /// database is a *different* database per connection.
    async fn db() -> (Db, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "moso-crud-store-{}-{}.sqlite",
            std::process::id(),
            uuid_like()
        ));
        let _ = std::fs::remove_file(&path);
        let db = Db::connect_url(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("SQLite opens");
        create_schema(&db).await.expect("the schema applies");
        (db, path)
    }

    /// A process-unique tag, without pulling `uuid` into the test.
    fn uuid_like() -> String {
        Id::<Post>::new().to_string()
    }

    fn body(title: &str) -> CreatePost {
        CreatePost {
            title: title.to_owned(),
            body: "…".to_owned(),
            publish: true,
        }
    }

    #[tokio::test]
    async fn a_created_post_gets_a_slug_from_its_title() {
        let (db, path) = db().await;
        let post = create(&db, body("Hello World"), "ada")
            .await
            .expect("created");
        assert_eq!(post.slug.as_str(), "hello-world");
        db.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_second_post_with_the_same_title_gets_a_distinct_slug() {
        let (db, path) = db().await;
        let first = create(&db, body("Hello"), "ada").await.expect("created");
        let second = create(&db, body("Hello"), "ada").await.expect("created");
        assert_eq!(first.slug.as_str(), "hello");
        assert_ne!(first.slug, second.slug);
        db.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn an_empty_patch_changes_nothing() {
        let (db, path) = db().await;
        let post = create(&db, body("Hello"), "ada").await.expect("created");
        let error = update(&db, &post, UpdatePost::default())
            .await
            .expect_err("nothing to do");
        assert_eq!(error.status(), 422);
        db.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn publishing_twice_keeps_the_first_timestamp() {
        let (db, path) = db().await;
        let mut draft = body("Hello");
        draft.publish = false;
        let post = create(&db, draft, "ada").await.expect("created");
        assert!(post.published_at.is_none());

        let first = publish(&db, post.id).await.expect("published");
        let second = publish(&db, post.id).await.expect("still published");
        assert_eq!(first.published_at, second.published_at);
        db.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_draft_is_hidden_from_everybody_but_its_author() {
        let (db, path) = db().await;
        let mut draft = body("Secret");
        draft.publish = false;
        create(&db, draft, "ada").await.expect("created");

        let codec = CursorCodec::new("a-signing-secret-that-is-plenty-long");
        let count = |visibility: DraftVisibility| {
            let filter = ListFilter {
                drafts: visibility,
                ..ListFilter::default()
            };
            let db = db.clone();
            let codec = codec.clone();
            async move {
                list(&db, &filter, None, 20, &codec)
                    .await
                    .expect("listed")
                    .items
                    .len()
            }
        };

        assert_eq!(count(DraftVisibility::None).await, 0);
        assert_eq!(count(DraftVisibility::Own("ada".to_owned())).await, 1);
        assert_eq!(count(DraftVisibility::Own("grace".to_owned())).await, 0);
        assert_eq!(count(DraftVisibility::All).await, 1);
        db.close().await;
        let _ = std::fs::remove_file(&path);
    }
}
