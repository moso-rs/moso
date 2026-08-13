//! The test harness: boot the real application, drive it, and check what comes
//! back against the document it publishes.
//!
//! This is deliberately small and explicit. It boots the *real* application —
//! the real provider map, the real middleware stack, the real OpenAPI document
//! — through `App::into_service()`, and drives it with `tower`'s `oneshot`, so
//! nothing about the request path is stubbed.
//!
//! The interesting piece is [`TestResponse::assert_matches_openapi`]: it looks
//! the operation up in the document this very application generated, finds the
//! response schema declared for the status that came back, and validates the
//! body against it. A handler that starts returning a field the document does
//! not mention fails here, which is the one class of bug an ordinary
//! status-code assertion cannot see.

#![allow(
    dead_code,
    reason = "each test binary uses a different part of this module"
)]

use std::sync::Arc;

use example_crud::config::AppConfig;
use moso::deps::axum::body::{Body, to_bytes};
use moso::deps::http::{HeaderMap, Request, StatusCode};
use moso::deps::tower::ServiceExt;
use serde_json::{Value, json};

/// The prefix every posts route is nested under.
pub const API: &str = "/api/v1";

// ---------------------------------------------------------------------------
// The application under test
// ---------------------------------------------------------------------------

/// A booted application, plus the document it generated and the API key it
/// seeded.
pub struct TestApp {
    service: moso::deps::axum::Router<()>,
    document: Arc<Value>,
    api_key: String,
}

impl TestApp {
    /// Boot the application with the declared configuration defaults and its own
    /// fresh, empty SQLite database.
    ///
    /// The configuration comes from `AppConfig::defaults()` rather than from
    /// `AppConfig::load()`, so a `PUBLIC_URL` exported in somebody's shell
    /// cannot change what this test asserts. `build` opens a database per call,
    /// so two `TestApp`s never see each other's rows.
    #[must_use]
    pub async fn new() -> Self {
        let app = example_crud::build(AppConfig::defaults().expect("the defaults load"))
            .await
            .expect("the application builds");
        let api_key = example_crud::demo_api_key(&app).expect("a key is seeded at boot");
        let document = serde_json::to_value(app.openapi()).expect("the document serialises");
        Self {
            service: app.into_service(),
            document: Arc::new(document),
            api_key,
        }
    }

    /// The OpenAPI document this instance generated.
    #[must_use]
    pub fn document(&self) -> &Value {
        &self.document
    }

    /// Start a request.
    #[must_use]
    pub fn request(&self, method: &str, path: &str) -> RequestBuilder<'_> {
        RequestBuilder {
            app: self,
            method: method.to_owned(),
            path: path.to_owned(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// `GET path`.
    #[must_use]
    pub fn get(&self, path: &str) -> RequestBuilder<'_> {
        self.request("GET", path)
    }

    /// `POST path`, as an authenticated writer.
    #[must_use]
    pub fn post(&self, path: &str) -> RequestBuilder<'_> {
        self.request("POST", path).key()
    }

