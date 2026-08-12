//! `Created<T>` and `Accepted<T>` — the two "it worked, here is where it went"
//! responses.

use http::{HeaderValue, StatusCode};
use moso_openapi::{OperationBuilder, ResponseSpec};

use crate::Response;
use crate::response::{Describe, IntoResponse, location_header_spec, restage};

/// A 201 carrying the created representation and a `Location` header.
///
/// ```
/// use moso::prelude::*;
/// # /// A post, as the API accepts one.
/// # #[derive(Schema)] pub struct CreatePost { /// Headline.
/// #     pub title: String }
/// # /// A post, as the API returns one.
/// # #[derive(Schema)] pub struct PostOut { /// URL-safe identifier.
/// #     pub slug: Slug }
/// /// Create a post.
/// #[endpoint]
/// async fn create(Json(body): Json<CreatePost>) -> Result<Created<PostOut>> {
///     let slug = Slug::from_title(&body.title).ok_or_else(|| Error::bad_request("empty title"))?;
///     Ok(Created::at(format!("/api/v1/posts/{slug}"), PostOut { slug }))
/// }
/// # fn main() {
/// let slug = Slug::from_title("Hello").unwrap();
/// let response = Created::at("/api/v1/posts/hello", PostOut { slug }).into_response();
/// assert_eq!(response.status(), 201);
/// assert_eq!(response.headers()["location"], "/api/v1/posts/hello");
/// # }
/// ```
///
/// `Location` is not optional in the constructor, and that is on purpose: a 201
/// without one is a documented interoperability problem, and making the caller
/// pass `""` to opt out is a better trade than making it easy to forget.
/// [`Created::without_location`] exists for the genuinely location-less case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created<T> {
    /// The created representation, serialised as the body.
    pub body: T,
    /// The value of the `Location` header, if any.
    pub location: Option<String>,
}

impl<T> Created<T> {
    /// A 201 at `location`.
    pub fn at(location: impl Into<String>, body: T) -> Self {
        Self {
            body,
            location: Some(location.into()),
        }
    }

    /// A 201 with no `Location`, for a resource that has no addressable URL.
    pub fn without_location(body: T) -> Self {
        Self {
            body,
            location: None,
        }
    }

    /// The created representation.
    pub fn into_inner(self) -> T {
        self.body
    }
}

impl<T: IntoResponse> IntoResponse for Created<T> {
    fn into_response(self) -> Response {
        let mut response = self.body.into_response();
        // An inner response that already failed keeps its status: overwriting a
        // 500 from a serialisation failure with a 201 would report success for
        // a body that was never written.
        if !response.status().is_client_error() && !response.status().is_server_error() {
            *response.status_mut() = StatusCode::CREATED;
        }
        if let Some(location) = self.location.as_deref().and_then(location_header) {
            response
                .headers_mut()
                .insert(http::header::LOCATION, location);
        }
        response
    }
}

impl<T: Describe> Describe for Created<T> {
    fn describe(op: &mut OperationBuilder) {
        // `T` documents itself at 200, because that is what `T` alone means.
        // Restage it, but only when it was this call that put it there — an
        // extractor may legitimately have documented a 200 already.
        let had_200 = op.spec().has_response("200");
        <T as Describe>::describe(op);
        if !had_200 {
            restage(op, 200, 201);
        }
        op.response(
            201,
            ResponseSpec::empty("Created").header_spec(
                "location",
                location_header_spec("Where the created resource can be fetched.", false),
            ),
        );
    }
}

/// A 202 carrying a representation of work that has been queued.
///
/// Use it when the response describes an *accepted intent* rather than a
/// finished state — a job id, a status URL — so a client knows not to treat the
/// body as the resource.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::Accepted;
///
/// /// How to follow a job that has not finished yet.
/// #[derive(Schema)]
/// pub struct JobHandle {
///     /// Poll this for progress.
///     pub id: String,
/// }
///
/// /// Start an export.
/// #[endpoint]
/// async fn export() -> Result<Accepted<JobHandle>> {
///     Ok(Accepted::at("/jobs/7", JobHandle { id: "7".to_owned() }))
/// }
/// # fn main() {
/// let response = Accepted::at("/jobs/7", JobHandle { id: "7".to_owned() }).into_response();
/// assert_eq!(response.status(), 202);
/// assert_eq!(response.headers()["location"], "/jobs/7");
/// # }
/// ```
///
/// `202` says "taken, not done". Use it when the work continues after the response;
/// use [`Created`] when the resource exists by the time the client reads the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted<T> {
    /// The representation of the accepted work.
    pub body: T,
    /// A URL a client can poll for progress.
    pub location: Option<String>,
}

