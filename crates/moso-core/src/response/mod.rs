//! Responses that describe themselves.
//!
//! [`IntoResponse`] is Axum's trait, re-exported rather than reinvented, so the
//! whole ecosystem's response types work here unchanged. What Moso adds is
//! [`Describe`]: a response type says what status it produces, what schema its
//! body has, and which headers it sets, so the OpenAPI document is generated
//! from the handler's return type instead of from an annotation beside it.
//!
//! # The built-in set
//!
//! | Type | Status | Documents as |
//! | --- | --- | --- |
//! | [`Json<T>`] | 200 | `T` |
//! | [`Created<T>`] | 201 | `T`, plus a `Location` header |
//! | [`Accepted<T>`] | 202 | `T` |
//! | [`NoContent`] / [`Empty`] | 204 | no body |
//! | [`Page<T>`] | 200 | `{items, next_cursor, prev_cursor, total?}` |
//! | [`Redirect`] | 302/303/307/308 | `Location` |
//! | [`File`] | 200/206 | binary, with `Range` support |
//! | [`Sse<S>`] | 200 | `text/event-stream` |
//! | [`Cached<T>`] | 200/304 | `T`, plus `ETag` |
//! | [`Either<A, B>`] | either | `oneOf` |
//! | [`Raw<T>`] | as given | `{}` — the honest escape hatch |
//! | [`Html`], [`Text`], [`Bytes`] | 200 | `text/html`, `text/plain`, `application/octet-stream` |
//! | [`Attachment`] | 200 | binary, as a download |
//! | `Result<T, E>` | from both | the union of `T`'s and `E`'s |
//!
//! # Two of these need the request
//!
//! [`IntoResponse`] is handed a value and nothing else, so a response type
//! cannot read a request header on its own. [`Cached::evaluate`] and
//! [`File::evaluate`] take the `HeaderMap` explicitly, and that is the whole of
//! the conditional-request and `Range` machinery. A handler that forgets them
//! still produces a correct 200; it just never produces a 304 or a 206.
//!
//! # Pagination cursors are signed
//!
//! [`Page<T>`]'s cursors are opaque tokens, and [`CursorCodec`] is what makes
//! them tamper-proof rather than merely obscure. A cursor decodes to a sort key
//! that goes into a `WHERE` clause, so an unauthenticated one is a query
//! parameter an attacker can edit.
//!
//! # Returning your own type
//!
//! `#[derive(Schema)]` generates [`IntoResponse`] and [`Describe`] for the type
//! itself, so a handler can return `Result<UserOut>` with no wrapper. There is
//! no blanket `impl<T: Schema> IntoResponse for T` and there cannot be: Rust's
//! orphan rules forbid a blanket impl of a foreign trait over an uncovered type
//! parameter. Generating the impl per type in the derive reaches the same
//! ergonomics with none of the coherence cost, and leaves room for
//! `#[derive(Responder)]` to override the status and headers.
//!
//! Without either derive, the return type gets a hand-written diagnostic
//! pointing at both.

pub mod cached;
pub mod created;
pub mod cursor;
pub mod either;
pub mod file;
pub mod nocontent;
pub mod page;
pub mod raw;
pub mod redirect;
pub mod sse;
pub mod text;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::StatusCode;
use moso_openapi::{Header, OperationBuilder, ResponseSpec};
use moso_schema::Schema;
use moso_schema::json_schema::StringBuilder;

use crate::Response;
use crate::error::Error;

pub use axum::response::IntoResponse;

pub use crate::extract::body::{Bytes, Text};
pub use crate::extract::json::Json;
pub use crate::response::cached::{Cached, ETag, Visibility};
pub use crate::response::created::{Accepted, Created};
pub use crate::response::cursor::CursorCodec;
pub use crate::response::either::Either;
pub use crate::response::file::{Attachment, Disposition, File};
pub use crate::response::nocontent::{Empty, NoContent};
pub use crate::response::page::{Page, PageLinks};
pub use crate::response::raw::Raw;
pub use crate::response::redirect::Redirect;
pub use crate::response::sse::{Event, Sse};
pub use crate::response::text::Html;

