//! `Json<T>` — parse, validate and document in one step.
//!
//! ```
//! use moso::prelude::*;
//! /// A user, as the API accepts one.
//! #[derive(Schema)]
//! pub struct CreateUser {
//!     /// Public handle.
//!     #[schema(len = 3..=32)]
//!     pub username: String,
//! }
//! /// A user, as the API returns one.
//! #[derive(Schema)]
//! pub struct UserOut {
//!     /// Stable identifier.
//!     pub id: u64,
//! }
//!
//! /// Create a user.
//! #[endpoint]
//! async fn create(Json(body): Json<CreateUser>) -> Result<Created<Json<UserOut>>> {
//!     let _ = body.username;
//!     Ok(Created::at("/users/1", Json(UserOut { id: 1 })))
//! }
//! # fn main() { assert_eq!(Router::new().post("/users", moso::ep!(create)).len(), 1); }
//! ```
//!
//! `body` cannot exist unless the payload parsed *and* passed
//! `CreateUser::validate` — a short `username` never reaches the function.
//!
//! # The three properties this exists to guarantee
//!
//! 1. **Deserialisation errors carry a field path.** `serde_path_to_error` is
//!    mandatory, not an option a team remembers to add:
//!    `{"detail": "invalid type: string, expected u32", "pointer": "/items/2/quantity"}`.
//! 2. **Validation is part of extraction.** There is no way to obtain a
//!    `CreateUser` from a request without `CreateUser::validate` having run.
//!    No `.validate()?` line to forget in a handler written at 5pm.
//! 3. **The 400 and the 422 are in the OpenAPI document**, generated from the
//!    same constraint declarations that enforce them.
//!
//! # Order of operations, and why it is that order
//!
//! ```text
//! read body with a hard byte cap   → 413 before allocating, never after
//!   ↓
//! scan nesting depth               → 400 before a recursive descent begins
//!   ↓
//! serde_path_to_error::deserialize → 400 with an RFC 6901 pointer
//!   ↓
//! T::validate                      → 422 with per-field codes and pointers
//! ```
//!
//! The cap comes first so that a 100 MiB body against a 1 MiB limit costs one
//! megabyte of memory, not a hundred. The depth scan comes second because
//! `[[[[…]]]]` is cheap to *write* and expensive to *parse*: two megabytes of
//! open brackets is a legal body under `http.body_max` and a million-frame
//! recursion under `serde_json`. See [`check_json_depth`]. Constrained types
//! such as
//! [`Email`](moso_schema::Email) fail during *deserialisation* rather than
//! validation; [`moso_schema::parse_serde_message`] recognises those and
//! promotes them from a 400 to a 422, so `{"email": "nope"}` reports a
//! constraint failure and `{"email": 7}` reports a type error.
//!
//! # As a response
//!
//! `Json<T>` is also a response type: `Json(user)` serialises `T` with a
//! `200 OK` and `application/json`. Handlers returning a bare `T: Schema` work
//! too — `#[derive(Schema)]` generates the `IntoResponse` and [`Describe`]
//! impls for the type itself.

use http::StatusCode;
use moso_openapi::{ContentType, OperationBuilder, ResponseSpec};
use moso_schema::{Schema, ValidationErrors, parse_serde_message};

use crate::ctx::RequestCtx;
use crate::error::{Error, Result};
use crate::extract::ExtractBody;
use crate::extract::body::read_limited;
use crate::extract::query::pointer_for_path;
use crate::response::{Describe, IntoResponse};
use crate::{Request, Response};

/// A JSON request body or response body carrying `T`.
///
/// The body is read under a hard byte cap, deserialised with `serde_path_to_error`
/// (so a malformed payload becomes a `400` naming the exact JSON Pointer), then
/// validated (so a well-formed but invalid one becomes a `422`, one entry per
/// failed field). There is no way to obtain a `T` from a request that skipped
/// either step.
///
/// Also a response type: returning `Json<T>` sends `T` as a `200`.
///
/// ```
/// use moso::prelude::*;
///
/// /// A user, as the API accepts one.
/// #[derive(Schema)]
/// pub struct CreateUser {
///     /// Public handle.
///     #[schema(len = 3..=32)]
///     pub username: String,
/// }
///
/// /// Create a user.
/// #[endpoint]
/// async fn create(Json(body): Json<CreateUser>) -> Result<Json<CreateUser>> {
///     // `body.username` is already known to be 3..=32 characters long.
///     Ok(Json(body))
/// }
/// # fn main() {
/// let response = Json(CreateUser { username: "ada".to_owned() }).into_response();
/// assert_eq!(response.headers()["content-type"], "application/json");
/// # }
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Json<T>(pub T);

