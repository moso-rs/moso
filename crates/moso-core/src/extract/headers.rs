//! `Headers<T>` — typed header parameters.
//!
//! ```
//! use moso::prelude::*;
//! use moso::extract::Headers;
//! # /// A post, as the API returns one.
//! # #[derive(Schema)] pub struct PostOut { /// URL-safe identifier.
//! #     pub slug: Slug }
//! /// The headers this endpoint reads.
//! #[derive(Schema)]
//! pub struct ApiHeaders {
//!     /// Which version of the contract the client expects.
//!     #[schema(rename = "x-api-version")]
//!     pub api_version: String,
//!     /// The client's cached entity tag.
//!     #[schema(rename = "if-none-match")]
//!     pub if_none_match: Option<String>,
//! }
//!
//! /// Show a post.
//! #[endpoint]
//! async fn show(Headers(h): Headers<ApiHeaders>) -> Result<Json<PostOut>> {
//!     let _ = h.if_none_match;
//!     Ok(Json(PostOut { slug: Slug::from_title("hello").unwrap() }))
//! }
//! # fn main() { assert_eq!(Router::new().get("/posts/x", moso::ep!(show)).len(), 1); }
//! ```
//!
//! Field names map to header names by lowercasing and replacing `_` with `-`;
//! `#[schema(rename = "…")]` overrides that where the header does not fit a
//! Rust identifier. Lookup is case-insensitive, as HTTP requires.
//!
//! # How the two spellings are reconciled
//!
//! Every header the request carries is offered to the deserialiser under
//! **both** spellings — `x-api-version` and `x_api_version` — so a field works
//! whether or not it was renamed, and no schema has to be generated per
//! request to find out. Only one of the two can match a given field, so the
//! duplication is invisible.
//!
//! A consequence worth stating: `Headers<T>` always ignores headers `T` does
//! not declare, even for a type marked `#[schema(deny_unknown)]`. Every request
//! carries `host`, `user-agent` and a dozen more that no struct will ever
//! declare, so rejecting unknown headers would reject every request.
//!
//! # What this deliberately does not do
//!
//! It does not read `Authorization`, `Cookie` or any header in the redaction
//! list into the OpenAPI document as a plain parameter. Those are security
//! schemes, documented by the authentication [`Dependency`] that consumes them,
//! and describing them twice — once as a header parameter and once as a scheme —
//! produces a document that generates broken clients.
//!
//! [`Dependency`]: crate::Dependency

use moso_openapi::{OperationBuilder, Param};
use moso_schema::Schema;

use crate::ctx::RequestCtx;
use crate::error::{Error, Result};
use crate::extract::Extract;
use crate::extract::query::{DeOptions, QueryMap, QueryValue, properties_of};

/// Request headers, deserialised into `T` and validated.
///
/// Each field of `T` becomes a documented header parameter. `#[schema(rename = ...)]`
/// names the header when it differs from the field, and an `Option` field is an
/// optional header.
///
/// ```
/// use moso::prelude::*;
/// use moso::extract::Headers;
/// use moso::response::NoContent;
///
/// /// The headers this endpoint reads.
/// #[derive(Schema)]
/// pub struct ApiHeaders {
///     /// Which version of the contract the client expects.
///     #[schema(rename = "x-api-version")]
///     pub api_version: String,
///     /// The client's cached entity tag.
///     #[schema(rename = "if-none-match")]
///     pub if_none_match: Option<String>,
/// }
///
/// /// Show a post.
/// #[endpoint]
/// async fn show(Headers(h): Headers<ApiHeaders>) -> Result<NoContent> {
///     let _ = (h.api_version, h.if_none_match);
///     Ok(NoContent)
/// }
/// # fn main() { assert_eq!(Router::new().get("/posts/x", moso::ep!(show)).len(), 1); }
/// ```
///
/// Sensitive headers — `authorization`, `cookie`, `proxy-authorization` — are
/// redacted in errors and logs rather than echoed back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Headers<T>(pub T);