/// What a response type contributes to the OpenAPI operation.
///
/// Required, with no default: a default of "contributes nothing" would let a
/// type silently produce an operation documented as returning nothing, which is
/// worse than a compile error.
///
/// Implemented by every response type Moso ships, by `#[derive(Schema)]` and by
/// `#[derive(Responder)]`. Write one by hand when a type answers with a status
/// or content type none of those cover.
///
/// ```
/// use moso::prelude::*;
/// use moso::openapi::{ContentType, OperationBuilder, ResponseSpec};
/// use moso::response::Describe;
/// use moso::schema::SchemaGenerator;
/// use moso::schema::json_schema::{JsonType, SchemaNode};
/// use moso::Response;
///
/// /// A `text/csv` export.
/// pub struct Csv(pub String);
///
/// impl IntoResponse for Csv {
///     fn into_response(self) -> Response {
///         ([("content-type", "text/csv")], self.0).into_response()
///     }
/// }
///
/// impl Describe for Csv {
///     fn describe(op: &mut OperationBuilder) {
///         op.response(
///             200,
///             ResponseSpec::with_content(
///                 ContentType::custom("text/csv"),
///                 SchemaNode::of_type(JsonType::String),
///             )
///                 .description("A CSV export"),
///         );
///     }
/// }
///
/// # fn main() {
/// let mut op = OperationBuilder::new(SchemaGenerator::default());
/// Csv::describe(&mut op);
/// let (spec, _) = op.finish();
/// assert!(spec.responses.contains_key("200"));
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be returned from a handler",
    label = "not a response",
    note = "help: add `#[derive(moso::Schema)]` to return it as a 200 JSON body",
    note = "help: or add `#[derive(moso::Responder)]` to control the status and the headers",
    note = "help: or wrap it: `Json<{Self}>`, `Created<{Self}>`, `Page<{Self}>`, `Raw<{Self}>`",
    note = "a handler usually returns `Result<T>`, which documents `T` and the error taxonomy \
            together",
    note = "an entity type deliberately does not implement `Schema` — define an output DTO with \
            `#[schema(from = {Self})]` so a password hash cannot leak into a response"
)]
pub trait Describe {
    /// Contribute the responses this type can produce.
    fn describe(op: &mut OperationBuilder);
}

/// The two halves of "this can be returned from a handler", as one bound.
///
/// A handler's return type has to do two things: turn into a [`Response`] and
/// describe itself in the document. Asked for separately, a type that does
/// neither fails **two** obligations and the reader gets two errors — one of
/// them rustc's own `IntoResponse` message, which lists `&'static [u8; N]` and
/// `(T1, T2, T3, R)` among the implementers and is exactly the trait-bound vomit
/// `41-diagnostics.md` exists to prevent.
///
/// Asked for as one bound, there is one obligation, one message, and
/// `#[diagnostic::do_not_recommend]` on the blanket impl stops the compiler
/// unfolding it back into the two halves.
///
/// Nobody implements this by hand: the blanket impl covers every type that has
/// both halves, and the way to acquire them is `#[derive(Schema)]` or
/// `#[derive(Responder)]`.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::{HandlerReturn, NoContent};
///
/// /// A user, as the API returns one.
/// #[derive(Schema)]
/// pub struct UserOut {
///     /// Stable identifier.
///     pub id: u64,
/// }
///
/// # fn main() {
/// // The derive supplies both halves, so the combined bound is satisfied.
/// fn returnable<T: HandlerReturn>() {}
/// returnable::<UserOut>();
/// returnable::<Json<UserOut>>();
/// returnable::<Created<UserOut>>();
/// returnable::<Result<NoContent>>();
///
/// // And rendering goes through the same bound `#[endpoint]` uses.
/// let response = UserOut { id: 1 }.into_handler_response();
/// assert_eq!(response.status(), 200);
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be returned from a handler",
    label = "not a response",
    note = "help: add `#[derive(moso::Schema)]` to return it as a 200 JSON body",
    note = "help: or add `#[derive(moso::Responder)]` to control the status and the headers",
    note = "help: or wrap it: `Json<{Self}>`, `Created<{Self}>`, `Page<{Self}>`, `Raw<{Self}>`",
    note = "a handler usually returns `Result<T>`, which documents `T` and the error taxonomy \
            together",
    note = "run `moso check` to see this handler's parameters and response together"
)]
pub trait HandlerReturn: Sized {
    /// Contribute this type's responses, through the combined bound.
    ///
    /// `#[endpoint]` calls this rather than [`Describe::describe`] so that a
    /// return type which satisfies neither half reports one failure instead of
    /// two.
    fn describe_response(op: &mut OperationBuilder);

    /// Render this value, through the combined bound.
    fn into_handler_response(self) -> Response;
}

/// The only implementation, and the reason `IntoResponse + Describe` are *not*
/// supertraits.
///
/// As supertraits they are reported individually — rustc names the unsatisfied
/// supertrait rather than `HandlerReturn`, and the hand-written message above is
/// never reached. As bounds on the blanket impl they are a single obligation
/// `T: HandlerReturn`, which `#[diagnostic::do_not_recommend]` then stops the
/// compiler unfolding.
#[diagnostic::do_not_recommend]
impl<T: IntoResponse + Describe> HandlerReturn for T {
    fn describe_response(op: &mut OperationBuilder) {
        <T as Describe>::describe(op);
    }

