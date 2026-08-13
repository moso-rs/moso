//! `Redirect` — the four redirects that are actually distinct.

use http::StatusCode;
use moso_openapi::{OperationBuilder, ResponseSpec};

use crate::Response;
use crate::error::Error;
use crate::response::created::location_header;
use crate::response::{Describe, IntoResponse, empty_response};

/// A redirect response.
///
/// The constructors are named after the *semantics* rather than the number,
/// because the numbers are famously confusing and picking the wrong one changes
/// whether a `POST` is replayed:
///
/// | Constructor | Status | Method preserved | Cacheable |
/// | --- | --- | --- | --- |
/// | [`Redirect::to`] | 303 See Other | no — becomes `GET` | no |
/// | [`Redirect::temporary`] | 307 | yes | no |
/// | [`Redirect::permanent`] | 308 | yes | yes |
/// | [`Redirect::found`] | 302 | in practice, no | no |
///
/// [`Redirect::to`] is the default for the post-then-redirect pattern, which is
/// the case most redirects are.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::Redirect;
///
/// /// Send a browser somewhere else after a form post.
/// #[endpoint]
/// async fn login() -> Result<Redirect> {
///     Ok(Redirect::to("/dashboard"))
/// }
/// # fn main() {
/// // `to` is 303 See Other: the correct answer to a POST a browser should not repeat.
/// assert_eq!(Redirect::to("/dashboard").into_response().status(), 303);
/// assert_eq!(Redirect::permanent("/new").into_response().status(), 308);
/// assert_eq!(Redirect::temporary("/busy").into_response().status(), 307);
/// # }
/// ```
///
/// The location is checked: a value with a control character or a newline in it is a
/// 500 rather than a response-splitting vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    status: StatusCode,
    location: String,
}

impl Redirect {
    /// `303 See Other` — the target should be fetched with `GET`.
    pub fn to(location: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SEE_OTHER,
            location: location.into(),
        }
    }

    /// `307 Temporary Redirect` — retry the same method at the new location.
    pub fn temporary(location: impl Into<String>) -> Self {
        Self {
            status: StatusCode::TEMPORARY_REDIRECT,
            location: location.into(),
        }
    }

    /// `308 Permanent Redirect` — the resource has moved for good.
    pub fn permanent(location: impl Into<String>) -> Self {
        Self {
            status: StatusCode::PERMANENT_REDIRECT,
            location: location.into(),
        }
    }

    /// `302 Found` — kept because the web is full of it, not because it is
    /// well-defined.
    pub fn found(location: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FOUND,
            location: location.into(),
        }
    }

    /// The status this redirect will send.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The target URL.
    pub fn location(&self) -> &str {
        &self.location
    }
}

/// Every status a [`Redirect`] can carry, in the order the document lists them.
const REDIRECT_STATUSES: [(u16, &str); 4] = [
    (
        302,
        "Found. Historic; most clients turn the follow-up request into a `GET`.",
    ),
    (303, "See Other. Fetch the target with `GET`."),
    (
        307,
        "Temporary Redirect. Retry the same method at the target.",
    ),
    (308, "Permanent Redirect. The resource has moved for good."),
];

impl IntoResponse for Redirect {
    fn into_response(self) -> Response {
        // A redirect whose URL cannot be a header value is a 500, not a
        // header-less 3xx. `Created` drops the header instead, because a 201
        // still carries the representation and degrades to something useful; a
        // 3xx with no `Location` carries nothing and no client can act on it.
        // Either way the newline never reaches the wire.
        let Some(location) = location_header(&self.location) else {
            return Error::internal_msg("the redirect target is not a valid header value")
                .into_response();
        };
        let mut response = empty_response(self.status);
        response
            .headers_mut()
            .insert(http::header::LOCATION, location);
        response
    }
}

impl Describe for Redirect {
    fn describe(op: &mut OperationBuilder) {
        // All four, because the *type* spans all four and `describe` sees the
        // type rather than the value. A handler that only ever returns one of
        // them can narrow the document with `#[endpoint(responses(...))]`.
        for (status, description) in REDIRECT_STATUSES {
            op.response(status, ResponseSpec::redirect(description));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::tests::described;

    #[test]
    fn constructors_pick_the_documented_statuses() {
        assert_eq!(Redirect::to("/a").status(), StatusCode::SEE_OTHER);
        assert_eq!(
            Redirect::temporary("/a").status(),
            StatusCode::TEMPORARY_REDIRECT
        );
        assert_eq!(
            Redirect::permanent("/a").status(),
            StatusCode::PERMANENT_REDIRECT
        );
        assert_eq!(Redirect::found("/a").status(), StatusCode::FOUND);
        assert_eq!(Redirect::to("/a").location(), "/a");
    }

    #[test]
    fn a_redirect_is_its_status_plus_a_location() {
        let response = Redirect::permanent("/posts/new-slug").into_response();
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response
                .headers()
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/posts/new-slug")
        );
        assert!(response.headers().get(http::header::CONTENT_TYPE).is_none());
    }

    #[test]
    fn a_response_splitting_target_becomes_a_500_rather_than_a_broken_3xx() {
        let response = Redirect::to("/evil\r\nSet-Cookie: admin=1").into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response.headers().get(http::header::LOCATION).is_none());
    }

    #[test]
    fn redirects_document_every_status_they_span_with_a_location_header() {
        let op = described::<Redirect>();
        for (status, _) in REDIRECT_STATUSES {
            let response = op
                .response(status)
                .unwrap_or_else(|| panic!("{status} documented"));
            assert!(
                response.headers.contains_key("location"),
                "{status} must document `Location`"
            );
            assert!(response.content.is_empty(), "{status} carries no body");
        }
        assert!(op.response(200).is_none());
    }
}