impl<T> Headers<T> {
    /// The deserialised headers.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> core::ops::Deref for Headers<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// The JSON Pointer root a header failure is reported under.
pub const HEADER_POINTER_ROOT: &str = "/header";

impl<T: Schema> Extract for Headers<T> {
    fn describe(op: &mut OperationBuilder) {
        let node = T::json_schema(op.generator());
        for property in properties_of(&node) {
            let name = header_name_for_field(property.name);
            if is_redacted(&name) {
                continue;
            }
            let required = property.required && property.schema.default.is_none();
            let mut param = Param::header(name)
                .required(required)
                .schema_node(property.schema.clone());
            if let Some(description) = &property.schema.description {
                param = param.description(description.to_string());
            }
            if property.schema.deprecated {
                param = param.deprecated(true);
            }
            op.parameter(param);
        }
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        let map = QueryMap::from_entries(header_entries(&parts.headers));
        let value: T = map.deserialize(
            DeOptions::HEADER,
            HEADER_POINTER_ROOT,
            header_name_for_field,
        )?;
        let mut validation = ctx.validation(HEADER_POINTER_ROOT);
        value.validate(&mut validation).map_err(Error::validation)?;
        Ok(Headers(value))
    }
}

/// Offer every header under both the wire spelling and the Rust-identifier one.
///
/// A repeated header becomes a [`QueryValue::List`], which is what `Vec<String>`
/// wants and what `Accept-Encoding: gzip` followed by `Accept-Encoding: br`
/// actually means.
///
/// A header whose value is not UTF-8 is skipped rather than rejected: a
/// non-declared binary header must not fail a request that never looked at it.
/// A *declared* field then sees it as missing, which is the honest report.
fn header_entries(headers: &http::HeaderMap) -> Vec<(String, QueryValue)> {
    let mut entries: Vec<(String, QueryValue)> = Vec::with_capacity(headers.len() * 2);
    for name in headers.keys() {
        let wire = name.as_str();
        let mut values = headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .peekable();
        if values.peek().is_none() {
            continue;
        }
        let collected: Vec<String> = values.collect();
        let value = match collected.len() {
            1 => QueryValue::Scalar(collected.into_iter().next().unwrap_or_default()),
            _ => QueryValue::List(collected),
        };
        let identifier = field_name_for_header(wire);
        entries.push((wire.to_owned(), value.clone()));
        if identifier != wire {
            entries.push((identifier, value));
        }
    }
    entries
}

/// The header name a field name maps to: lowercased, `_` becomes `-`.
pub fn header_name_for_field(field: &str) -> String {
    field.replace('_', "-").to_ascii_lowercase()
}

/// The Rust-identifier spelling of a header name: lowercased, `-` becomes `_`.
pub fn field_name_for_header(header: &str) -> String {
    header.replace('-', "_").to_ascii_lowercase()
}

/// Headers that are never rendered into a log line, an error, or the OpenAPI
/// document as a plain parameter.
///
/// Used by the `sensitive_headers` middleware, by the error logger and by
/// [`Headers::describe`], from this one list, so the three cannot drift apart.
pub const REDACTED_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
    "x-csrf-token",
];

/// Whether `name` is redacted. Case-insensitive.
pub fn is_redacted(name: &str) -> bool {
    REDACTED_HEADERS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(name))
}

/// Read `HeaderMap` itself, for the rare handler that genuinely wants all of it.
///
/// Contributes nothing to the document — "this operation reads some headers" is
/// not a contract a client can act on.
impl Extract for http::HeaderMap {
    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        let _ = ctx;
        Ok(parts.headers.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn headers_of(pairs: &[(&str, &str)]) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    fn decode<T: serde::de::DeserializeOwned>(pairs: &[(&str, &str)]) -> Result<T> {
        let map = QueryMap::from_entries(header_entries(&headers_of(pairs)));
        map.deserialize(
            DeOptions::HEADER,
            HEADER_POINTER_ROOT,
            header_name_for_field,
        )
    }

    #[test]
    fn redaction_is_case_insensitive() {
        assert!(is_redacted("Authorization"));
        assert!(is_redacted("SET-COOKIE"));
        assert!(!is_redacted("x-request-id"));
    }

    #[test]
    fn field_names_map_to_header_names_and_back() {
        assert_eq!(header_name_for_field("api_version"), "api-version");
        assert_eq!(header_name_for_field("X_Api_Key"), "x-api-key");
        assert_eq!(header_name_for_field("accept"), "accept");
        assert_eq!(field_name_for_header("x-api-version"), "x_api_version");
        assert_eq!(field_name_for_header("Accept"), "accept");
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct ApiHeaders {
        #[serde(rename = "x-api-version")]
        api_version: String,
        #[serde(rename = "if-none-match")]
        if_none_match: Option<String>,
    }

    #[test]
    fn renamed_fields_read_the_wire_spelling() {
        assert_eq!(
            decode::<ApiHeaders>(&[("x-api-version", "2024-01-01"), ("host", "example.com")])
                .unwrap(),
            ApiHeaders {
                api_version: "2024-01-01".into(),
                if_none_match: None,
            }
        );
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct PlainHeaders {
        user_agent: String,
        x_request_id: Option<String>,
    }

    #[test]
    fn unrenamed_fields_read_the_identifier_spelling() {
        assert_eq!(
            decode::<PlainHeaders>(&[("user-agent", "curl/8"), ("host", "example.com")]).unwrap(),
            PlainHeaders {
                user_agent: "curl/8".into(),
                x_request_id: None,
            }
        );
    }

    #[test]
    fn a_missing_required_header_is_reported_at_its_wire_name() {
        let error = decode::<ApiHeaders>(&[("host", "example.com")]);
        assert!(error.is_err(), "a missing required header must fail");
    }

    #[test]
    fn a_repeated_header_collects_into_a_vec() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Accepted {
            accept_encoding: Vec<String>,
        }
        assert_eq!(
            decode::<Accepted>(&[("accept-encoding", "gzip"), ("accept-encoding", "br")]).unwrap(),
            Accepted {
                accept_encoding: vec!["gzip".into(), "br".into()]
            }
        );
    }

    #[test]
    fn a_non_utf8_header_is_skipped_rather_than_fatal() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::HeaderName::from_static("x-binary"),
            http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        headers.insert(
            http::HeaderName::from_static("user-agent"),
            http::HeaderValue::from_static("curl/8"),
        );
        let entries = header_entries(&headers);
        assert!(entries.iter().all(|(name, _)| name != "x-binary"));
        assert!(entries.iter().any(|(name, _)| name == "user-agent"));
    }
}
