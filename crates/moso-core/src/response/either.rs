//! `Either<A, B>` — one operation, two genuinely different shapes.

use indexmap::IndexMap;
use moso_openapi::{OperationBuilder, Response as ResponseSpecWire};
use moso_schema::json_schema::SchemaNode;

use crate::Response;
use crate::response::{Describe, IntoResponse};

/// One of two response types, documented as a `oneOf`.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::{Either, Redirect};
/// # /// A post, as the API returns one.
/// # #[derive(Schema)] pub struct PostOut { /// URL-safe identifier.
/// #     pub slug: Slug }
/// # /// A post, stored.
/// # pub enum Post { /// Still here.
/// #     Current(PostOut), /// Moved elsewhere.
/// #     Moved(Slug) }
/// # fn find(_: u64) -> Post { Post::Moved(Slug::from_title("new home").unwrap()) }
/// /// Show a post, following a move if there was one.
/// #[endpoint]
/// async fn show(Path(id): Path<u64>) -> Result<Either<Json<PostOut>, Redirect>> {
///     match find(id) {
///         Post::Current(post) => Ok(Either::A(Json(post))),
///         Post::Moved(slug)   => Ok(Either::B(Redirect::permanent(format!("/posts/{slug}")))),
///     }
/// }
/// # fn main() {
/// let moved: Either<Json<PostOut>, Redirect> =
///     Either::B(Redirect::permanent("/posts/new-home"));
/// assert_eq!(moved.into_response().status(), 308);
/// # }
/// ```
///
/// Use it when an operation genuinely has two outcomes with different bodies.
/// Do **not** use it as a poor person's error type: an error belongs in the
/// `Err` arm, where the taxonomy documents it and the boundary logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Either<A, B> {
    /// The first alternative.
    A(A),
    /// The second alternative.
    B(B),
}

impl<A, B> Either<A, B> {
    /// Whether this is the first alternative.
    pub fn is_a(&self) -> bool {
        matches!(self, Either::A(_))
    }

    /// Whether this is the second alternative.
    pub fn is_b(&self) -> bool {
        matches!(self, Either::B(_))
    }
}

impl<A: IntoResponse, B: IntoResponse> IntoResponse for Either<A, B> {
    fn into_response(self) -> Response {
        match self {
            Either::A(a) => a.into_response(),
            Either::B(b) => b.into_response(),
        }
    }
}

impl<A: Describe, B: Describe> Describe for Either<A, B> {
    fn describe(op: &mut OperationBuilder) {
        // Each arm is described against the same baseline, so neither sees the
        // other's contributions, and the two results are folded back in
        // afterwards. Describing them in sequence instead would let the second
        // arm's body be swallowed by `merge_missing`, which fills only absent
        // members — the exact silent loss this type exists to avoid.
        let baseline = op.spec().responses.clone();

        <A as Describe>::describe(op);
        let from_a = core::mem::replace(&mut op.spec_mut().responses, baseline.clone());

        <B as Describe>::describe(op);
        let from_b = core::mem::replace(&mut op.spec_mut().responses, baseline);

        for (status, response) in from_a {
            op.spec_mut().merge_response(status, response);
        }
        for (status, response) in from_b {
            merge_alternative(&mut op.spec_mut().responses, status, response);
        }
    }
}

/// Merge `response` into whatever is already documented at `status`, turning a
/// genuine disagreement about the body into a `oneOf` instead of dropping it.
fn merge_alternative(
    responses: &mut IndexMap<String, ResponseSpecWire>,
    status: String,
    response: ResponseSpecWire,
) {
    let Some(existing) = responses.get_mut(&status) else {
        responses.insert(status, response);
        return;
    };

    for (media_type, media) in &response.content {
        let (Some(new_schema), Some(slot)) = (&media.schema, existing.content.get_mut(media_type))
        else {
            continue;
        };
        let Some(current) = slot.schema.as_mut() else {
            continue;
        };
        if current == new_schema {
            continue;
        }
        *current = union_of(current.clone(), new_schema.clone());
    }

    // Descriptions, headers, links and any media type the first arm did not
    // describe still merge by the ordinary first-writer-wins rule.
    existing.merge_missing(response);
}

/// `a | b`, flattening a `oneOf` that is already one.
fn union_of(a: SchemaNode, b: SchemaNode) -> SchemaNode {
    let is_bare_union = |node: &SchemaNode| {
        !node.one_of.is_empty() && *node == SchemaNode::one_of(node.one_of.clone())
    };

    let mut variants = if is_bare_union(&a) { a.one_of } else { vec![a] };
    if is_bare_union(&b) {
        for variant in b.one_of {
            if !variants.contains(&variant) {
                variants.push(variant);
            }
        }
    } else if !variants.contains(&b) {
        variants.push(b);
    }
    SchemaNode::one_of(variants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::tests::described;
    use crate::response::{Json, NoContent, Redirect};

    #[test]
    fn either_reports_which_arm_it_holds() {
        let value: Either<u8, &str> = Either::A(1);
        assert!(value.is_a());
        assert!(!value.is_b());
        let value: Either<u8, &str> = Either::B("x");
        assert!(value.is_b());
        assert!(!value.is_a());
    }

    #[test]
    fn either_renders_whichever_arm_it_holds() {
        let a: Either<NoContent, Redirect> = Either::A(NoContent);
        assert_eq!(a.into_response().status(), http::StatusCode::NO_CONTENT);

        let b: Either<NoContent, Redirect> = Either::B(Redirect::permanent("/x"));
        assert_eq!(
            b.into_response().status(),
            http::StatusCode::PERMANENT_REDIRECT
        );
    }

    #[test]
    fn arms_on_different_statuses_are_both_documented() {
        let op = described::<Either<Json<u32>, NoContent>>();
        assert!(op.response(200).is_some());
        assert!(op.response(204).is_some());
    }

    #[test]
    fn arms_on_the_same_status_become_a_one_of_rather_than_one_of_them_winning() {
        let op = described::<Either<Json<u32>, Json<String>>>();
        let schema = op
            .response(200)
            .and_then(|r| r.content.get("application/json"))
            .and_then(|m| m.schema.as_ref())
            .expect("a 200 JSON schema");
        assert_eq!(schema.one_of.len(), 2, "both bodies survive: {schema:?}");
    }

    #[test]
    fn an_arm_that_repeats_a_schema_does_not_produce_a_pointless_union() {
        let op = described::<Either<Json<u32>, Json<u32>>>();
        let schema = op
            .response(200)
            .and_then(|r| r.content.get("application/json"))
            .and_then(|m| m.schema.as_ref())
            .expect("a 200 JSON schema");
        assert!(schema.one_of.is_empty(), "identical bodies stay one schema");
    }

    #[test]
    fn nested_eithers_flatten_into_one_union() {
        let op = described::<Either<Either<Json<u32>, Json<String>>, Json<bool>>>();
        let schema = op
            .response(200)
            .and_then(|r| r.content.get("application/json"))
            .and_then(|m| m.schema.as_ref())
            .expect("a 200 JSON schema");
        assert_eq!(schema.one_of.len(), 3, "no nested oneOf: {schema:?}");
    }
}