    fn into_handler_response(self) -> Response {
        <T as IntoResponse>::into_response(self)
    }
}

/// `Result` documents both arms: the success type's responses and the error
/// type's.
impl<T: Describe, E: Describe> Describe for Result<T, E> {
    fn describe(op: &mut OperationBuilder) {
        <T as Describe>::describe(op);
        <E as Describe>::describe(op);
    }
}

/// The framework error contributes the responses its taxonomy can produce.
///
/// Deliberately conservative: only the statuses every operation can genuinely
/// return — a 500, and a 503 while shutting down. Operation-specific errors
/// come from the extractors that raise them and from `#[endpoint(errors = …)]`,
/// so a document does not claim every endpoint can return a 409.
impl Describe for Error {
    fn describe(op: &mut OperationBuilder) {
        op.response(
            500,
            ResponseSpec::problem(
                "An unhandled failure. The `detail` is suppressed unless \
                 `http.expose_internal_errors` is set; the `request_id` identifies the \
                 occurrence in the server log.",
            ),
        );
        op.response(
            503,
            ResponseSpec::problem(
                "The server is draining for shutdown, or a hard dependency is unavailable. \
                 Retryable.",
            ),
        );
    }
}

/// An optional body documents as its inner type plus a 404.
///
/// `Result<Option<T>>` is the shape of "fetch by id", and the 404 is what an
/// absent value becomes on the wire.
impl<T: Describe> Describe for Option<T> {
    fn describe(op: &mut OperationBuilder) {
        <T as Describe>::describe(op);
        op.response(404, ResponseSpec::problem("No such resource"));
    }
}

/// The unit response: a 200 with no body.
///
/// Prefer [`NoContent`], which is a 204 and says so.
impl Describe for () {
    fn describe(op: &mut OperationBuilder) {
        op.response(200, ResponseSpec::empty("Success, with no body"));
    }
}

/// An explicit status paired with a body, matching Axum's tuple form.
impl<T: Describe> Describe for (StatusCode, T) {
    fn describe(op: &mut OperationBuilder) {
        <T as Describe>::describe(op);
    }
}

// ---------------------------------------------------------------------------
// Helpers the derives generate calls to
// ---------------------------------------------------------------------------

/// Serialise `value` as a JSON response with `status`.
///
/// The single implementation `#[derive(Schema)]` and every JSON-shaped response
/// type routes through, so the content type, the serialisation settings and the
/// failure behaviour are decided once.
///
/// A serialisation failure becomes a 500 problem rather than a panic: a type
/// whose `Serialize` can fail is unusual but legal, and killing the connection
/// over it is not proportionate.
pub fn json_response<T: Schema>(status: StatusCode, value: &T) -> Response {
    // A status that forbids a body gets one anyway if we are not careful: Axum
    // will happily attach bytes to a 204, and a proxy in the middle will then
    // disagree with us about where the next response starts.
    if status_forbids_body(status) {
        return empty_response(status);
    }
    match serde_json::to_vec(value) {
        Ok(body) => {
            let mut response = Response::new(axum::body::Body::from(body));
            *response.status_mut() = status;
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            response
        }
        Err(error) => Error::internal(error)
            .with_detail("the response body could not be serialised")
            .into_response(),
    }
}

/// Contribute `response(status, ResponseSpec::json_of::<T>())`.
///
/// The counterpart to [`json_response`], so the code that *emits* a body and the
/// code that *documents* it are written next to each other and change together.
pub fn describe_json<T: Schema>(op: &mut OperationBuilder, status: u16) {
    op.response(status, ResponseSpec::json_of::<T>());
}

/// An empty response with `status` and no content type.
pub fn empty_response(status: StatusCode) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = status;
    response
}

/// Set a header on a response, replacing any existing value.
///
/// Silently drops a header whose name or value is invalid, because a response
/// builder that can fail turns every handler into a `Result` juggle. Invalid
/// values come from application code and are caught by tests, not by users.
pub fn set_header(response: &mut Response, name: http::HeaderName, value: &str) {
    if let Ok(value) = http::HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}

/// Whether RFC 9110 forbids a body at `status`.
///
/// `204` and `304` carry no body by definition, and neither does a `1xx`. A
/// `HEAD` response also carries none, but that is decided by the router, not by
/// the response type.
pub(crate) fn status_forbids_body(status: StatusCode) -> bool {
    status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
        || status.is_informational()
}

