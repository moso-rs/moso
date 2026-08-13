//! The development preview inbox at `/_mail`.
//!
//! The highest-value two hundred lines in this crate. Seeing the rendered
//! message in a browser — with the HTML part, the text part, the headers and
//! the attachments — is the difference between iterating on an email in
//! seconds and iterating on it in minutes with a real SMTP account.
//!
//! Mounted only when [`MailConfig::preview`](crate::MailConfig::preview) is on,
//! which is the default for a local backend and off everywhere else. A
//! production profile that turns it on gets a boot warning: the inbox shows
//! message bodies, and message bodies contain password-reset links.

use moso_core::Router;
use serde::{Deserialize, Serialize};

use crate::RenderedEmail;

/// The path the inbox is mounted at.
///
/// ```
/// assert_eq!(moso_mail::preview::PREVIEW_PATH, "/_mail");
/// ```
pub const PREVIEW_PATH: &str = "/_mail";

/// A message as the inbox lists it.
///
/// ```no_run
/// use moso_mail::preview::PreviewItem;
///
/// # fn f(i: &PreviewItem) {
/// let _: &str = &i.subject;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PreviewItem {
    /// A stable identifier within this process's inbox.
    pub id: String,
    /// The Rust type name of the [`Email`](crate::Email) it came from.
    pub kind: String,
    /// The rendered subject.
    pub subject: String,
    /// Every recipient, rendered as header values.
    pub to: Vec<String>,
    /// When it was sent, as an RFC 3339 timestamp.
    pub sent_at: String,
    /// How many attachments it carries.
    pub attachments: usize,
}

/// What a preview backend has to be able to do.
///
/// Implemented by [`ConsoleMailer`](crate::backend::ConsoleMailer) and
/// [`MemoryMailer`](crate::backend::MemoryMailer). A remote backend cannot
/// implement it, which is exactly why the inbox is not offered in production.
///
/// ```no_run
/// use moso_mail::preview::{Inbox, PreviewItem};
///
/// fn list(inbox: &dyn Inbox) -> Vec<PreviewItem> {
///     inbox.list(20)
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot back the `/_mail` preview inbox",
    label = "not an inbox",
    note = "an inbox implements `list` and `get`, which means it must retain sent messages",
    note = "help: only local backends can — use `ConsoleMailer` in development or \
            `MemoryMailer` in tests; a provider backend has nothing to show"
)]
pub trait Inbox: Send + Sync + 'static {
    /// The most recent `limit` messages, newest first.
    fn list(&self, limit: usize) -> Vec<PreviewItem>;

    /// One message in full.
    fn get(&self, id: &str) -> Option<RenderedEmail>;

    /// Forget everything, for the inbox's "clear" button.
    fn clear(&self);
}

/// The routes that render the inbox.
///
/// Four of them: the index, one message's HTML part, one message's text part,
/// and the raw `.eml`. The HTML part is served inside a sandboxed `iframe`
/// with `Content-Security-Policy: sandbox`, because a preview inbox that
/// executes the message's scripts is a self-XSS in the developer's own origin.
///
/// ```no_run
/// use std::sync::Arc;
///
/// use moso_mail::preview::{routes, Inbox};
///
/// fn mount(inbox: Arc<dyn Inbox>) -> moso_core::Router {
///     routes(inbox)
/// }
/// ```
#[must_use]
pub fn routes(inbox: std::sync::Arc<dyn Inbox>) -> Router {
    use axum::routing::{get, post};

    // `mount_axum` rather than Moso's own routing, because these four handlers
    // capture the inbox they were handed. Everything under this prefix is
    // absent from the OpenAPI document by construction, which is what we want:
    // a development inbox is not part of anybody's API.
    let axum_routes = axum::Router::new()
        .route("/", get(index))
        .route("/{id}/html", get(html))
        .route("/{id}/text", get(text))
        .route("/{id}/raw", get(raw))
        .route("/clear", post(clear))
        .with_state(inbox);

    Router::new().mount_axum(PREVIEW_PATH, axum_routes)
}

/// How many messages the index lists.
const PAGE: usize = 100;