    /// `PATCH path`, as an authenticated writer.
    #[must_use]
    pub fn patch(&self, path: &str) -> RequestBuilder<'_> {
        self.request("PATCH", path).key()
    }

    /// `DELETE path`, as an authenticated writer.
    #[must_use]
    pub fn delete(&self, path: &str) -> RequestBuilder<'_> {
        self.request("DELETE", path).key()
    }

    /// Create a post through the API and return the decoded body.
    ///
    /// The shortest path to "given a post exists"; every list and update test
    /// starts with one or more of these.
    pub async fn create_post(&self, title: &str, publish: bool) -> Value {
        self.post(&format!("{API}/posts"))
            .author("ada")
            .json(&json!({
                "title": title,
                "body": "…",
                "publish": publish,
            }))
            .send()
            .await
            .assert_status(201)
            .json()
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// One request, under construction.
pub struct RequestBuilder<'a> {
    app: &'a TestApp,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

impl RequestBuilder<'_> {
    /// Add a header.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Present the API key this application seeded at boot.
    #[must_use]
    pub fn key(self) -> Self {
        let key = self.app.api_key.clone();
        self.header(example_crud::auth::API_KEY_HEADER, &key)
    }

    /// Present a *wrong* API key.
    #[must_use]
    pub fn bad_key(self) -> Self {
        self.header(example_crud::auth::API_KEY_HEADER, "nope")
    }

    /// Act as a named author.
    #[must_use]
    pub fn author(self, name: &str) -> Self {
        self.header(example_crud::auth::AUTHOR_HEADER, name)
    }

    /// Act as an editor.
    #[must_use]
    pub fn editor(self) -> Self {
        self.header(example_crud::auth::ROLE_HEADER, "editor")
    }

    /// Send a JSON body.
    #[must_use]
    pub fn json(mut self, body: &Value) -> Self {
        self.body = Some(body.to_string());
        self.header("content-type", "application/json")
    }

    /// Send a body that is not JSON at all.
    #[must_use]
    pub fn raw(mut self, body: &str) -> Self {
        self.body = Some(body.to_owned());
        self.header("content-type", "application/json")
    }

    /// Send it.
    pub async fn send(self) -> TestResponse {
        let mut request = Request::builder()
            .method(self.method.as_str())
            .uri(&self.path);
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let request = request
            .body(self.body.map_or_else(Body::empty, Body::from))
            .expect("a well-formed request");

        let response = self
            .app
            .service
            .clone()
            .oneshot(request)
            .await
            .expect("the service is infallible");

        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 1 << 22)
            .await
            .expect("the body is readable");

        TestResponse {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            method: self.method.to_lowercase(),
            path: self.path,
            document: Arc::clone(&self.app.document),
        }
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// What came back.
pub struct TestResponse {
    /// The status code.
    pub status: StatusCode,
    /// The response headers.
    pub headers: HeaderMap,
    /// The body, as text.
    pub body: String,
    /// The request method, lower-cased, for the document lookup.
    pub method: String,
    /// The request path, concrete, for the document lookup.
    pub path: String,
    /// The document the application published.
    pub document: Arc<Value>,
}

impl TestResponse {
    /// Assert the status, printing the body when it does not match — which is
    /// the difference between "expected 201, got 422" and knowing why.
    #[track_caller]
    pub fn assert_status(self, expected: u16) -> Self {
        assert_eq!(
            self.status.as_u16(),
            expected,
            "{} {} returned {}:\n{}",
            self.method.to_uppercase(),
            self.path,
            self.status,
            self.body
        );
        self
    }

    /// The body, decoded.
    #[must_use]
    #[track_caller]
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("the body is not JSON ({error}):\n{}", self.body))
    }

    /// The value at an RFC 6901 pointer, or `Value::Null`.
    #[must_use]
    #[track_caller]
    pub fn at(&self, pointer: &str) -> Value {
        self.json().pointer(pointer).cloned().unwrap_or(Value::Null)
    }

    /// Assert the value at a pointer.
    #[track_caller]
    pub fn assert_json_at(self, pointer: &str, expected: Value) -> Self {
        assert_eq!(self.at(pointer), expected, "at `{pointer}`:\n{}", self.body);
        self
    }

    /// Assert a response header.
    #[track_caller]
    pub fn assert_header(self, name: &str, expected: &str) -> Self {
        let found = self
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("(absent)");
        assert_eq!(found, expected, "header `{name}`");
        self
    }

    /// Assert that the body matches the schema the document declares for this
    /// operation and this status.
    ///
    /// The assertion a status-code check cannot make. It resolves the concrete
    /// request path back to its templated form, finds the operation, finds the
    /// response, resolves `$ref`s against `components/schemas`, and validates.
    /// An undocumented field is a failure: the drift this exists to catch is a
    /// handler returning something the published contract does not mention.
    #[track_caller]
    pub fn assert_matches_openapi(self) -> Self {
        let template = self.template().unwrap_or_else(|| {
            panic!(
                "the document has no path matching `{}`; the route exists but is undocumented",
                self.path
            )
        });

        let operation = self.document["paths"][&template][&self.method].clone();
        assert!(
            operation.is_object(),
            "the document has no `{} {template}` operation",
            self.method.to_uppercase()
        );

        let status = self.status.as_u16().to_string();
        let responses = &operation["responses"];
        let declared = [status.as_str(), &format!("{}XX", &status[..1]), "default"]
            .into_iter()
            .find_map(|key| responses.get(key))
            .unwrap_or_else(|| {
                panic!(
                    "`{} {template}` does not document a {status} response; it documents {:?}",
                    self.method.to_uppercase(),
                    responses
                        .as_object()
                        .map(|map| map.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                )
            });

        let Some(schema) = declared.pointer("/content/application~1json/schema") else {
            // A documented response with no JSON body — 204, or a problem
            // document declared without a schema. Nothing to validate, and
            // nothing wrong.
            return self;
        };

        if self.body.is_empty() {
            panic!(
                "`{} {template}` documents a {status} body and returned none",
                self.method.to_uppercase()
            );
        }

        let violations = validate(&self.document, schema, &self.json());
        assert!(
            violations.is_empty(),
            "the {status} body of `{} {template}` does not match its published schema:\n  {}\nbody:\n{}",
            self.method.to_uppercase(),
            violations.join("\n  "),
            self.body
        );
        self
    }

    /// The templated path in the document that this request matched.
    fn template(&self) -> Option<String> {
        let path = self.path.split('?').next().unwrap_or_default();
        let actual: Vec<&str> = path.trim_matches('/').split('/').collect();

        self.document["paths"]
            .as_object()?
            .keys()
            .find(|candidate| {
                let expected: Vec<&str> = candidate.trim_matches('/').split('/').collect();
                expected.len() == actual.len()
                    && expected
                        .iter()
                        .zip(&actual)
                        .all(|(expected, actual)| expected.starts_with('{') || expected == actual)
            })
            .cloned()
    }
}

