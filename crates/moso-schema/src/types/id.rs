//! [`Id`] — a UUID that knows what it identifies.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::json_schema::{SchemaGenerator, SchemaNode, SchemaRef, StringBuilder};
use crate::schema::Schema;
use crate::types::ConstraintError;
use crate::validate::{Validate, ValidationCtx, ValidationErrors};

/// Anything an [`Id`] can be parameterised by.
///
/// Blanket-implemented for every `'static` type, so `Id<User>` works whether or
/// not `User` is anything in particular — including a bare marker
/// `pub struct User;`.
///
/// ```
/// use moso_schema::types::{Id, IdMarker};
///
/// /// Nothing but a name for the type system.
/// pub struct User;
///
/// /// A full domain type works just as well.
/// pub struct Post { pub title: String }
///
/// fn is_marker<T: IdMarker>() {}
/// is_marker::<User>();
/// is_marker::<Post>();
///
/// // Which is what makes the two identifier types distinct.
/// let user: Id<User> = Id::new();
/// let post: Id<Post> = Id::new();
/// # let _ = (user, post);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be used as an `Id` marker",
    label = "not `'static`",
    note = "an `Id<E>` marker must be a `'static` type; borrowed types cannot be markers",
    note = "help: use an owned type:\n    pub struct Marker;\n    pub type UserId = moso::Id<Marker>;"
)]
pub trait IdMarker: 'static {}

#[diagnostic::do_not_recommend]
impl<T: ?Sized + 'static> IdMarker for T {}

/// A UUID tagged with the type it identifies.
///
/// `fn get(id: Id<User>)` cannot be called with an `Id<Post>`. That eliminates
/// an entire class of production bug — passing the right-shaped wrong
/// identifier — and costs nothing at runtime: the marker lives in a
/// `PhantomData<fn() -> E>`, so `Id<E>` is `Copy`, `Send` and `Sync` no matter
/// what `E` is, and is exactly the size of a `Uuid`.
///
/// New identifiers are UUIDv7, which sorts by creation time and so does not
/// fragment a B-tree index the way v4 does.
///
/// ```text
/// JSON Schema: { "type": "string", "format": "uuid" }
/// ```
///
/// # The mistake it prevents
///
/// ```compile_fail,E0308
/// use moso_schema::types::Id;
/// struct User;
/// struct Post;
///
/// fn delete_user(id: Id<User>) { /* … */ }
///
/// let post_id: Id<Post> = Id::new();
/// delete_user(post_id); // error[E0308]: expected `Id<User>`, found `Id<Post>`
/// ```
///
/// The same code with both identifiers spelled `Uuid` compiles, runs, and
/// deletes the wrong row. Crossing the boundary on purpose is possible, but it
/// has to be written down — see [`Id::cast`].
///
/// ```
/// use moso_schema::types::Id;
/// struct User;
/// struct Post;
///
/// let post_id: Id<Post> = Id::new();
/// let user_id: Id<User> = post_id.cast();
/// assert_eq!(post_id.into_uuid(), user_id.into_uuid());
/// ```
pub struct Id<E: IdMarker>(Uuid, PhantomData<fn() -> E>);

impl<E: IdMarker> Id<E> {
    /// The JSON Schema `format` this type emits.
    pub const FORMAT: &'static str = "uuid";

    /// The all-zero identifier.
    pub const NIL: Self = Self(Uuid::nil(), PhantomData);

