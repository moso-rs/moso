//! `Text` and `Html` — the two non-JSON bodies worth a named type.
//!
//! [`Text`] lives in [`crate::extract::body`] because it is a body extractor as
//! well as a response; it is re-exported here so `moso::response::Text` resolves.

use moso_openapi::{ContentType, OperationBuilder, ResponseSpec};
use moso_schema::json_schema::{JsonType, SchemaNode};

use crate::Response;
use crate::response::{Describe, IntoResponse};

pub use crate::extract::body::{Bytes, Text};

/// An HTML response.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::Html;
///
/// /// The marketing page.
/// #[endpoint]
/// async fn landing() -> Result<Html> {
///     Ok(Html("<!doctype html><title>hello</title>".into()))
/// }
/// # fn main() {
/// let response = Html::from("<p>hi</p>").into_response();
/// assert_eq!(response.headers()["content-type"], "text/html; charset=utf-8");
/// # }
/// ```
///
/// **No escaping happens here.** `Html` is a statement that the bytes are
/// already HTML, so interpolating a request value into one is exactly the
/// injection you would expect. Render through a template engine, or use
/// [`moso_schema::Sanitised`] on the way in.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Html(pub String);

impl Html {
    /// The markup.
    pub fn into_inner(self) -> String {
        self.0
    }

    /// The markup as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Html {
    fn from(html: String) -> Self {
        Html(html)
    }
}

impl From<&str> for Html {
    fn from(html: &str) -> Self {
        Html(html.to_owned())
    }
}

impl IntoResponse for Html {
    fn into_response(self) -> Response {
        (
            [(http::header::CONTENT_TYPE, ContentType::Html.as_str())],
            self.0,
        )
            .into_response()
    }
}

impl Describe for Html {
    fn describe(op: &mut OperationBuilder) {
        op.response(
            200,
            ResponseSpec::with_content(ContentType::Html, SchemaNode::of_type(JsonType::String))
                .description("An HTML document"),
        );
    }
}

/// The five characters that have to go, and what they become.
///
/// `'` becomes `&#39;` rather than `&apos;`: the named entity is HTML 5 only,
/// and the numeric one is understood by everything.
const ESCAPES: [(char, &str); 5] = [
    ('&', "&amp;"),
    ('<', "&lt;"),
    ('>', "&gt;"),
    ('"', "&quot;"),
    ('\'', "&#39;"),
];

/// Escape `&`, `<`, `>`, `\"` and `'` for interpolation into HTML.
///
/// Used by the developer error page, which interpolates a panic message. Not a
/// general-purpose templating solution and not presented as one.
pub fn escape_html(input: &str) -> std::borrow::Cow<'_, str> {
    let first = input.find(|c| ESCAPES.iter().any(|(needle, _)| *needle == c));
    // Nothing to do is the common case — a message with no markup in it — and
    // it costs no allocation.
    let Some(first) = first else {
        return std::borrow::Cow::Borrowed(input);
    };

    let mut out = String::with_capacity(input.len() + 16);
    out.push_str(&input[..first]);
    for c in input[first..].chars() {
        match ESCAPES.iter().find(|(needle, _)| *needle == c) {
            Some((_, replacement)) => out.push_str(replacement),
            None => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::tests::described;
    use std::borrow::Cow;

    #[test]
    fn html_wraps_a_string() {
        let html = Html::from("<p>");
        assert_eq!(html.as_str(), "<p>");
        assert_eq!(Html::from(String::from("<p>")).into_inner(), "<p>");
    }

    #[test]
    fn html_is_served_as_html() {
        let response = Html::from("<!doctype html>").into_response();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
    }

    #[test]
    fn html_documents_itself_as_html_not_json() {
        let op = described::<Html>();
        let response = op.response(200).expect("200 documented");
        assert!(response.content.contains_key("text/html; charset=utf-8"));
        assert!(!response.content.contains_key("application/json"));
    }

    #[test]
    fn escaping_borrows_when_there_is_nothing_to_escape() {
        assert!(matches!(escape_html("plain text"), Cow::Borrowed(_)));
        assert!(matches!(escape_html(""), Cow::Borrowed(_)));
        assert!(matches!(escape_html("a & b"), Cow::Owned(_)));
    }

    #[test]
    fn escaping_covers_every_character_that_can_break_out() {
        assert_eq!(
            escape_html(r#"<script>alert("x" & 'y')</script>"#),
            "&lt;script&gt;alert(&quot;x&quot; &amp; &#39;y&#39;)&lt;/script&gt;"
        );
        // The prefix before the first escape is kept verbatim.
        assert_eq!(escape_html("safe <b>"), "safe &lt;b&gt;");
        // Multi-byte characters survive the byte-index split.
        assert_eq!(escape_html("héllo <b>"), "héllo &lt;b&gt;");
    }
}
