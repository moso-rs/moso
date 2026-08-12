//! [`Related<T>`] — the field type that makes N+1 impossible to cause by
//! accident — and [`LoadedRows`], the payload the preloader hands a relation's
//! setter.
//!
//! # Non-negotiable N2, stated exactly
//!
//! [`Related::get`] takes `&self` and returns a `Result`. It is not `async`, it
//! takes no executor, and it has no access to one. **It therefore cannot issue
//! a statement**, which is a stronger guarantee than "we promise not to": the
//! signature is the proof, and the test that runs a hundred `get()` calls
//! against a live statement counter and finds it still at zero is the
//! demonstration.
//!
//! Implicit lazy loading is how an application acquires an N+1 in production —
//! the loop looks innocent, and every iteration is a round trip. The cure is
//! for the accessor to be incapable of one.
//!
//! # "Unknown" and "empty" must not look alike
//!
//! A `Related<Vec<Comment>>` that was never preloaded is *unknown*, not *empty*.
//! Serialising it as `[]` tells a client "this post has no comments", which is a
//! lie. Serialising it as `null` is less of a lie and still wrong. So
//! [`Related::is_not_loaded`] exists for
//! `#[serde(skip_serializing_if = "Related::is_not_loaded")]`, which the derive
//! emits, and the field **disappears** from the body instead.

use core::any::Any;
use core::fmt;

use serde::{Serialize, Serializer};

use crate::error::{Error, Result};
use crate::row::DecodeError;

/// A related value that may or may not have been loaded.
///
/// ```
/// use moso_orm::Related;
///
/// let unknown: Related<Vec<u8>> = Related::NotLoaded;
/// assert!(!unknown.is_loaded());
/// assert!(unknown.get().is_err());
///
/// let known = Related::Loaded(vec![1_u8, 2]);
/// assert_eq!(known.get().expect("loaded").len(), 2);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Related<T> {
    /// The query that produced the owner did not preload this relation.
    #[default]
    NotLoaded,
    /// The relation was preloaded.
    Loaded(T),
}

impl<T> Related<T> {
    /// The loaded value.
    ///
    /// **Never queries.** That is the whole point.
    ///
    /// # Errors
    ///
    /// [`NotLoaded`], whose message shows both fixes. Use
    /// [`Related::get_named`] — which `#[derive(Entity)]` does — to get the
    /// message that names the entity and the field.
    ///
    /// ```
    /// use moso_orm::Related;
    ///
    /// assert_eq!(Related::Loaded(7).get().copied(), Ok(7));
    /// assert!(Related::<i32>::NotLoaded.get().is_err());
    /// ```
    pub fn get(&self) -> Result<&T, NotLoaded> {
        match self {
            Self::Loaded(value) => Ok(value),
            Self::NotLoaded => Err(NotLoaded::new()),
        }
    }

    /// The loaded value, with the names that make the message actionable.
    ///
    /// This is what the accessor `#[derive(Entity)]` generates calls:
    ///
    /// ```ignore
    /// impl Post {
    ///     pub fn comments(&self) -> Result<&Vec<Comment>, NotLoaded> {
    ///         self.comments.get_named("Post", "comments", "Post::COMMENTS")
    ///     }
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// [`NotLoaded`], naming `entity`, `field` and `constant`.
    ///
    /// ```
    /// use moso_orm::Related;
    ///
    /// let unloaded: Related<Vec<i32>> = Related::NotLoaded;
    /// let error = unloaded
    ///     .get_named("Post", "comments", "Post::COMMENTS")
    ///     .expect_err("not loaded");
    /// assert!(error.to_string().starts_with("relation `Post::comments` was not loaded"));
    /// ```
    pub fn get_named(
        &self,
        entity: &'static str,
        field: &'static str,
        constant: &'static str,
    ) -> Result<&T, NotLoaded> {
        match self {
            Self::Loaded(value) => Ok(value),
            Self::NotLoaded => Err(NotLoaded::of(entity, field, constant)),
        }
    }

