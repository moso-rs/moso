//! The acceptance criteria of `docs/03-batteries/34-mail-storage-realtime.md`,
//! exercised over the real routes rather than asserted by inspection.
//!
//! > **Mail:** template variable checked at compile time; console backend
//! > renders a browsable preview; suppression prevents send; provider webhook
//! > signatures verified; `app.mail()` assertions work.
//!
//! The compile-time half would belong to a `#[derive(Email)]`, which
//! `moso-macros` does not ship; what this file checks is the half that exists
//! — [`TemplateEngine::variables`], the list a test compares against the keys
//! its context sets — plus the criteria that are properties of the whole path,
//! the enforced send deadline among them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use moso_mail::backend::{ConsoleMailer, MemoryMailer};
use moso_mail::preview::{Inbox, PREVIEW_PATH};
use moso_mail::{
    Address, Email, Mailer, MemorySuppressionList, RenderedEmail, Result, Suppression,
    SuppressionList, SuppressionReason, Template, TemplateEngine,
};
use tower::ServiceExt as _;

/// The message the criteria are exercised with.
struct WelcomeEmail {
    /// Who signed up.
    to: Address,
    /// The link that verifies their address.
    verify_url: String,
}

impl Email for WelcomeEmail {
    fn to(&self) -> Vec<Address> {
        vec![self.to.clone()]
    }

    fn subject(&self) -> Result<String> {
        Ok("Welcome to Shop".to_owned())
    }

    fn html(&self) -> Result<String> {
        Ok(format!(
            "<p>Welcome!</p><p><a href=\"{}\">verify your address</a></p>",
            self.verify_url,
        ))
    }

    fn text(&self) -> Result<String> {
        Ok(moso_mail::html_to_text(&self.html()?))
    }
}

