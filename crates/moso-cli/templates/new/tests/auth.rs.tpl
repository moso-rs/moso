//! The authentication flows, end to end.
//!
//! Everything here goes over HTTP against the composed application —
//! `App::into_service()` hands back the real router, the real middleware stack
//! and the real dependency graph — because that is where the bugs are.
//! Constructing a handler's arguments by hand would skip the session layer,
//! the validation and the serialisation, which is most of what these routes do.
//!
//! # The two properties worth asserting
//!
//! A flow test that only checks the happy path proves the least interesting
//! thing. The two that matter here are:
//!
//! - **a failure says nothing about who exists.** A wrong password, an unknown
//!   address and a suspended account are one answer, and asking for a reset
//!   link is one answer whether or not the address has an account;
//! - **a reset ends the old password.** Setting a new one has to make the old
//!   one stop working, or the flow has moved a credential rather than replaced
//!   it.
//!
//! # Where the reset token comes from
//!
//! Nothing sends email, so `Outbox` in `src/auth.rs` keeps the last few minted
//! tokens outside the production profile and this file reads one back. When you
//! replace `Outbox::send` with your mailer, replace this with whatever your
//! mailer can be asked in a test.

use std::sync::Arc;

use moso::auth::TokenPurpose;
use moso::deps::axum::Router;
use moso::deps::axum::body::{Body, to_bytes};
use moso::deps::http::{Request, StatusCode};
use moso::deps::serde_json::Value;
use moso::deps::tower::ServiceExt;

/// A long, unguessable password that names nothing about the account.
const PASSWORD: &str = "tumbling-otter-vestibule-41";

/// The replacement, for the reset flow.
const NEW_PASSWORD: &str = "quiet-lantern-basalt-onward-7";

/// One browser: the composed application, and whatever cookie it was handed.
///
/// The cookie is carried by hand because that is what a browser does, and
/// because it is the only way to prove the session survives one request and
/// stops at another.
struct Browser {
    /// The composed application.
    service: Router,
    /// The provider the handlers take, so a test can read the outbox.
    auth: Arc<@@LIB_NAME@@::auth::Auth>,
    /// `name=value`, as a client would send it back.
    cookie: Option<String>,
}

impl Browser {
    /// Boot the application the way `main` does.
    fn open() -> Self {
        let app = @@LIB_NAME@@::build().expect("the application builds");
        // The same provider the handlers reach with `Inject<Auth>`, taken
        // before `into_service` consumes the application.
        let auth = app
            .resolver()
            .get::<@@LIB_NAME@@::auth::Auth>()
            .expect("src/lib.rs provides Auth");
        Self {
            service: app.into_service(),
            auth,
            cookie: None,
        }
    }

    /// Send one request, remembering any cookie the answer set.
    async fn send(&mut self, request: Request<Body>) -> (StatusCode, String) {
        let response = self
            .service
            .clone()
            .oneshot(request)
            .await
            .expect("the service is infallible");

        if let Some(header) = response.headers().get("set-cookie") {
            // Everything before the first `;` is `name=value`, which is all a
            // client ever sends back; the attributes are the server's business.
            self.cookie = header
                .to_str()
                .ok()
                .and_then(|value| value.split(';').next())
                .map(str::to_owned);
        }

        let status = response.status();
        let body = to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("a readable body");
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// Build a request carrying this browser's cookie.
    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(cookie) = &self.cookie {
            builder = builder.header("cookie", cookie);
        }
        match body {
            Some(json) => builder
                .header("content-type", "application/json")
                .body(Body::from(json.to_owned()))
                .expect("a valid request"),
            None => builder.body(Body::empty()).expect("a valid request"),
        }
    }

    /// `GET path`.
    async fn get(&mut self, path: &str) -> (StatusCode, String) {
        let request = self.request("GET", path, None);
        self.send(request).await
    }

    /// `POST path` with a JSON body.
    async fn post(&mut self, path: &str, body: &str) -> (StatusCode, String) {
        let request = self.request("POST", path, Some(body));
        self.send(request).await
    }

    /// `DELETE path`.
    async fn delete(&mut self, path: &str) -> (StatusCode, String) {
        let request = self.request("DELETE", path, None);
        self.send(request).await
    }