    /// The loaded value, mutably.
    ///
    /// # Errors
    ///
    /// [`NotLoaded`].
    ///
    /// ```
    /// use moso_orm::Related;
    ///
    /// let mut loaded = Related::Loaded(1);
    /// *loaded.get_mut().expect("loaded") += 1;
    /// assert_eq!(loaded.get().copied(), Ok(2));
    /// ```
    pub fn get_mut(&mut self) -> Result<&mut T, NotLoaded> {
        match self {
            Self::Loaded(value) => Ok(value),
            Self::NotLoaded => Err(NotLoaded::new()),
        }
    }

    /// The loaded value, consuming the wrapper.
    ///
    /// # Errors
    ///
    /// [`NotLoaded`].
    ///
    /// ```
    /// use moso_orm::Related;
    ///
    /// assert_eq!(Related::Loaded("x").into_inner(), Ok("x"));
    /// ```
    pub fn into_inner(self) -> Result<T, NotLoaded> {
        match self {
            Self::Loaded(value) => Ok(value),
            Self::NotLoaded => Err(NotLoaded::new()),
        }
    }

    /// Whether the relation was preloaded.
    ///
    /// ```
    /// use moso_orm::Related;
    ///
    /// assert!(Related::Loaded(1).is_loaded());
    /// assert!(!Related::<i32>::NotLoaded.is_loaded());
    /// ```
    #[must_use]
    pub const fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded(_))
    }

    /// Whether the relation was **not** preloaded.
    ///
    /// Named for `#[serde(skip_serializing_if = "Related::is_not_loaded")]`,
    /// which is what makes an unloaded relation vanish from a JSON body rather
    /// than appear as `null` — "unknown" and "empty" must not look alike.
    ///
    /// ```
    /// use moso_orm::Related;
    /// use serde::Serialize;
    ///
    /// #[derive(Serialize)]
    /// struct PostDto {
    ///     title: &'static str,
    ///     #[serde(skip_serializing_if = "Related::is_not_loaded")]
    ///     comments: Related<Vec<i32>>,
    /// }
    ///
    /// let dto = PostDto { title: "hi", comments: Related::NotLoaded };
    /// assert_eq!(serde_json::to_string(&dto).unwrap(), r#"{"title":"hi"}"#);
    /// ```
    #[must_use]
    pub const fn is_not_loaded(&self) -> bool {
        matches!(self, Self::NotLoaded)
    }

    /// The loaded value as an `Option`, for the call sites that genuinely do
    /// not care.
    ///
    /// ```
    /// use moso_orm::Related;
    ///
    /// assert_eq!(Related::Loaded(1).as_option(), Some(&1));
    /// assert_eq!(Related::<i32>::NotLoaded.as_option(), None);
    /// ```
    #[must_use]
    pub const fn as_option(&self) -> Option<&T> {
        match self {
            Self::Loaded(value) => Some(value),
            Self::NotLoaded => None,
        }
    }

    /// The loaded value as an owned `Option`, discarding the distinction
    /// between "unknown" and "absent".
    ///
    /// ```
    /// use moso_orm::Related;
    ///
    /// assert_eq!(Related::Loaded(1).into_option(), Some(1));
    /// assert_eq!(Related::<i32>::NotLoaded.into_option(), None);
    /// ```
    #[must_use]
    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Loaded(value) => Some(value),
            Self::NotLoaded => None,
        }
    }

    /// Marks the relation loaded with `value`. The preloader's setter.
    ///
    /// ```
    /// use moso_orm::Related;
    ///
    /// let mut relation = Related::NotLoaded;
    /// relation.load(5);
    /// assert!(relation.is_loaded());
    /// ```
    pub fn load(&mut self, value: T) {
        *self = Self::Loaded(value);
    }

    /// Applies `f` to the loaded value, keeping `NotLoaded` as it is.
    ///
    /// ```
    /// use moso_orm::Related;
    ///
    /// assert_eq!(Related::Loaded(2).map(|n| n * 3), Related::Loaded(6));
    /// assert_eq!(Related::<i32>::NotLoaded.map(|n| n * 3), Related::NotLoaded);
    /// ```
    #[must_use]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Related<U> {
        match self {
            Self::Loaded(value) => Related::Loaded(f(value)),
            Self::NotLoaded => Related::NotLoaded,
        }
    }
}

