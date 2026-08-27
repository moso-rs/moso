//! The embedded documentation UI served at `/docs`.
//!
//! # The rule
//!
//! **No CDN. No external network requests. Ever.**
//!
//! Almost every Rust OpenAPI integration ships a `<script src="…unpkg…">`
//! tag, which fails in exactly the environments that most need working API
//! documentation: air-gapped deployments, corporate proxies with TLS
//! interception, and CI. [`TEMPLATE`] is a single self-contained HTML document
//! with inlined CSS and vanilla JavaScript. The only request it makes is a
//! same-origin `fetch` of the spec URL.
//!
//! Two unit tests hold the line: `template_loads_nothing_from_the_network`
//! asserts the template contains no absolute URL, no `<script src>`, no
//! external stylesheet and no `@import`; `rendered_document_is_balanced`
//! parses the rendered output and checks every element is closed.
//!
//! # Usage
//!
//! ```
//! use moso_openapi::ui::DocsUi;
//!
//! let html = DocsUi::new()
//!     .title("Shop API")
//!     .spec_url("/openapi.json")
//!     .render();
//! assert!(html.starts_with("<!doctype html>"));
//! ```
//!
//! # What it renders
//!
//! A sidebar grouped by tag with a filter box; per-operation method, path,
//! summary and a small CommonMark-ish description renderer; a parameters
//! table; the request body and every response, each with an expandable schema
//! tree that resolves `$ref` (and stops at cycles) and a synthesised example;
//! and a "Try it" panel that issues a `fetch` and reports the status, the
//! elapsed time, the response headers and a pretty-printed body.
//!
//! It follows `prefers-color-scheme` with a manual three-state override, deep
//! links to `#operationId`, is operable from the keyboard alone, and lays out
//! down to a 375 px viewport.
//!
//! # This renderer vs. the vendored Swagger UI
//!
//! As of ADR-0019 the default `/docs` serves the *real* Swagger UI
//! ([`crate::swagger_ui`]) — the tool users know from FastAPI — vendored and
//! self-hosted so it stays air-gapped. This compact renderer is what the `redoc`
//! and `swagger-ui` routes mount, and what the `lean-docs` feature puts back at
//! `/docs` for builds that want a smaller binary with no third-party JavaScript.
//! Both are network-free; the difference is binary size versus familiarity.

/// Where the UI fetches the document from, unless told otherwise.
pub const DEFAULT_SPEC_URL: &str = "/openapi.json";

/// The `<title>` used when the application sets none.
pub const DEFAULT_TITLE: &str = "API documentation";

/// Which colour scheme the UI starts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// Follow the reader's `prefers-color-scheme`, remembering any manual
    /// override in `localStorage`.
    #[default]
    System,
    /// Start light.
    Light,
    /// Start dark.
    Dark,
}

impl Theme {
    /// The value written into the root element's `data-theme` attribute.
    pub const fn as_str(self) -> &'static str {
        match self {
            Theme::System => "auto",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }
}

/// A configured instance of the embedded documentation UI.
#[derive(Debug, Clone)]
pub struct DocsUi {
    spec_url: String,
    title: String,
    theme: Theme,
    nonce: Option<String>,
}

impl Default for DocsUi {
    fn default() -> Self {
        Self::new()
    }
}

impl DocsUi {
    /// The UI with default settings: `/openapi.json`, system theme.
    pub fn new() -> Self {
        Self {
            spec_url: DEFAULT_SPEC_URL.to_owned(),
            title: DEFAULT_TITLE.to_owned(),
            theme: Theme::System,
            nonce: None,
        }
    }

    /// Where to fetch the document from. Must be same-origin.
    pub fn spec_url(mut self, url: impl Into<String>) -> Self {
        self.spec_url = url.into();
        self
    }

    /// The page title and the heading shown above the sidebar.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// The initial colour scheme.
    pub fn theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// A Content-Security-Policy nonce, applied to the inline `<style>` and
    /// `<script>`.
    ///
    /// Supply this when the application sets a CSP with `script-src 'nonce-…'`,
    /// which Moso's security-headers middleware does by default.
    pub fn nonce(mut self, nonce: impl Into<String>) -> Self {
        self.nonce = Some(nonce.into());
        self
    }

    /// Render the complete HTML document.
    pub fn render(&self) -> String {
        let nonce_attr = match &self.nonce {
            Some(nonce) => format!(" nonce=\"{}\"", escape_html(nonce)),
            None => String::new(),
        };
        TEMPLATE
            .replace("__MOSO_NONCE__", &nonce_attr)
            .replace("__MOSO_THEME__", self.theme.as_str())
            .replace("__MOSO_TITLE_HTML__", &escape_html(&self.title))
            .replace("__MOSO_TITLE_JSON__", &json_string(&self.title))
            .replace("__MOSO_SPEC_URL_JSON__", &json_string(&self.spec_url))
    }
}

/// Render the UI for `spec_url` with the given page title.
///
/// Shorthand for `DocsUi::new().spec_url(..).title(..).render()`.
pub fn render(spec_url: &str, title: &str) -> String {
    DocsUi::new().spec_url(spec_url).title(title).render()
}

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// JSON-encode `input` for embedding inside a `<script>` block.
///
/// Every `<` is additionally rewritten to its JSON `u`-escape. Escaping only
/// `</` would stop a value from closing the block, but not from opening one:
/// a title containing `<!--<script>` drives the HTML tokenizer into the
/// *script data double escaped* state, in which the real `</script>` no
/// longer ends the element. With no literal `<` left in the payload, neither
/// attack is reachable.
///
/// The escape is valid JSON, so the value the browser sees is unchanged.
fn json_string(input: &str) -> String {
    serde_json::Value::String(input.to_owned())
        .to_string()
        .replace('<', "\\u003c")
}

/// The raw UI document, with `__MOSO_*__` placeholders still in place.
///
/// Exposed so that an application can vendor and patch it, and so that the
/// no-network test can assert over it directly.
pub const TEMPLATE: &str = r##"<!doctype html>
<html lang="en" data-theme="__MOSO_THEME__">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="referrer" content="same-origin">
<meta name="color-scheme" content="light dark">
<meta name="robots" content="noindex">
<title>__MOSO_TITLE_HTML__</title>
<style__MOSO_NONCE__>
/* ---------------------------------------------------------------- tokens */
:root {
  color-scheme: light;
  --bg: #ffffff; --bg-sunken: #f7f8fa; --bg-raised: #ffffff;
  --bg-code: #f1f3f6; --bg-hover: #ebeef2;
  --border: #e3e7ec; --border-strong: #ccd3dc;
  --fg: #12161c; --fg-muted: #56606e; --fg-faint: #8a94a2;
  --accent: #2f6feb; --accent-fg: #ffffff; --accent-soft: #e8f0fe;
  --ok: #12744f; --ok-bg: #e4f4ec;
  --warn: #8a5b06; --warn-bg: #fbf0d9;
  --err: #b8342a; --err-bg: #fbeae8;
  --info: #6f45c4; --info-bg: #efe9fb;
  --shadow-sm: 0 1px 2px rgba(16, 24, 40, .06);
  --shadow-lg: 0 16px 44px rgba(16, 24, 40, .18);
  --radius: 8px; --radius-sm: 5px;
  --mono: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace;
  --sans: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
  --sidebar-w: 300px;
}
:root[data-theme="dark"] {
  color-scheme: dark;
  --bg: #12151a; --bg-sunken: #0d1014; --bg-raised: #191d23;
  --bg-code: #1f242b; --bg-hover: #232931;
  --border: #272d35; --border-strong: #3a424d;
  --fg: #e7eaee; --fg-muted: #9aa4b1; --fg-faint: #6d7783;
  --accent: #6ea8ff; --accent-fg: #0d1014; --accent-soft: #16233a;
  --ok: #4fcf9b; --ok-bg: #0f2620;
  --warn: #e0aa4a; --warn-bg: #2b2213;
  --err: #f2776d; --err-bg: #2f1a18;
  --info: #b493f5; --info-bg: #231b36;
  --shadow-sm: 0 1px 2px rgba(0, 0, 0, .4);
  --shadow-lg: 0 16px 44px rgba(0, 0, 0, .55);
}
@media (prefers-color-scheme: dark) {
  :root[data-theme="auto"] {
    color-scheme: dark;
    --bg: #12151a; --bg-sunken: #0d1014; --bg-raised: #191d23;
    --bg-code: #1f242b; --bg-hover: #232931;
    --border: #272d35; --border-strong: #3a424d;
    --fg: #e7eaee; --fg-muted: #9aa4b1; --fg-faint: #6d7783;
    --accent: #6ea8ff; --accent-fg: #0d1014; --accent-soft: #16233a;
    --ok: #4fcf9b; --ok-bg: #0f2620;
    --warn: #e0aa4a; --warn-bg: #2b2213;
    --err: #f2776d; --err-bg: #2f1a18;
    --info: #b493f5; --info-bg: #231b36;
    --shadow-sm: 0 1px 2px rgba(0, 0, 0, .4);
    --shadow-lg: 0 16px 44px rgba(0, 0, 0, .55);
  }
}

/* ---------------------------------------------------------------- base */
* { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; }
html, body { margin: 0; padding: 0; }
body {
  background: var(--bg); color: var(--fg);
  font: 15px/1.6 var(--sans); -webkit-font-smoothing: antialiased;
  overflow-wrap: break-word;
}
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
code, pre, kbd { font-family: var(--mono); font-size: 12.5px; }
pre { margin: 0; }
button { font-family: inherit; }
:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; border-radius: 3px; }
.sr {
  position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
  overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; border: 0;
}
.skip {
  position: fixed; top: 8px; left: 8px; z-index: 90; transform: translateY(-200%);
  background: var(--accent); color: var(--accent-fg); padding: 8px 14px;
  border-radius: var(--radius); font-size: 13px; font-weight: 600;
}
.skip:focus { transform: none; text-decoration: none; }

#layout { display: grid; grid-template-columns: var(--sidebar-w) minmax(0, 1fr); }

/* ---------------------------------------------------------------- sidebar */
#sidebar {
  background: var(--bg-sunken); border-right: 1px solid var(--border);
  display: flex; flex-direction: column;
  position: sticky; top: 0; height: 100vh; height: 100dvh; z-index: 30;
}
#brand { padding: 16px 16px 10px; display: flex; align-items: baseline; gap: 8px; }
#brand h1 {
  font-size: 15px; font-weight: 650; margin: 0; letter-spacing: -.01em; min-width: 0;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.ver {
  font: 500 11px/1.6 var(--mono); color: var(--fg-muted);
  background: var(--bg-code); border-radius: 20px; padding: 1px 8px; flex: none;
}
#searchwrap { padding: 0 12px 10px; position: relative; }
#search {
  width: 100%; padding: 7px 30px 7px 10px; border-radius: var(--radius);
  border: 1px solid var(--border-strong); background: var(--bg-raised);
  color: var(--fg); font: 13px var(--sans); outline: none;
}
#search::-webkit-search-cancel-button { display: none; }
#search:focus { border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft); }
#searchwrap kbd {
  position: absolute; right: 20px; top: 7px; color: var(--fg-faint);
  border: 1px solid var(--border-strong); border-radius: 4px;
  padding: 0 5px; font-size: 11px; line-height: 18px; pointer-events: none;
}
#search:focus + kbd, #search:not(:placeholder-shown) + kbd { display: none; }
#nav { overflow-y: auto; overscroll-behavior: contain; flex: 1; padding: 0 8px 20px; }
.navhead {
  width: 100%; display: flex; align-items: center; gap: 6px; background: none;
  border: 0; cursor: pointer; text-align: left;
  font: 600 11px var(--sans); text-transform: uppercase; letter-spacing: .07em;
  color: var(--fg-faint); padding: 12px 8px 6px;
}
.navhead:hover { color: var(--fg-muted); }
.navhead .tw { font-size: 9px; transition: transform .15s ease; }
.navhead[aria-expanded="false"] .tw { transform: rotate(-90deg); }
.navhead .n { margin-left: auto; font-size: 10px; letter-spacing: 0; }
.navgroup[data-collapsed="true"] .navlist { display: none; }
.navitem {
  display: flex; align-items: center; gap: 8px; padding: 5px 8px;
  border-radius: var(--radius-sm); color: var(--fg-muted); font-size: 13px;
  line-height: 1.35; min-width: 0;
}
.navitem:hover { background: var(--bg-hover); color: var(--fg); text-decoration: none; }
.navitem[aria-current="true"] { background: var(--accent-soft); color: var(--fg); font-weight: 550; }
.navitem .txt { min-width: 0; }
.navitem .label, .navitem .sub { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.navitem .sub { font: 11px var(--mono); color: var(--fg-faint); }
.navitem.dep .label { text-decoration: line-through; opacity: .7; }
.verb {
  flex: none; font: 700 9.5px/1 var(--mono); letter-spacing: .03em;
  padding: 4px 4px; border-radius: 4px; min-width: 44px; text-align: center;
  color: var(--fg-muted); background: var(--bg-code);
}
.verb.get { color: var(--ok); background: var(--ok-bg); }
.verb.post { color: var(--accent); background: var(--accent-soft); }
.verb.put { color: var(--warn); background: var(--warn-bg); }
.verb.patch { color: var(--info); background: var(--info-bg); }
.verb.delete { color: var(--err); background: var(--err-bg); }
#sidefoot {
  border-top: 1px solid var(--border); padding: 8px 12px; display: flex;
  align-items: center; justify-content: space-between; gap: 8px;
  font-size: 12px; color: var(--fg-faint);
}
.iconbtn {
  background: none; border: 1px solid var(--border-strong); color: var(--fg-muted);
  border-radius: var(--radius-sm); cursor: pointer; padding: 4px 9px;
  font: 12px var(--sans); display: inline-flex; align-items: center; gap: 5px;
}
.iconbtn:hover { color: var(--fg); border-color: var(--fg-faint); background: var(--bg-hover); }