fn welcome(to: &str) -> WelcomeEmail {
    WelcomeEmail {
        to: Address::new(to).expect("a valid address"),
        verify_url: "https://shop.example/verify?token=abc".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// the template check
// ---------------------------------------------------------------------------

/// The runtime half of "template variable checked at compile time": the engine
/// reports exactly the variable paths a template references, in dotted form,
/// which is the list `#[derive(Email)]` compares against the struct's fields.
///
/// A change here that stopped reporting nested paths would silently turn the
/// derive's check into a check of top-level names only, and `{{ user.nmae }}`
/// would compile again.
#[test]
fn the_engine_reports_the_variables_a_derive_would_check() {
    let mut engine = moso_mail::Jinja::new();
    engine
        .add(Template::inline(
            "emails/welcome.html",
            "<p>Hi {{ user.name }},</p>\
             <p><a href=\"{{ verify_url }}\">verify</a></p>\
             {% if user.trial %}<p>{{ app_name }}</p>{% endif %}",
        ))
        .expect("the template parses");

    assert_eq!(
        engine.variables("emails/welcome.html"),
        vec![
            "app_name".to_owned(),
            "user.name".to_owned(),
            "user.trial".to_owned(),
            "verify_url".to_owned(),
        ],
    );

    // And the runtime backstop for a template the derive never saw: an
    // undefined variable fails the render rather than producing "Hello ,".
    let error = engine
        .render(
            "emails/welcome.html",
            &serde_json::json!({ "user": { "name": "Ada", "trial": false } }),
        )
        .expect_err("`verify_url` is undefined");
    assert!(matches!(error, moso_mail::Error::Template { .. }));
}

// ---------------------------------------------------------------------------
// the browsable preview inbox
// ---------------------------------------------------------------------------

/// Drive one request through the `/_mail` routes.
async fn request(
    inbox: Arc<dyn Inbox>,
    method: &str,
    path: &str,
) -> (http::StatusCode, http::HeaderMap, String) {
    let router = moso_mail::preview::routes(inbox).into_axum();
    let response = router
        .oneshot(
            http::Request::builder()
                .method(method)
                .uri(path)
                .body(axum::body::Body::empty())
                .expect("a well-formed request"),
        )
        .await
        .expect("the router answers");

    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("the body reads");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// The acceptance criterion: the console backend renders a browsable preview.
///
/// Browsable means all four of it — an index that lists what was sent, the HTML
/// part, the text part, and the raw `.eml` — reachable over HTTP with no SMTP
/// account and no configuration.
#[tokio::test]
async fn the_console_backend_renders_a_browsable_preview() {
    let mailer =
        Arc::new(ConsoleMailer::new().from(Address::new("hello@shop.example").expect("valid")));
    mailer
        .send(&welcome("ada@example.com"))
        .await
        .expect("sends");

    let inbox: Arc<dyn Inbox> = mailer.clone();

    // The index lists the message, by kind, subject and recipient.
    let (status, headers, page) = request(inbox.clone(), "GET", PREVIEW_PATH).await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(
        headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/html; charset=utf-8"),
    );
    assert!(page.contains("Welcome to Shop"), "the subject is listed");
    assert!(page.contains("ada@example.com"), "the recipient is listed");
    assert!(page.contains("WelcomeEmail"), "the message type is listed");

    let id = inbox.list(1).first().expect("one message").id.clone();

    // The HTML part, sandboxed. A preview that executes the message's scripts
    // is a self-XSS in the developer's own origin.
    let (status, headers, html) =
        request(inbox.clone(), "GET", &format!("{PREVIEW_PATH}/{id}/html")).await;
    assert_eq!(status, http::StatusCode::OK);
    assert!(html.contains("verify your address"));
    let csp = headers
        .get(http::header::CONTENT_SECURITY_POLICY)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(csp.contains("sandbox"), "the preview is sandboxed: {csp}");
    assert_eq!(
        headers
            .get(http::header::X_CONTENT_TYPE_OPTIONS)
            .and_then(|v| v.to_str().ok()),
        Some("nosniff"),
    );

    // The text part, which is what a text client would show.
    let (status, _, text) =
        request(inbox.clone(), "GET", &format!("{PREVIEW_PATH}/{id}/text")).await;
    assert_eq!(status, http::StatusCode::OK);
    assert!(text.contains("https://shop.example/verify?token=abc"));
    assert!(
        !text.contains('<'),
        "the text part carries no markup: {text}"
    );

    // The raw message, which opens in a real mail client.
    let (status, headers, raw) =
        request(inbox.clone(), "GET", &format!("{PREVIEW_PATH}/{id}/raw")).await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(
        headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("message/rfc822"),
    );
    assert!(raw.starts_with("From: hello@shop.example"));
    assert!(raw.contains("multipart/alternative"));

    // A message that has aged out is a 404 rather than a panic.
    let (status, _, _) =
        request(inbox.clone(), "GET", &format!("{PREVIEW_PATH}/999999/html")).await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);

    // And the clear button empties it.
    let (status, _, _) = request(inbox.clone(), "POST", &format!("{PREVIEW_PATH}/clear")).await;
    assert_eq!(status, http::StatusCode::SEE_OTHER);
    assert!(inbox.list(10).is_empty());
}

/// The index shows a subject an application chose, and an application that
/// interpolated user input into one without escaping it must not be able to
/// script the developer's own page.
#[tokio::test]
async fn a_hostile_subject_is_escaped_in_the_index() {
    struct Hostile;

    impl Email for Hostile {
        fn to(&self) -> Vec<Address> {
            vec![Address::new("ada@example.com").expect("valid")]
        }
        fn subject(&self) -> Result<String> {
            Ok("<script>alert(document.cookie)</script>".to_owned())
        }
        fn html(&self) -> Result<String> {
            Ok("<p>hi</p>".to_owned())
        }
        fn text(&self) -> Result<String> {
            Ok("hi".to_owned())
        }
    }

    let mailer = Arc::new(ConsoleMailer::new());
    mailer.send(&Hostile).await.expect("sends");

    let (_, _, page) = request(mailer.clone(), "GET", PREVIEW_PATH).await;
    assert!(
        !page.contains("<script>alert"),
        "the subject must be escaped"
    );
    assert!(page.contains("&lt;script&gt;alert"));
}

// ---------------------------------------------------------------------------
// suppression, end to end
// ---------------------------------------------------------------------------

/// The acceptance criterion: suppression prevents the send, and it does so
/// through composition, so no backend can forget the check.
#[tokio::test]
async fn suppression_prevents_the_send_whatever_the_backend() {
    let inner = Arc::new(MemoryMailer::new());
    let list = Arc::new(MemorySuppressionList::new());
    list.record(Suppression::new(
        Address::new("bounced@example.com").expect("valid"),
        SuppressionReason::HardBounce,
    ))
    .await
    .expect("records");

    let mailer = moso_mail::backend::Suppressing::new(inner.clone(), list.clone());

    let error = mailer
        .send(&welcome("bounced@example.com"))
        .await
        .expect_err("the recipient is suppressed");
    assert!(error.is_suppressed());
    assert!(inner.sent().is_empty(), "nothing reached the backend");

    // The error becomes a 422 with a pointer, not an opaque 500: the caller
    // sent something the server will not act on, and can fix it.
    let http: moso_core::Error = error.into();
    assert_eq!(http.status(), http::StatusCode::UNPROCESSABLE_ENTITY);

    // Anybody else still gets their mail.
    mailer
        .send(&welcome("fresh@example.com"))
        .await
        .expect("not suppressed");
    assert_eq!(inner.sent().len(), 1);
}

/// A verified webhook feeds the suppression list, which then prevents the next
/// send — the whole loop, with nothing mocked but the provider's HTTP call.
#[tokio::test]
async fn a_verified_webhook_suppresses_the_next_send() {
    use moso_core::config::SecretString;
    use moso_mail::WebhookVerifier as _;
    use moso_mail::webhook::{SharedSecretVerifier, WebhookScheme};

    let verifier =
        SharedSecretVerifier::new(WebhookScheme::Postmark, SecretString::new("hook-token"));
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "x-postmark-token",
        http::HeaderValue::from_static("hook-token"),
    );

    let payload = bytes::Bytes::from_static(
        br#"{"RecordType":"Bounce","Type":"HardBounce","Email":"gone@example.com",
             "Description":"550 user unknown"}"#,
    );

    let events = verifier
        .verify(&headers, &payload)
        .expect("the signature verifies");
    assert_eq!(events.len(), 1);

    let list = Arc::new(MemorySuppressionList::new());
    assert_eq!(
        moso_mail::apply_events(list.as_ref(), &events)
            .await
            .expect("applies"),
        1,
    );

    let inner = Arc::new(MemoryMailer::new());
    let mailer = moso_mail::backend::Suppressing::new(inner.clone(), list);
    assert!(mailer.send(&welcome("gone@example.com")).await.is_err());
    assert!(inner.sent().is_empty());

    // A forged webhook changes nothing.
    let mut forged = http::HeaderMap::new();
    forged.insert(
        "x-postmark-token",
        http::HeaderValue::from_static("guessed"),
    );
    assert!(verifier.verify(&forged, &payload).is_err());
}