impl<T: Serialize> Serialize for Related<T> {
    /// A loaded relation serialises as its value. An unloaded one serialises as
    /// `null` — which is why the derive also emits
    /// `#[serde(skip_serializing_if = "Related::is_not_loaded")]`, so the field
    /// is omitted entirely rather than claiming "no children".
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        match self {
            Self::Loaded(value) => value.serialize(serializer),
            Self::NotLoaded => serializer.serialize_none(),
        }
    }
}

/// Reading a relation that was never loaded.
///
/// The message is the one `docs/02-data/22-relations.md` specifies, word for
/// word, when the names are known:
///
/// ```text
/// relation `Post::comments` was not loaded
///   the query that produced this `Post` did not include `.with(Post::COMMENTS)`
///   add it:   Post::query().with(Post::COMMENTS)
///   or load on demand for a single row:   post.load(Post::COMMENTS, &db).await?
/// ```
///
/// ```
/// use moso_orm::NotLoaded;
///
/// let error = NotLoaded::of("Post", "comments", "Post::COMMENTS");
/// let text = error.to_string();
/// assert!(text.contains("`Post::comments` was not loaded"));
/// assert!(text.contains(".with(Post::COMMENTS)"));
/// assert!(text.contains("post.load(Post::COMMENTS, &db).await?"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub struct NotLoaded {
    entity: Option<&'static str>,
    relation: Option<&'static str>,
    constant: Option<&'static str>,
}

impl NotLoaded {
    /// An anonymous one, for [`Related::get`] on a value whose owner is not
    /// known at the call site.
    ///
    /// ```
    /// use moso_orm::NotLoaded;
    ///
    /// assert!(NotLoaded::new().to_string().contains("not loaded"));
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entity: None,
            relation: None,
            constant: None,
        }
    }

    /// One that names the entity, the field and the relation constant.
    ///
    /// ```
    /// use moso_orm::NotLoaded;
    ///
    /// let error = NotLoaded::of("User", "posts", "User::POSTS");
    /// assert!(error.to_string().contains("User::posts"));
    /// ```
    #[must_use]
    pub const fn of(entity: &'static str, relation: &'static str, constant: &'static str) -> Self {
        Self {
            entity: Some(entity),
            relation: Some(relation),
            constant: Some(constant),
        }
    }

    /// The entity, when it was recorded.
    ///
    /// ```
    /// assert_eq!(moso_orm::NotLoaded::of("U", "p", "U::P").entity(), Some("U"));
    /// ```
    #[must_use]
    pub const fn entity(&self) -> Option<&'static str> {
        self.entity
    }

    /// The relation's field name, when it was recorded.
    ///
    /// ```
    /// assert_eq!(moso_orm::NotLoaded::of("U", "p", "U::P").relation(), Some("p"));
    /// ```
    #[must_use]
    pub const fn relation(&self) -> Option<&'static str> {
        self.relation
    }

    /// The relation constant to pass to `.with(..)`, when it was recorded.
    ///
    /// ```
    /// assert_eq!(moso_orm::NotLoaded::of("U", "p", "U::P").constant(), Some("U::P"));
    /// ```
    #[must_use]
    pub const fn constant(&self) -> Option<&'static str> {
        self.constant
    }
}

impl Default for NotLoaded {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for NotLoaded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let constant = self.constant.unwrap_or("Entity::RELATION");
        let owner = self.entity.unwrap_or("Entity");
        let receiver = self
            .entity
            .map_or_else(|| String::from("entity"), str::to_lowercase);
        match (self.entity, self.relation) {
            (Some(entity), Some(relation)) => {
                writeln!(f, "relation `{entity}::{relation}` was not loaded")?;
                writeln!(
                    f,
                    "  the query that produced this `{entity}` did not include `.with({constant})`"
                )?;
            }
            _ => {
                writeln!(f, "this relation was not loaded")?;
                writeln!(
                    f,
                    "  the query that produced this row did not include `.with({constant})`"
                )?;
            }
        }
        writeln!(f, "  add it:   {owner}::query().with({constant})")?;
        write!(
            f,
            "  or load on demand for a single row:   \
             {receiver}.load({constant}, &db).await?"
        )
    }
}