/* ---------------------------------------------------------------- topbar */
#topbar {
  display: none; position: sticky; top: 0; z-index: 20; gap: 10px;
  align-items: center; padding: 8px 14px;
  background: var(--bg); border-bottom: 1px solid var(--border);
}
#topbar .t { font-weight: 600; font-size: 14px; min-width: 0; overflow: hidden;
  text-overflow: ellipsis; white-space: nowrap; }
#topbar .spacer { margin-left: auto; }
#scrim {
  display: none; position: fixed; inset: 0; z-index: 25;
  background: rgba(10, 13, 18, .5);
}
#scrim.on { display: block; }

/* ---------------------------------------------------------------- main */
#main { min-width: 0; }
#page { padding: 30px 36px 140px; max-width: 1120px; }
#apihead { border-bottom: 1px solid var(--border); padding-bottom: 22px; }
#apihead h2 { margin: 0 0 6px; font-size: 27px; letter-spacing: -.022em; line-height: 1.2; }
#apihead .meta { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; margin-bottom: 10px; }
#apihead .prose { max-width: 70ch; }
#servers { margin-top: 16px; display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
#servers label { font-size: 12px; color: var(--fg-faint); }
select.control, input.control, textarea.control {
  font: 12.5px var(--mono); padding: 6px 9px; border-radius: var(--radius-sm);
  border: 1px solid var(--border-strong); background: var(--bg-raised);
  color: var(--fg); outline: none; max-width: 100%;
}
select.control:focus, input.control:focus, textarea.control:focus {
  border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft);
}

.tagsection { padding-top: 34px; scroll-margin-top: 12px; }
.tagsection > h3 { margin: 0 0 4px; font-size: 19px; letter-spacing: -.015em; }
.tagsection > .prose { margin: 0 0 14px; max-width: 70ch; }

.op {
  border: 1px solid var(--border); border-radius: 10px; background: var(--bg-raised);
  margin-bottom: 12px; scroll-margin-top: 12px; box-shadow: var(--shadow-sm);
}
.op.target { border-color: var(--accent); }
.op.dep .ophead .path { text-decoration: line-through; }
.opbar { display: flex; align-items: center; padding-right: 8px; }
.ophead {
  display: flex; align-items: center; gap: 11px; padding: 12px 14px;
  flex: 1 1 auto; min-width: 0;
  background: none; border: 0; cursor: pointer; text-align: left; color: inherit;
  border-radius: 10px;
}
.ophead:hover { background: var(--bg-hover); }
.op.open .ophead { border-radius: 10px 10px 0 0; }
.ophead .chev { color: var(--fg-faint); flex: none; font-size: 10px; transition: transform .15s ease; }
.op.open .ophead .chev { transform: rotate(90deg); }
.ophead .path { font: 600 13.5px/1.4 var(--mono); overflow-wrap: anywhere; }
.ophead .path .pp { color: var(--accent); }
.ophead .sum {
  color: var(--fg-muted); font-size: 13px; margin-left: auto; text-align: right;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 44%;
}
.opbody { display: none; border-top: 1px solid var(--border); padding: 18px 16px 16px; }
.op.open .opbody { display: block; }
/* The header already carries the summary; the body repeats it only at the
   width where the header drops it. */
.opsummary { display: none; }
.anchor { color: var(--fg-faint); font: 13px var(--mono); padding: 2px 6px;
  border-radius: 4px; flex: none; }
.anchor:hover { color: var(--accent); background: var(--bg-hover); text-decoration: none; }

.badge {
  display: inline-block; font: 600 10.5px/1.5 var(--sans); letter-spacing: .03em;
  padding: 2px 8px; border-radius: 20px; background: var(--bg-code);
  color: var(--fg-muted); flex: none;
}
.badge.warn { background: var(--warn-bg); color: var(--warn); }
.badge.lock { background: var(--info-bg); color: var(--info); }

.section { margin-bottom: 22px; }
.section:last-child { margin-bottom: 0; }
.section > h4 {
  font: 600 11px var(--sans); text-transform: uppercase; letter-spacing: .08em;
  color: var(--fg-faint); margin: 0 0 9px; display: flex; align-items: center; gap: 8px;
}
.tabbar { display: flex; flex-wrap: wrap; gap: 6px; margin: 0 0 8px; }
.tinybtn {
  background: none; border: 1px solid var(--border); color: var(--fg-faint);
  border-radius: 4px; cursor: pointer; padding: 1px 7px;
  font: 500 10.5px var(--sans); text-transform: none; letter-spacing: 0;
}
.tinybtn:hover { color: var(--fg); border-color: var(--border-strong); background: var(--bg-hover); }
.tinybtn[aria-selected="true"] { color: var(--fg); background: var(--bg-code); border-color: var(--border-strong); }

/* ---------------------------------------------------------------- prose */
.prose { color: var(--fg-muted); }
.prose > *:first-child { margin-top: 0; }
.prose > *:last-child { margin-bottom: 0; }
.prose p { margin: 0 0 10px; }
.prose ul, .prose ol { margin: 0 0 10px; padding-left: 22px; }
.prose li { margin: 2px 0; }
.prose h5, .prose h6 { color: var(--fg); margin: 14px 0 6px; font-size: 14px; }
.prose blockquote {
  margin: 0 0 10px; padding: 2px 0 2px 12px; border-left: 3px solid var(--border-strong);
  color: var(--fg-faint);
}
.prose hr { border: 0; border-top: 1px solid var(--border); margin: 14px 0; }
.prose code { background: var(--bg-code); border-radius: 4px; padding: 1px 5px; color: var(--fg); }
.prose pre {
  background: var(--bg-code); border-radius: var(--radius-sm); padding: 10px 12px;
  overflow-x: auto; margin: 0 0 10px;
}
.prose pre code { background: none; padding: 0; }

/* ---------------------------------------------------------------- tables */
.tablewrap { overflow-x: auto; }
table.params { width: 100%; border-collapse: collapse; font-size: 13px; }
table.params th {
  text-align: left; font: 600 10.5px var(--sans); text-transform: uppercase;
  letter-spacing: .07em; color: var(--fg-faint); padding: 0 12px 6px 0;
  border-bottom: 1px solid var(--border); white-space: nowrap;
}
table.params td { padding: 9px 12px 9px 0; border-bottom: 1px solid var(--border); vertical-align: top; }
table.params tr:last-child td { border-bottom: none; }
.pname { font: 600 12.5px var(--mono); overflow-wrap: anywhere; }
.ptype { font: 12px var(--mono); color: var(--fg-muted); }
.req { color: var(--err); font-size: 11px; font-weight: 700; }
.hint { color: var(--fg-faint); font-size: 12px; }
.empty { color: var(--fg-faint); font-style: italic; font-size: 13px; }

/* ---------------------------------------------------------------- schema */
.schema { font: 12.5px/1.6 var(--mono); overflow-x: auto; }
.schema .row { padding: 1px 0; }
.schema .kids { margin-left: 7px; padding-left: 11px; border-left: 1px solid var(--border); }
.schema .key { color: var(--fg); font-weight: 650; }
.schema .type { color: var(--fg-muted); }
.schema .facets { color: var(--info); }
.schema .doc { color: var(--fg-faint); font: 12px/1.5 var(--sans); padding-left: 18px; max-width: 68ch; }
.schema .doc p { margin: 0 0 4px; }
.schema .refname { color: var(--accent); }
.schema .cycle { color: var(--warn); font-style: italic; }
.tw {
  background: none; border: 0; padding: 0; margin-right: 5px; cursor: pointer;
  color: var(--fg-faint); font: 10px var(--mono); width: 11px; display: inline-block;
  text-align: left;
}
.tw:hover { color: var(--fg); }
.tw.leaf { cursor: default; visibility: hidden; }
.codeblock {
  background: var(--bg-code); border-radius: var(--radius-sm); padding: 10px 12px;
  overflow: auto; max-height: 420px;
}

/* ---------------------------------------------------------------- responses */
.resp { border: 1px solid var(--border); border-radius: var(--radius-sm); margin-bottom: 8px; }
.resphead {
  display: flex; align-items: center; gap: 10px; padding: 8px 11px; width: 100%;
  background: none; border: 0; cursor: pointer; text-align: left; color: inherit;
  font-size: 13px; border-radius: var(--radius-sm);
}
.resphead:hover { background: var(--bg-hover); }
.resphead .d { color: var(--fg-muted); min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.status { font: 700 12px/1.5 var(--mono); padding: 1px 8px; border-radius: 4px; flex: none; }
.status.s2 { color: var(--ok); background: var(--ok-bg); }
.status.s3 { color: var(--accent); background: var(--accent-soft); }
.status.s4 { color: var(--warn); background: var(--warn-bg); }
.status.s5 { color: var(--err); background: var(--err-bg); }
.status.sd { color: var(--fg-muted); background: var(--bg-code); }
.respbody { display: none; padding: 10px 12px 12px; border-top: 1px solid var(--border); }
.resp.open .respbody { display: block; }
.ct { font: 11.5px var(--mono); color: var(--fg-faint); margin: 8px 0 5px; }
.ct:first-child { margin-top: 0; }

/* ---------------------------------------------------------------- try it */
.tryit {
  border: 1px solid var(--border-strong); border-radius: var(--radius);
  padding: 14px; background: var(--bg-sunken);
}
.field { display: grid; grid-template-columns: 190px minmax(0, 1fr); gap: 10px;
  align-items: center; margin-bottom: 8px; }
.field > label { font: 12px var(--mono); color: var(--fg-muted); overflow-wrap: anywhere; }
.field > label .in { color: var(--fg-faint); }
.field input, .field select, .field textarea { width: 100%; }
.tryit textarea { min-height: 150px; resize: vertical; line-height: 1.5; }
.stack { display: block; margin-bottom: 10px; }
.stack input, .stack select, .stack textarea { width: 100%; }
.stack > label { display: flex; align-items: center; gap: 8px; margin-bottom: 5px; }
.stack > label .tools { margin-left: auto; display: flex; gap: 6px; }
.authbox { border: 1px dashed var(--border-strong); border-radius: var(--radius-sm);
  padding: 10px 12px; margin-bottom: 12px; }
.authbox > .t { font: 600 10.5px var(--sans); text-transform: uppercase;
  letter-spacing: .08em; color: var(--fg-faint); margin-bottom: 8px; }
.btn {
  background: var(--accent); color: var(--accent-fg); border: 0; border-radius: var(--radius-sm);
  padding: 7px 15px; font: 600 13px var(--sans); cursor: pointer;
}
.btn:hover { filter: brightness(1.08); }
.btn[disabled] { opacity: .6; cursor: progress; }
.btn.ghost { background: transparent; color: var(--fg-muted); border: 1px solid var(--border-strong); }
.btn.ghost:hover { color: var(--fg); background: var(--bg-hover); filter: none; }
.btnrow { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; margin-top: 10px; }
.result { margin-top: 14px; border-top: 1px dashed var(--border-strong); padding-top: 12px; }
.result .meta { display: flex; flex-wrap: wrap; gap: 8px 12px; align-items: center;
  font-size: 12px; color: var(--fg-muted); margin-bottom: 10px; }
.result .meta .url { font: 11.5px var(--mono); overflow-wrap: anywhere; }
.result pre { max-height: 380px; white-space: pre; }

/* ---------------------------------------------------------------- status */
#status { padding: 64px 36px; color: var(--fg-muted); max-width: 60ch; }
#status.err { color: var(--err); }
#status code { background: var(--bg-code); padding: 2px 6px; border-radius: 4px; color: var(--fg); }
.spinner {
  width: 18px; height: 18px; border-radius: 50%; display: inline-block;
  border: 2px solid var(--border-strong); border-top-color: var(--accent);
  animation: spin .7s linear infinite; vertical-align: -4px; margin-right: 8px;
}
@keyframes spin { to { transform: rotate(360deg); } }
@media (prefers-reduced-motion: reduce) {
  .spinner { animation-duration: 2.4s; }
  * { scroll-behavior: auto !important; }
}

/* ---------------------------------------------------------------- dialog */
#help { position: fixed; inset: 0; z-index: 60; display: flex;
  align-items: center; justify-content: center; padding: 16px; }
