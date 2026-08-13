//! Temporary compile check for documentation snippets. Delete after use.

use moso::deps::http::HeaderMap;
use moso::prelude::*;
use moso::response::{Cached, ETag};

/// A post, as the API returns one.
#[derive(Schema)]
pub struct PostOut {
    /// URL-safe identifier.
    pub slug: Slug,
    /// Bumped on every edit.
    pub version: u32,
}

/// Create a post, cached and located.
#[endpoint]
async fn create(headers: HeaderMap) -> Result<Created<Cached<Json<PostOut>>>> {
    let post = PostOut {
        slug: Slug::from_title("hello").unwrap(),
        version: 1,
    };
    Ok(Created::at(
        "/posts/hello",
        Cached::new(Json(post))
            .etag(ETag::strong(1))
            .evaluate(&headers),
    ))
}

#[test]
fn it_compiles() {
    assert_eq!(Router::new().post("/posts", moso::ep!(create)).len(), 1);
}
