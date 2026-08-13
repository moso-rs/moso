//! `NoContent` — a 204, and the alias that reads better in a `Result`.

use http::StatusCode;
use moso_openapi::{OperationBuilder, ResponseSpec};

use crate::Response;
use crate::response::{Describe, IntoResponse, empty_response};

/// A `204 No Content` response.
///
/// The right answer for a `DELETE`, and for a `PUT`/`PATCH` whose caller
/// already has the representation.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::NoContent;
///
/// /// Delete a post.
/// #[endpoint]
/// async fn destroy(Path(_slug): Path<Slug>) -> Result<NoContent> {
///     Ok(NoContent)
/// }
/// # fn main() {
/// let response = NoContent.into_response();
/// assert_eq!(response.status(), 204);
/// # }
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoContent;

impl IntoResponse for NoContent {
    fn into_response(self) -> Response {
        empty_response(StatusCode::NO_CONTENT)
    }
}

impl Describe for NoContent {
    fn describe(op: &mut OperationBuilder) {
        op.response(204, ResponseSpec::empty("No content"));
    }
}

/// [`NoContent`] under the name that reads better in a return type.
///
/// `Result<Empty>` says "this either works or it fails" without the reader
/// having to notice that `NoContent` is not a body. Same type, same 204.
pub type Empty = NoContent;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::tests::described;

    #[test]
    fn no_content_sends_a_204_with_nothing_attached() {
        let response = NoContent.into_response();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response.headers().get(http::header::CONTENT_TYPE).is_none());
        assert!(response.headers().is_empty());
    }

    #[test]
    fn no_content_documents_a_204_and_no_body() {
        let op = described::<NoContent>();
        let response = op.response(204).expect("204 documented");
        assert!(response.content.is_empty());
        assert_eq!(response.description.as_deref(), Some("No content"));
        assert!(op.response(200).is_none());
    }

    #[test]
    fn empty_is_the_same_type() {
        let _: Empty = NoContent;
    }
}
