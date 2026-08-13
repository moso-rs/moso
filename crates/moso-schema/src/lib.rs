#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "Moso's model layer: one type definition doing three jobs."]
//!
//! A Rust type that is a validated, documented API model normally needs three
//! ecosystems and two vocabularies for the same constraint:
//!
//! ```text
//! #[derive(Serialize, Deserialize, ToSchema, Validate)]
//! pub struct CreateUser {
//!     #[validate(length(min = 3, max = 32))]   // enforced
//!     #[schema(min_length = 3, max_length = 32)] // documented — must match by hand
//!     pub username: String,
//! }
//! ```
//!
//! They drift, and then the documentation lies. Moso's answer is one attribute
//! vocabulary and one derive, with the OpenAPI constraint *generated from* the
//! validation rule — they cannot disagree because there is only one of them.
//!
//! ```
//! use moso::prelude::*;
//!
//! /// A user, as the API accepts one.
//! #[derive(Schema)]
//! pub struct CreateUser {
//!     /// Public handle. Lowercase letters, digits and underscores.
//!     #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
//!     pub username: String,
//!     /// Contact address — the type carries the constraint.
//!     pub email: Email,
//!     /// Optional age, in years.
//!     #[schema(range = 13..=130)]
//!     pub age: Option<u8>,
//! }
//! # fn main() {
//! use moso_schema::Validate;
//!
//! let user: CreateUser = serde_json::from_str(
//!     r#"{"username":"ada","email":"ada@example.com","age":36}"#,
//! ).unwrap();
//! assert!(user.validate(&mut moso_schema::ValidationCtx::new()).is_ok());
//!
//! // The same attribute produced the documented constraint.
//! let mut generator = moso_schema::json_schema::SchemaGenerator::default();
//! let node = <CreateUser as moso_schema::Schema>::json_schema(&mut generator);
//! assert_eq!(node.properties["username"].max_length, Some(32));
//! # }
//! ```
//!
//! # What is in here
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`schema`] | the [`Schema`] trait |
//! | [`validate`] | [`Validate`], [`ValidationCtx`], [`ValidationErrors`], the closed [`codes`] set |
//! | [`checks`] | the `check_*` helpers generated validation bodies call |
//! | [`json_schema`] | the JSON Schema 2020-12 model, its builders and [`SchemaGenerator`] |
//! | [`message`] | English messages and the [`MessageProvider`] extension point |
//! | [`types`] | constrained types: [`Email`], [`Password`], [`Id`], … |
//!
//! # Independence
//!
//! Nothing here knows about HTTP. The crate is usable on its own — for a CLI's
//! configuration model, or a message-queue payload — and `moso-openapi`
//! depends on *it*, not the other way round.
//!
//! # Two guarantees worth stating plainly
//!
//! **Attributes and types are not alternatives.** `#[schema(len = …)]` protects
//! one field; a constrained type such as [`Email`] protects every value of that
//! type everywhere in the program, including ones constructed in code that
//! never saw a request. Prefer the type where one exists.
//!
//! **Validation is synchronous and pure.** There is deliberately no
//! `async fn validate`. "Is this email already taken?" is not validation: a
//! check-then-act against a database is a race, and the correct place for that
//! rule is the transaction that will enforce it, surfacing as a `409 Conflict`
//! with a field pointer.

pub mod checks;
pub mod json_schema;
pub mod message;
pub mod schema;
pub mod types;
pub mod validate;

/// The regular-expression engine `#[schema(pattern = "…")]` compiles against.
///
/// [`checks::check_pattern`] takes a `&regex::Regex`, and the `Validate` body a
/// derive emits builds one in a `OnceLock`, so the generated code needs to name
/// this crate. Re-exporting it here — and, through the facade, at
/// `moso::__private::regex` — means an application never adds `regex` to its own
/// manifest and can never end up compiling its patterns against a different
/// version from the one `check_pattern` matches with.
pub use regex;

pub use checks::{Bounds, field_error};
pub use json_schema::{
    AdditionalProperties, ArrayBuilder, DEFAULT_REF_PREFIX, DIALECT, Discriminator, IntoNumber,
    JsonType, NumberBuilder, ObjectBuilder, SchemaCollision, SchemaGenerator, SchemaNode,
    SchemaRef, StringBuilder, TypeSet,
};
pub use message::{
    ChainedMessages, DefaultMessages, InvalidLocale, Locale, MessageProvider, default_message,
};
pub use schema::{Schema, generic_schema_name, inline_schema_ref};
pub use types::{
    Bounded, ConstraintError, Cursor, Email, EscapeHtml, Hostname, Id, IdMarker, IntegerValue,
    IpCidr, Length, Measured, NonEmpty, Password, PhoneE164, SERDE_ERROR_PREFIX, SanitisePolicy,
    Sanitised, Slug, StripTags, Trimmed, Url, parse_serde_message,
};
pub use validate::{
    DEFAULT_MAX_ERRORS, ErrorCode, FieldError, Validate, ValidationCtx, ValidationErrors, codes,
    escape_token, push_token,
};