impl<T> Accepted<T> {
    /// A 202 carrying `body`.
    pub fn new(body: T) -> Self {
        Self {
            body,
            location: None,
        }
    }

    /// A 202 whose `Location` points at a status resource.
    pub fn at(location: impl Into<String>, body: T) -> Self {
        Self {
            body,
            location: Some(location.into()),
        }
    }

    /// The representation.
    pub fn into_inner(self) -> T {
        self.body
    }
}

impl<T: IntoResponse> IntoResponse for Accepted<T> {
    fn into_response(self) -> Response {
        let mut response = self.body.into_response();
        if !response.status().is_client_error() && !response.status().is_server_error() {
            *response.status_mut() = StatusCode::ACCEPTED;
        }
        if let Some(location) = self.location.as_deref().and_then(location_header) {
            response
                .headers_mut()
                .insert(http::header::LOCATION, location);
        }
        response
    }
}

impl<T: Describe> Describe for Accepted<T> {
    fn describe(op: &mut OperationBuilder) {
        let had_200 = op.spec().has_response("200");
        <T as Describe>::describe(op);
        if !had_200 {
            restage(op, 200, 202);
        }
        op.response(
            202,
            ResponseSpec::empty("Accepted for processing").header_spec(
                "location",
                location_header_spec(
                    "Where the progress of the accepted work can be polled.",
                    false,
                ),
            ),
        );
    }
}

/// Build a `Location` header value, dropping it if the URL is not header-safe.
///
/// A URL containing a newline is a response-splitting attempt; dropping the
/// header is the correct response and is preferable to failing the request,
/// which would turn a data problem into an outage.
pub fn location_header(url: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(url).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::Json;
    use crate::response::tests::described;

    #[test]
    fn created_carries_its_location() {
        let created = Created::at("/users/1", 42u32);
        assert_eq!(created.location.as_deref(), Some("/users/1"));
        assert_eq!(created.into_inner(), 42);
    }

    #[test]
    fn accepted_defaults_to_no_location() {
        assert!(Accepted::new(()).location.is_none());
    }

    #[test]
    fn created_is_a_201_with_a_location() {
        let response = Created::at("/users/1", Json(7u32)).into_response();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response
                .headers()
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/users/1")
        );
    }

    #[test]
    fn a_created_without_a_location_sends_none() {
        let response = Created::without_location(Json(7u32)).into_response();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(response.headers().get(http::header::LOCATION).is_none());
    }

    #[test]
    fn a_response_splitting_location_is_dropped_rather_than_sent() {
        assert!(location_header("/ok").is_some());
        assert!(location_header("/evil\r\nSet-Cookie: a=b").is_none());

        let response = Created::at("/evil\r\nSet-Cookie: a=b", Json(1u32)).into_response();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(response.headers().get(http::header::LOCATION).is_none());
    }

    #[test]
    fn accepted_is_a_202() {
        let response = Accepted::at("/jobs/1", Json(7u32)).into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response
                .headers()
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/jobs/1")
        );
    }

    #[test]
    fn a_failed_body_keeps_its_own_status() {
        let response = Created::at("/x", crate::Error::not_found("post")).into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn created_documents_the_body_at_201_and_nothing_at_200() {
        let op = described::<Created<Json<u32>>>();
        assert!(op.response(200).is_none(), "the body moved off 200");
        let created = op.response(201).expect("201 documented");
        assert!(created.content.contains_key("application/json"));
        assert!(created.headers.contains_key("location"));
    }

    #[test]
    fn accepted_documents_the_body_at_202() {
        let op = described::<Accepted<Json<u32>>>();
        assert!(op.response(200).is_none());
        assert!(
            op.response(202)
                .is_some_and(|r| r.content.contains_key("application/json"))
        );
    }

    #[test]
    fn a_body_less_created_still_documents_a_201() {
        let op = described::<Created<()>>();
        assert!(op.response(201).is_some());
    }
}