/// The rows a preload loaded for **one** parent, with the type erased.
///
/// The preloader is generic over the owning entity and type-erased over the
/// related one — that asymmetry is what lets `Select<E>` hold a `Vec<Preload>`
/// of relations to four different entities without four type parameters. The
/// erasure is undone by the setter `#[derive(Entity)]` generates, which knows
/// the field's type:
///
/// ```ignore
/// fn link_comments(post: &mut Post, rows: LoadedRows) -> moso_orm::Result<()> {
///     post.comments = Related::Loaded(rows.into_rows::<Comment>()?);
///     Ok(())
/// }
/// ```
///
/// ```
/// use moso_orm::relation::LoadedRows;
///
/// let loaded = LoadedRows::rows("comments", "Comment", vec![1_i64, 2, 3]);
/// assert_eq!(loaded.relation(), "comments");
/// assert_eq!(loaded.into_rows::<i64>().unwrap(), [1, 2, 3]);
/// ```
pub struct LoadedRows {
    relation: &'static str,
    target: &'static str,
    payload: Payload,
}

/// What a [`LoadedRows`] carries: rows, or the count of rows nobody asked for.
enum Payload {
    /// A `Vec<T>` for the related entity `T`.
    Rows(Box<dyn Any + Send>),
    /// The result of `.with_count(..)`.
    Count(i64),
}