/// Move the response documented at `from` to `to`, merging into whatever is
/// already at `to`.
///
/// This is how [`Created<T>`] documents `T` at 201: `T::describe` puts its body
/// at 200 because that is what `T` means on its own, and the wrapper restages
/// it. Doing it by rewriting the accumulated spec — rather than by giving
/// [`Describe`] a status parameter — keeps the trait to one method and lets any
/// wrapper compose with any body type, including ones the framework has never
/// seen.
///
/// Returns whether anything moved.
pub(crate) fn restage(op: &mut OperationBuilder, from: u16, to: u16) -> bool {
    let from = from.to_string();
    let Some(response) = op.spec_mut().responses.shift_remove(&from) else {
        return false;
    };
    op.spec_mut().merge_response(to.to_string(), response);
    true
}

/// The `Location` response header, as OpenAPI documents it.
pub(crate) fn location_header_spec(description: &str, required: bool) -> Header {
    let header = Header::new(
        StringBuilder::new()
            .format("uri-reference")
            .example("/users/42")
            .build(),
    )
    .with_description(description);
    if required { header.required() } else { header }
}

// ---------------------------------------------------------------------------
// HTTP dates
// ---------------------------------------------------------------------------

/// Days per month name, in `Last-Modified` order.
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Day names indexed from Sunday, which is what the epoch-day arithmetic below
/// produces.
const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Days from the Unix epoch to a proleptic Gregorian date.
///
/// Howard Hinnant's `days_from_civil`, which is exact for the whole range of
/// `i64` and needs no table.
pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`]: `(year, month, day)`.
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Whole seconds between `time` and the Unix epoch, negative before it.
pub(crate) fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(delta) => i64::try_from(delta.as_secs()).unwrap_or(i64::MAX),
        Err(error) => i64::try_from(error.duration().as_secs())
            .map(|seconds| -seconds)
            .unwrap_or(i64::MIN),
    }
}

/// Render `time` as an RFC 9110 IMF-fixdate: `Sun, 06 Nov 1994 08:49:37 GMT`.
///
/// Hand-rolled rather than pulled in as a dependency: this is the only date
/// formatting the runtime does, the format is frozen by the specification, and
/// a crate for it would be a third of the size of this module.
pub(crate) fn format_http_date(time: SystemTime) -> String {
    let seconds = unix_seconds(time);
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let weekday = (days + 4).rem_euclid(7) as usize;
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAY_NAMES[weekday],
        day,
        MONTH_NAMES[(month - 1) as usize],
        year,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
    )
}

/// Parse an RFC 9110 IMF-fixdate.
///
/// The two obsolete formats (RFC 850 and `asctime`) are **not** accepted. The
/// only thing Moso does with a parsed date is decide whether to send a 304, and
/// failing to parse means "not modified is unproven", which sends the full
/// response — always correct, merely less efficient. Accepting a two-digit year
/// would buy a 304 for clients that have not existed for twenty years, at the
/// cost of a windowing rule nobody can test.
pub(crate) fn parse_http_date(input: &str) -> Option<SystemTime> {
    let (_weekday, rest) = input.trim().split_once(", ")?;
    let mut fields = rest.split(' ').filter(|field| !field.is_empty());

    let day: i64 = fields.next()?.parse().ok()?;
    let month_name = fields.next()?;
    let month = MONTH_NAMES.iter().position(|m| *m == month_name)? as i64 + 1;
    let year: i64 = fields.next()?.parse().ok()?;

    let mut clock = fields.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if clock.next().is_some() {
        return None;
    }

    if fields.next()? != "GMT" || fields.next().is_some() {
        return None;
    }
    // A leap second is legal on the wire and collapses onto :59 here, because
    // `SystemTime` has no room for it.
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second.min(59);
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moso_schema::json_schema::SchemaGenerator;

    /// A builder over a fresh generator, which is what every `describe` test
    /// wants.
    pub(crate) fn builder() -> OperationBuilder {
        OperationBuilder::new(SchemaGenerator::default())
    }

    /// The `Describe` contribution of `T`, as the wire `Operation`.
    pub(crate) fn described<T: Describe>() -> moso_openapi::Operation {
        let mut op = builder();
        T::describe(&mut op);
        op.into_spec().into_operation()
    }

    #[test]
    fn describe_is_object_safe_free() {
        fn assert_describe<T: Describe>() {}
        assert_describe::<Result<(), Error>>();
    }

    #[test]
    fn the_error_taxonomy_documents_only_what_every_route_can_return() {
        let op = described::<Error>();
        assert!(op.response(500).is_some());
        assert!(op.response(503).is_some());
        assert!(op.response(409).is_none(), "409 is operation-specific");
        assert_eq!(
            op.response(500)
                .and_then(|r| r.content.keys().next())
                .map(String::as_str),
            Some("application/problem+json")
        );
    }

    #[test]
    fn a_result_unions_both_arms() {
        let op = described::<Result<(), Error>>();
        assert!(op.response(200).is_some());
        assert!(op.response(500).is_some());
    }

    #[test]
    fn an_option_adds_a_404() {
        let op = described::<Option<()>>();
        assert!(op.response(200).is_some());
        assert!(op.response(404).is_some());
    }

    #[test]
    fn a_status_tuple_documents_its_body() {
        let op = described::<(StatusCode, ())>();
        assert!(op.response(200).is_some());
    }

    #[test]
    fn json_responses_carry_the_content_type_and_the_status() {
        let response = json_response(StatusCode::CREATED, &7u32);
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn a_body_less_status_never_gets_a_body() {
        let response = json_response(StatusCode::NO_CONTENT, &7u32);
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response.headers().get(http::header::CONTENT_TYPE).is_none());
        assert!(status_forbids_body(StatusCode::NOT_MODIFIED));
        assert!(status_forbids_body(StatusCode::CONTINUE));
        assert!(!status_forbids_body(StatusCode::OK));
    }

    #[test]
    fn set_header_drops_an_unrepresentable_value() {
        let mut response = empty_response(StatusCode::OK);
        set_header(&mut response, http::header::LOCATION, "/ok");
        set_header(&mut response, http::header::SERVER, "bad\nvalue");
        assert_eq!(
            response
                .headers()
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/ok")
        );
        assert!(response.headers().get(http::header::SERVER).is_none());
    }

    #[test]
    fn restage_moves_a_documented_status() {
        let mut op = builder();
        op.response(200, ResponseSpec::empty("body"));
        assert!(restage(&mut op, 200, 201));
        assert!(!restage(&mut op, 200, 201));
        let spec = op.into_spec();
        assert!(!spec.has_response("200"));
        assert_eq!(
            spec.responses
                .get("201")
                .and_then(|r| r.description.as_deref()),
            Some("body")
        );
    }

    #[test]
    fn http_dates_round_trip() {
        // RFC 9110's own example.
        let parsed = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").expect("parses");
        assert_eq!(unix_seconds(parsed), 784_111_777);
        assert_eq!(format_http_date(parsed), "Sun, 06 Nov 1994 08:49:37 GMT");

        assert_eq!(
            format_http_date(UNIX_EPOCH),
            "Thu, 01 Jan 1970 00:00:00 GMT"
        );
        let epoch = parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT").expect("parses");
        assert_eq!(epoch, UNIX_EPOCH);
    }

    #[test]
    fn http_date_parsing_rejects_what_it_cannot_represent() {
        for input in [
            "",
            "Sunday, 06-Nov-94 08:49:37 GMT", // RFC 850, deliberately unsupported
            "Sun Nov  6 08:49:37 1994",       // asctime, deliberately unsupported
            "Sun, 06 Nov 1994 08:49:37 UTC",  // only GMT is legal
            "Sun, 06 Nov 1994 08:49:37",      // no zone
            "Sun, 06 Foo 1994 08:49:37 GMT",  // no such month
            "Sun, 32 Nov 1994 08:49:37 GMT",  // no such day
            "Sun, 06 Nov 1994 24:49:37 GMT",  // no such hour
            "Sun, 06 Nov 1994 08:49:37 GMT x", // trailing junk
        ] {
            assert!(parse_http_date(input).is_none(), "{input:?} must not parse");
        }
    }

    #[test]
    fn civil_dates_round_trip_across_leap_years() {
        for (year, month, day) in [
            (1970, 1, 1),
            (2000, 2, 29),
            (2024, 2, 29),
            (1900, 3, 1),
            (2100, 1, 1),
            (1899, 12, 31),
        ] {
            let days = days_from_civil(year, month, day);
            assert_eq!(civil_from_days(days), (year, month, day));
        }
    }

    #[test]
    fn dates_before_the_epoch_survive_the_round_trip() {
        let formatted = format_http_date(UNIX_EPOCH - Duration::from_secs(86_400));
        assert_eq!(formatted, "Wed, 31 Dec 1969 00:00:00 GMT");
        assert_eq!(
            parse_http_date(&formatted),
            Some(UNIX_EPOCH - Duration::from_secs(86_400))
        );
    }
}
