//! `Raw<T>` — the escape hatch that documents itself honestly.

use moso_openapi::{ContentType, OperationBuilder, ResponseSpec};
use moso_schema::json_schema::SchemaNode;

use crate::Response;
use crate::response::{Describe, IntoResponse};

/// Any Axum response, returned from a Moso handler.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::Raw;
/// use moso::Response;
/// # fn upstream() -> Response { moso::response::NoContent.into_response() }
/// /// Forward an upstream response verbatim.
/// #[endpoint]
/// async fn proxy() -> Result<Raw<Response>> {
///     Ok(Raw(upstream()))
/// }
/// # fn main() { assert_eq!(Raw(upstream()).into_response().status(), 204); }
/// ```
///
/// Documents itself as an **unknown** body — `{}` in the schema — rather than
/// guessing. That is the whole point: `Raw` exists so that "I need to return
/// something the framework has never heard of" has an answer that does not
/// require lying to the document. Invariant I3 of the architecture holds
/// because the one way out is labelled.
///
/// If you find yourself reaching for `Raw` often, the response type is
/// under-modelled; `#[derive(Responder)]` covers most of what people use it for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Raw<T>(pub T);

impl<T> Raw<T> {
    /// The wrapped response.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: IntoResponse> IntoResponse for Raw<T> {
    fn into_response(self) -> Response {
        self.0.into_response()
    }
}

/// The media type `Raw` documents.
///
/// `*/*` rather than `application/json`: the point of `Raw` is that the
/// framework does not know what comes back, and naming a concrete media type
/// would be a guess dressed up as a fact.
const UNSPECIFIED_MEDIA_TYPE: &str = "*/*";

/// The response extension marking an operation whose body is deliberately
/// unmodelled, so `moso openapi lint` can count them without guessing.
pub const RAW_RESPONSE_EXTENSION: &str = "x-moso-raw-response";

impl<T> Describe for Raw<T> {
    fn describe(op: &mut OperationBuilder) {
        op.response(
            200,
            ResponseSpec::with_content(
                ContentType::custom(UNSPECIFIED_MEDIA_TYPE),
                SchemaNode::any(),
            )
            .description(
                "An unspecified body. This operation returns `Raw`, which documents nothing \
                 about the payload rather than guessing at it.",
            )
            .extension(RAW_RESPONSE_EXTENSION, true),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::tests::described;

    #[test]
    fn raw_passes_the_inner_response_through_untouched() {
        let inner = (http::StatusCode::IM_A_TEAPOT, "brew").into_response();
        let response = Raw(inner).into_response();
        assert_eq!(response.status(), http::StatusCode::IM_A_TEAPOT);
    }

    #[test]
    fn raw_documents_an_unspecified_body_rather_than_a_flattering_one() {
        let op = described::<Raw<Response>>();
        let response = op.response(200).expect("200 documented");
        let media = response
            .content
            .get(UNSPECIFIED_MEDIA_TYPE)
            .expect("documented under */*");
        assert!(
            media.schema.as_ref().is_some_and(SchemaNode::is_any),
            "the schema must assert nothing"
        );
        assert!(!response.content.contains_key("application/json"));
        assert_eq!(
            response.extensions.get(RAW_RESPONSE_EXTENSION),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn into_inner_gives_the_wrapped_value_back() {
        assert_eq!(Raw(7u32).into_inner(), 7);
    }
}