/// The inbox index: the list on the left, the message on the right.
async fn index(
    axum::extract::State(inbox): axum::extract::State<std::sync::Arc<dyn Inbox>>,
) -> axum::response::Response {
    let items = inbox.list(PAGE);
    let selected = items.first().map(|item| item.id.clone());

    let mut rows = String::new();
    for item in &items {
        let attachments = if item.attachments == 0 {
            String::new()
        } else {
            format!("<span class=\"clip\">{} 📎</span>", item.attachments)
        };
        rows.push_str(&format!(
            "<li><a href=\"#{id}\" onclick=\"show('{id}')\" id=\"row-{id}\">\
             <span class=\"kind\">{kind}</span>\
             <span class=\"subject\">{subject}</span>\
             <span class=\"to\">{to}</span>\
             <span class=\"when\">{when}</span>{attachments}</a></li>",
            id = escape(&item.id),
            kind = escape(&item.kind),
            subject = escape(&item.subject),
            to = escape(&item.to.join(", ")),
            when = escape(&item.sent_at),
        ));
    }

    if items.is_empty() {
        rows.push_str(
            "<li class=\"empty\">Nothing sent yet. Send a message and reload — \
             the console backend keeps the last hundred.</li>",
        );
    }

    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>Moso mail ({count})</title>{STYLE}</head><body>\
         <header><h1>Moso mail</h1>\
         <form method=\"post\" action=\"{PREVIEW_PATH}/clear\">\
         <button type=\"submit\">Clear</button></form></header>\
         <main><ul class=\"list\">{rows}</ul>\
         <section class=\"viewer\">\
         <nav><button onclick=\"tab('html')\" id=\"t-html\" class=\"on\">HTML</button>\
         <button onclick=\"tab('text')\" id=\"t-text\">Text</button>\
         <a id=\"download\" download>Download .eml</a></nav>\
         <iframe id=\"frame\" sandbox src=\"{src}\"></iframe>\
         </section></main>{SCRIPT}</body></html>",
        count = items.len(),
        src = selected
            .as_deref()
            .map_or_else(String::new, |id| format!("{PREVIEW_PATH}/{id}/html")),
    );

    let mut response = axum::response::Response::new(axum::body::Body::from(page));
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    no_store(&mut response);
    response
}

/// One message's HTML part, sandboxed.
async fn html(
    axum::extract::State(inbox): axum::extract::State<std::sync::Arc<dyn Inbox>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    let Some(message) = inbox.get(&id) else {
        return not_found();
    };
    body(message.html, "text/html; charset=utf-8")
}

/// One message's text part.
async fn text(
    axum::extract::State(inbox): axum::extract::State<std::sync::Arc<dyn Inbox>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    let Some(message) = inbox.get(&id) else {
        return not_found();
    };
    body(message.text, "text/plain; charset=utf-8")
}

/// One message as RFC 5322 bytes, for opening in a real mail client.
async fn raw(
    axum::extract::State(inbox): axum::extract::State<std::sync::Arc<dyn Inbox>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    let Some(message) = inbox.get(&id) else {
        return not_found();
    };
    let bytes = crate::mime::to_rfc5322(&message, &format!("{id}@moso.invalid"));

    let mut response = axum::response::Response::new(axum::body::Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("message/rfc822"),
    );
    headers.insert(
        http::header::CONTENT_DISPOSITION,
        http::HeaderValue::from_str(&format!("attachment; filename=\"{id}.eml\""))
            .unwrap_or_else(|_| http::HeaderValue::from_static("attachment")),
    );
    no_store(&mut response);
    response
}

/// The "clear" button.
async fn clear(
    axum::extract::State(inbox): axum::extract::State<std::sync::Arc<dyn Inbox>>,
) -> axum::response::Response {
    inbox.clear();
    let mut response = axum::response::Response::new(axum::body::Body::empty());
    *response.status_mut() = http::StatusCode::SEE_OTHER;
    response.headers_mut().insert(
        http::header::LOCATION,
        http::HeaderValue::from_static(PREVIEW_PATH),
    );
    response
}

