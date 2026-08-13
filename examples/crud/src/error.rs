//! The failures this application's domain can produce.
//!
//! `#[derive(moso::Error)]` maps each variant onto an RFC 9457 problem
//! document: a status, a `type` URI, a title and an interpolated `detail`. The
//! generated `From<BlogError> for moso::Error` is what makes `?` work in a
//! handler, and the generated `Describe` is what puts these statuses in the
//! OpenAPI document when a handler declares `#[endpoint(errors = BlogError)]`.

use moso::prelude::*;

/// Everything the blog can refuse to do.
#[derive(Debug, moso::Error)]
#[error(type_base = "https://moso.example/errors/")]
pub enum BlogError {
    /// No post has that identifier.
    #[error(status = 404, detail = "No post with id {id}")]
    PostNotFound {
        /// The identifier that was asked for.
        id: String,
    },

    /// Two posts would end up with the same slug.
    #[error(status = 409, detail = "The slug `{slug}` is already taken")]
    SlugTaken {
        /// The slug that collided.
        slug: String,
    },

    /// A PATCH that asked for nothing.
    #[error(status = 422, detail = "Provide at least one field to change")]
    NothingToUpdate,

    /// The title has no letters or digits, so no slug can be derived from it.
    #[error(
        status = 422,
        detail = "The title must contain at least one letter or digit"
    )]
    UnsluggableTitle,
}

impl BlogError {
    /// The 404 for a post that is absent, or that the caller may not see.
    ///
    /// A draft belonging to somebody else is reported as absent rather than as
    /// forbidden: a 403 would confirm that the identifier exists, which is an
    /// oracle a draft should not hand out.
    #[must_use]
    pub fn post_not_found(id: Id<crate::models::Post>) -> Self {
        Self::PostNotFound { id: id.to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moso::IntoResponse;

    #[test]
    fn a_missing_post_is_a_404_with_the_id_in_the_detail() {
        let error = BlogError::post_not_found(Id::NIL);
        let rendered = error.to_string();
        assert!(
            rendered.contains(&Id::<crate::models::Post>::NIL.to_string()),
            "{rendered}"
        );

        let problem: Error = error.into();
        assert_eq!(problem.status(), 404);
    }

    #[test]
    fn a_slug_collision_is_a_409() {
        let problem: Error = BlogError::SlugTaken {
            slug: "hello".to_owned(),
        }
        .into();
        assert_eq!(problem.into_response().status(), 409);
    }

    #[test]
    fn an_empty_patch_is_a_422() {
        let problem: Error = BlogError::NothingToUpdate.into();
        assert_eq!(problem.status(), 422);
    }
}
