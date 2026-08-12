//! `/api/v1/posts` — the whole CRUD surface.
//!
//! Note what is *absent* from this file: no OpenAPI annotations, no
//! `.validate()?` calls, no manual 404 mapping, no serialisation code, no
//! hand-rolled pagination. The document, the 422s, the JSON and the cursors all
//! come from the types in the signatures and from the ORM.

use moso::db::Db;
use moso::openapi::SecurityRequirement;
use moso::prelude::*;
use moso::response::NoContent;
use moso::response::cursor::CursorCodec;

use crate::auth::{Actor, ApiKeyGuard, Editor};
use crate::config::AppConfig;
use crate::error::BlogError;
use crate::models::{CreatePost, ListPosts, Post, PostOut, UpdatePost};
use crate::store::{self, DraftVisibility, ListFilter};

// ---------------------------------------------------------------------------
// The route table
// ---------------------------------------------------------------------------

/// Every posts route.
///
/// Two tables rather than one, because the second is guarded: `.guard(…)`
/// applies to the routes registered *so far*, so splitting the reads from the
/// writes is what says "the writes need a key" once instead of four times —
/// and the guard puts the 401 and the security requirement on each of those
/// four operations in the document.
pub fn router() -> Router {
    let public = moso::routes! {
        GET "/posts"      => list,
        GET "/posts/{id}" => show,
    };

    let protected = moso::routes! {
        POST   "/posts"              => create,
        PATCH  "/posts/{id}"         => update,
        DELETE "/posts/{id}"         => destroy,
        POST   "/posts/{id}/publish" => publish,
    }
    .guard(ApiKeyGuard);

    public.merge(protected).tag("posts")
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// List posts.
///
/// Published posts, newest first. Name yourself with `x-author` to see your own
/// drafts as well; an editor may ask for every draft with `?drafts=true`.
/// Results are cursor-paginated: pass the previous page's `next_cursor` back as
/// `?cursor=`.
#[endpoint(errors = BlogError)]
async fn list(
    Inject(db): Inject<Db>,
    Inject(codec): Inject<CursorCodec>,
    Inject(config): Inject<AppConfig>,
    Depends(actor): Depends<Actor>,
    Query(query): Query<ListPosts>,
) -> Result<Page<PostOut>> {
    let limit = query.limit.unwrap_or(config.posts.page_size);
    let filter = ListFilter {
        search: query.search,
        drafts: visibility(&actor, query.drafts),
    };

    // The ORM issues one statement for the page and one for the total, appends
    // the primary key as a tiebreaker, and signs the cursor — so this handler
    // has no cursor arithmetic and no N+1.
    let page = store::list(&db, &filter, query.cursor, limit, &codec).await?;
    Ok(page.map(PostOut::from))
}

/// Create a post.
///
/// The slug is derived from the title and suffixed if it collides, so two posts
/// called "Hello" become `hello` and `hello-2`. Requires the API key.
#[endpoint(errors = BlogError)]
async fn create(
    Inject(db): Inject<Db>,
    Depends(actor): Depends<Actor>,
    Json(body): Json<CreatePost>,
) -> Result<Created<PostOut>> {
    let post = store::create(&db, body, &actor.name).await?;
    let location = format!("/api/v1/posts/{}", post.id);
    Ok(Created::at(location, post.into()))
}

/// Fetch one post.
///
/// A draft is visible only to its author and to an editor. To anybody else it
/// is a 404 rather than a 403: a 403 would confirm that the identifier exists.
#[endpoint(errors = BlogError)]
async fn show(
    Inject(db): Inject<Db>,
    Depends(actor): Depends<Actor>,
    Path(id): Path<Id<Post>>,
) -> Result<Json<PostOut>> {
    let post = store::get(&db, id).await?;
    if !may_read(&post, &actor) {
        return Err(BlogError::post_not_found(id).into());
    }
    Ok(Json(post.into()))
}

/// Edit a post.
///
/// Only the author or an editor may edit. Every field of the body is optional;
/// an empty body is a 422, because it is more likely to be a bug in the client
/// than a request to do nothing.
#[endpoint(errors = BlogError)]
async fn update(
    Inject(db): Inject<Db>,
    Depends(actor): Depends<Actor>,
    Path(id): Path<Id<Post>>,
    Json(body): Json<UpdatePost>,
) -> Result<Json<PostOut>> {
    let post = authorize_write(&db, id, &actor).await?;
    Ok(Json(store::update(&db, &post, body).await?.into()))
}

/// Delete a post.
///
/// Only the author or an editor may delete. Deleting twice is a 404 the second
/// time: the client asked to remove something that was not there.
#[endpoint(errors = BlogError)]
async fn destroy(
    Inject(db): Inject<Db>,
    Depends(actor): Depends<Actor>,
    Path(id): Path<Id<Post>>,
) -> Result<NoContent> {
    let post = authorize_write(&db, id, &actor).await?;
    store::delete(&db, &post).await?;
    Ok(NoContent)
}

/// Publish a post.
///
/// Publishing an already-published post is a no-op. `Depends<Editor>` is the
/// authorisation rule: a caller without `x-role: editor` never reaches the
/// body, and gets a 403 with the message the dependency declares.
#[endpoint(errors = BlogError)]
async fn publish(
    Inject(db): Inject<Db>,
    Depends(_editor): Depends<Editor>,
    Path(id): Path<Id<Post>>,
) -> Result<Json<PostOut>> {
    Ok(Json(store::publish(&db, id).await?.into()))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Which drafts this actor may see in a listing.
fn visibility(actor: &Actor, asked_for_drafts: bool) -> DraftVisibility {
    match (actor.editor, asked_for_drafts) {
        (true, true) => DraftVisibility::All,
        _ => DraftVisibility::Own(actor.name.clone()),
    }
}

/// Whether this actor may read this post.
fn may_read(post: &Post, actor: &Actor) -> bool {
    post.is_published() || actor.editor || post.author == actor.name
}

/// Whether this actor may change this post.
fn may_write(post: &Post, actor: &Actor) -> bool {
    actor.editor || post.author == actor.name
}

/// Load a post and check that `actor` may change it.
///
/// A post the actor may not even *read* is reported as absent, for the same
/// reason [`show`] does it: a 403 on a draft is an existence oracle.
async fn authorize_write(db: &Db, id: Id<Post>, actor: &Actor) -> Result<Post> {
    let post = store::get(db, id).await?;
    if !may_read(&post, actor) {
        return Err(BlogError::post_not_found(id).into());
    }
    if !may_write(&post, actor) {
        return Err(Error::forbidden(
            "only the author or an editor may change this post",
        ));
    }
    Ok(post)
}

/// The security requirement the guarded half of this router advertises.
///
/// Exposed so the composition root can declare the matching scheme once, and so
/// a test can assert the two agree.
#[must_use]
pub fn write_security() -> SecurityRequirement {
    SecurityRequirement::scheme(crate::auth::API_KEY_SCHEME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn post(author: &str, published: bool) -> Post {
        let now = Utc::now();
        Post {
            id: Id::new(),
            slug: Slug::new_unchecked("hello"),
            title: "Hello".to_owned(),
            body: "…".to_owned(),
            author: author.to_owned(),
            published_at: published.then_some(now),
            created_at: now,
            updated_at: now,
        }
    }

    fn actor(name: &str, editor: bool) -> Actor {
        Actor {
            name: name.to_owned(),
            editor,
        }
    }

    #[test]
    fn a_published_post_is_readable_by_anybody() {
        assert!(may_read(&post("ada", true), &Actor::anonymous()));
    }

    #[test]
    fn a_draft_is_readable_only_by_its_author_or_an_editor() {
        let draft = post("ada", false);
        assert!(may_read(&draft, &actor("ada", false)));
        assert!(may_read(&draft, &actor("grace", true)));
        assert!(!may_read(&draft, &actor("grace", false)));
    }

    #[test]
    fn only_the_author_or_an_editor_may_write() {
        let published = post("ada", true);
        assert!(may_write(&published, &actor("ada", false)));
        assert!(may_write(&published, &actor("grace", true)));
        assert!(!may_write(&published, &actor("grace", false)));
    }

    #[test]
    fn drafts_are_scoped_to_the_actor_unless_an_editor_asks_for_all() {
        assert_eq!(
            visibility(&actor("ada", false), true),
            DraftVisibility::Own("ada".to_owned()),
            "a non-editor cannot widen the scope by asking"
        );
        assert_eq!(visibility(&actor("ada", true), true), DraftVisibility::All);
        assert_eq!(
            visibility(&actor("ada", true), false),
            DraftVisibility::Own("ada".to_owned())
        );
    }

    #[test]
    fn the_router_registers_every_operation() {
        assert_eq!(router().len(), 6);
    }
}