// ---------------------------------------------------------------------------
// the test-harness assertions
// ---------------------------------------------------------------------------

/// The acceptance criterion: the assertions `docs/04-devex/43-testing.md`
/// specifies work against the memory backend.
///
/// `app.mail()` itself belongs to `moso-test`; what this crate owes it is the
/// seam — `sent`, `sent_count`, `sent_of`, `count_of`, `clear`, `fail_with`
/// and `delay` — and the guarantee that what those report is what was actually
/// sent.
#[tokio::test]
async fn the_memory_backend_supports_the_documented_assertions() {
    let mailer = MemoryMailer::new();
    mailer.set_from(Some(Address::new("hello@shop.example").expect("valid")));

    // assert_none_sent
    assert_eq!(mailer.sent_count(), 0);
    assert!(mailer.sent().is_empty());

    mailer
        .send(&welcome("ada@example.com"))
        .await
        .expect("sends");
    mailer
        .send(&welcome("grace@example.com"))
        .await
        .expect("sends");

    // assert_sent::<WelcomeEmail>(2)
    assert_eq!(mailer.count_of::<WelcomeEmail>(), 2);
    assert_eq!(mailer.sent_of::<WelcomeEmail>().len(), 2);
    // Either spelling of the name finds the same messages.
    assert_eq!(mailer.sent_of_kind("WelcomeEmail").len(), 2);
    assert_eq!(
        mailer
            .sent_of_kind(std::any::type_name::<WelcomeEmail>())
            .len(),
        2,
    );
    assert_eq!(mailer.count_of_kind("PasswordReset"), 0);

    // last().assert_to(..).assert_html_contains(..)
    let last: &RenderedEmail = &mailer.sent()[1];
    assert_eq!(last.to[0].address(), "grace@example.com");
    assert!(last.html.contains("verify"));
    assert_eq!(last.from.address(), "hello@shop.example");
    assert!(!last.text.is_empty(), "a text part is never optional");

    mailer.clear();
    assert_eq!(mailer.sent_count(), 0);
}

/// The acceptance criterion for the deadline: an application that configured
/// one gets it, and a provider that stops answering costs it that deadline
/// rather than the job.
///
/// `delay` stands in for the provider, which is what makes this a test of the
/// deadline rather than a test of somebody's network.
#[tokio::test]
async fn a_send_that_overruns_its_deadline_fails_instead_of_hanging() {
    let mailer = MemoryMailer::new().timeout(Duration::from_millis(20));
    mailer.delay(Some(Duration::from_secs(60)));

    let started = Instant::now();
    let error = mailer
        .send(&welcome("ada@example.com"))
        .await
        .expect_err("the deadline fires");

    assert!(matches!(error, moso_mail::Error::Timeout { .. }), "{error}");
    assert!(error.retryable(), "a job must be able to try again");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the caller waited for the deadline, not for the provider",
    );
    assert_eq!(mailer.sent_count(), 0, "nothing was recorded as delivered");

    // The problem is the provider, not the message: the same message sends
    // once the stall is over, which is the retry a job would perform.
    mailer.delay(None);
    mailer
        .send(&welcome("ada@example.com"))
        .await
        .expect("sends");
    assert_eq!(mailer.count_of::<WelcomeEmail>(), 1);
}