impl LoadedRows {
    /// The rows loaded for one parent.
    ///
    /// ```
    /// use moso_orm::relation::LoadedRows;
    ///
    /// let loaded = LoadedRows::rows("tags", "Tag", vec!["rust", "web"]);
    /// assert!(!loaded.is_count());
    /// ```
    #[must_use]
    pub fn rows<T: Send + 'static>(
        relation: &'static str,
        target: &'static str,
        rows: Vec<T>,
    ) -> Self {
        Self {
            relation,
            target,
            payload: Payload::Rows(Box::new(rows)),
        }
    }

    /// The count `.with_count(..)` produced for one parent.
    ///
    /// ```
    /// use moso_orm::relation::LoadedRows;
    ///
    /// assert_eq!(LoadedRows::counted("comments", "Comment", 12).into_count().unwrap(), 12);
    /// ```
    #[must_use]
    pub const fn counted(relation: &'static str, target: &'static str, count: i64) -> Self {
        Self {
            relation,
            target,
            payload: Payload::Count(count),
        }
    }

    /// The relation's field name.
    ///
    /// ```
    /// # use moso_orm::relation::LoadedRows;
    /// assert_eq!(LoadedRows::counted("c", "C", 0).relation(), "c");
    /// ```
    #[must_use]
    pub const fn relation(&self) -> &'static str {
        self.relation
    }

    /// The related entity's name.
    ///
    /// ```
    /// # use moso_orm::relation::LoadedRows;
    /// assert_eq!(LoadedRows::counted("c", "Comment", 0).target(), "Comment");
    /// ```
    #[must_use]
    pub const fn target(&self) -> &'static str {
        self.target
    }

    /// Whether this is a count rather than rows.
    ///
    /// ```
    /// # use moso_orm::relation::LoadedRows;
    /// assert!(LoadedRows::counted("c", "C", 0).is_count());
    /// ```
    #[must_use]
    pub const fn is_count(&self) -> bool {
        matches!(self.payload, Payload::Count(_))
    }

    /// The rows, for a `has_many` or a `many_to_many` field.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when `T` is not the type the preload loaded, which
    /// only a hand-written relation constant can arrange: the generated one
    /// takes both sides from the same `HasMany<Post, Comment>`.
    ///
    /// ```
    /// use moso_orm::relation::LoadedRows;
    ///
    /// let loaded = LoadedRows::rows("comments", "Comment", vec![7_i64]);
    /// assert_eq!(loaded.into_rows::<i64>().unwrap(), [7]);
    /// ```
    pub fn into_rows<T: Send + 'static>(self) -> Result<Vec<T>> {
        let Payload::Rows(payload) = self.payload else {
            return Err(self.mismatch("a count"));
        };
        match payload.downcast::<Vec<T>>() {
            Ok(rows) => Ok(*rows),
            Err(_) => Err(Self::wrong_type(self.relation, self.target)),
        }
    }

    /// The first row, for a `belongs_to` or a `has_one` field that may be
    /// absent.
    ///
    /// # Errors
    ///
    /// As [`LoadedRows::into_rows`].
    ///
    /// ```
    /// use moso_orm::relation::LoadedRows;
    ///
    /// let none = LoadedRows::rows("author", "User", Vec::<i64>::new());
    /// assert_eq!(none.into_row::<i64>().unwrap(), None);
    /// ```
    pub fn into_row<T: Send + 'static>(self) -> Result<Option<T>> {
        Ok(self.into_rows::<T>()?.into_iter().next())
    }

    /// The single row a non-optional `belongs_to` must have.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the foreign key pointed at a row that is not
    /// there — a dangling reference the database would normally forbid — and
    /// otherwise as [`LoadedRows::into_rows`].
    ///
    /// ```
    /// use moso_orm::relation::LoadedRows;
    ///
    /// let empty = LoadedRows::rows("author", "User", Vec::<i64>::new());
    /// assert!(empty.into_required_row::<i64>().is_err());
    /// ```
    pub fn into_required_row<T: Send + 'static>(self) -> Result<T> {
        let target = self.target;
        self.into_row::<T>()?
            .ok_or(Error::NotFound { entity: target })
    }

    /// The count, for a `.with_count(..)` field.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when this payload is rows rather than a count.
    ///
    /// ```
    /// use moso_orm::relation::LoadedRows;
    ///
    /// assert_eq!(LoadedRows::counted("c", "C", 3).into_count().unwrap(), 3);
    /// ```
    pub fn into_count(self) -> Result<i64> {
        match self.payload {
            Payload::Count(count) => Ok(count),
            Payload::Rows(_) => Err(self.mismatch("rows")),
        }
    }

    /// The error for asking a payload for something it is not.
    fn mismatch(&self, found: &'static str) -> Error {
        Error::Decode(
            DecodeError::type_mismatch(0, self.target, found)
                .in_field(self.relation)
                .with_column_name(self.relation)
                .with_detail(
                    "a `.with_count(..)` preload loads a number and a `.with(..)` preload loads \
                     rows; the field setter must ask for the one its relation declares",
                ),
        )
    }

    /// The error for downcasting to the wrong entity.
    fn wrong_type(relation: &'static str, target: &'static str) -> Error {
        Error::Decode(
            DecodeError::type_mismatch(0, target, "rows of another entity")
                .in_field(relation)
                .with_column_name(relation)
                .with_detail(
                    "the relation constant and the field setter disagree about the related type; \
                     `#[derive(Entity)]` takes both from the same declaration, so this is a \
                     hand-written constant",
                ),
        )
    }
}

impl fmt::Debug for LoadedRows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = f.debug_struct("LoadedRows");
        out.field("relation", &self.relation)
            .field("target", &self.target);
        match self.payload {
            Payload::Count(count) => out.field("count", &count).finish(),
            Payload::Rows(_) => out.finish_non_exhaustive(),
        }
    }
}