/// A body served under the sandbox, whatever it contains.
///
/// The sandbox is not decoration. A preview inbox renders whatever an
/// application put in a message, and an application that interpolates user
/// input into an email without escaping it has just handed the developer's own
/// origin a stored XSS. `sandbox` with no allow-list means no scripts, no
/// forms and no same-origin access.
fn body(content: String, content_type: &'static str) -> axum::response::Response {
    let mut response = axum::response::Response::new(axum::body::Body::from(content));
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static(content_type),
    );
    headers.insert(
        http::header::CONTENT_SECURITY_POLICY,
        http::HeaderValue::from_static("sandbox; default-src 'none'; style-src 'unsafe-inline'"),
    );
    headers.insert(
        http::header::X_CONTENT_TYPE_OPTIONS,
        http::HeaderValue::from_static("nosniff"),
    );
    no_store(&mut response);
    response
}

/// A message that has aged out of the ring buffer.
fn not_found() -> axum::response::Response {
    let mut response = axum::response::Response::new(axum::body::Body::from(
        "no such message — the inbox keeps only the most recent ones",
    ));
    *response.status_mut() = http::StatusCode::NOT_FOUND;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    no_store(&mut response);
    response
}

/// Never cache a preview: the inbox changes on every send.
fn no_store(response: &mut axum::response::Response) {
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
}

/// Escape text for interpolation into the index's markup.
///
/// The index shows subjects and recipients an application chose, and a subject
/// containing `<script>` must render as text.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The inbox's stylesheet. Inline, because a development page must not need a
/// second request to be readable.
const STYLE: &str = "<style>\
:root{color-scheme:light dark}\
*{box-sizing:border-box}\
body{margin:0;font:14px/1.5 ui-sans-serif,system-ui,sans-serif;height:100vh;display:flex;\
flex-direction:column}\
header{display:flex;align-items:center;justify-content:space-between;padding:.6rem 1rem;\
border-bottom:1px solid color-mix(in srgb,currentColor 20%,transparent)}\
h1{font-size:1rem;margin:0;letter-spacing:.02em}\
button,nav a{font:inherit;padding:.25rem .7rem;border-radius:.4rem;cursor:pointer;\
border:1px solid color-mix(in srgb,currentColor 25%,transparent);background:transparent;\
color:inherit;text-decoration:none}\
main{flex:1;display:grid;grid-template-columns:minmax(18rem,34%) 1fr;min-height:0}\
.list{margin:0;padding:0;list-style:none;overflow:auto;\
border-right:1px solid color-mix(in srgb,currentColor 20%,transparent)}\
.list a{display:grid;gap:.1rem;padding:.6rem 1rem;text-decoration:none;color:inherit;\
border-bottom:1px solid color-mix(in srgb,currentColor 10%,transparent)}\
.list a.on{background:color-mix(in srgb,currentColor 8%,transparent)}\
.kind{font-size:.72rem;text-transform:uppercase;letter-spacing:.06em;opacity:.6}\
.subject{font-weight:600}\
.to,.when{font-size:.8rem;opacity:.7}\
.clip{font-size:.75rem;opacity:.7}\
.empty{padding:2rem 1rem;opacity:.7}\
.viewer{display:flex;flex-direction:column;min-width:0}\
nav{display:flex;gap:.5rem;padding:.6rem 1rem;\
border-bottom:1px solid color-mix(in srgb,currentColor 20%,transparent)}\
nav button.on{background:color-mix(in srgb,currentColor 12%,transparent)}\
iframe{flex:1;border:0;width:100%;background:#fff}\
</style>";

/// The inbox's behaviour: pick a message, pick a part. Twelve lines, inline,
/// no framework.
const SCRIPT: &str = "<script>\
let current=document.querySelector('.list a')?.id?.slice(4)||'';\
let part='html';\
function paint(){\
 if(!current)return;\
 document.querySelectorAll('.list a').forEach(a=>a.classList.remove('on'));\
 document.getElementById('row-'+current)?.classList.add('on');\
 document.getElementById('t-html').className=part==='html'?'on':'';\
 document.getElementById('t-text').className=part==='text'?'on':'';\
 document.getElementById('frame').src='/_mail/'+current+'/'+part;\
 document.getElementById('download').href='/_mail/'+current+'/raw';}\
function show(id){current=id;paint();}\
function tab(next){part=next;paint();}\
paint();\
</script>";
