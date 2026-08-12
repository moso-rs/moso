//! The post: one **entity**, three DTOs, and the projection the API returns.
//!
//! The split is the point of the model layer, and Moso makes it a compile-time
//! rule rather than a convention:
//!
//! - [`Post`] is a real [`#[derive(Entity)]`](moso::Entity) over the `posts`
//!   table. It is what the application *stores*.
//! - [`PostOut`] is what the API *returns*. `Post` deliberately does **not**
//!   implement `Schema` — an entity is not a schema (ADR-0008) — so returning a
//!   `Post` from a handler is a compile error, not a review catch. The one
//!   sanctioned bridge is `#[schema(from = Post)]`, which writes the conversion
//!   field by field and stops compiling the moment the two drift apart.
//!
//! There is no `PostKey` and no hand-rolled cursor here anymore: keyset
//! pagination, its tiebreaker and its signed opaque cursor all come from the
//! ORM's [`Select::paginate`](moso::db::Select::paginate). The framework owns
//! the part that is easy to get subtly wrong.

use chrono::{DateTime, Utc};
use moso::Entity;
use moso::prelude::*;

// ---------------------------------------------------------------------------
// The entity
// ---------------------------------------------------------------------------

/// A post, as the database stores it — one row of `posts`.
///
/// `#[derive(Entity)]` generates the column constants (`Post::SLUG`,
/// `Post::CREATED_AT`, …), the `NewPost` insert struct, and the
/// `query`/`find`/`insert`/`update`/`delete` builders. It does **not** generate
/// a `Schema`: try returning a `Post` from a handler and the build fails with
/// "the trait bound `Post: Describe` is not satisfied", which is the whole
/// reason [`PostOut`] exists.
#[derive(Entity, Debug, Clone, PartialEq, Eq)]
#[entity(table = "posts")]
pub struct Post {
    /// Primary key. A UUIDv7, so it sorts by creation time and doubles as the
    /// pagination tiebreaker.
    #[entity(pk)]
    pub id: Id<Post>,

    /// The URL-safe name, derived from the title and unique across the table.
    #[entity(unique)]
    pub slug: Slug,

    /// Headline.
    pub title: String,

    /// The body, in CommonMark.
    pub body: String,

    /// Who wrote it.
    pub author: String,

    /// When it went public, or `None` while it is a draft.
    pub published_at: Option<DateTime<Utc>>,

    /// When the row was created.
    pub created_at: DateTime<Utc>,

    /// When the row last changed. Not exposed by the API.
    pub updated_at: DateTime<Utc>,
}

impl Post {
    /// Whether the post is visible to a reader who is not its author.
    #[must_use]
    pub fn is_published(&self) -> bool {
        self.published_at.is_some()
    }
}

// ---------------------------------------------------------------------------
// Input DTOs
// ---------------------------------------------------------------------------

/// A post, as the API accepts one.
//
// Every constraint below is enforced *and* documented from one attribute:
// `len = 3..=200` becomes both the 422 and the `minLength`/`maxLength` in the
// generated schema, so the two cannot drift apart.
#[derive(Schema, Debug, Clone, PartialEq, Eq)]
pub struct CreatePost {
    /// Headline, shown in listings.
    #[schema(len = 3..=200, trim)]
    pub title: String,

    /// The body, in CommonMark.
    #[schema(len = 1..=100_000)]
    pub body: String,

    /// Publish immediately instead of saving a draft.
    #[schema(default = false)]
    pub publish: bool,
}

/// The fields a post may be edited with.
///
/// Every field is optional: absent means "leave it alone", which is what PATCH
/// means.
//
// `Option<String>` with a `len` constraint checks the value only when one is
// present, so "absent" and "invalid" stay distinguishable.
#[derive(Schema, Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdatePost {
    /// A new headline.
    #[schema(len = 3..=200, trim)]
    pub title: Option<String>,

    /// A new body.
    #[schema(len = 1..=100_000)]
    pub body: Option<String>,
}

impl UpdatePost {
    /// Whether the request asked for no change at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.body.is_none()
    }
}

// ---------------------------------------------------------------------------
// Output DTO
// ---------------------------------------------------------------------------

/// A post, as the API returns one.
//
// `#[schema(from = Post)]` generates `impl From<Post> for PostOut` field by
// field. Rename a field on `Post` and this file stops compiling, which is the
// only way a projection stays honest. The note is a `//` comment rather than a
// `///` one on purpose: a doc comment here would be published to every client
// in `openapi.json`, and how the conversion is generated is nobody's business
// but this crate's.
#[derive(Schema, Debug, Clone, PartialEq, Eq)]
#[schema(from = Post)]
pub struct PostOut {
    /// Primary key.
    pub id: Id<Post>,
    /// The URL-safe name.
    pub slug: Slug,
    /// Headline.
    pub title: String,
    /// The body, in CommonMark.
    pub body: String,
    /// Who wrote it.
    pub author: String,
    /// When it went public, or `null` while it is a draft.
    pub published_at: Option<DateTime<Utc>>,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

/// The query string `GET /api/v1/posts` accepts.
//
// A `Schema` used as query parameters: each field becomes one documented
// parameter with its constraint attached, so `limit` shows up in the document
// as `minimum: 1, maximum: 100`.
#[derive(Schema, Debug, Clone, PartialEq, Eq)]
pub struct ListPosts {
    /// Case-insensitive substring match over the title.
    #[schema(len = ..=100, trim)]
    pub search: Option<String>,

    /// Include drafts. Honoured only for an editor; ignored otherwise.
    #[schema(default = false)]
    pub drafts: bool,

    /// The `next_cursor` of the previous page.
    pub cursor: Option<Cursor>,

    /// How many posts to return. Defaults to the configured `posts.page_size`.
    #[schema(range = 1..=100)]
    pub limit: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use moso::schema::{Validate, ValidationCtx};

    fn post(title: &str) -> Post {
        let now = Utc::now();
        Post {
            id: Id::new(),
            slug: Slug::from_title(title).expect("a title makes a slug"),
            title: title.to_owned(),
            body: "…".to_owned(),
            author: "ada".to_owned(),
            published_at: Some(now),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn the_entity_names_its_table_and_columns() {
        assert_eq!(<Post as moso::db::Entity>::TABLE.name().as_str(), "posts");
        assert_eq!(Post::SLUG.column_ref().name().as_str(), "slug");
    }

    #[test]
    fn the_projection_carries_every_public_field() {
        let source = post("Hello");
        let out = PostOut::from(source.clone());
        assert_eq!(out.id, source.id);
        assert_eq!(out.slug, source.slug);
        assert_eq!(out.author, source.author);
        assert_eq!(out.published_at, source.published_at);
    }

    #[test]
    fn a_short_title_fails_validation_at_its_own_pointer() {
        let body = CreatePost {
            title: "ab".to_owned(),
            body: "…".to_owned(),
            publish: false,
        };
        let errors = body
            .validate(&mut ValidationCtx::new())
            .expect_err("`ab` is shorter than three characters");
        let pointers: Vec<String> = errors.iter().map(|e| e.pointer.to_string()).collect();
        assert_eq!(pointers, ["/title"]);
        assert_eq!(errors.iter().next().expect("one error").code, "len");
    }
}