    /// A fresh, time-ordered UUIDv7.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7(), PhantomData)
    }

    /// A fresh, time-ordered UUIDv7. Spelled out, for call sites where the
    /// version matters to the reader.
    #[must_use]
    pub fn new_v7() -> Self {
        Self::new()
    }

    /// The all-zero identifier — [`Id::NIL`] as a function, for use where a
    /// constant does not fit (`unwrap_or_else(Id::nil)`).
    #[must_use]
    pub const fn nil() -> Self {
        Self::NIL
    }

    /// True for the all-zero identifier.
    #[must_use]
    pub const fn is_nil(&self) -> bool {
        self.0.is_nil()
    }

    /// Tag an existing UUID.
    #[must_use]
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id, PhantomData)
    }

    /// The untagged UUID.
    #[must_use]
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }

    /// Borrow the untagged UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Parse a hyphenated or simple UUID string.
    ///
    /// Accepts every spelling `uuid` does — hyphenated, simple, braced and
    /// URN — and normalises on the way in, so `{67E55044-...}` and
    /// `67e55044...` are the same identifier.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `format`.
    pub fn parse(value: &str) -> Result<Self, ConstraintError> {
        Uuid::parse_str(value).map(Self::from_uuid).map_err(|_| {
            ConstraintError::format(
                Self::FORMAT,
                "must be a UUID, e.g. `67e55044-10b1-426f-9247-bb680e5fe0c8`",
            )
        })
    }

    /// Retag this identifier as pointing at a different entity.
    ///
    /// Deliberately explicit and easy to grep for: it defeats the whole point
    /// of the type, and should appear only at a boundary where the retag is
    /// provably correct.
    #[must_use]
    pub const fn cast<F: IdMarker>(self) -> Id<F> {
        Id(self.0, PhantomData)
    }
}

impl<E: IdMarker> Default for Id<E> {
    /// [`Id::NIL`], not a fresh identifier: a `Default` that allocates
    /// randomness surprises people.
    fn default() -> Self {
        Self::NIL
    }
}

// The derives would all add a `E: Trait` bound, which is wrong: `Id<E>` is
// `Copy` even when `E` is not, because it does not contain an `E`.
impl<E: IdMarker> Clone for Id<E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E: IdMarker> Copy for Id<E> {}

impl<E: IdMarker> PartialEq for Id<E> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<E: IdMarker> Eq for Id<E> {}

impl<E: IdMarker> PartialOrd for Id<E> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<E: IdMarker> Ord for Id<E> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl<E: IdMarker> Hash for Id<E> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<E: IdMarker> fmt::Debug for Id<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The marker's name is the point of the type, so print it.
        let marker = std::any::type_name::<E>();
        let short = marker.rsplit("::").next().unwrap_or(marker);
        write!(f, "Id<{short}>({})", self.0)
    }
}

impl<E: IdMarker> fmt::Display for Id<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl<E: IdMarker> From<Uuid> for Id<E> {
    fn from(u: Uuid) -> Self {
        Self::from_uuid(u)
    }
}

impl<E: IdMarker> From<Id<E>> for Uuid {
    fn from(id: Id<E>) -> Self {
        id.0
    }
}

impl<E: IdMarker> FromStr for Id<E> {
    type Err = ConstraintError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl<E: IdMarker> TryFrom<String> for Id<E> {
    type Error = ConstraintError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl<'a, E: IdMarker> TryFrom<&'a str> for Id<E> {
    type Error = ConstraintError;

    fn try_from(s: &'a str) -> Result<Self, Self::Error> {
        Self::parse(s)
    }
}

impl<E: IdMarker> Serialize for Id<E> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de, E: IdMarker> Deserialize<'de> for Id<E> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Uuid::deserialize(d).map(Self::from_uuid)
    }
}

impl<E: IdMarker> Validate for Id<E> {
    fn validate(&self, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        Ok(())
    }
}

