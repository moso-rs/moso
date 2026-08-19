//! The real Swagger UI, vendored and served from the crate itself.
//!
//! [`crate::ui`] is Moso's own compact renderer. This module is the alternative
//! ADR-0019 (`docs/adr/0019-real-swagger-ui.md`) chose: the *genuine* Swagger UI
//! bundle, the same one FastAPI serves at `/docs`, so the page is byte-for-byte
//! the tool users already know.
//!
//! # It is still air-gapped
//!
//! FastAPI loads Swagger UI from a CDN. Moso does **not** — the `.css` and `.js`
//! are `include_bytes!`-embedded from `vendor/swagger-ui/` and served on
//! same-origin sub-paths of the docs route. The rendered page names no absolute
//! URL and makes no external request, so the promise [`crate::ui`] documents —
//! *works in an air-gapped deployment* — holds here too. `the_page_names_no_external_url`
//! keeps that line.
//!
//! # Content-Security-Policy
//!
//! The page loads its bundle with a `<script src>` (covered by `script-src
//! 'self'`) and runs one inline bootstrap `<script>` that carries the
//! per-response nonce. Swagger UI injects element styles at runtime, so the
//! host must serve it with `style-src 'self' 'unsafe-inline'` — moso-core sets
//! exactly that policy on the `/docs` response, and only there.
//!
//! # Usage
//!
//! ```
//! use moso_openapi::swagger_ui::SwaggerUi;
//!
//! let html = SwaggerUi::new()
//!     .spec_url("/openapi.json")
//!     .base_path("/docs")
//!     .title("Shop API")
//!     .render();
//! assert!(html.starts_with("<!doctype html>"));
//! assert!(html.contains("SwaggerUIBundle"));
//! assert!(html.contains("/docs/swagger-ui-bundle.js"));
//! ```

// ---------------------------------------------------------------------------
// Vendored assets
// ---------------------------------------------------------------------------

/// The upstream `swagger-ui-dist` release the vendored assets come from.
///
/// Bumping it means re-fetching both files from
/// `https://unpkg.com/swagger-ui-dist@<version>/` and updating this constant in
/// the same change, so the served bundle and its recorded provenance never drift.
pub const SWAGGER_UI_VERSION: &str = "5.17.14";

/// The vendored Swagger UI stylesheet (`swagger-ui.css`).
pub const SWAGGER_UI_CSS: &[u8] = include_bytes!("../vendor/swagger-ui/swagger-ui.css");

/// The vendored Swagger UI JavaScript bundle (`swagger-ui-bundle.js`).
pub const SWAGGER_UI_BUNDLE_JS: &[u8] = include_bytes!("../vendor/swagger-ui/swagger-ui-bundle.js");

/// One vendored static asset the page references from a docs sub-path.
///
/// A host mounts each of [`ASSETS`] at `<docs_path>/<file_name>` and answers it
/// with `bytes` under `content_type`. Keeping the triple together means the
/// route, the payload and its media type cannot be wired up inconsistently.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct SwaggerAsset {
    /// The last path segment the page requests, e.g. `swagger-ui.css`.
    pub file_name: &'static str,
    /// The file's bytes, embedded at compile time.
    pub bytes: &'static [u8],
    /// The `Content-Type` the asset must be served with.
    pub content_type: &'static str,
}

/// Every static asset the Swagger UI page needs, in the order a host mounts them.
///
/// ```
/// assert_eq!(moso_openapi::swagger_ui::ASSETS.len(), 2);
/// assert!(moso_openapi::swagger_ui::ASSETS.iter().all(|asset| !asset.bytes.is_empty()));
/// ```
pub const ASSETS: &[SwaggerAsset] = &[
    SwaggerAsset {
        file_name: "swagger-ui.css",
        bytes: SWAGGER_UI_CSS,
        content_type: "text/css; charset=utf-8",
    },
    SwaggerAsset {
        file_name: "swagger-ui-bundle.js",
        bytes: SWAGGER_UI_BUNDLE_JS,
        content_type: "application/javascript; charset=utf-8",
    },
];

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

/// Builder for the Swagger UI HTML page.
///
/// The page references its stylesheet and bundle at `<base_path>/<file_name>`
/// (absolute, same-origin) and boots `SwaggerUIBundle` against [`spec_url`]. A
/// host renders one per request so it can stamp a fresh CSP `nonce` on the
/// inline bootstrap script.
///
/// [`spec_url`]: SwaggerUi::spec_url
///
/// ```
/// use moso_openapi::swagger_ui::SwaggerUi;
///
/// let html = SwaggerUi::new().spec_url("/openapi.json").base_path("/docs").render();
/// assert!(html.contains("url: \"/openapi.json\""));
/// ```
#[derive(Debug, Clone)]
pub struct SwaggerUi {
    spec_url: String,
    title: String,
    base_path: String,
    nonce: Option<String>,
}

impl Default for SwaggerUi {
    fn default() -> Self {
        Self::new()
    }
}

