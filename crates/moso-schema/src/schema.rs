//! The [`Schema`] trait: one type definition doing three jobs.
//!
//! A `Schema` type is simultaneously
//!
//! 1. a serde model (`Serialize + DeserializeOwned`),
//! 2. a set of runtime constraints ([`Validate`]),
//! 3. a JSON Schema 2020-12 document ([`Schema::json_schema`]),
//!
//! and — this is the point — jobs 2 and 3 are generated from the *same*
//! `#[schema(...)]` attributes, so the documented constraint and the enforced
//! constraint cannot drift apart.
//!
//! # Implementing it
//!
//! Almost always with `#[derive(Schema)]`. Hand impls are for primitives and
//! foreign types; [`crate::json_schema`] provides the builders they need.

use std::borrow::Cow;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::json_schema::{SchemaGenerator, SchemaNode, SchemaRef};
use crate::validate::Validate;

/// A validated, documented API model.
///
/// # Naming
///
/// [`Schema::schema_name`] is the key a type occupies in
/// `components/schemas`, and generated client type names are derived from it,
/// so it is part of the public API surface: changing it breaks generated
/// clients. Two distinct types returning the same name is a boot error.
///
/// Generic types mangle their parameters into the name — `Page<UserOut>`
/// becomes `Page_UserOut` — using a documented, stable function so client type
/// names are predictable.
///
/// Types with no *component* identity — `Vec<T>`, `Option<T>`, tuples,
/// primitives, and everything in [`crate::types`] — **override
/// [`Schema::schema_ref`]** to return [`SchemaRef::Inline`]. They are written
/// out in place rather than registered, so their `schema_name` is advisory: it
/// is used only when mangling a generic name (`Page<String>` →
/// `Page_String`) and never becomes a `components/schemas` key. That is also
/// why `Id<User>` and `Id<Post>` sharing the name `Id` is not a collision.
///
/// A truly nameless type returns `""`, which mangles as `Anonymous`.
///
/// # `HAS_CONSTRAINTS`
///
/// Computed by the derive. It drives whether a `422 Unprocessable Entity`
/// response is documented for operations taking this type as a body, so a
/// constraint-free DTO does not pollute the OpenAPI document with an
/// impossible response.
///
/// # Example
///
/// Implemented by `#[derive(moso::Schema)]`, by every primitive, and by the
/// constrained types in [`crate::types`]. `Json<T>`, `Query<T>`, `Path<T>` and
/// `Headers<T>` all require it, and it is what puts the type in
/// `components/schemas`.
///
/// ```
/// use moso::prelude::*;
/// use moso::schema::SchemaGenerator;
///
/// /// A user, as the API accepts one.
/// #[derive(Schema)]
/// pub struct CreateUser {
///     /// Public handle.
///     #[schema(len = 3..=32)]
///     pub username: String,
///     /// Contact address.
///     pub email: Email,
/// }
///
/// # fn main() {
/// assert_eq!(CreateUser::schema_name(), "CreateUser");
/// assert!(CreateUser::HAS_CONSTRAINTS);
///
/// let mut generator = SchemaGenerator::default();
/// let node = CreateUser::json_schema(&mut generator);
///
/// assert_eq!(node.required, ["username", "email"]);
/// assert_eq!(node.properties["username"].min_length, Some(3));
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a Moso schema type",
    label = "not a schema",
    note = "request bodies, response bodies and typed parameters must implement `Schema`",
    note = "entities deliberately do not implement `Schema`: define a separate DTO so your \
            database columns are not your API contract",
    note = "help: derive it:\n    #[derive(moso::Schema)]\n    pub struct {Self} {{ /* … */ }}",
    note = "help: for a foreign type you do not own, wrap it in a newtype and derive `Schema` \
            on the wrapper"
)]
pub trait Schema: Serialize + DeserializeOwned + Validate + Send + Sync + 'static {
    /// The stable name this type occupies in `components/schemas`.
    ///
    /// Return `""` for anonymous types; see the trait docs.
    fn schema_name() -> Cow<'static, str>;

    /// Emit this type's schema, registering any named types it references with
    /// `generator`.
    ///
    /// Implementations must reach nested types through
    /// [`SchemaGenerator::subschema_for`] rather than calling their
    /// `json_schema` directly, or those types will not be registered and the
    /// document will contain dangling `$ref`s.
    ///
    /// The parameter is named `generator` rather than `gen` because `gen` is a
    /// reserved keyword in Rust 2024.
    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode;

    /// A cheap reference to this type's schema.
    ///
    /// **Registers nothing.** Use it only where the schema is already known to
    /// be registered; when assembling a document, call
    /// [`SchemaGenerator::subschema_for`] instead.
    fn schema_ref() -> SchemaRef {
        SchemaRef::inline_or_named(Self::schema_name())
    }

    /// True when at least one field carries a constraint, so a `422` is
    /// reachable.
    const HAS_CONSTRAINTS: bool = false;
}