impl<E: IdMarker> Schema for Id<E> {
    /// Advisory only: `Id<E>` is inlined, never registered, so two different
    /// markers do not collide despite sharing this name.
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Id")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
        StringBuilder::new().format(Self::FORMAT).build()
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }

    const HAS_CONSTRAINTS: bool = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct User;
    struct Post;

    #[test]
    fn ids_are_copy_and_sized_like_a_uuid() {
        let a = Id::<User>::from_uuid(Uuid::nil());
        let b = a;
        assert_eq!(a, b, "Id must be Copy, not moved");
        assert_eq!(
            std::mem::size_of::<Id<User>>(),
            std::mem::size_of::<Uuid>(),
            "the marker must cost nothing"
        );
    }

    #[test]
    fn markers_do_not_need_to_be_send_or_sync_themselves() {
        fn assert_send_sync<T: Send + Sync>() {}
        // `*const ()` is neither `Send` nor `Sync`, but `Id` over it is both.
        assert_send_sync::<Id<*const ()>>();
    }

    #[test]
    fn debug_names_the_marker() {
        let id = Id::<Post>::from_uuid(Uuid::nil());
        let rendered = format!("{id:?}");
        assert!(rendered.starts_with("Id<Post>("), "got {rendered}");
    }

    #[test]
    fn cast_retags_without_changing_the_value() {
        let user: Id<User> = Id::from_uuid(Uuid::nil());
        let post: Id<Post> = user.cast();
        assert_eq!(user.into_uuid(), post.into_uuid());
    }

    #[test]
    fn parses_every_uuid_spelling() {
        let canonical = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        for input in [
            canonical,
            "67E55044-10B1-426F-9247-BB680E5FE0C8",
            "67e5504410b1426f9247bb680e5fe0c8",
            "{67e55044-10b1-426f-9247-bb680e5fe0c8}",
            "urn:uuid:67e55044-10b1-426f-9247-bb680e5fe0c8",
        ] {
            let id = Id::<User>::parse(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
            assert_eq!(id.to_string(), canonical, "for {input:?}");
        }
    }

    #[test]
    fn rejects_non_uuids_with_a_format_code() {
        for input in ["", "nope", "67e55044-10b1-426f-9247", "12345"] {
            let e = Id::<User>::parse(input).expect_err(input);
            assert_eq!(e.code().as_str(), crate::validate::codes::FORMAT);
            assert_eq!(
                e.params().get("format"),
                Some(&serde_json::json!("uuid")),
                "for {input:?}"
            );
            assert!(
                e.message().contains("67e55044"),
                "no example in the message"
            );
        }
        assert!("nope".parse::<Id<User>>().is_err());
        assert!(Id::<User>::try_from(String::from("nope")).is_err());
        assert!(Id::<User>::try_from("67e55044-10b1-426f-9247-bb680e5fe0c8").is_ok());
    }

    #[test]
    fn new_v7_is_time_ordered_and_nil_is_zero() {
        let first = Id::<User>::new_v7();
        let second = Id::<User>::new_v7();
        assert_ne!(first, second, "two fresh identifiers must differ");
        assert_eq!(first.as_uuid().get_version_num(), 7, "must be a v7 UUID");
        assert_eq!(second.as_uuid().get_version_num(), 7);
        // The millisecond prefix is what makes v7 index-friendly; it is
        // non-decreasing even when two calls land in the same millisecond.
        assert!(first.as_uuid().as_bytes()[..6] <= second.as_uuid().as_bytes()[..6]);

        assert!(Id::<User>::nil().is_nil());
        assert_eq!(Id::<User>::nil(), Id::<User>::default());
        assert_eq!(
            Id::<User>::nil().to_string(),
            "00000000-0000-0000-0000-000000000000"
        );
        assert!(!first.is_nil());
    }

    #[test]
    fn serialises_as_a_bare_uuid_string() {
        let id = Id::<User>::parse("67e55044-10b1-426f-9247-bb680e5fe0c8").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"67e55044-10b1-426f-9247-bb680e5fe0c8\"");
        assert_eq!(serde_json::from_str::<Id<User>>(&json).unwrap(), id);
        // A `Post` identifier deserialises from the same bytes — the tagging is
        // a compile-time property, not a wire-format one.
        assert_eq!(
            serde_json::from_str::<Id<Post>>(&json).unwrap().into_uuid(),
            id.into_uuid()
        );
        assert!(serde_json::from_str::<Id<User>>("\"nope\"").is_err());
    }

    #[test]
    fn json_schema_is_an_inline_uuid_string() {
        let node = Id::<User>::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            serde_json::json!({ "type": "string", "format": "uuid" })
        );
        assert!(
            Id::<User>::schema_ref().as_node().is_some(),
            "`Id` must be inlined, or every marker would collide on the name `Id`"
        );
    }
}