impl SwaggerUi {
    /// A page pointed at `/openapi.json`, with assets under `/docs`.
    ///
    /// ```
    /// assert!(moso_openapi::swagger_ui::SwaggerUi::new().render().contains("swagger-ui"));
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            spec_url: crate::ui::DEFAULT_SPEC_URL.to_owned(),
            title: crate::ui::DEFAULT_TITLE.to_owned(),
            base_path: "/docs".to_owned(),
            nonce: None,
        }
    }

    /// Where the page fetches the OpenAPI document from.
    #[must_use]
    pub fn spec_url(mut self, url: impl Into<String>) -> Self {
        self.spec_url = url.into();
        self
    }

    /// The `<title>` of the page.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// The path the page's assets are mounted under — the same path the page
    /// itself answers on, so `swagger-ui.css` resolves to `<base>/swagger-ui.css`.
    #[must_use]
    pub fn base_path(mut self, base: impl Into<String>) -> Self {
        let base = base.into();
        // Trailing slash would double up (`/docs//swagger-ui.css`); strip it so
        // the join is always exactly one separator.
        self.base_path = base.trim_end_matches('/').to_owned();
        self
    }

    /// The Content-Security-Policy nonce to stamp on the inline bootstrap script.
    #[must_use]
    pub fn nonce(mut self, nonce: impl Into<String>) -> Self {
        self.nonce = Some(nonce.into());
        self
    }

    /// Render the page to a complete HTML document.
    #[must_use]
    pub fn render(&self) -> String {
        let title = escape_html(&self.title);
        let base = escape_html(&self.base_path);
        let spec = escape_js_string(&self.spec_url);
        let nonce_attr = match &self.nonce {
            Some(nonce) => format!(" nonce=\"{}\"", escape_html(nonce)),
            None => String::new(),
        };

        format!(
            "<!doctype html>\n\
             <html lang=\"en\">\n\
             <head>\n\
             <meta charset=\"utf-8\">\n\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
             <meta name=\"referrer\" content=\"same-origin\">\n\
             <title>{title}</title>\n\
             <link rel=\"stylesheet\" href=\"{base}/swagger-ui.css\">\n\
             </head>\n\
             <body>\n\
             <div id=\"swagger-ui\"></div>\n\
             <script src=\"{base}/swagger-ui-bundle.js\"></script>\n\
             <script{nonce_attr}>\n\
             window.onload = function () {{\n\
             \x20 window.ui = SwaggerUIBundle({{\n\
             \x20   url: \"{spec}\",\n\
             \x20   dom_id: \"#swagger-ui\",\n\
             \x20   deepLinking: true,\n\
             \x20   presets: [SwaggerUIBundle.presets.apis],\n\
             \x20   layout: \"BaseLayout\"\n\
             \x20 }});\n\
             }};\n\
             </script>\n\
             </body>\n\
             </html>\n"
        )
    }
}

/// HTML-escape text bound for element content or a double-quoted attribute.
fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Escape text bound for a double-quoted JavaScript string literal.
///
/// `<` and `>` are escaped as well so the value can never open a `</script>`
/// sequence inside the inline bootstrap.
fn escape_js_string(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_boots_swagger_ui_against_the_spec_url() {
        let html = SwaggerUi::new()
            .spec_url("/openapi.json")
            .base_path("/docs")
            .render();
        assert!(html.contains("SwaggerUIBundle"));
        assert!(html.contains("url: \"/openapi.json\""));
        assert!(html.contains("<link rel=\"stylesheet\" href=\"/docs/swagger-ui.css\">"));
        assert!(html.contains("<script src=\"/docs/swagger-ui-bundle.js\"></script>"));
    }

    #[test]
    fn the_page_names_no_external_url() {
        // The whole point: air-gapped. No scheme-qualified URL, no CDN.
        let html = SwaggerUi::new().render();
        assert!(!html.contains("http://"), "{html}");
        assert!(!html.contains("https://"), "{html}");
        assert!(!html.contains("//unpkg"), "{html}");
        assert!(!html.contains("cdn."), "{html}");
    }

    #[test]
    fn a_trailing_slash_on_the_base_path_does_not_double_up() {
        let html = SwaggerUi::new().base_path("/docs/").render();
        assert!(html.contains("/docs/swagger-ui.css"));
        assert!(!html.contains("/docs//swagger-ui.css"));
    }

    #[test]
    fn the_nonce_lands_only_on_the_inline_bootstrap() {
        let html = SwaggerUi::new().nonce("abc123").render();
        // The inline script carries it…
        assert!(html.contains("<script nonce=\"abc123\">"));
        // …and the external bundle does not (it is covered by script-src 'self').
        assert!(html.contains("<script src=\"/docs/swagger-ui-bundle.js\"></script>"));
    }

    #[test]
    fn a_spec_url_cannot_break_out_of_the_bootstrap_string() {
        let html = SwaggerUi::new()
            .spec_url("/openapi.json\"</script><script>alert(1)</script>")
            .render();
        assert!(!html.contains("</script><script>alert(1)"));
        assert!(html.contains("\\u003c/script"));
    }

    #[test]
    fn every_asset_is_non_empty_and_typed() {
        for asset in ASSETS {
            assert!(!asset.bytes.is_empty(), "{}", asset.file_name);
            assert!(asset.content_type.contains('/'), "{}", asset.file_name);
        }
    }
}