    /// Register `ada@example.com` with [`PASSWORD`].
    async fn register_ada(&mut self) -> (StatusCode, String) {
        self.post(
            "/auth/register",
            &format!(r#"{{"email":"ada@example.com","name":"Ada","password":"{PASSWORD}"}}"#),
        )
        .await
    }

    /// Sign in as `ada@example.com` with `password`.
    async fn sign_in(&mut self, password: &str) -> (StatusCode, String) {
        self.post(
            "/auth/login",
            &format!(r#"{{"email":"ada@example.com","password":"{password}"}}"#),
        )
        .await
    }
}

/// One answer, with the part that is *meant* to differ removed.
///
/// `request_id` is new on every response and exists so a client's screenshot
/// ties to a log line. Everything else about two refusals has to be identical,
/// so it is the one field these comparisons drop.
fn comparable(answer: &(StatusCode, String)) -> (StatusCode, Value) {
    let mut body: Value = moso::deps::serde_json::from_str(&answer.1).unwrap_or(Value::Null);
    if let Some(fields) = body.as_object_mut() {
        fields.remove("request_id");
    }
    (answer.0, body)
}

#[tokio::test]
async fn an_account_registers_signs_in_and_sees_its_own_session() {
    let mut browser = Browser::open();

    let (status, body) = browser.register_ada().await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let (status, body) = browser.sign_in(PASSWORD).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("ada@example.com"), "{body}");
    assert!(!body.contains("password"), "no credential may be echoed: {body}");

    let (status, body) = browser.get("/auth/sessions").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"current\":true"), "{body}");
    // A listing is not a list of credentials: no identifier of any kind.
    assert!(!body.contains("\"id\""), "{body}");

    let (status, body) = browser.post("/auth/logout", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body) = browser.get("/auth/sessions").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn a_wrong_password_and_an_address_with_no_account_are_the_same_answer() {
    let mut browser = Browser::open();
    assert_eq!(browser.register_ada().await.0, StatusCode::ACCEPTED);

    let wrong = browser.sign_in("a-different-long-password").await;
    let missing = browser
        .post(
            "/auth/login",
            &format!(r#"{{"email":"nobody@example.com","password":"{PASSWORD}"}}"#),
        )
        .await;

    assert_eq!(wrong.0, StatusCode::UNAUTHORIZED, "{}", wrong.1);
    assert_eq!(
        comparable(&wrong),
        comparable(&missing),
        "the two failures must be indistinguishable"
    );
}

#[tokio::test]
async fn asking_for_a_reset_says_nothing_about_whether_the_address_exists() {
    let mut browser = Browser::open();
    assert_eq!(browser.register_ada().await.0, StatusCode::ACCEPTED);

    let known = browser
        .post("/auth/password/forgot", r#"{"email":"ada@example.com"}"#)
        .await;
    let unknown = browser
        .post("/auth/password/forgot", r#"{"email":"nobody@example.com"}"#)
        .await;

    assert_eq!(known.0, StatusCode::ACCEPTED, "{}", known.1);
    assert_eq!(
        comparable(&known),
        comparable(&unknown),
        "the two answers must be the same"
    );
}

#[tokio::test]
async fn a_reset_link_replaces_the_password_and_the_old_one_stops_working() {
    let mut browser = Browser::open();
    assert_eq!(browser.register_ada().await.0, StatusCode::ACCEPTED);

    let (status, body) = browser
        .post("/auth/password/forgot", r#"{"email":"ada@example.com"}"#)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let sent = browser
        .auth
        .outbox()
        .latest(TokenPurpose::ResetPassword)
        .expect("a reset token was minted");
    assert_eq!(sent.destination, "ada@example.com");

    let (status, body) = browser
        .post(
            "/auth/password/reset",
            &format!(
                r#"{{"token":"{}","password":"{NEW_PASSWORD}"}}"#,
                sent.token
            ),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body) = browser.sign_in(PASSWORD).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the old password: {body}");

    let (status, body) = browser.sign_in(NEW_PASSWORD).await;
    assert_eq!(status, StatusCode::OK, "the new password: {body}");

    // Single use: the same link must not work twice.
    let (status, body) = browser
        .post(
            "/auth/password/reset",
            &format!(
                r#"{{"token":"{}","password":"{NEW_PASSWORD}"}}"#,
                sent.token
            ),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn every_session_route_refuses_an_anonymous_caller() {
    let mut browser = Browser::open();

    assert_eq!(
        browser.get("/auth/sessions").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        browser.delete("/auth/sessions").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        browser.post("/auth/logout", "").await.0,
        StatusCode::NO_CONTENT,
        "logging out when nobody is signed in is not an error"
    );
}

#[tokio::test]
async fn a_password_that_is_too_short_names_the_field_that_is_wrong() {
    let mut browser = Browser::open();
    let (status, body) = browser
        .post(
            "/auth/register",
            r#"{"email":"ada@example.com","name":"Ada","password":"short"}"#,
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("/password"), "{body}");
}