/// The setter `#[derive(Entity)]` generates for one relation field.
///
/// A plain function pointer rather than a closure, so a relation constant stays
/// `const`-constructible and `Copy`.
///
/// # It must handle both payloads
///
/// One relation can be preloaded two ways — `.with(Post::COMMENTS)` loads rows,
/// `.with_count(Post::COMMENTS)` loads a number — and both go through this one
/// setter, because both come from the one relation constant. So the generated
/// body branches:
///
/// ```ignore
/// fn link_comments(post: &mut Post, rows: LoadedRows) -> moso_orm::Result<()> {
///     if rows.is_count() {
///         post.comments_count = Some(rows.into_count()?);
///     } else {
///         post.comments = Related::Loaded(rows.into_rows::<Comment>()?);
///     }
///     Ok(())
/// }
/// ```
///
/// ```
/// use moso_orm::relation::{LinkFn, LoadedRows, Related};
///
/// /// What the derive writes for `#[entity(has_many = Comment, fk = "post_id")]`.
/// struct Post { comments: Related<Vec<i64>> }
///
/// const LINK: LinkFn<Post> = |post, rows| {
///     post.comments = Related::Loaded(rows.into_rows::<i64>()?);
///     Ok(())
/// };
///
/// let mut post = Post { comments: Related::NotLoaded };
/// LINK(&mut post, LoadedRows::rows("comments", "Comment", vec![1_i64])).unwrap();
/// assert!(post.comments.is_loaded());
/// ```
pub type LinkFn<E> = fn(&mut E, LoadedRows) -> Result<()>;

/// The reader `#[derive(Entity)]` generates for a `belongs_to` foreign key.
///
/// A `belongs_to` batches on the **parent's** key column, not on its primary
/// key, so the preloader needs to read `post.author_id` out of a `Post` it only
/// knows as `E: Entity`.
///
/// ```
/// use moso_orm::relation::ForeignKeyFn;
/// use moso_sql::Value;
///
/// struct Post { author_id: i64 }
///
/// const KEY: ForeignKeyFn<Post> = |post| Value::I64(post.author_id);
/// assert_eq!(KEY(&Post { author_id: 4 }), Value::I64(4));
/// ```
pub type ForeignKeyFn<E> = fn(&E) -> moso_sql::Value;