// ---------------------------------------------------------------------------
// The validator
// ---------------------------------------------------------------------------

/// Validate `value` against `schema`, resolving `$ref` against `document`.
///
/// Returns every disagreement rather than the first: a body with three wrong
/// fields should be one test run, not three.
#[must_use]
pub fn validate(document: &Value, schema: &Value, value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    check(document, schema, value, "", &mut out, 0);
    out
}

/// How deep a chain of `$ref`s may go before this gives up. Far beyond any real
/// document; a guard against a cyclic reference.
const MAX_DEPTH: usize = 64;

fn check(
    document: &Value,
    schema: &Value,
    value: &Value,
    at: &str,
    out: &mut Vec<String>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        out.push(format!(
            "{}: schema nests more than {MAX_DEPTH} deep",
            label(at)
        ));
        return;
    }

    // `$ref`, resolved against the document that declared it.
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        match resolve(document, reference) {
            Some(target) => check(document, target, value, at, out, depth + 1),
            None => out.push(format!("{}: `{reference}` does not resolve", label(at))),
        }
        return;
    }

    // A union passes when any arm passes. `Option<T>` and `Either<A, B>` both
    // land here.
    for key in ["oneOf", "anyOf"] {
        if let Some(arms) = schema.get(key).and_then(Value::as_array) {
            let matched = arms
                .iter()
                .any(|arm| validate_nested(document, arm, value, depth + 1));
            if !matched {
                out.push(format!("{}: matches no arm of `{key}`", label(at)));
            }
            return;
        }
    }
    if let Some(arms) = schema.get("allOf").and_then(Value::as_array) {
        for arm in arms {
            check(document, arm, value, at, out, depth + 1);
        }
    }

    check_type(schema, value, at, out);

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        out.push(format!("{}: {value} is not one of {allowed:?}", label(at)));
    }

    match value {
        Value::Object(members) => check_object(document, schema, members, at, out, depth),
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    check(
                        document,
                        item_schema,
                        item,
                        &format!("{at}/{index}"),
                        out,
                        depth + 1,
                    );
                }
            }
        }
        _ => {}
    }
}

/// The declared types this value may have, if the schema says.
fn check_type(schema: &Value, value: &Value, at: &str, out: &mut Vec<String>) {
    let declared: Vec<&str> = match schema.get("type") {
        Some(Value::String(one)) => vec![one.as_str()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        _ => return,
    };

    let matches = declared.iter().any(|name| match *name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    });

    if !matches {
        out.push(format!(
            "{}: expected {}, found {}",
            label(at),
            declared.join(" or "),
            kind_of(value)
        ));
    }
}

/// Required members must be present, and — because this is a contract test —
/// every member present must be one the schema declares.
fn check_object(
    document: &Value,
    schema: &Value,
    members: &serde_json::Map<String, Value>,
    at: &str,
    out: &mut Vec<String>,
    depth: usize,
) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };

    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !members.contains_key(name) {
                out.push(format!("{}: required member `{name}` is absent", label(at)));
            }
        }
    }

    let open = matches!(schema.get("additionalProperties"), Some(Value::Bool(true)))
        || schema
            .get("additionalProperties")
            .is_some_and(Value::is_object);

    for (name, member) in members {
        match properties.get(name) {
            Some(member_schema) => check(
                document,
                member_schema,
                member,
                &format!("{at}/{name}"),
                out,
                depth + 1,
            ),
            None if !open => out.push(format!(
                "{}: member `{name}` is not in the published schema",
                label(at)
            )),
            None => {}
        }
    }
}

/// Whether `value` satisfies `schema`, with no report.
fn validate_nested(document: &Value, schema: &Value, value: &Value, depth: usize) -> bool {
    let mut out = Vec::new();
    check(document, schema, value, "", &mut out, depth);
    out.is_empty()
}

/// Resolve a local `$ref` such as `#/components/schemas/PostOut`.
fn resolve<'a>(document: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    document.pointer(pointer)
}

/// A pointer, or `(root)` for the empty one.
fn label(at: &str) -> &str {
    if at.is_empty() { "(root)" } else { at }
}

/// The JSON type name of a value, for a failure message.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
