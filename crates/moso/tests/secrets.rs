//! The canary test: a value that must never be printed, put through every
//! printing route the framework has, and grepped for afterwards.
//!
//! Every other test in this suite asserts that something *works*. This one
//! asserts that something *never happens*, which needs a different shape: one
//! distinctive string is planted in a [`SecretString`] and a [`Password`], the
//! application is driven through its success and failure paths with the log
//! captured, and then every byte the process produced — response bodies,
//! headers, log lines, `Debug` renderings, the OpenAPI document, the boot
//! report — is searched for it.
//!
//! A secret that leaks does so exactly once, into a log aggregator that keeps
//! it for a year. There is no partial credit here, so the assertion is over the
//! union of everything rather than one output at a time.

#![allow(dead_code)]

use std::sync::Arc;

use moso::config::SecretString;
use moso::prelude::*;
use moso::response::NoContent;
use moso::schema::Password;

mod support;
use support::{LogCapture, Reply, get, post_json, send};

/// The string that must not appear anywhere. Distinctive enough that a match is
/// never a coincidence, and long enough not to be a substring of anything.
const CANARY: &str = "CANARY-c0ffee-DO-NOT-LOG-8fdb21";

/// A second canary, for the request-side secret.
const BODY_CANARY: &str = "CANARY-body-secret-4d19aa";

// ---------------------------------------------------------------------------
// The application
// ---------------------------------------------------------------------------

/// Configuration carrying a secret, as a real application's would.
#[derive(Config, Debug, Clone)]
pub struct Cfg {
    /// The database URL, which contains a password.
    pub database_url: SecretString,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            database_url: SecretString::new(format!("postgres://user:{CANARY}@db:5432/app")),
        }
    }
}

/// A credential store, injected.
#[derive(Debug)]
pub struct Vault {
    /// The signing key.
    pub key: SecretString,
}

impl Default for Vault {
    fn default() -> Self {
        Self {
            key: SecretString::new(CANARY),
        }
    }
}

/// A sign-in body, carrying a plaintext password.
#[derive(Schema, Debug, Clone)]
pub struct SignIn {
    /// Who is signing in.
    #[schema(len = 1..=64)]
    pub username: String,
    /// Their password.
    pub password: Password,
}

/// Log the configuration and the vault, the way a careless startup line would.
#[endpoint]
async fn leaky_log(Inject(vault): Inject<Vault>, Inject(config): Inject<Cfg>) -> Result<NoContent> {
    // Deliberately hostile: `?` sigils on both, which is the sigil that reaches
    // for `Debug` and the one a reviewer is least likely to question.
    moso::deps::tracing::info!(
        target: "test::leaky",
        vault = ?vault,
        config = ?config,
        url = %config.database_url,
        "starting up",
    );
    Ok(NoContent)
}

/// Fail, with the secret in the error's own source chain.
#[endpoint]
async fn leaky_error(Inject(config): Inject<Cfg>) -> Result<NoContent> {
    Err(Error::internal_msg(format!(
        "could not reach {:?}",
        config.database_url
    )))
}

/// Accept a password and answer without echoing it.
#[endpoint]
async fn sign_in(Json(body): Json<SignIn>) -> Result<Json<String>> {
    // The one legitimate read, spelled out so it is greppable.
    assert!(!body.password.expose().is_empty());
    Ok(Json(body.username))
}

/// Try to echo the password straight back, which must not succeed quietly.
#[endpoint]
async fn echo(Json(body): Json<SignIn>) -> Result<Json<SignIn>> {
    Ok(Json(body))
}

/// Log the request body, the way a debugging line would.
#[endpoint]
async fn leaky_body(Json(body): Json<SignIn>) -> Result<NoContent> {
    moso::deps::tracing::warn!(target: "test::leaky", body = ?body, "received");
    Ok(NoContent)
}

fn router() -> Router {
    moso::routes! {
        GET  "/leaky-log"   => leaky_log,
        GET  "/leaky-error" => leaky_error,
        POST "/sign-in"     => sign_in,
        POST "/echo"        => echo,
        POST "/leaky-body"  => leaky_body,
    }
}