/// The [`Schema::schema_ref`] implementation for an anonymous type.
///
/// Builds the node with a throwaway generator, which is correct exactly when
/// `T::json_schema` registers nothing — true of every primitive and every
/// constrained newtype in [`crate::types`]. A type whose schema references a
/// *named* type must not use this, or that name will never reach
/// `components/schemas`.
#[must_use]
pub fn inline_schema_ref<T: Schema>() -> SchemaRef {
    SchemaRef::inline(T::json_schema(&mut SchemaGenerator::default()))
}

/// Register `T` with `generator` and return the node describing it.
///
/// The free-function spelling of [`SchemaGenerator::subschema_for`]. Generated
/// code prefers it because `schema_of::<T>(__g)` survives macro substitution of
/// an arbitrary field type, where `__g.subschema_for::<T>()` needs the turbofish
/// to be re-parsed at the call site.
///
/// Named types come back as `{"$ref": …}` and are registered with `generator`;
/// anonymous ones ([`Vec<T>`], [`Option<T>`], primitives) are written out in
/// place, with any named type they mention still registered.
///
/// ```
/// # use moso_schema::json_schema::{SchemaGenerator, JsonType};
/// # use moso_schema::schema::schema_of;
/// let mut generator = SchemaGenerator::default();
/// let node = schema_of::<Vec<String>>(&mut generator);
/// assert_eq!(node.types.primary(), Some(JsonType::Array));
/// assert!(generator.definitions().is_empty(), "no named type was involved");
/// ```
#[must_use]
pub fn schema_of<T: Schema>(generator: &mut SchemaGenerator) -> SchemaNode {
    generator.subschema_for::<T>()
}

/// Mangle a generic type's parameters into a schema name.
///
/// `Page<UserOut>` → `Page_UserOut`; `Either<A, B>` → `Either_A_B`. The
/// function is public and documented because generated client type names
/// depend on it and must be predictable.
///
/// Parameter names come from each argument's own [`Schema::schema_name`], so
/// nesting composes: `Page<Vec<UserOut>>` uses the anonymous `Vec` name and
/// falls back to `Array`.
#[must_use]
pub fn generic_schema_name(base: &str, arguments: &[Cow<'static, str>]) -> Cow<'static, str> {
    if arguments.is_empty() {
        return Cow::Owned(base.to_owned());
    }
    let mut name = String::with_capacity(base.len() + arguments.len() * 8);
    name.push_str(base);
    for a in arguments {
        name.push('_');
        name.push_str(if a.is_empty() { "Anonymous" } else { a });
    }
    Cow::Owned(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_names_are_stable() {
        assert_eq!(generic_schema_name("Page", &[]), "Page");
        assert_eq!(
            generic_schema_name("Page", &[Cow::Borrowed("UserOut")]),
            "Page_UserOut"
        );
        assert_eq!(
            generic_schema_name("Either", &[Cow::Borrowed("A"), Cow::Borrowed("B")]),
            "Either_A_B"
        );
        assert_eq!(
            generic_schema_name("Page", &[Cow::Borrowed("")]),
            "Page_Anonymous"
        );
    }

    #[test]
    fn generic_names_nest_left_to_right() {
        // `Page<Vec<UserOut>>` — the argument's own name is used verbatim, so
        // the mangling of a nested generic is the mangling of its parts.
        let inner = generic_schema_name("Array", &[Cow::Borrowed("UserOut")]);
        assert_eq!(inner, "Array_UserOut");
        assert_eq!(generic_schema_name("Page", &[inner]), "Page_Array_UserOut");
    }

    #[test]
    fn inline_schema_ref_never_registers() {
        // The contract of `inline_schema_ref`: it uses a throwaway generator, so
        // it is only correct for types that register nothing.
        let r = inline_schema_ref::<String>();
        assert!(r.as_node().is_some());
        assert!(!r.is_ref());
    }

    #[test]
    fn schema_of_matches_subschema_for() {
        let mut a = SchemaGenerator::default();
        let mut b = SchemaGenerator::default();
        assert_eq!(
            schema_of::<Option<Vec<u8>>>(&mut a),
            b.subschema_for::<Option<Vec<u8>>>()
        );
    }
}