/// The JSON Pointer root a body failure is reported under: the document root.
///
/// A body *is* the document, so `/tags/2` addresses it exactly as RFC 6901
/// intends. Only the non-body sources need a `/query`, `/path` or `/header`
/// prefix to say which part of the request they came from.
pub const BODY_POINTER_ROOT: &str = "";

impl<T> Json<T> {
    /// The wrapped value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> core::ops::Deref for Json<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> From<T> for Json<T> {
    fn from(value: T) -> Self {
        Json(value)
    }
}

impl<T: Schema> ExtractBody for Json<T> {
    fn describe(op: &mut OperationBuilder) {
        op.request_body_of::<T>(ContentType::Json, true);
        op.response(
            400,
            ResponseSpec::problem("Malformed JSON, or nesting past `http.json_depth_max`"),
        );
        op.response(
            413,
            ResponseSpec::problem("The body exceeded `http.body_max`"),
        );
        op.response(415, ResponseSpec::problem("The `Content-Type` is not JSON"));
        // A 422 is only reachable when `T` declares at least one constraint.
        // Documenting one for a constraint-free DTO would tell a client to
        // handle a response the server can never send.
        if T::HAS_CONSTRAINTS {
            op.response(422, ResponseSpec::validation_problem_of::<T>());
            op.mark_validated();
        }
    }

    async fn extract_body(req: Request, ctx: &RequestCtx) -> Result<Self> {
        if !is_json_content_type(req.headers()) {
            return Err(Error::unsupported_media(content_type_of(req.headers())));
        }
        let bytes = read_limited(req, ctx.limits().body_max).await?;
        check_json_depth(bytes.as_slice(), ctx.limits().json_depth_max)?;
        let value = from_slice::<T>(bytes.as_slice())?;
        let mut validation = ctx.validation(BODY_POINTER_ROOT);
        value.validate(&mut validation).map_err(Error::validation)?;
        Ok(Json(value))
    }
}

impl<T: Schema> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        crate::response::json_response(StatusCode::OK, &self.0)
    }
}

impl<T: Schema> Describe for Json<T> {
    fn describe(op: &mut OperationBuilder) {
        crate::response::describe_json::<T>(op, 200);
    }
}

/// Deserialise a JSON document into `T`, with a JSON Pointer on failure.
///
/// Shared with anything else that reads a JSON payload — the CLI, `moso-test` —
/// so the pointer and the 400-versus-422 split are decided once.
///
/// # Errors
/// 400 with an RFC 6901 pointer for malformed or wrongly-typed JSON; 422 with a
/// constraint code when a constrained type such as
/// [`Email`](moso_schema::Email) rejects the value during deserialisation.
pub fn from_slice<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    serde_path_to_error::deserialize(&mut deserializer).map_err(json_error)
}