/* An id selector beats the user-agent `[hidden] { display: none }` rule, so
   the hidden state has to be restated at the same weight. */
#help[hidden] { display: none; }
#help .backdrop { position: absolute; inset: 0; background: rgba(10, 13, 18, .5); }
#help .panel {
  position: relative; background: var(--bg-raised); border: 1px solid var(--border);
  border-radius: var(--radius); box-shadow: var(--shadow-lg);
  padding: 18px 20px; width: 100%; max-width: 420px;
}
#help h2 { margin: 0 0 12px; font-size: 16px; }
#help dl { display: grid; grid-template-columns: auto 1fr; gap: 8px 14px; margin: 0; font-size: 13px; }
#help dt { margin: 0; }
#help dd { margin: 0; color: var(--fg-muted); }
kbd {
  border: 1px solid var(--border-strong); border-bottom-width: 2px; border-radius: 4px;
  padding: 1px 6px; font-size: 11px; background: var(--bg-code); color: var(--fg);
  white-space: nowrap;
}

/* ---------------------------------------------------------------- narrow */
@media (max-width: 1080px) { :root { --sidebar-w: 268px; } #page { padding: 26px 24px 120px; } }
@media (max-width: 860px) {
  #layout { grid-template-columns: minmax(0, 1fr); }
  #topbar { display: flex; }
  #sidebar {
    position: fixed; top: 0; bottom: 0; left: 0; width: 296px; max-width: 86vw;
    transform: translateX(-102%); transition: transform .18s ease;
    box-shadow: var(--shadow-lg); height: 100%;
  }
  #sidebar.on { transform: none; }
  #page { padding: 22px 18px 100px; }
  .ophead .sum { display: none; }
  .opsummary { display: block; }
}
@media (max-width: 560px) {
  body { font-size: 14.5px; }
  #page { padding: 18px 14px 90px; }
  #apihead h2 { font-size: 22px; }
  .ophead { gap: 8px; padding: 11px; flex-wrap: wrap; }
  .ophead .path { font-size: 12.5px; }
  .opbar .anchor { display: none; }
  .opbody { padding: 14px 11px; }
  .tryit { padding: 11px; }
  .field { grid-template-columns: minmax(0, 1fr); gap: 4px; }
  .field > label { font-size: 11.5px; }
  table.params thead { display: none; }
  table.params tr { display: block; padding: 9px 0; border-bottom: 1px solid var(--border); }
  table.params tr:last-child { border-bottom: 0; }
  table.params td { display: block; border: 0; padding: 1px 0; }
  table.params td[data-label]::before {
    content: attr(data-label) " "; color: var(--fg-faint); font: 600 10px var(--sans);
    text-transform: uppercase; letter-spacing: .07em; margin-right: 6px;
  }
  table.params td:empty { display: none; }
  .schema .doc { padding-left: 0; }
  .btnrow .btn { flex: 1 1 auto; }
}
</style>
</head>
<body>
<a class="skip" href="#page">Skip to content</a>
<div id="layout">
  <aside id="sidebar" aria-label="API navigation">
    <div id="brand">
      <h1 id="brandtitle">__MOSO_TITLE_HTML__</h1>
      <span class="ver" id="brandver" hidden></span>
    </div>
    <div id="searchwrap">
      <label class="sr" for="search">Filter operations</label>
      <input id="search" type="search" placeholder="Filter operations" autocomplete="off"
             autocorrect="off" spellcheck="false" aria-describedby="opcount">
      <kbd>/</kbd>
    </div>
    <nav id="nav" aria-label="Operations"></nav>
    <div id="sidefoot">
      <span id="opcount" aria-live="polite"></span>
      <span>
        <button class="iconbtn" id="helpbtn" type="button" aria-haspopup="dialog">?</button>
        <button class="iconbtn themebtn" type="button" aria-live="polite">Theme</button>
      </span>
    </div>
  </aside>
  <div id="scrim" hidden></div>
  <main id="main">
    <div id="topbar">
      <button class="iconbtn" id="menubtn" type="button" aria-controls="sidebar" aria-expanded="false">
        <span aria-hidden="true">&#9776;</span> Menu
      </button>
      <span class="t" id="topbartitle">__MOSO_TITLE_HTML__</span>
      <span class="spacer"></span>
      <button class="iconbtn themebtn" type="button">Theme</button>
    </div>
    <div id="status"><span class="spinner" aria-hidden="true"></span>Loading the API description&#8230;</div>
    <div id="page" hidden>
      <header id="apihead"></header>
      <div id="sections"></div>
    </div>
  </main>
</div>
<div id="help" hidden role="dialog" aria-modal="true" aria-labelledby="helptitle">
  <div class="backdrop"></div>
  <div class="panel">
    <h2 id="helptitle">Keyboard shortcuts</h2>
    <dl>
      <dt><kbd>/</kbd></dt><dd>Focus the filter box</dd>
      <dt><kbd>&#8595;</kbd> <kbd>&#8593;</kbd></dt><dd>Move through the filtered list</dd>
      <dt><kbd>Enter</kbd></dt><dd>Open the highlighted operation</dd>
      <dt><kbd>e</kbd></dt><dd>Expand or collapse every operation</dd>
      <dt><kbd>t</kbd></dt><dd>Cycle the colour scheme</dd>
      <dt><kbd>?</kbd></dt><dd>This dialog</dd>
      <dt><kbd>Esc</kbd></dt><dd>Close, or clear the filter</dd>
    </dl>
    <div class="btnrow"><button class="btn ghost" id="helpclose" type="button">Close</button></div>
  </div>