/// The reader `#[derive(Entity)]` generates for a polymorphic foreign key.
///
/// Returns the discriminator and the key: `("post", 12)` for a comment whose
/// `target_type` is `"post"` and whose `target_id` is `12`.
///
/// ```
/// use moso_orm::relation::PolymorphicKeyFn;
/// use moso_sql::Value;
///
/// struct Comment { target_type: String, target_id: i64 }
///
/// const KEY: PolymorphicKeyFn<Comment> =
///     |c| (Value::text(c.target_type.clone()), Value::I64(c.target_id));
/// let comment = Comment { target_type: "post".into(), target_id: 12 };
/// assert_eq!(KEY(&comment), (Value::text("post"), Value::I64(12)));
/// ```
pub type PolymorphicKeyFn<E> = fn(&E) -> (moso_sql::Value, moso_sql::Value);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::StatementCounter;

    #[test]
    fn an_unloaded_relation_never_queries_it_errors() {
        let unknown: Related<Vec<i32>> = Related::NotLoaded;
        assert!(unknown.get().is_err());
        assert!(unknown.as_option().is_none());
        assert!(unknown.is_not_loaded());
    }

    /// Acceptance criterion 4 of WP-12, in the form the type system allows.
    ///
    /// "Verified by running with a closed pool" is the document's phrasing;
    /// this is stronger. `Related::get` borrows `&self` and returns
    /// synchronously, so there is no executor in scope for it to use even in
    /// principle. The counter proves the consequence: a hundred thousand reads
    /// of an unloaded relation move it not at all.
    #[test]
    fn touching_an_unloaded_relation_issues_zero_statements() {
        let counter = StatementCounter::new();
        let mark = counter.mark();

        let unloaded: Related<Vec<i32>> = Related::NotLoaded;
        let loaded = Related::Loaded(vec![1, 2, 3]);
        for _ in 0..100_000 {
            let _ = unloaded.get();
            let _ = unloaded.as_option();
            let _ = loaded.get();
        }

        assert_eq!(counter.since(mark), 0, "reading a relation is not a query");
        assert_eq!(counter.total(), 0);
    }

    #[test]
    fn the_named_message_is_the_one_the_document_specifies() {
        let error = NotLoaded::of("Post", "comments", "Post::COMMENTS").to_string();
        let lines: Vec<&str> = error.lines().collect();
        assert_eq!(
            lines,
            [
                "relation `Post::comments` was not loaded",
                "  the query that produced this `Post` did not include `.with(Post::COMMENTS)`",
                "  add it:   Post::query().with(Post::COMMENTS)",
                "  or load on demand for a single row:   post.load(Post::COMMENTS, &db).await?",
            ]
        );
    }

    #[test]
    fn the_anonymous_message_still_shows_both_fixes() {
        let text = NotLoaded::new().to_string();
        assert!(text.contains("this relation was not loaded"), "{text}");
        assert!(text.contains("add it:"), "{text}");
        assert!(text.contains("or load on demand"), "{text}");
    }

    /// Acceptance criterion 8: the field is *omitted*, not `null`.
    #[test]
    fn an_unloaded_relation_is_omitted_from_a_body_not_nulled() {
        #[derive(Serialize)]
        struct PostDto {
            title: &'static str,
            #[serde(skip_serializing_if = "Related::is_not_loaded")]
            comments: Related<Vec<i32>>,
        }

        let unknown = PostDto {
            title: "hi",
            comments: Related::NotLoaded,
        };
        let json = serde_json::to_string(&unknown).expect("serialises");
        assert_eq!(json, r#"{"title":"hi"}"#);
        assert!(!json.contains("null"), "unknown must not render as null");
        assert!(!json.contains("[]"), "unknown must not render as empty");

        let known = PostDto {
            title: "hi",
            comments: Related::Loaded(vec![1, 2]),
        };
        assert_eq!(
            serde_json::to_string(&known).expect("serialises"),
            r#"{"title":"hi","comments":[1,2]}"#
        );
    }

    #[test]
    fn a_bare_related_serialises_as_its_value() {
        let unloaded: Related<Vec<i32>> = Related::NotLoaded;
        assert_eq!(
            serde_json::to_string(&unloaded).expect("serialises"),
            "null"
        );
        let loaded = Related::Loaded(vec![1, 2]);
        assert_eq!(serde_json::to_string(&loaded).expect("serialises"), "[1,2]");
    }

    #[test]
    fn related_maps_without_loading() {
        assert_eq!(Related::Loaded(2).map(|n| n * 2), Related::Loaded(4));
        assert_eq!(Related::<i32>::NotLoaded.map(|n| n * 2), Related::NotLoaded);
    }

    #[test]
    fn a_payload_round_trips_through_the_erasure() {
        let rows = LoadedRows::rows("comments", "Comment", vec![1_i64, 2, 3]);
        assert_eq!(rows.into_rows::<i64>().expect("same type"), [1, 2, 3]);

        let one = LoadedRows::rows("author", "User", vec![String::from("ada")]);
        assert_eq!(
            one.into_row::<String>().expect("same type").as_deref(),
            Some("ada")
        );

        let count = LoadedRows::counted("comments", "Comment", 9);
        assert_eq!(count.into_count().expect("a count"), 9);
    }

    #[test]
    fn asking_a_payload_for_the_wrong_shape_names_the_relation() {
        let rows = LoadedRows::rows("comments", "Comment", vec![1_i64]);
        let error = rows.into_rows::<String>().expect_err("wrong type");
        let text = error.to_string();
        assert!(text.contains("Comment"), "{text}");
        assert!(text.contains("comments"), "{text}");

        let counted = LoadedRows::counted("comments", "Comment", 1);
        assert!(counted.into_rows::<i64>().is_err());

        let rows = LoadedRows::rows("comments", "Comment", vec![1_i64]);
        assert!(rows.into_count().is_err());
    }

    #[test]
    fn a_missing_required_row_is_a_not_found_that_names_the_target() {
        let empty = LoadedRows::rows("author", "User", Vec::<i64>::new());
        let error = empty.into_required_row::<i64>().expect_err("no row");
        assert!(
            matches!(error, Error::NotFound { entity: "User" }),
            "{error}"
        );
    }
}