/// Refuse a document that nests deeper than `max_depth`.
///
/// A byte scan, run **before** `serde_json` sees the payload. `serde_json`'s own
/// recursion limit is a fixed 128 and is a stack-overflow guard rather than a
/// policy: `http.json_depth_max` is the number an operator configured, and it is
/// usually smaller. Doing it here also means the deep document is rejected
/// before any of the tree it describes is allocated, which is the whole point of
/// a limit.
///
/// Braces and brackets inside a string literal are not structure, so the scan
/// tracks string state and its backslash escapes. Nothing else in JSON can
/// contain a delimiter, so that is the whole grammar this needs to know: an
/// unbalanced closer, a stray comma, a truncated document — every other way of
/// being malformed is `serde_json`'s to report, with the pointer it can produce
/// and this cannot.
///
/// ```
/// use moso::extract::check_json_depth;
///
/// assert!(check_json_depth(br#"{"a":[1,2,3]}"#, 2).is_ok());
/// assert!(check_json_depth(br#"{"a":{"b":{}}}"#, 2).is_err());
///
/// // A bracket inside a string is text, not structure.
/// assert!(check_json_depth(br#"{"a":"[[[[[[["}"#, 1).is_ok());
/// ```
///
/// # Errors
/// A 400 naming the limit when the document nests deeper than `max_depth`. A
/// `max_depth` of zero is treated as one, because a limit that rejects `{}` is a
/// misconfiguration rather than a policy.
pub fn check_json_depth(bytes: &[u8], max_depth: usize) -> Result<()> {
    let limit = max_depth.max(1);
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;

    for byte in bytes {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > limit {
                    return Err(Error::bad_request(format!(
                        "the JSON body nests deeper than the {limit}-level limit"
                    ))
                    .with_extension("max_depth", limit));
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// Map a `serde_json` failure onto the taxonomy.
///
/// A constrained type rejecting a value inside `Deserialize` is a *validation*
/// failure that happens to surface through serde, so it is promoted to a 422
/// carrying the constraint's own code. Everything else is a 400: the document
/// did not parse, or a member had the wrong JSON type.
fn json_error(error: serde_path_to_error::Error<serde_json::Error>) -> Error {
    let message = error.inner().to_string();
    if let Some((code, detail)) = parse_serde_message(&message) {
        let pointer = pointer_for_path("", error.path());
        return Error::validation(ValidationErrors::one(
            pointer,
            code.to_owned(),
            strip_position(detail).to_owned(),
        ));
    }
    Error::from_json_path(error)
}

/// Drop the ` at line N column M` `serde_json` appends to a custom message.
///
/// A byte offset into a document the client no longer has is noise; the JSON
/// Pointer is the part that lets them fix the request.
fn strip_position(message: &str) -> &str {
    match message.rfind(" at line ") {
        Some(index) if message[index..].contains(" column ") => &message[..index],
        _ => message,
    }
}

/// Check the `Content-Type` of an incoming JSON body.
///
/// Accepts `application/json` and any `+json` suffix (`application/merge-patch+json`,
/// `application/vnd.api+json`), with or without parameters. A missing
/// `Content-Type` on a request that has a body is accepted, because too many
/// clients omit it and rejecting them buys nothing.
pub fn is_json_content_type(headers: &http::HeaderMap) -> bool {
    let Some(value) = headers.get(http::header::CONTENT_TYPE) else {
        return true;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let essence = value.split(';').next().unwrap_or("").trim();
    if essence.is_empty() {
        return true;
    }
    let Some((kind, subtype)) = essence.split_once('/') else {
        return false;
    };
    if !kind.eq_ignore_ascii_case("application") {
        return false;
    }
    subtype.eq_ignore_ascii_case("json")
        || subtype
            .rsplit_once('+')
            .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("json"))
}

/// The `Content-Type` as a string, for the 415's detail.
fn content_type_of(headers: &http::HeaderMap) -> String {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("(none)")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn headers(content_type: &str) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_str(content_type).unwrap(),
        );
        headers
    }

    #[test]
    fn json_is_transparent_in_both_directions() {
        let value: Json<u32> = 7u32.into();
        assert_eq!(*value, 7);
        assert_eq!(value.into_inner(), 7);
    }

    #[test]
    fn json_content_types_are_recognised() {
        assert!(is_json_content_type(&http::HeaderMap::new()));
        assert!(is_json_content_type(&headers("application/json")));
        assert!(is_json_content_type(&headers(
            "application/json; charset=utf-8"
        )));
        assert!(is_json_content_type(&headers("APPLICATION/JSON")));
        assert!(is_json_content_type(&headers(
            "application/merge-patch+json"
        )));
        assert!(is_json_content_type(&headers("application/vnd.api+json")));
    }

    #[test]
    fn other_content_types_are_rejected() {
        assert!(!is_json_content_type(&headers("text/plain")));
        assert!(!is_json_content_type(&headers(
            "application/x-www-form-urlencoded"
        )));
        assert!(!is_json_content_type(&headers("multipart/form-data")));
        assert!(!is_json_content_type(&headers("application/jsonish")));
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Item {
        quantity: u32,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Order {
        items: Vec<Item>,
    }

    #[test]
    fn a_wrongly_typed_member_is_addressed_by_its_pointer() {
        let json = br#"{"items":[{"quantity":1},{"quantity":2},{"quantity":"lots"}]}"#;
        let mut deserializer = serde_json::Deserializer::from_slice(json);
        let error = serde_path_to_error::deserialize::<_, Order>(&mut deserializer)
            .expect_err("the document does not deserialise");
        assert_eq!(pointer_for_path("", error.path()), "/items/2/quantity");
    }

    #[test]
    fn a_well_formed_document_deserialises() {
        let order: Order = from_slice(br#"{"items":[{"quantity":3}]}"#).unwrap();
        assert_eq!(order.items, vec![Item { quantity: 3 }]);
    }

    #[test]
    fn a_trailing_position_is_stripped_from_a_constraint_message() {
        assert_eq!(
            strip_position("must be a valid email address at line 1 column 24"),
            "must be a valid email address"
        );
        assert_eq!(strip_position("no position here"), "no position here");
        assert_eq!(
            strip_position("mentions at line but no column"),
            "mentions at line but no column"
        );
    }

    #[test]
    fn a_constraint_message_is_recognised_through_serde() {
        let message =
            moso_schema::ConstraintError::format("email", "must be a valid email address")
                .to_serde_message();
        assert_eq!(
            parse_serde_message(&message),
            Some(("format", "must be a valid email address"))
        );
    }

    // ── the nesting bound ─────────────────────────────────────────────────

    #[test]
    fn a_document_within_the_depth_limit_is_accepted() {
        assert!(check_json_depth(b"null", 1).is_ok());
        assert!(check_json_depth(br#"{"a":1}"#, 1).is_ok());
        assert!(check_json_depth(br#"{"a":[1,2,3]}"#, 2).is_ok());
        // Siblings are not depth: the counter comes back down on each closer.
        assert!(check_json_depth(br#"[[1],[2],[3],[4]]"#, 2).is_ok());
    }

    #[test]
    fn a_document_past_the_depth_limit_is_a_400_naming_the_limit() {
        let error =
            check_json_depth(br#"{"a":{"b":{"c":1}}}"#, 2).expect_err("three levels against two");

        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.extensions()["max_depth"], serde_json::json!(2));
        assert!(
            error
                .detail()
                .is_some_and(|detail| detail.contains("2-level limit"))
        );
    }

    #[test]
    fn a_bracket_inside_a_string_is_text_rather_than_structure() {
        assert!(check_json_depth(br#"{"a":"[[[[[[[[[["}"#, 1).is_ok());
        assert!(check_json_depth(br#"{"a":"{{{{"}"#, 1).is_ok());
        // An escaped quote does not end the string, so the `[` stays text.
        assert!(check_json_depth(br#"{"a":"say \"[[[[\" now"}"#, 1).is_ok());
        // An escaped backslash *does* end it, so the next `[` is structure.
        assert!(check_json_depth(br#"{"a":"c:\\","b":[1]}"#, 1).is_err());
    }

    #[test]
    fn the_bomb_this_exists_to_stop_is_rejected_before_serde_sees_it() {
        // Two thousand open brackets: well inside `http.body_max`, and a
        // recursion `serde_json` would have to walk.
        let bomb = "[".repeat(2000).into_bytes();
        let error = check_json_depth(&bomb, 64).expect_err("2000 levels against 64");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_zero_limit_is_read_as_one_rather_than_rejecting_everything() {
        assert!(check_json_depth(br#"{}"#, 0).is_ok());
        assert!(check_json_depth(br#"{"a":{}}"#, 0).is_err());
    }

    #[test]
    fn an_unbalanced_closer_is_left_for_serde_to_report() {
        // Reporting it here would produce a second, worse error for the same
        // document: `serde_json` can say *where* it went wrong, and this cannot.
        assert!(check_json_depth(br#"}}}}{"a":1}"#, 1).is_ok());
        assert!(from_slice::<Item>(br#"}}}}{"quantity":1}"#).is_err());
    }
}