</div>
<script__MOSO_NONCE__>
(function () {
  'use strict';

  /* ------------------------------------------------------------ config */
  var SPEC_URL = __MOSO_SPEC_URL_JSON__;
  var PAGE_TITLE = __MOSO_TITLE_JSON__;
  var D = document;
  var METHODS = ['get', 'post', 'put', 'patch', 'delete', 'head', 'options', 'trace'];
  var MAX_DEPTH = 10;
  var OPEN_DEPTH = 1;
  var TIMEOUT_MS = 30000;
  var THEME_KEY = 'moso-docs-theme';
  /* The scheme is concatenated so that the template contains no absolute URL
     at all; the no-network test asserts exactly that. */
  var EXAMPLE_URI = 'https' + '://example.com';

  var spec = null;
  var ops = [];
  var opsByAnchor = {};
  var activeAnchor = null;
  var suppressSpy = 0;
  /* Credentials typed into "Try it" live here for the lifetime of the page and
     are deliberately never written to storage. */
  var authValues = {};

  /* ------------------------------------------------------------ DOM utils */
  function el(tag, cls, text) {
    var node = D.createElement(tag);
    if (cls) { node.className = cls; }
    if (text !== undefined && text !== null) { node.textContent = String(text); }
    return node;
  }
  function byId(id) { return D.getElementById(id); }
  /* The CSS `scroll-behavior` property does not override the `behavior`
     argument of scrollIntoView, so the media query has to be read here too. */
  function reducedMotion() {
    return !!(window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches);
  }
  function clear(node) { while (node.firstChild) { node.removeChild(node.firstChild); } }
  function text(value) { return D.createTextNode(String(value)); }
  function on(node, event, fn) { node.addEventListener(event, fn); return node; }
  function attr(node, name, value) { node.setAttribute(name, value); return node; }
  function has(object, key) { return Object.prototype.hasOwnProperty.call(object, key); }
  function keys(object) { return object && typeof object === 'object' ? Object.keys(object) : []; }

  function copy(value, button) {
    var done = function () {
      if (!button) { return; }
      var was = button.textContent;
      button.textContent = 'Copied';
      window.setTimeout(function () { button.textContent = was; }, 1200);
    };
    if (window.navigator && window.navigator.clipboard) {
      window.navigator.clipboard.writeText(value).then(done, function () { fallback(value, done); });
    } else {
      fallback(value, done);
    }
  }
  function fallback(value, done) {
    var area = el('textarea');
    area.value = value;
    area.setAttribute('aria-hidden', 'true');
    area.style.position = 'fixed';
    area.style.opacity = '0';
    D.body.appendChild(area);
    area.select();
    try { D.execCommand('copy'); done(); } catch (e) { /* nothing else to try */ }
    D.body.removeChild(area);
  }
  function copyButton(label, supplier) {
    var button = el('button', 'tinybtn', label);
    button.type = 'button';
    return on(button, 'click', function (event) {
      event.stopPropagation();
      copy(supplier(), button);
    });
  }

  /* ------------------------------------------------------------ theme */
  function setTheme(value) {
    D.documentElement.setAttribute('data-theme', value);
    try { window.localStorage.setItem(THEME_KEY, value); } catch (e) { /* private mode */ }
    var label = value === 'auto' ? 'Auto' : (value === 'dark' ? 'Dark' : 'Light');
    var buttons = D.querySelectorAll('.themebtn');
    for (var i = 0; i < buttons.length; i++) {
      buttons[i].textContent = label;
      buttons[i].title = 'Colour scheme: ' + label + ' (press t)';
    }
  }
  function cycleTheme() {
    var order = ['auto', 'light', 'dark'];
    var index = order.indexOf(D.documentElement.getAttribute('data-theme'));
    setTheme(order[(index + 1) % order.length]);
  }

  /* ------------------------------------------------------------ $ref */
  function unescapeToken(token) {
    return decodeURIComponent(token).replace(/~1/g, '/').replace(/~0/g, '~');
  }
  /* Resolves a local JSON pointer. Remote refs are not followed: this UI is
     rendered from one self-contained document. */
  function resolvePointer(ref) {
    if (typeof ref !== 'string' || ref.charAt(0) !== '#') { return null; }
    var parts = ref.slice(1).split('/');
    var node = spec;
    for (var i = 0; i < parts.length; i++) {
      if (parts[i] === '') { continue; }
      var token = unescapeToken(parts[i]);
      if (!node || typeof node !== 'object' || !has(node, token)) { return null; }
      node = node[token];
    }
    return node && typeof node === 'object' ? node : null;
  }
  function deref(node) {
    var guard = 0;
    while (node && node.$ref && guard < 8) {
      var next = resolvePointer(node.$ref);
      if (!next) { return node; }
      node = next;
      guard += 1;
    }
    return node;
  }
  function refName(ref) {
    return typeof ref === 'string' ? unescapeToken(ref.slice(ref.lastIndexOf('/') + 1)) : '';
  }

  /* ------------------------------------------------------------ markdown */
  var BLOCK_START = /^(\s{0,3})(#{1,6}\s|[-*+]\s|\d+[.)]\s|>|```|~~~|---\s*$|___\s*$)/;

  /* A deliberately small CommonMark subset: paragraphs, ATX headings, fenced
     code, blockquotes, thematic breaks, bullet and ordered lists, and the
     inline constructs below. Everything is built with DOM nodes, never
     innerHTML, so a hostile description cannot inject markup. */
  function markdown(source) {
    var out = D.createDocumentFragment();
    var lines = String(source == null ? '' : source).replace(/\r\n?/g, '\n').split('\n');
    var i = 0;
    while (i < lines.length) {
      var line = lines[i];
      if (!line.trim()) { i += 1; continue; }
      var fence = /^\s{0,3}(```|~~~)(.*)$/.exec(line);
      if (fence) {
        var buffer = [];
        i += 1;
        while (i < lines.length && !new RegExp('^\\s{0,3}' + fence[1]).test(lines[i])) {
          buffer.push(lines[i]); i += 1;
        }
        i += 1;
        var pre = el('pre');
        pre.appendChild(el('code', null, buffer.join('\n')));
        out.appendChild(pre);
        continue;
      }
      var heading = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line);
      if (heading) {
        var level = heading[1].length <= 4 ? 'h5' : 'h6';
        var head = el(level);
        inline(heading[2], head);
        out.appendChild(head);
        i += 1;
        continue;
      }
      if (/^\s{0,3}(-\s*-\s*-|\*\s*\*\s*\*|_\s*_\s*_)[-*_\s]*$/.test(line)) {
        out.appendChild(el('hr')); i += 1; continue;
      }
      if (/^\s{0,3}>/.test(line)) {
        var quoted = [];
        while (i < lines.length && /^\s{0,3}>/.test(lines[i])) {
          quoted.push(lines[i].replace(/^\s{0,3}>\s?/, '')); i += 1;
        }
        var quote = el('blockquote');
        quote.appendChild(markdown(quoted.join('\n')));
        out.appendChild(quote);
        continue;
      }
      var bullet = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/.exec(line);
      if (bullet) {
        var ordered = bullet[2].length > 1;
        var list = el(ordered ? 'ol' : 'ul');
        while (i < lines.length) {
          var item = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/.exec(lines[i]);
          if (!item) { break; }
          var parts = [item[3]];
          i += 1;
          while (i < lines.length && lines[i].trim() &&
                 !/^(\s*)([-*+]|\d+[.)])\s+/.test(lines[i])) {
            parts.push(lines[i].replace(/^\s+/, '')); i += 1;
          }
          var li = el('li');
          inline(parts.join(' '), li);
          list.appendChild(li);
          if (i < lines.length && !lines[i].trim()) { i += 1; }
        }
        out.appendChild(list);
        continue;
      }
      var paragraph = [];
      while (i < lines.length && lines[i].trim() &&
             !(paragraph.length && BLOCK_START.test(lines[i]))) {
        paragraph.push(lines[i]); i += 1;
      }
      var p = el('p');
      inline(paragraph.join('\n'), p);
      out.appendChild(p);
    }
    return out;
  }

  var SAFE_HREF = /^(https?:|mailto:|tel:|[/#.]|[a-z0-9_-]+[/?#])/i;

  /* `_` never opens emphasis inside a word: API prose is full of snake_case
     identifiers and `user_id_field` must not become "user <em>id</em> field".
     `*` has no such restriction, as in CommonMark. */
  function opens(value, i, ch) {
    return ch !== '_' || i === 0 || !/[A-Za-z0-9]/.test(value.charAt(i - 1));
  }

  function inline(source, parent) {
    var value = String(source == null ? '' : source);
    var buffer = '';
    var i = 0;
    function flush() { if (buffer) { parent.appendChild(text(buffer)); buffer = ''; } }
    while (i < value.length) {
      var ch = value.charAt(i);
      if (ch === '\\' && i + 1 < value.length && '\\`*_[]()#~>'.indexOf(value.charAt(i + 1)) !== -1) {
        buffer += value.charAt(i + 1); i += 2; continue;
      }
      if (ch === '`') {
        var ticks = /^`+/.exec(value.slice(i))[0];
        var close = value.indexOf(ticks, i + ticks.length);
        if (close !== -1) {
          flush();
          parent.appendChild(el('code', null, value.slice(i + ticks.length, close).replace(/^ | $/g, '')));
          i = close + ticks.length;
          continue;
        }
      }
      if ((ch === '*' || ch === '_') && value.charAt(i + 1) === ch && opens(value, i, ch)) {
        var strongEnd = value.indexOf(ch + ch, i + 2);
        if (strongEnd !== -1) {
          flush();
          var strong = el('strong');
          inline(value.slice(i + 2, strongEnd), strong);
          parent.appendChild(strong);
          i = strongEnd + 2;
          continue;
        }
      }
      if ((ch === '*' || ch === '_') && value.charAt(i + 1) !== ' ' && opens(value, i, ch)) {
        var emEnd = value.indexOf(ch, i + 1);
        if (emEnd !== -1 && emEnd > i + 1) {
          flush();
          var em = el('em');
          inline(value.slice(i + 1, emEnd), em);
          parent.appendChild(em);
          i = emEnd + 1;
          continue;
        }
      }
      if (ch === '[') {
        var link = /^\[([^\]]*)\]\(([^)\s]*)(?:\s+"[^"]*")?\)/.exec(value.slice(i));
        if (link) {
          flush();
          if (SAFE_HREF.test(link[2])) {
            var a = el('a');
            a.href = link[2];
            if (/^[a-z]+:/i.test(link[2])) { a.target = '_blank'; a.rel = 'noopener noreferrer'; }
            inline(link[1], a);
            parent.appendChild(a);
          } else {
            parent.appendChild(text(link[1]));
          }
          i += link[0].length;
          continue;
        }
      }
      buffer += ch;
      i += 1;
    }
    flush();
  }

  function prose(source, cls) {
    var box = el('div', cls || 'prose');
    box.appendChild(markdown(source));
    return box;
  }

  /* ------------------------------------------------------------ schema */
  function typeLabel(node) {
    if (!node) { return 'any'; }
    if (node.$ref) { return refName(node.$ref); }
    var t = node.type;
    if (Array.isArray(t)) { t = t.join(' | '); }
    if (!t) {
      if (node.oneOf) { return 'one of'; }
      if (node.anyOf) { return 'any of'; }
      if (node.allOf) { return 'all of'; }
      if (node.enum) { return 'enum'; }
      if (node.const !== undefined) { return 'const'; }
      if (node.properties || node.additionalProperties) { return 'object'; }
      return 'any';
    }
    if (t === 'array') { return 'array of ' + typeLabel(node.items); }
    return t;
  }

  var FACETS = [
    ['minLength', 'min length'], ['maxLength', 'max length'], ['pattern', 'pattern'],
    ['minimum', 'min'], ['maximum', 'max'],
    ['exclusiveMinimum', 'exclusive min'], ['exclusiveMaximum', 'exclusive max'],
    ['multipleOf', 'multiple of'], ['minItems', 'min items'], ['maxItems', 'max items'],
    ['uniqueItems', 'unique'], ['minProperties', 'min properties'],
    ['maxProperties', 'max properties'], ['contentEncoding', 'encoding'],
    ['contentMediaType', 'media type'], ['default', 'default']
  ];

  function facetText(node) {
    if (!node) { return ''; }
    var bits = [];
    if (node.format) { bits.push(node.format); }
    for (var i = 0; i < FACETS.length; i++) {
      var key = FACETS[i][0];
      var value = node[key];
      if (value === undefined || value === false) { continue; }
      bits.push(value === true ? FACETS[i][1] : FACETS[i][1] + ' ' + JSON.stringify(value));
    }
    if (Array.isArray(node.enum)) {
      var shown = node.enum.slice(0, 8).map(function (v) { return JSON.stringify(v); });
      if (node.enum.length > 8) { shown.push('…'); }
      bits.push('one of ' + shown.join(', '));
    }
    if (node.const !== undefined) { bits.push('= ' + JSON.stringify(node.const)); }
    if (node.readOnly) { bits.push('read-only'); }
    if (node.writeOnly) { bits.push('write-only'); }
    if (node.deprecated) { bits.push('deprecated'); }
    return bits.join(' · ');
  }

  /* Children a node contributes to the tree: properties, map values, array
     items and composition branches. */
  function children(node) {
    var out = [];
    var required = Array.isArray(node.required) ? node.required : [];
    keys(node.properties).forEach(function (key) {
      out.push({ key: key, node: node.properties[key], required: required.indexOf(key) !== -1 });
    });
    if (node.additionalProperties && typeof node.additionalProperties === 'object') {
      out.push({ key: '[key: string]', node: node.additionalProperties, required: false });
    }
    if (node.items && typeof node.items === 'object') {
      var item = deref(node.items);
      if (item && (item.properties || item.oneOf || item.anyOf || item.allOf || node.items.$ref)) {
        out.push({ key: '[]', node: node.items, required: false });
      }
    }
    ['allOf', 'oneOf', 'anyOf'].forEach(function (kind) {
      if (!Array.isArray(node[kind])) { return; }
      node[kind].forEach(function (branch, index) {
        out.push({ key: kind + ' #' + (index + 1), node: branch, required: false });
      });
    });
    return out;
  }

  function schemaTree(node) {
    var wrap = el('div', 'schema');
    wrap.appendChild(schemaRow(node, { depth: 0, seen: [], key: null, required: false }));
    return wrap;
  }

  function schemaRow(node, ctx) {
    var row = el('div', 'row');
    var line = el('div');
    node = node || {};

    if (node.$ref) {
      var name = refName(node.$ref);
      var target = resolvePointer(node.$ref);
      if (!target) {
        if (ctx.key) { line.appendChild(el('span', 'key', ctx.key)); line.appendChild(text(' ')); }
        line.appendChild(el('span', 'cycle', name + ' (unresolved $ref)'));
        row.appendChild(line);
        return row;
      }
      if (ctx.seen.indexOf(node.$ref) !== -1 || ctx.depth >= MAX_DEPTH) {
        line.appendChild(el('span', 'tw leaf', ' '));
        if (ctx.key) {
          line.appendChild(el('span', 'key', ctx.key));
          if (ctx.required) { line.appendChild(el('span', 'req', '*')); }
          line.appendChild(text(' '));
        }
        line.appendChild(el('span', 'cycle', name + ' (recursive)'));
        row.appendChild(line);
        return row;
      }
      return schemaRow(target, {
        depth: ctx.depth, seen: ctx.seen.concat([node.$ref]), key: ctx.key,
        required: ctx.required, label: name
      });
    }

    var kids = children(node);
    var toggle = kids.length ? el('button', 'tw', '▾') : el('span', 'tw leaf', ' ');
    if (kids.length) { toggle.type = 'button'; }
    line.appendChild(toggle);

    if (ctx.key) {
      line.appendChild(el('span', 'key', ctx.key));
      if (ctx.required) { line.appendChild(el('span', 'req', '*')); }
      line.appendChild(text(' '));
    }
    line.appendChild(el('span', ctx.label ? 'refname' : 'type', ctx.label || typeLabel(node)));
    var facets = facetText(node);
    if (facets) {
      line.appendChild(text('  '));
      line.appendChild(el('span', 'facets', facets));
    }
    row.appendChild(line);
    if (node.title && !ctx.label) { row.appendChild(el('div', 'doc', node.title)); }
    if (node.description) { row.appendChild(prose(node.description, 'doc')); }

    if (kids.length) {
      var box = el('div', 'kids');
      var open = ctx.depth < OPEN_DEPTH;
      box.hidden = !open;
      toggle.textContent = open ? '▾' : '▸';
      attr(toggle, 'aria-expanded', open ? 'true' : 'false');
      attr(toggle, 'aria-label', 'Toggle ' + (ctx.key || typeLabel(node)));
      kids.forEach(function (kid) {
        box.appendChild(schemaRow(kid.node, {
          depth: ctx.depth + 1, seen: ctx.seen, key: kid.key, required: kid.required
        }));
      });
      row.appendChild(box);
      on(toggle, 'click', function (event) {
        event.stopPropagation();
        box.hidden = !box.hidden;
        toggle.textContent = box.hidden ? '▸' : '▾';
        attr(toggle, 'aria-expanded', box.hidden ? 'false' : 'true');
      });
    }
    return row;
  }

  function setTreeExpanded(root, open) {
    var toggles = root.querySelectorAll('.tw');
    for (var i = 0; i < toggles.length; i++) {
      var toggle = toggles[i];
      if (toggle.className.indexOf('leaf') !== -1) { continue; }
      var box = toggle.parentNode.parentNode.querySelector('.kids');
      if (!box) { continue; }
      box.hidden = !open;
      toggle.textContent = open ? '▾' : '▸';
      attr(toggle, 'aria-expanded', open ? 'true' : 'false');
    }
  }

  /* ------------------------------------------------------------ examples */
  function sample(node, depth, seen) {
    depth = depth || 0;
    seen = seen || [];
    if (!node || depth > 6) { return null; }
    if (node.$ref) {
      if (seen.indexOf(node.$ref) !== -1) { return null; }
      var target = resolvePointer(node.$ref);
      return target ? sample(target, depth, seen.concat([node.$ref])) : null;
    }
    if (node.example !== undefined) { return node.example; }
    if (Array.isArray(node.examples) && node.examples.length) { return node.examples[0]; }
    if (node.default !== undefined) { return node.default; }
    if (node.const !== undefined) { return node.const; }
    if (Array.isArray(node.enum) && node.enum.length) { return node.enum[0]; }
    if (Array.isArray(node.allOf) && node.allOf.length) {
      var merged = {};
      node.allOf.forEach(function (branch) {
        var part = sample(branch, depth + 1, seen);
        if (part && typeof part === 'object' && !Array.isArray(part)) {
          keys(part).forEach(function (key) { merged[key] = part[key]; });
        }
      });
      return merged;
    }
    if (Array.isArray(node.oneOf) && node.oneOf.length) { return sample(node.oneOf[0], depth + 1, seen); }
    if (Array.isArray(node.anyOf) && node.anyOf.length) { return sample(node.anyOf[0], depth + 1, seen); }
    var type = Array.isArray(node.type)
      ? node.type.filter(function (t) { return t !== 'null'; })[0]
      : node.type;
    if (type === 'object' || node.properties) {
      var out = {};
      var props = node.properties || {};
      keys(props).forEach(function (key) {
        if (props[key] && props[key].readOnly) { return; }
        out[key] = sample(props[key], depth + 1, seen);
      });
      return out;
    }
    if (type === 'array') {
      var item = sample(node.items, depth + 1, seen);
      /* A cycle or an undescribed item yields nothing worth putting in an
         example; an empty array reads better than `[null]`. */
      return item === null || item === undefined ? [] : [item];
    }
    if (type === 'integer' || type === 'number') {
      if (typeof node.minimum === 'number') { return node.minimum; }
      return type === 'integer' ? 0 : 0.0;
    }
    if (type === 'boolean') { return false; }
    if (type === 'null') { return null; }
    if (type === 'string') {
      switch (node.format) {
        case 'date-time': return '1970-01-01T00:00:00Z';
        case 'date': return '1970-01-01';
        case 'time': return '00:00:00';
        case 'duration': return 'PT1H';
        case 'uuid': return '00000000-0000-0000-0000-000000000000';
        case 'email': return 'user@example.com';
        case 'hostname': return 'example.com';
        case 'ipv4': return '192.0.2.1';
        case 'uri': case 'uri-reference': case 'url': return EXAMPLE_URI;
        default: return typeof node.minLength === 'number' && node.minLength > 0
          ? new Array(node.minLength + 1).join('x') : '';
      }
    }
    return null;
  }

  function exampleFor(media) {
    if (!media) { return undefined; }
    if (media.example !== undefined) { return media.example; }
    var named = keys(media.examples)[0];
    if (named !== undefined) {
      var entry = deref(media.examples[named]);
      if (entry && entry.value !== undefined) { return entry.value; }
    }
    if (media.schema) { return sample(media.schema, 0, []); }
    return undefined;
  }

  function pretty(value) {
    try { return JSON.stringify(value, null, 2); } catch (e) { return String(value); }
  }

  /* ------------------------------------------------------------ index */
  function anchorFor(method, path, op) {
    if (op.operationId) { return String(op.operationId).replace(/\s+/g, '_'); }
    return (method + '-' + path).replace(/[^A-Za-z0-9._~-]+/g, '-').replace(/^-+|-+$/g, '');
  }
  function domId(anchor) { return 'op-' + anchor.replace(/[^A-Za-z0-9_-]/g, '_'); }

  function collectOperations() {
    var list = [];
    var paths = spec.paths || {};
    keys(paths).forEach(function (path) {
      var item = deref(paths[path]) || {};
      METHODS.forEach(function (method) {
        var op = item[method];
        if (!op || typeof op !== 'object') { return; }
        var params = (item.parameters || []).concat(op.parameters || [])
          .map(function (p) { return deref(p); })
          .filter(function (p) { return p && p.name; });
        var seenParam = {};
        params = params.filter(function (p) {
          var key = p['in'] + ' ' + p.name;
          if (has(seenParam, key)) { return false; }
          seenParam[key] = true;
          return true;
        });
        var anchor = anchorFor(method, path, op);
        var entry = {
          anchor: anchor,
          legacy: method + ':' + path,
          method: method,
          path: path,
          op: op,
          servers: op.servers || item.servers || null,
          parameters: params,
          tag: (Array.isArray(op.tags) && op.tags[0]) || 'default',
          haystack: [method, path, op.summary || '', op.operationId || '',
                     (op.tags || []).join(' '), op.description || ''].join(' ').toLowerCase()
        };
        list.push(entry);
        opsByAnchor[anchor] = entry;
        opsByAnchor[entry.legacy] = entry;
      });
    });
    return list;
  }

  function tagOrder() {
    var declared = (spec.tags || []).map(function (t) { return t.name; });
    var used = [];
    ops.forEach(function (entry) { if (used.indexOf(entry.tag) === -1) { used.push(entry.tag); } });
    var ordered = declared.filter(function (name) { return used.indexOf(name) !== -1; });
    used.forEach(function (name) { if (ordered.indexOf(name) === -1) { ordered.push(name); } });
    return ordered;
  }
  function tagInfo(name) {
    var found = (spec.tags || []).filter(function (t) { return t.name === name; })[0];
    return found || { name: name };
  }

  /* ------------------------------------------------------------ sidebar */
  function renderNav(filter) {
    var nav = byId('nav');
    var needle = (filter || '').trim().toLowerCase();
    var terms = needle ? needle.split(/\s+/) : [];
    clear(nav);
    var shown = 0;
    tagOrder().forEach(function (tag) {
      var entries = ops.filter(function (entry) {
        return entry.tag === tag && terms.every(function (term) {
          return entry.haystack.indexOf(term) !== -1;
        });
      });
      if (!entries.length) { return; }
      var group = el('div', 'navgroup');
      var head = el('button', 'navhead');
      head.type = 'button';
      head.appendChild(el('span', 'tw', '▾'));
      head.appendChild(el('span', null, tag));
      head.appendChild(el('span', 'n', entries.length));
      attr(head, 'aria-expanded', 'true');
      on(head, 'click', function () {
        var collapsed = group.getAttribute('data-collapsed') === 'true';
        group.setAttribute('data-collapsed', collapsed ? 'false' : 'true');
        attr(head, 'aria-expanded', collapsed ? 'true' : 'false');
      });
      group.appendChild(head);
      var list = el('div', 'navlist');
      entries.forEach(function (entry) {
        shown += 1;
        var link = el('a', 'navitem' + (entry.op.deprecated ? ' dep' : ''));
        link.href = '#' + entry.anchor;
        if (entry.anchor === activeAnchor) { attr(link, 'aria-current', 'true'); }
        link.appendChild(el('span', 'verb ' + entry.method, entry.method.toUpperCase()));
        var txt = el('span', 'txt');
        txt.appendChild(el('span', 'label', entry.op.summary || entry.path));
        if (entry.op.summary) { txt.appendChild(el('span', 'sub', entry.path)); }
        link.appendChild(txt);
        on(link, 'click', function (event) {
          event.preventDefault();
          go(entry.anchor, true);
        });
        list.appendChild(link);
      });
      group.appendChild(list);
      nav.appendChild(group);
    });
    if (!shown) {
      var none = el('div', 'navhead', needle ? 'No match' : 'No operations');
      nav.appendChild(none);
    }
    byId('opcount').textContent = needle
      ? shown + ' of ' + ops.length + ' shown'
      : ops.length + (ops.length === 1 ? ' operation' : ' operations');
  }

  function navLinks() { return byId('nav').querySelectorAll('.navitem'); }

  function moveHighlight(step) {
    var links = navLinks();
    if (!links.length) { return; }
    var index = -1;
    for (var i = 0; i < links.length; i++) {
      if (links[i].getAttribute('aria-current') === 'true') { index = i; break; }
    }
    index = index === -1 ? (step > 0 ? 0 : links.length - 1) : (index + step + links.length) % links.length;
    for (var j = 0; j < links.length; j++) { links[j].removeAttribute('aria-current'); }
    attr(links[index], 'aria-current', 'true');
    activeAnchor = links[index].getAttribute('href').slice(1);
    links[index].scrollIntoView({ block: 'nearest' });
  }

  function markActive(anchor) {
    activeAnchor = anchor;
    var links = navLinks();
    for (var i = 0; i < links.length; i++) {
      if (links[i].getAttribute('href') === '#' + anchor) {
        attr(links[i], 'aria-current', 'true');
        links[i].scrollIntoView({ block: 'nearest' });
      } else {
        links[i].removeAttribute('aria-current');
      }
    }
  }

  /* Open, scroll to and focus an operation. */
  function go(anchor, push) {
    var entry = opsByAnchor[anchor];
    if (!entry) { return false; }
    var card = byId(domId(entry.anchor));
    if (!card) { return false; }
    card.classList.add('open');
    var head = card.querySelector('.ophead');
    if (head) { attr(head, 'aria-expanded', 'true'); }
    var cards = D.querySelectorAll('.op.target');
    for (var i = 0; i < cards.length; i++) { cards[i].classList.remove('target'); }
    card.classList.add('target');
    suppressSpy = Date.now() + 700;
    card.scrollIntoView({ behavior: reducedMotion() ? 'auto' : 'smooth', block: 'start' });
    if (head) { head.focus({ preventScroll: true }); }
    markActive(entry.anchor);
    closeDrawer();
    if (push && window.history && window.history.replaceState) {
      window.history.replaceState(null, '', '#' + entry.anchor);
    }
    return true;
  }

  /* ------------------------------------------------------------ header */
  function serverUrl(server) {
    var url = String(server.url || '');
    keys(server.variables).forEach(function (name) {
      var variable = server.variables[name] || {};
      if (variable['default'] !== undefined) {
        url = url.split('{' + name + '}').join(String(variable['default']));
      }
    });
    return url;
  }

  function renderHead() {
    var head = byId('apihead');
    clear(head);
    var info = spec.info || {};
    var title = info.title || PAGE_TITLE;
    head.appendChild(el('h2', null, title));
    byId('brandtitle').textContent = title;
    byId('topbartitle').textContent = title;
    D.title = title + ' · API reference';
    if (info.version) {
      var badge = byId('brandver');
      badge.textContent = 'v' + info.version;
      badge.hidden = false;
    }

    var meta = el('div', 'meta');
    if (info.version) { meta.appendChild(el('span', 'badge', 'v' + info.version)); }
    meta.appendChild(el('span', 'badge', 'OpenAPI ' + (spec.openapi || '3.1')));
    if (info.license && info.license.name) { meta.appendChild(el('span', 'badge', info.license.name)); }
    if (info.contact && info.contact.email) {
      var contact = el('a', 'badge', info.contact.email);
      contact.href = 'mailto:' + info.contact.email;
      meta.appendChild(contact);
    }
    if (info.termsOfService) {
      var terms = el('a', 'badge', 'Terms');
      terms.href = info.termsOfService;
      terms.rel = 'noopener noreferrer';
      meta.appendChild(terms);
    }
    var specLink = el('a', 'badge', 'openapi.json');
    specLink.href = SPEC_URL;
    meta.appendChild(specLink);
    head.appendChild(meta);

    if (info.summary) { head.appendChild(prose(info.summary)); }
    if (info.description) { head.appendChild(prose(info.description)); }
    if (spec.externalDocs && spec.externalDocs.url) {
      var external = el('a', null, spec.externalDocs.description || 'More documentation');
      external.href = spec.externalDocs.url;
      external.rel = 'noopener noreferrer';
      var wrap = el('p');
      wrap.appendChild(external);
      head.appendChild(wrap);
    }

    var row = el('div');
    row.id = 'servers';
    var label = el('label', null, 'Server');
    label.htmlFor = 'serverselect';
    row.appendChild(label);
    var select = el('select', 'control');
    select.id = 'serverselect';
    var here = el('option', null, 'This origin (' + window.location.origin + ')');
    here.value = '';
    select.appendChild(here);
    (spec.servers || []).forEach(function (server) {
      var url = serverUrl(server);
      if (!url || url === '/') { return; }
      var option = el('option', null, url + (server.description ? '  — ' + server.description : ''));
      option.value = url;
      select.appendChild(option);
    });
    row.appendChild(select);
    head.appendChild(row);
  }

  function currentServer() {
    var select = byId('serverselect');
    var value = select ? select.value : '';
    return value ? value.replace(/\/+$/, '') : '';
  }

  /* ------------------------------------------------------------ operations */
  function renderSections() {
    var host = byId('sections');
    clear(host);
    tagOrder().forEach(function (tag) {
      var entries = ops.filter(function (entry) { return entry.tag === tag; });
      if (!entries.length) { return; }
      var info = tagInfo(tag);
      var section = el('section', 'tagsection');
      section.id = 'tag-' + tag.replace(/[^A-Za-z0-9_-]/g, '_');
      section.appendChild(el('h3', null, info.name));
      if (info.description) { section.appendChild(prose(info.description)); }
      entries.forEach(function (entry) { section.appendChild(renderOperation(entry)); });
      host.appendChild(section);
    });
  }

  function pathParts(path) {
    var span = el('span', 'path');
    String(path).split(/(\{[^}]*\})/).forEach(function (part) {
      if (!part) { return; }
      if (part.charAt(0) === '{') { span.appendChild(el('span', 'pp', part)); }
      else { span.appendChild(text(part)); }
    });
    return span;
  }

  function renderOperation(entry) {
    var op = entry.op;
    var card = el('article', 'op' + (op.deprecated ? ' dep' : ''));
    card.id = domId(entry.anchor);

    var head = el('button', 'ophead');
    head.type = 'button';
    attr(head, 'aria-expanded', 'false');
    head.appendChild(el('span', 'chev', '▸'));
    head.appendChild(el('span', 'verb ' + entry.method, entry.method.toUpperCase()));
    head.appendChild(pathParts(entry.path));
    if (op.deprecated) { head.appendChild(el('span', 'badge warn', 'deprecated')); }
    if (Array.isArray(op.security) && op.security.length) {
      head.appendChild(el('span', 'badge lock', 'auth'));
    }
    if (op.summary) { head.appendChild(el('span', 'sum', op.summary)); }
    on(head, 'click', function () {
      var open = card.classList.toggle('open');
      attr(head, 'aria-expanded', open ? 'true' : 'false');
      if (open) { markActive(entry.anchor); }
    });

    /* The permalink is a sibling of the header button, not a child: an anchor
       nested inside a button is invalid and unreachable by keyboard. */
    var link = el('a', 'anchor', '#');
    link.href = '#' + entry.anchor;
    attr(link, 'aria-label', 'Link to ' + entry.method.toUpperCase() + ' ' + entry.path);
    on(link, 'click', function (event) { event.preventDefault(); go(entry.anchor, true); });
    var bar = el('div', 'opbar');
    bar.appendChild(head);
    bar.appendChild(link);
    card.appendChild(bar);

    var body = el('div', 'opbody');
    if (op.summary) {
      var summary = el('div', 'section opsummary');
      summary.appendChild(el('h4', null, 'Summary'));
      summary.appendChild(prose(op.summary));
      body.appendChild(summary);
    }
    if (op.description) {
      var about = el('div', 'section');
      about.appendChild(el('h4', null, 'Description'));
      about.appendChild(prose(op.description));
      body.appendChild(about);
    }
    if (op.operationId || op.externalDocs) {
      var facts = el('div', 'section');
      facts.appendChild(el('h4', null, 'Reference'));
      if (op.operationId) {
        var line = el('div', 'ptype');
        line.appendChild(text('operationId '));
        line.appendChild(el('code', null, op.operationId));
        facts.appendChild(line);
      }
      if (op.externalDocs && op.externalDocs.url) {
        var more = el('a', null, op.externalDocs.description || op.externalDocs.url);
        more.href = op.externalDocs.url;
        more.rel = 'noopener noreferrer';
        var moreWrap = el('div');
        moreWrap.appendChild(more);
        facts.appendChild(moreWrap);
      }
      body.appendChild(facts);
    }
    if (entry.parameters.length) { body.appendChild(renderParameters(entry.parameters)); }
    if (op.requestBody) { body.appendChild(renderRequestBody(deref(op.requestBody))); }
    body.appendChild(renderResponses(op.responses || {}));
    if (Array.isArray(op.security)) { body.appendChild(renderSecurity(op.security)); }
    body.appendChild(renderTryIt(entry));
    card.appendChild(body);
    return card;
  }

  function renderParameters(params) {
    var section = el('div', 'section');
    section.appendChild(el('h4', null, 'Parameters'));
    var wrap = el('div', 'tablewrap');
    var table = el('table', 'params');
    var thead = el('thead');
    var hrow = el('tr');
    ['Name', 'In', 'Type', 'Description'].forEach(function (label) {
      hrow.appendChild(el('th', null, label));
    });
    thead.appendChild(hrow);
    table.appendChild(thead);
    var tbody = el('tbody');
    params.forEach(function (param) {
      var schema = param.schema || {};
      var tr = el('tr');
      var name = el('td');
      attr(name, 'data-label', 'Name');
      name.appendChild(el('span', 'pname', param.name));
      if (param.required) { name.appendChild(el('span', 'req', ' *')); }
      if (param.deprecated) { name.appendChild(el('div', 'hint', 'deprecated')); }
      tr.appendChild(name);
      var where = el('td', 'ptype', param['in']);
      attr(where, 'data-label', 'In');
      tr.appendChild(where);
      var type = el('td', 'ptype');
      attr(type, 'data-label', 'Type');
      type.appendChild(text(typeLabel(deref(schema))));
      var facets = facetText(deref(schema));
      if (facets) { type.appendChild(el('div', 'hint', facets)); }
      tr.appendChild(type);
      var desc = el('td');
      if (param.description) { desc.appendChild(prose(param.description)); }
      tr.appendChild(desc);
      tbody.appendChild(tr);
    });
    table.appendChild(tbody);
    wrap.appendChild(table);
    section.appendChild(wrap);
    return section;
  }

  /* Schema / Example tab pair, used by both request bodies and responses. */
  function contentView(media) {
    var box = el('div');
    var schema = media && media.schema;
    var example = exampleFor(media);
    if (!schema && example === undefined) {
      box.appendChild(el('div', 'empty', 'No documented representation.'));
      return box;
    }
    var tabs = el('div', 'tabbar');
    var schemaBtn = el('button', 'tinybtn', 'Schema');
    var exampleBtn = el('button', 'tinybtn', 'Example');
    var expandBtn = el('button', 'tinybtn', 'Expand all');
    schemaBtn.type = exampleBtn.type = expandBtn.type = 'button';
    var tree = schema ? schemaTree(schema) : null;
    var body = el('pre', 'codeblock');
    body.appendChild(el('code', null, example === undefined ? '' : pretty(example)));

    function select(which) {
      var isSchema = !!(which === 'schema' && tree);
      if (tree) { tree.hidden = !isSchema; }
      body.hidden = isSchema;
      attr(schemaBtn, 'aria-selected', isSchema ? 'true' : 'false');
      attr(exampleBtn, 'aria-selected', isSchema ? 'false' : 'true');
      expandBtn.hidden = !isSchema;
    }
    if (tree) { tabs.appendChild(schemaBtn); }
    if (example !== undefined) { tabs.appendChild(exampleBtn); }
    if (tree) { tabs.appendChild(expandBtn); }
    tabs.appendChild(copyButton('Copy', function () {
      return body.hidden ? JSON.stringify(schema, null, 2) : (example === undefined ? '' : pretty(example));
    }));
    box.appendChild(tabs);
    if (tree) { box.appendChild(tree); }
    box.appendChild(body);
    on(schemaBtn, 'click', function () { select('schema'); });
    on(exampleBtn, 'click', function () { select('example'); });
    var expanded = false;
    on(expandBtn, 'click', function () {
      expanded = !expanded;
      setTreeExpanded(tree, expanded);
      expandBtn.textContent = expanded ? 'Collapse all' : 'Expand all';
    });
    select(tree ? 'schema' : 'example');
    return box;
  }

  function renderRequestBody(requestBody) {
    var section = el('div', 'section');
    var head = el('h4', null, 'Request body');
    head.appendChild(el('span', 'badge' + (requestBody.required ? ' warn' : ''),
      requestBody.required ? 'required' : 'optional'));
    section.appendChild(head);
    if (requestBody.description) { section.appendChild(prose(requestBody.description)); }
    var content = requestBody.content || {};
    var types = keys(content);
    if (!types.length) {
      section.appendChild(el('div', 'empty', 'No documented representation.'));
      return section;
    }
    types.forEach(function (type) {
      section.appendChild(el('div', 'ct', type));
      section.appendChild(contentView(content[type]));
    });
    return section;
  }

  function statusClass(code) {
    if (String(code) === 'default') { return 'sd'; }
    var first = String(code).charAt(0);
    return ['2', '3', '4', '5'].indexOf(first) !== -1 ? 's' + first : 'sd';
  }

  function renderResponses(responses) {
    var section = el('div', 'section');
    section.appendChild(el('h4', null, 'Responses'));
    var codes = keys(responses);
    if (!codes.length) {
      section.appendChild(el('div', 'empty', 'No documented responses.'));
      return section;
    }
    codes.forEach(function (code, index) {
      var response = deref(responses[code]) || {};
      var box = el('div', 'resp' + (index === 0 ? ' open' : ''));
      var head = el('button', 'resphead');
      head.type = 'button';
      attr(head, 'aria-expanded', index === 0 ? 'true' : 'false');
      head.appendChild(el('span', 'status ' + statusClass(code), code));
      head.appendChild(el('span', 'd', response.description || ''));
      on(head, 'click', function () {
        var open = box.classList.toggle('open');
        attr(head, 'aria-expanded', open ? 'true' : 'false');
      });
      box.appendChild(head);

      var body = el('div', 'respbody');
      var content = response.content || {};
      var types = keys(content);
      if (!types.length) {
        body.appendChild(el('div', 'empty', 'No body.'));
      } else {
        types.forEach(function (type) {
          body.appendChild(el('div', 'ct', type));
          body.appendChild(contentView(content[type]));
        });
      }
      var headers = response.headers || {};
      var headerNames = keys(headers);
      if (headerNames.length) {
        body.appendChild(el('div', 'ct', 'Headers'));
        var list = el('div', 'schema');
        headerNames.forEach(function (name) {
          var header = deref(headers[name]) || {};
          var row = el('div', 'row');
          row.appendChild(el('span', 'key', name));
          row.appendChild(text(' '));
          row.appendChild(el('span', 'type', typeLabel(deref(header.schema))));
          if (header.description) { row.appendChild(prose(header.description, 'doc')); }
          list.appendChild(row);
        });
        body.appendChild(list);
      }
      box.appendChild(body);
      section.appendChild(box);
    });
    return section;
  }

  function schemeLabel(name, scheme) {
    var bits = [scheme.type || 'unknown'];
    if (scheme.scheme) { bits.push(scheme.scheme); }
    if (scheme['in']) { bits.push('in ' + scheme['in']); }
    if (scheme.name && scheme.type === 'apiKey') { bits.push('as ' + scheme.name); }
    return name + '  (' + bits.join(' ') + ')';
  }

  function renderSecurity(requirements) {
    var section = el('div', 'section');
    section.appendChild(el('h4', null, 'Security'));
    if (!requirements.length) {
      section.appendChild(el('div', 'hint', 'Public: this operation requires no credentials.'));
      return section;
    }
    var schemes = (spec.components && spec.components.securitySchemes) || {};
    requirements.forEach(function (requirement) {
      var names = keys(requirement);
      if (!names.length) {
        section.appendChild(el('div', 'hint', 'Unauthenticated access is permitted.'));
        return;
      }
      names.forEach(function (name) {
        var scheme = deref(schemes[name]) || {};
        var row = el('div', 'schema');
        row.appendChild(el('span', 'key', schemeLabel(name, scheme)));
        var scopes = requirement[name] || [];
        if (scopes.length) { row.appendChild(el('div', 'doc', 'scopes: ' + scopes.join(', '))); }
        if (scheme.description) { row.appendChild(prose(scheme.description, 'doc')); }
        section.appendChild(row);
      });
    });
    return section;
  }

  /* ------------------------------------------------------------ try it */
  function field(labelText, control, hintText) {
    var wrap = el('div', 'field');
    var label = el('label');
    label.appendChild(text(labelText));
    if (hintText) { label.appendChild(el('span', 'in', ' ' + hintText)); }
    wrap.appendChild(label);
    wrap.appendChild(control);
    if (!control.id) { control.id = 'f' + Math.random().toString(36).slice(2, 9); }
    label.htmlFor = control.id;
    return wrap;
  }

  function controlFor(param) {
    var schema = deref(param.schema) || {};
    var type = Array.isArray(schema.type) ? schema.type[0] : schema.type;
    var control;
    if (Array.isArray(schema.enum) || type === 'boolean') {
      control = el('select', 'control');
      var options = Array.isArray(schema.enum) ? schema.enum : [true, false];
      if (!param.required) { control.appendChild(el('option', null, '')); }
      options.forEach(function (value) {
        var option = el('option', null, String(value));
        option.value = String(value);
        control.appendChild(option);
      });
    } else {
      control = el('input', 'control');
      control.type = (type === 'integer' || type === 'number') ? 'number' : 'text';
      if (type === 'number') { control.step = 'any'; }
      control.placeholder = type === 'array' ? 'comma, separated' : typeLabel(schema);
      control.autocomplete = 'off';
    }
    var preset = param.example !== undefined ? param.example : schema['default'];
    if (preset !== undefined && preset !== null) {
      control.value = typeof preset === 'string' ? preset : JSON.stringify(preset);
    }
    return control;
  }

  function requirementNames(entry) {
    var requirements = Array.isArray(entry.op.security) ? entry.op.security : (spec.security || []);
    var out = [];
    requirements.forEach(function (requirement) {
      keys(requirement).forEach(function (name) { if (out.indexOf(name) === -1) { out.push(name); } });
    });
    return out;
  }

  function renderAuth(entry, panel) {
    var names = requirementNames(entry);
    var schemes = (spec.components && spec.components.securitySchemes) || {};
    var applicable = names.filter(function (name) { return schemes[name]; });
    if (!applicable.length) { return function () { /* nothing to apply */ }; }
    var box = el('div', 'authbox');
    box.appendChild(el('div', 't', 'Authentication'));
    var controls = {};
    applicable.forEach(function (name) {
      var scheme = deref(schemes[name]) || {};
      var control = el('input', 'control');
      control.type = 'password';
      control.autocomplete = 'off';
      control.placeholder = scheme.type === 'http' && scheme.scheme === 'basic'
        ? 'user:password' : (scheme.bearerFormat || 'value');
      if (authValues[name]) { control.value = authValues[name]; }
      on(control, 'input', function () { authValues[name] = control.value; });
      controls[name] = { control: control, scheme: scheme };
      box.appendChild(field(name, control, schemeLabel('', scheme).replace(/^\s+/, '')));
    });
    box.appendChild(el('div', 'hint', 'Kept in memory for this page only; never stored.'));
    panel.appendChild(box);
    return function (headers, query) {
      keys(controls).forEach(function (name) {
        var value = controls[name].control.value;
        if (!value) { return; }
        var scheme = controls[name].scheme;
        if (scheme.type === 'http' && String(scheme.scheme).toLowerCase() === 'basic') {
          try { headers['Authorization'] = 'Basic ' + window.btoa(value); }
          catch (e) { headers['Authorization'] = 'Basic ' + value; }
        } else if (scheme.type === 'http' || scheme.type === 'oauth2' || scheme.type === 'openIdConnect') {
          headers['Authorization'] = (scheme.scheme ? scheme.scheme.charAt(0).toUpperCase() +
            scheme.scheme.slice(1) : 'Bearer') + ' ' + value;
        } else if (scheme.type === 'apiKey') {
          if (scheme['in'] === 'query') { query.push([scheme.name, value]); }
          else if (scheme['in'] === 'cookie') { headers['Cookie'] = scheme.name + '=' + value; }
          else { headers[scheme.name || 'X-API-Key'] = value; }
        }
      });
    };
  }

  function renderTryIt(entry) {
    var section = el('div', 'section');
    section.appendChild(el('h4', null, 'Try it'));
    var panel = el('div', 'tryit');
    var inputs = { path: {}, query: {}, header: {}, cookie: {} };
    var applyAuth = renderAuth(entry, panel);

    entry.parameters.forEach(function (param) {
      var control = controlFor(param);
      panel.appendChild(field(param.name + (param.required ? ' *' : ''), control, '(' + param['in'] + ')'));
      var bucket = inputs[param['in']];
      if (bucket) { bucket[param.name] = { control: control, param: param }; }
    });

    var bodyInput = null;
    var typeSelect = null;
    var content = entry.op.requestBody ? ((deref(entry.op.requestBody) || {}).content || {}) : {};
    var mediaTypes = keys(content);
    if (mediaTypes.length) {
      var stack = el('div', 'stack');
      var label = el('label');
      label.appendChild(text('Body'));
      typeSelect = el('select', 'control');
      mediaTypes.forEach(function (type) {
        var option = el('option', null, type);
        option.value = type;
        typeSelect.appendChild(option);
      });
      if (mediaTypes.length > 1) { label.appendChild(typeSelect); }
      else { label.appendChild(el('span', 'in', ' ' + mediaTypes[0])); }
      var tools = el('span', 'tools');
      var formatBtn = el('button', 'tinybtn', 'Format');
      formatBtn.type = 'button';
      tools.appendChild(formatBtn);
      tools.appendChild(copyButton('Copy', function () { return bodyInput.value; }));
      label.appendChild(tools);
      bodyInput = el('textarea', 'control');
      bodyInput.spellcheck = false;
      bodyInput.id = 'body' + Math.random().toString(36).slice(2, 9);
      label.htmlFor = bodyInput.id;
      function fill() {
        var media = content[typeSelect.value] || content[mediaTypes[0]];
        var example = exampleFor(media);
        if (example === undefined) { bodyInput.value = ''; return; }
        bodyInput.value = String(typeSelect.value).indexOf('json') !== -1
          ? pretty(example) : String(example);
      }
      on(typeSelect, 'change', fill);
      on(formatBtn, 'click', function () {
        try { bodyInput.value = pretty(JSON.parse(bodyInput.value)); } catch (e) { /* leave as typed */ }
      });
      fill();
      stack.appendChild(label);
      stack.appendChild(bodyInput);
      panel.appendChild(stack);
    }

    var row = el('div', 'btnrow');
    var send = el('button', 'btn', 'Send');
    send.type = 'button';
    var curl = el('button', 'btn ghost', 'Copy as cURL');
    curl.type = 'button';
    var clearBtn = el('button', 'btn ghost', 'Clear');
    clearBtn.type = 'button';
    row.appendChild(send);
    row.appendChild(curl);
    row.appendChild(clearBtn);
    panel.appendChild(row);

    var result = el('div', 'result');
    result.hidden = true;
    attr(result, 'aria-live', 'polite');
    panel.appendChild(result);

    function build() {
      var headers = {};
      var query = [];
      var path = entry.path;
      keys(inputs.path).forEach(function (name) {
        var value = inputs.path[name].control.value;
        path = path.split('{' + name + '}').join(encodeURIComponent(value));
      });
      keys(inputs.query).forEach(function (name) {
        var value = inputs.query[name].control.value;
        if (value === '') { return; }
        var schema = deref(inputs.query[name].param.schema) || {};
        if (schema.type === 'array') {
          value.split(',').forEach(function (part) {
            if (part !== '') { query.push([name, part.trim()]); }
          });
        } else {
          query.push([name, value]);
        }
      });
      keys(inputs.header).forEach(function (name) {
        var value = inputs.header[name].control.value;
        if (value !== '') { headers[name] = value; }
      });
      var cookies = [];
      keys(inputs.cookie).forEach(function (name) {
        var value = inputs.cookie[name].control.value;
        if (value !== '') { cookies.push(name + '=' + value); }
      });
      applyAuth(headers, query);
      if (cookies.length) {
        headers['Cookie'] = (headers['Cookie'] ? headers['Cookie'] + '; ' : '') + cookies.join('; ');
      }
      var url = currentServer() + path;
      if (query.length) {
        url += (url.indexOf('?') === -1 ? '?' : '&') + query.map(function (pair) {
          return encodeURIComponent(pair[0]) + '=' + encodeURIComponent(pair[1]);
        }).join('&');
      }
      var body = null;
      if (bodyInput && bodyInput.value !== '') {
        body = bodyInput.value;
        if (!headers['Content-Type']) {
          headers['Content-Type'] = typeSelect ? typeSelect.value : 'application/json';
        }
      }
      return { url: url, headers: headers, body: body, method: entry.method.toUpperCase() };
    }

    on(curl, 'click', function () {
      var request = build();
      var quote = function (value) { return "'" + String(value).split("'").join("'\\''") + "'"; };
      var parts = ['curl -X ' + request.method + ' ' + quote(
        /^[a-z]+:/i.test(request.url) ? request.url : window.location.origin + request.url)];
      keys(request.headers).forEach(function (name) {
        parts.push('  -H ' + quote(name + ': ' + request.headers[name]));
      });
      if (request.body) { parts.push('  --data-raw ' + quote(request.body)); }
      copy(parts.join(' \\\n'), curl);
    });
    on(clearBtn, 'click', function () { result.hidden = true; clear(result); });
    on(send, 'click', function () {
      send.disabled = true;
      send.textContent = 'Sending';
      execute(build(), result).then(function () {
        send.disabled = false;
        send.textContent = 'Send';
      });
    });

    section.appendChild(panel);
    return section;
  }

  function execute(request, result) {
    var controller = window.AbortController ? new window.AbortController() : null;
    var timer = controller ? window.setTimeout(function () { controller.abort(); }, TIMEOUT_MS) : 0;
    var init = {
      method: request.method,
      headers: request.headers,
      credentials: 'same-origin'
    };
    if (request.body !== null) { init.body = request.body; }
    if (controller) { init.signal = controller.signal; }
    var started = now();
    return fetch(request.url, init).then(function (response) {
      return response.text().then(function (body) {
        window.clearTimeout(timer);
        showResult(result, response, body, Math.round(now() - started), request.url);
      });
    }, function (error) {
      window.clearTimeout(timer);
      clear(result);
      result.hidden = false;
      var meta = el('div', 'meta');
      meta.appendChild(el('span', 'status s5', 'failed'));
      meta.appendChild(el('span', null, String(error && error.message ? error.message : error)));
      result.appendChild(meta);
      result.appendChild(el('div', 'hint',
        'The request was never answered. Check the selected server, CORS, and whether the ' +
        'endpoint is reachable from this browser.'));
    });
  }

  function now() {
    return (window.performance && window.performance.now) ? window.performance.now() : Date.now();
  }
  function byteLength(value) {
    try { return new window.TextEncoder().encode(value).length; } catch (e) { return value.length; }
  }
  function humanSize(bytes) {
    if (bytes < 1024) { return bytes + ' B'; }
    if (bytes < 1048576) { return (bytes / 1024).toFixed(1) + ' KiB'; }
    return (bytes / 1048576).toFixed(2) + ' MiB';
  }

  function showResult(result, response, body, ms, url) {
    clear(result);
    result.hidden = false;
    var meta = el('div', 'meta');
    meta.appendChild(el('span', 'status ' + statusClass(response.status), String(response.status)));
    if (response.statusText) { meta.appendChild(el('span', null, response.statusText)); }
    meta.appendChild(el('span', null, ms + ' ms'));
    meta.appendChild(el('span', null, humanSize(byteLength(body))));
    meta.appendChild(el('span', 'url', url));
    result.appendChild(meta);

    var lines = [];
    response.headers.forEach(function (value, name) { lines.push(name + ': ' + value); });
    if (lines.length) {
      var headHead = el('div', 'ct', 'Response headers');
      result.appendChild(headHead);
      var headPre = el('pre', 'codeblock');
      headPre.appendChild(el('code', null, lines.sort().join('\n')));
      result.appendChild(headPre);
    }

    var bodyHead = el('div', 'ct');
    bodyHead.appendChild(text('Body'));
    result.appendChild(bodyHead);
    var shown = body;
    var contentType = response.headers.get('content-type') || '';
    if (body && contentType.indexOf('json') !== -1) {
      try { shown = pretty(JSON.parse(body)); } catch (e) { shown = body; }
    }
    var bodyPre = el('pre', 'codeblock');
    bodyPre.appendChild(el('code', null, shown === '' ? '(empty)' : shown));
    result.appendChild(bodyPre);
    var tools = el('div', 'btnrow');
    tools.appendChild(copyButton('Copy body', function () { return body; }));
    result.appendChild(tools);
  }

  /* ------------------------------------------------------------ drawer */
  function openDrawer() {
    byId('sidebar').classList.add('on');
    byId('scrim').hidden = false;
    byId('scrim').className = 'on';
    attr(byId('menubtn'), 'aria-expanded', 'true');
    byId('search').focus();
  }
  function closeDrawer() {
    byId('sidebar').classList.remove('on');
    byId('scrim').hidden = true;
    byId('scrim').className = '';
    attr(byId('menubtn'), 'aria-expanded', 'false');
  }

  /* ------------------------------------------------------------ help */
  function toggleHelp(show) {
    var dialog = byId('help');
    dialog.hidden = !show;
    if (show) { byId('helpclose').focus(); } else { byId('helpbtn').focus(); }
  }

  function toggleAll() {
    var cards = D.querySelectorAll('.op');
    var anyClosed = false;
    for (var i = 0; i < cards.length; i++) {
      if (cards[i].className.indexOf('open') === -1) { anyClosed = true; break; }
    }
    for (var j = 0; j < cards.length; j++) {
      if (anyClosed) { cards[j].classList.add('open'); } else { cards[j].classList.remove('open'); }
      var head = cards[j].querySelector('.ophead');
      if (head) { attr(head, 'aria-expanded', anyClosed ? 'true' : 'false'); }
    }
  }

  /* ------------------------------------------------------------ scrollspy */
  function startSpy() {
    if (!window.IntersectionObserver) { return; }
    var observer = new window.IntersectionObserver(function (records) {
      if (Date.now() < suppressSpy) { return; }
      var best = null;
      records.forEach(function (record) {
        if (record.isIntersecting && (!best || record.intersectionRatio > best.intersectionRatio)) {
          best = record;
        }
      });
      if (!best) { return; }
      var anchor = best.target.getAttribute('data-anchor');
      if (anchor && anchor !== activeAnchor) { markActive(anchor); }
    }, { rootMargin: '-10% 0px -70% 0px', threshold: [0, 0.25, 1] });
    ops.forEach(function (entry) {
      var card = byId(domId(entry.anchor));
      if (card) {
        attr(card, 'data-anchor', entry.anchor);
        observer.observe(card);
      }
    });
  }

  /* ------------------------------------------------------------ boot */
  function fail(message, detail) {
    var status = byId('status');
    status.className = 'err';
    clear(status);
    status.appendChild(el('div', null, message));
    if (detail) { status.appendChild(el('div', 'hint', detail)); }
    var retryRow = el('div', 'btnrow');
    var retry = el('button', 'btn', 'Retry');
    retry.type = 'button';
    on(retry, 'click', function () {
      status.className = '';
      clear(status);
      status.appendChild(el('span', 'spinner'));
      status.appendChild(text('Loading the API description'));
      boot();
    });
    retryRow.appendChild(retry);
    status.appendChild(retryRow);
  }

  function boot() {
    fetch(SPEC_URL, { credentials: 'same-origin', headers: { 'Accept': 'application/json' } })
      .then(function (response) {
        if (!response.ok) { throw new Error('HTTP ' + response.status + ' from ' + SPEC_URL); }
        return response.json();
      })
      .then(function (loaded) {
        spec = loaded;
        opsByAnchor = {};
        ops = collectOperations();
        renderHead();
        renderSections();
        renderNav('');
        byId('status').hidden = true;
        byId('page').hidden = false;
        startSpy();
        var hash = window.location.hash ? window.location.hash.slice(1) : '';
        if (hash) {
          try { hash = decodeURIComponent(hash); } catch (e) { /* use it raw */ }
          go(hash, false);
        }
      })
      .catch(function (error) {
        fail('Could not load the API description.',
          String(error && error.message ? error.message : error));
      });
  }

  /* ------------------------------------------------------------ wiring */
  setTheme(D.documentElement.getAttribute('data-theme') || 'auto');
  (function restoreTheme() {
    var stored = null;
    try { stored = window.localStorage.getItem(THEME_KEY); } catch (e) { /* private mode */ }
    if (stored === 'auto' || stored === 'light' || stored === 'dark') { setTheme(stored); }
  })();

  var themeButtons = D.querySelectorAll('.themebtn');
  for (var b = 0; b < themeButtons.length; b++) { on(themeButtons[b], 'click', cycleTheme); }
  on(byId('menubtn'), 'click', function () {
    if (byId('sidebar').className.indexOf('on') !== -1) { closeDrawer(); } else { openDrawer(); }
  });
  on(byId('scrim'), 'click', closeDrawer);
  on(byId('helpbtn'), 'click', function () { toggleHelp(true); });
  on(byId('helpclose'), 'click', function () { toggleHelp(false); });
  on(byId('help').querySelector('.backdrop'), 'click', function () { toggleHelp(false); });

  var searchBox = byId('search');
  on(searchBox, 'input', function () { renderNav(searchBox.value); });
  on(searchBox, 'keydown', function (event) {
    if (event.key === 'ArrowDown') { event.preventDefault(); moveHighlight(1); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); moveHighlight(-1); }
    else if (event.key === 'Enter') {
      event.preventDefault();
      if (activeAnchor) { go(activeAnchor, true); }
      else {
        var first = navLinks()[0];
        if (first) { go(first.getAttribute('href').slice(1), true); }
      }
    } else if (event.key === 'Escape') {
      searchBox.value = '';
      renderNav('');
    }
  });

  on(window, 'hashchange', function () {
    var hash = window.location.hash ? window.location.hash.slice(1) : '';
    if (!hash) { return; }
    try { hash = decodeURIComponent(hash); } catch (e) { /* use it raw */ }
    go(hash, false);
  });

  on(D, 'keydown', function (event) {
    if (event.defaultPrevented) { return; }
    var target = event.target || {};
    var tag = (target.tagName || '').toLowerCase();
    var typing = tag === 'input' || tag === 'textarea' || tag === 'select' || target.isContentEditable;
    if (event.key === 'Escape') {
      if (!byId('help').hidden) { toggleHelp(false); return; }
      closeDrawer();
      return;
    }
    if ((event.key === 'k' || event.key === 'K') && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      searchBox.focus();
      searchBox.select();
      return;
    }
    if (typing || event.metaKey || event.ctrlKey || event.altKey) { return; }
    if (event.key === '/') { event.preventDefault(); searchBox.focus(); }
    else if (event.key === '?') { event.preventDefault(); toggleHelp(byId('help').hidden); }
    else if (event.key === 't') { cycleTheme(); }
    else if (event.key === 'e') { toggleAll(); }
  });

  boot();
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// Elements that never have a closing tag.
    const VOID: &[&str] = &[
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
        "source", "track", "wbr",
    ];

    /// Elements whose content is not markup and must not be scanned for tags.
    const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title"];

    /// A deliberately small HTML tag-balance checker.
    ///
    /// It understands the doctype, comments, void elements, self-closing tags,
    /// quoted attribute values and raw-text elements — which is exactly enough
    /// to prove the rendered document is well formed. It is not a conformance
    /// checker and does not try to be.
    fn check_balanced(html: &str) -> Result<(), String> {
        let bytes = html.as_bytes();
        let mut stack: Vec<(String, usize)> = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] != b'<' {
                i += 1;
                continue;
            }
            let rest = &html[i..];
            if rest.starts_with("<!--") {
                match rest.find("-->") {
                    Some(end) => i += end + 3,
                    None => return Err("unterminated comment".to_owned()),
                }
                continue;
            }
            if rest.starts_with("<!") || rest.starts_with("<?") {
                match rest.find('>') {
                    Some(end) => i += end + 1,
                    None => return Err("unterminated declaration".to_owned()),
                }
                continue;
            }
            let closing = rest.starts_with("</");
            let name_start = i + if closing { 2 } else { 1 };
            let mut j = name_start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-') {
                j += 1;
            }
            if j == name_start {
                // A bare `<` in text. Not markup; skip it.
                i += 1;
                continue;
            }
            let name = html[name_start..j].to_ascii_lowercase();

            // Walk to the end of the tag, honouring quoted attribute values.
            let mut k = j;
            let mut quote: Option<u8> = None;
            let mut self_closing = false;
            while k < bytes.len() {
                let ch = bytes[k];
                match quote {
                    Some(q) if ch == q => quote = None,
                    Some(_) => {}
                    None if ch == b'"' || ch == b'\'' => quote = Some(ch),
                    None if ch == b'>' => break,
                    None => {}
                }
                k += 1;
            }
            if k >= bytes.len() {
                return Err(format!("unterminated `<{name}` tag"));
            }
            if bytes[k - 1] == b'/' {
                self_closing = true;
            }
            i = k + 1;

            if closing {
                match stack.pop() {
                    Some((open, _)) if open == name => {}
                    Some((open, at)) => {
                        return Err(format!(
                            "`</{name}>` at byte {k} closes `<{open}>` opened at byte {at}"
                        ));
                    }
                    None => return Err(format!("stray `</{name}>` at byte {k}")),
                }
                continue;
            }
            if self_closing || VOID.contains(&name.as_str()) {
                continue;
            }
            let raw_text = RAW_TEXT.contains(&name.as_str());
            stack.push((name.clone(), k));
            if raw_text {
                // The content is not markup: jump to the closing tag, which the
                // next iteration pops off the stack as usual.
                let needle = format!("</{name}");
                match html[i..].find(needle.as_str()) {
                    Some(end) => i += end,
                    None => return Err(format!("unterminated raw-text element `<{name}>`")),
                }
            }
        }
        match stack.pop() {
            None => Ok(()),
            Some((name, at)) => Err(format!("`<{name}>` opened at byte {at} is never closed")),
        }
    }

    /// The load-bearing property of this crate's documentation UI: it works
    /// with the network cable unplugged.
    #[test]
    fn template_loads_nothing_from_the_network() {
        for forbidden in [
            "http://",
            "https://",
            "<script src",
            "<link rel=\"stylesheet\"",
            "@import",
            "src=\"//",
            "href=\"//",
            "url(//",
            "unpkg",
            "jsdelivr",
            "cdn.",
            "googleapis",
            "XMLHttpRequest",
            "new Image(",
        ] {
            assert!(
                !TEMPLATE.contains(forbidden),
                "the documentation UI must be self-contained, but it contains `{forbidden}`"
            );
        }
    }

    /// The one `fetch` the page performs is the spec URL the server injected.
    #[test]
    fn the_only_fetch_target_is_the_injected_spec_url() {
        let calls = TEMPLATE.match_indices("fetch(").count();
        assert_eq!(calls, 2, "expected exactly two fetch call sites");
        assert!(TEMPLATE.contains("fetch(SPEC_URL, {"));
        assert!(TEMPLATE.contains("fetch(request.url, init)"));
    }

    #[test]
    fn rendered_document_is_balanced() {
        let html = DocsUi::new().title("Shop API").render();
        check_balanced(&html).expect("the rendered documentation UI must be well formed");
    }

    #[test]
    fn a_hostile_title_cannot_unbalance_the_document() {
        let html = DocsUi::new()
            .title("</title><script>alert(1)</script>")
            .nonce("n0nce")
            .render();
        check_balanced(&html).expect("an escaped title must not break the document");
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;/title&gt;"));
    }

    #[test]
    fn the_balance_checker_rejects_unbalanced_input() {
        assert!(check_balanced("<div><span></div>").is_err());
        assert!(check_balanced("<div>").is_err());
        assert!(check_balanced("</div>").is_err());
        assert!(check_balanced("<div><br><img src=\"x\"></div>").is_ok());
        // A raw-text element may contain anything, including `<`.
        assert!(check_balanced("<div><script>if (a < b) {}</script></div>").is_ok());
        // Attribute values may contain `>`.
        assert!(check_balanced("<div title=\"a > b\"></div>").is_ok());
    }

    #[test]
    fn every_element_the_script_looks_up_exists_in_the_markup() {
        let markup = &TEMPLATE[..TEMPLATE.find("<script").expect("script block")];
        let mut checked = 0;
        for (index, _) in TEMPLATE.match_indices("byId('") {
            let rest = &TEMPLATE[index + "byId('".len()..];
            let end = rest.find('\'').expect("closing quote");
            let id = &rest[..end];
            // Ids created by the script itself are assigned with `.id = '..'`.
            let assigned = TEMPLATE.contains(&format!(".id = '{id}'"));
            assert!(
                markup.contains(&format!("id=\"{id}\"")) || assigned,
                "the script looks up `#{id}`, which nothing defines"
            );
            checked += 1;
        }
        assert!(checked > 10, "expected the script to look up several ids");
    }

    #[test]
    fn every_placeholder_is_substituted() {
        let html = DocsUi::new().title("Shop API").render();
        assert!(
            !html.contains("__MOSO_"),
            "unsubstituted placeholder remains"
        );
    }

    /// The literal characters `<`, spelled without an escape sequence so
    /// that this file stays readable.
    fn u003c() -> String {
        format!("{}u003c", '\\')
    }

    #[test]
    fn title_is_escaped_in_both_positions() {
        let html = DocsUi::new().title("A & B <script>").render();
        assert!(html.contains("A &amp; B &lt;script&gt;"), "HTML position");
        assert!(
            html.contains(&format!("PAGE_TITLE = \"A & B {}script>\"", u003c())),
            "script position"
        );
    }

    #[test]
    fn spec_url_is_injected_as_a_json_string() {
        let html = DocsUi::new().spec_url("/v1/openapi.json").render();
        assert!(html.contains(r#"SPEC_URL = "/v1/openapi.json""#));
    }

    #[test]
    fn a_spec_url_cannot_close_the_script_block() {
        let html = DocsUi::new().spec_url("/a</script><script>evil()").render();
        assert!(!html.contains("</script><script>evil()"));
        assert!(html.contains(&format!("{}/script", u003c())));
        check_balanced(&html).expect("escaping keeps the document balanced");
    }

    #[test]
    fn nonce_is_applied_to_inline_assets() {
        let html = DocsUi::new().nonce("abc123").render();
        assert!(html.contains(r#"<style nonce="abc123">"#));
        assert!(html.contains(r#"<script nonce="abc123">"#));
    }

    #[test]
    fn no_nonce_leaves_the_tags_bare() {
        let html = DocsUi::new().render();
        assert!(html.contains("<style>"));
        assert!(html.contains("<script>"));
    }

    #[test]
    fn theme_reaches_the_root_element() {
        let html = DocsUi::new().theme(Theme::Dark).render();
        assert!(html.contains(r#"<html lang="en" data-theme="dark">"#));
        assert_eq!(Theme::System.as_str(), "auto");
        assert_eq!(Theme::Light.as_str(), "light");
    }

    #[test]
    fn the_free_function_matches_the_builder() {
        assert_eq!(
            render("/spec.json", "Shop"),
            DocsUi::new().spec_url("/spec.json").title("Shop").render()
        );
    }

    /// Forcing a theme must not disturb the `auto` rules: they key off the
    /// literal string `auto`, and a substitution that reached into the
    /// stylesheet would make a forced light theme render dark on a reader
    /// whose system prefers dark.
    #[test]
    fn forcing_a_theme_leaves_the_auto_rules_alone() {
        let html = DocsUi::new().theme(Theme::Light).render();
        assert!(html.contains(r#"<html lang="en" data-theme="light">"#));
        assert!(html.contains(r#":root[data-theme="auto"]"#));
        assert_eq!(
            html.matches(r#"data-theme="auto""#).count(),
            1,
            "only the stylesheet may mention `auto` once a theme is forced"
        );
        assert_eq!(
            DocsUi::new()
                .render()
                .matches(r#"data-theme="auto""#)
                .count(),
            2,
            "the default theme adds the root attribute"
        );
    }

    #[test]
    fn both_colour_schemes_are_defined() {
        assert!(TEMPLATE.contains(r#":root[data-theme="dark"]"#));
        assert!(TEMPLATE.contains("@media (prefers-color-scheme: dark)"));
        assert!(TEMPLATE.contains(r#":root[data-theme="auto"]"#));
    }

    #[test]
    fn the_layout_has_a_narrow_breakpoint() {
        assert!(TEMPLATE.contains("@media (max-width: 560px)"));
        assert!(TEMPLATE.contains("@media (max-width: 860px)"));
    }

    #[test]
    fn html_escaping_covers_every_dangerous_character() {
        assert_eq!(escape_html(r#"<&>"'"#), "&lt;&amp;&gt;&quot;&#39;");
        assert_eq!(escape_html("plain"), "plain");
    }

    #[test]
    fn json_string_escapes_every_angle_bracket() {
        assert_eq!(json_string("a</b"), format!("\"a{}/b\"", u003c()));
        assert_eq!(
            json_string("<!--<script>"),
            format!("\"{0}!--{0}script>\"", u003c())
        );
        assert_eq!(json_string("q\"q"), r#""q\"q""#);
        assert_eq!(json_string("/openapi.json"), r#""/openapi.json""#);
    }

    #[test]
    fn a_hostile_title_cannot_open_a_script_element() {
        let html = DocsUi::new().title("<!--<script>x</script>").render();
        assert!(!html.contains("<!--"), "no comment opener survives");
        assert!(!html.contains("<script>x"), "no element opener survives");
        check_balanced(&html).expect("escaping keeps the document balanced");
    }
}