fn app() -> axum::Router<()> {
    App::new(Cfg::default())
        .provide(Vault::default())
        .mount(router())
        .build()
        .expect("builds")
        .into_service()
}

/// A sign-in body carrying the second canary.
fn credentials() -> String {
    format!(r#"{{"username":"ada","password":"{BODY_CANARY}-pad"}}"#)
}

/// Everything one request produced, as one searchable string.
fn transcript(reply: &Reply) -> String {
    let mut out = String::new();
    out.push_str(&reply.status.to_string());
    out.push('\n');
    for (name, value) in &reply.headers {
        out.push_str(name.as_str());
        out.push(':');
        out.push_str(value.to_str().unwrap_or("<binary>"));
        out.push('\n');
    }
    out.push_str(&reply.body);
    out
}

// ---------------------------------------------------------------------------
// The types themselves
// ---------------------------------------------------------------------------

#[test]
fn the_secret_types_refuse_every_formatting_route() {
    let secret = SecretString::new(CANARY);
    assert!(!format!("{secret:?}").contains(CANARY));
    assert!(!format!("{secret}").contains(CANARY));
    assert!(!format!("{secret:#?}").contains(CANARY));

    // Serialising a `SecretString` is an error, not a redaction: a redacted
    // value that round-trips is a configuration file that silently loses its
    // password.
    assert!(serde_json::to_string(&secret).is_err());

    let password = Password::from_trusted(CANARY);
    assert!(!format!("{password:?}").contains(CANARY));
    assert!(serde_json::to_string(&password).is_err());

    // And the only way to read either is the greppable one.
    assert_eq!(secret.expose(), CANARY);
    assert_eq!(password.expose(), CANARY);
}

#[test]
fn a_struct_that_derives_debug_around_a_secret_is_still_safe() {
    // This is the actual failure mode: nobody prints a `SecretString`, they
    // print the struct three levels up that happens to contain one.
    let config = Cfg::default();
    let rendered = format!("{config:#?}");
    assert!(!rendered.contains(CANARY), "{rendered}");
    assert!(rendered.contains("***"), "{rendered}");
}

// ---------------------------------------------------------------------------
// The canary sweep
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_canary_reaches_any_output_of_any_request() {
    let logs = LogCapture::new();
    let guard = logs.install();

    let service = app();
    let mut everything = String::new();

    for request in [
        get("/leaky-log"),
        get("/leaky-error"),
        post_json("/sign-in", &credentials()),
        post_json("/echo", &credentials()),
        post_json("/leaky-body", &credentials()),
        // A body that fails validation, so the 422 has the field in it.
        post_json("/sign-in", r#"{"username":"","password":"short"}"#),
        // A malformed body, so the 400 quotes what it could not read.
        post_json("/sign-in", &format!(r#"{{"password":"{BODY_CANARY}"#)),
    ] {
        let reply = send(service.clone(), request).await;
        everything.push_str(&transcript(&reply));
        everything.push('\n');
    }

    drop(guard);
    everything.push_str(&logs.joined());

    for canary in [CANARY, BODY_CANARY] {
        assert!(
            !everything.contains(canary),
            "the canary `{canary}` escaped. Everything the process produced:\n{everything}"
        );
    }

    // The sweep is only meaningful if the requests actually ran, so prove the
    // haystack is the real one.
    assert!(
        everything.contains("starting up"),
        "the log capture saw nothing; the sweep proved nothing:\n{everything}"
    );
    assert!(everything.contains("could not reach"), "{everything}");
}

#[tokio::test]
async fn a_secret_in_a_5xx_detail_is_suppressed_by_default() {
    let reply = send(app(), get("/leaky-error")).await;

    assert_eq!(reply.status, 500, "{}", reply.body);
    assert!(
        reply.json()["detail"].is_null(),
        "a 5xx must not narrate itself to a client: {}",
        reply.body
    );
    assert!(!reply.body.contains(CANARY), "{}", reply.body);
}

#[tokio::test]
async fn even_exposing_internal_errors_cannot_expose_the_secret_itself() {
    // The escape hatch discloses the *detail*, which is why `SecretString`'s
    // redaction has to happen at the `Debug` boundary rather than at the
    // problem-document boundary: by the time the error exists, the string is
    // already `***`.
    let service = App::new(Cfg::default())
        .provide(Vault::default())
        .mount(router())
        .http_config(moso::http_config::HttpConfig {
            expose_internal_errors: true,
            ..moso::http_config::HttpConfig::default()
        })
        .build()
        .expect("builds")
        .into_service();

    let reply = send(service, get("/leaky-error")).await;
    assert_eq!(reply.status, 500, "{}", reply.body);
    assert!(
        reply.body.contains("could not reach"),
        "the detail should be disclosed here: {}",
        reply.body
    );
    assert!(
        !reply.body.contains(CANARY),
        "and the secret still must not be: {}",
        reply.body
    );
}

#[tokio::test]
async fn a_password_that_reaches_a_response_fails_loudly_rather_than_quietly() {
    let reply = send(app(), post_json("/echo", &credentials())).await;

    assert_ne!(
        reply.status, 200,
        "serialising a `Password` must not succeed: {}",
        reply.body
    );
    assert!(!reply.body.contains(BODY_CANARY), "{}", reply.body);
}

#[tokio::test]
async fn a_validation_failure_on_a_password_does_not_quote_the_value() {
    // Under `Password::MIN_LENGTH`, so the constraint fires and the response is
    // a problem document about this exact field. The temptation every framework
    // gives in to is `invalid value: "hunter2", expected …`.
    let short = "CAN-8fdb21";
    let reply = send(
        app(),
        post_json(
            "/sign-in",
            &format!(r#"{{"username":"ada","password":"{short}"}}"#),
        ),
    )
    .await;

    assert!(
        reply.status == 400 || reply.status == 422,
        "a password under the minimum must be refused: {} {}",
        reply.status,
        reply.body
    );
    assert!(
        !reply.body.contains(short),
        "the rejected value must not be echoed: {}",
        reply.body
    );
    assert!(
        reply.body.contains("12"),
        "the client still has to be told the policy: {}",
        reply.body
    );
}

#[tokio::test]
async fn the_authorization_header_is_never_logged() {
    let logs = LogCapture::new();
    let guard = logs.install();

    let request = axum::http::Request::builder()
        .uri("/leaky-error")
        .header("authorization", format!("Bearer {CANARY}"))
        .header("cookie", format!("session={CANARY}"))
        .body(axum::body::Body::empty())
        .expect("request");
    let reply = send(app(), request).await;

    drop(guard);

    assert_eq!(reply.status, 500);
    let logged = logs.joined();
    assert!(
        !logged.contains(CANARY),
        "credential headers are marked sensitive for exactly this reason:\n{logged}"
    );
}

// ---------------------------------------------------------------------------
// The document and the boot report
// ---------------------------------------------------------------------------

#[test]
fn the_openapi_document_marks_a_password_write_only_and_carries_no_value() {
    let application = App::new(Cfg::default())
        .provide(Vault::default())
        .mount(router())
        .build()
        .expect("builds");
    let document = serde_json::to_string(application.openapi()).expect("json");

    assert!(!document.contains(CANARY), "{document}");
    assert!(!document.contains(BODY_CANARY), "{document}");

    let parsed: serde_json::Value = serde_json::from_str(&document).expect("json");
    let password = &parsed["components"]["schemas"]["SignIn"]["properties"]["password"];
    assert_eq!(password["writeOnly"], true, "{password}");
    assert_eq!(password["format"], "password", "{password}");
    assert!(
        password["default"].is_null() && password["examples"].is_null(),
        "a documented example of a password field is a documented password: {password}"
    );
}

#[test]
fn a_boot_report_that_mentions_the_config_type_does_not_print_its_value() {
    /// Needs something nobody provided, so the report has to render.
    #[endpoint]
    async fn needs_missing(Inject(_v): Inject<Arc<u128>>) -> Result<NoContent> {
        Ok(NoContent)
    }

    let error = App::new(Cfg::default())
        .mount(moso::routes! { GET "/x" => needs_missing })
        .build()
        .expect_err("the provider is missing");

    let rendered = format!("{error}{error:?}");
    assert!(!rendered.contains(CANARY), "{rendered}");
}
