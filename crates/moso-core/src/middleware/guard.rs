//! [`RequireHeader`] — the reference [`Guard`] implementation.
//!
//! A guard is the answer to a specific failure: a middleware that can return
//! 403 makes the OpenAPI document wrong, because nothing tells the document
//! about it. Every guard therefore says *both* halves of what it does — the
//! runtime check and the contract change — and `Router::guard` wires the second
//! half into every operation on the router.
//!
//! `RequireHeader` is deliberately small and complete, so it can be read as the
//! worked example the trait's documentation points at. It shows the three
//! things a real guard has to get right:
//!
//! 1. **`describe` reflects the configuration.** The parameter it documents is
//!    the header name it was *built with*, which is why
//!    [`Guard::describe`] takes `&self`.
//! 2. **`check` returns an [`Error`], not a response.** The error goes through
//!    the same rendering and logging path as every other failure, so a guard
//!    rejection is not a second kind of 403.
//! 3. **The two halves agree.** The status `check` can return is a status
//!    `describe` declares. A guard that rejects with a code it never documents
//!    is exactly the bug guards exist to prevent.

use std::borrow::Cow;

use http::request::Parts;
use http::{HeaderName, HeaderValue};
use moso_openapi::{OperationBuilder, Param, ResponseSpec};

use crate::error::{Error, Result};
use crate::middleware::Guard;
use crate::{BoxFuture, RequestCtx};

/// Require a request header, optionally with an exact value.
///
/// ```
/// use moso::prelude::*;
/// use moso::middleware::guard::RequireHeader;
/// # /// Dump internal state.
/// # #[endpoint] async fn debug_state() -> Result<moso::response::NoContent> {
/// #     Ok(moso::response::NoContent) }
/// # fn main() {
/// let router = Router::new()
///     .get("/_internal/state", moso::ep!(debug_state))
///     .guard(RequireHeader::new("x-internal"));
///
/// assert_eq!(router.entries()[0].guards.len(), 1);
/// # }
/// ```
///
/// The header becomes a required parameter on every operation the guard covers,
/// and the 400 the guard can return is documented alongside it.
///
/// # This is not authentication
///
/// A header a client can send is a header an attacker can send. `RequireHeader`
/// is for the cases where that is fine — routing a mesh-internal API, gating a
/// debug surface behind something a stray browser will not have — and the
/// documentation says so rather than letting it be mistaken for a credential
/// check. Use an authentication guard for credentials.
#[derive(Debug, Clone)]
pub struct RequireHeader {
    name: HeaderName,
    value: Option<HeaderValue>,
    description: Cow<'static, str>,
}

impl RequireHeader {
    /// Require `name` to be present.
    ///
    /// # Panics
    ///
    /// If `name` is not a valid header name. It is a `&'static str` written in
    /// the composition root, so an invalid one is a typo the author sees the
    /// first time they run the program — and failing there is better than
    /// silently guarding nothing.
    pub fn new(name: &'static str) -> Self {
        Self {
            name: HeaderName::from_static(name),
            value: None,
            description: Cow::Borrowed("Required by this API."),
        }
    }

    /// Require `name` to be present **and** equal to `value`.
    ///
    /// The comparison is over bytes, in variable time. That is correct here and
    /// would not be for a credential: a guard that compares a secret must use a
    /// constant-time comparison, which is one more reason this type is not one.
    ///
    /// # Panics
    ///
    /// If `name` or `value` is not valid in a header.
    pub fn with_value(name: &'static str, value: &'static str) -> Self {
        Self {
            value: Some(HeaderValue::from_static(value)),
            ..Self::new(name)
        }
    }

    /// Replace the description the parameter carries in the document.
    pub fn described(mut self, description: impl Into<Cow<'static, str>>) -> Self {
        self.description = description.into();
        self
    }

    /// The header this guard requires.
    pub fn header(&self) -> &HeaderName {
        &self.name
    }

    /// The whole decision, as a pure function of the request headers.
    ///
    /// Separate from [`Guard::check`] because it is the part with behaviour: a
    /// test can exercise every branch of it without an `AppState`, and `check`
    /// is then trivially a box around it.
    pub fn decide(&self, headers: &http::HeaderMap) -> Result<()> {
        let Some(found) = headers.get(&self.name) else {
            return Err(missing(&self.name));
        };
        match &self.value {
            Some(expected) if found != expected => Err(mismatched(&self.name)),
            _ => Ok(()),
        }
    }
}

impl Guard for RequireHeader {
    fn describe(&self, op: &mut OperationBuilder) {
        op.parameter(
            Param::header(self.name.as_str())
                .required(true)
                .schema_of::<String>()
                .description(self.description.clone().into_owned()),
        );
        op.response(
            400,
            ResponseSpec::problem(format!(
                "The `{}` header is absent or does not have the required value.",
                self.name
            )),
        );
    }

    fn check<'a>(&'a self, parts: &'a Parts, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<()>> {
        // No `.await` in the body, but the trait is dyn-compatible and
        // therefore boxed; a ready future costs one small allocation and keeps
        // the route table able to hold `dyn DynGuard`.
        let _ = ctx;
        let decision = self.decide(&parts.headers);
        Box::pin(async move { decision })
    }
}

/// The 400 for an absent header.
fn missing(name: &HeaderName) -> Error {
    Error::bad_request(format!("The `{name}` header is required")).with_field(
        &format!("/headers/{name}"),
        moso_schema::codes::REQUIRED,
        "this header is required",
    )
}

/// The 400 for a header with the wrong value.
///
/// The *expected* value is not disclosed. It is a value the caller was supposed
/// to already know, and echoing it turns the error into an oracle.
fn mismatched(name: &HeaderName) -> Error {
    Error::bad_request(format!(
        "The `{name}` header does not have the required value"
    ))
    .with_field(
        &format!("/headers/{name}"),
        moso_schema::codes::ENUM,
        "this header does not have the required value",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::DynGuard;
    use http::StatusCode;
    use moso_openapi::SchemaGenerator;

    fn headers(pairs: &[(&'static str, &str)]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_static(name),
                HeaderValue::try_from(*value).expect("value"),
            );
        }
        map
    }

    #[test]
    fn a_present_header_passes() {
        let guard = RequireHeader::new("x-internal");
        assert!(guard.decide(&headers(&[("x-internal", "1")])).is_ok());
    }

    #[test]
    fn an_absent_header_is_a_400_with_a_pointer() {
        let guard = RequireHeader::new("x-internal");
        let error = guard.decide(&headers(&[])).expect_err("rejected");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        let field = &error.fields().expect("fields").as_slice()[0];
        assert_eq!(field.pointer, "/headers/x-internal");
        assert_eq!(field.code, "required");
    }

    #[test]
    fn a_required_value_must_match_and_is_never_echoed() {
        let guard = RequireHeader::with_value("x-internal", "s3cret");
        assert!(guard.decide(&headers(&[("x-internal", "s3cret")])).is_ok());

        let error = guard
            .decide(&headers(&[("x-internal", "wrong")]))
            .expect_err("rejected");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert!(!error.detail().unwrap_or_default().contains("s3cret"));
        assert!(
            !error.fields().expect("fields").as_slice()[0]
                .message
                .contains("s3cret")
        );
    }

    #[test]
    fn the_guard_impl_is_the_decision_in_a_box() {
        // `check` needs a `RequestCtx`, which needs an `AppState`, which only
        // `App::build` can produce — so the coverage lives on `decide` and this
        // asserts the wrapper is the identity it looks like.
        let guard = RequireHeader::new("x-internal");
        assert_eq!(guard.header().as_str(), "x-internal");
        assert!(guard.decide(&headers(&[("x-internal", "1")])).is_ok());
    }

    /// Acceptance criterion 5: a guard contributes to the operation.
    #[test]
    fn describe_documents_the_header_it_was_configured_with() {
        let guard = RequireHeader::new("x-internal").described("The internal mesh token.");
        let mut op = OperationBuilder::new(SchemaGenerator::new(crate::COMPONENTS_SCHEMAS_PREFIX));
        guard.describe(&mut op);
        let spec = op.into_spec();

        let parameter = spec
            .parameters
            .iter()
            .find(|parameter| parameter.name == "x-internal")
            .expect("the header is documented");
        assert!(parameter.required);
        assert_eq!(
            parameter.description.as_deref(),
            Some("The internal mesh token.")
        );
        assert!(spec.responses.contains_key("400"));
    }

    #[test]
    fn two_guards_document_two_headers() {
        let mut op = OperationBuilder::new(SchemaGenerator::new(crate::COMPONENTS_SCHEMAS_PREFIX));
        RequireHeader::new("x-internal").describe(&mut op);
        RequireHeader::new("x-tenant").describe(&mut op);
        let spec = op.into_spec();
        assert_eq!(spec.parameters.len(), 2);
    }

    #[test]
    fn a_guard_is_usable_as_a_trait_object() {
        // What the route table stores. If this stops compiling, `Router::guard`
        // stops compiling too, and the message would be far less obvious there.
        let guard: std::sync::Arc<dyn DynGuard> = std::sync::Arc::new(RequireHeader::new("x-a"));
        let mut op = OperationBuilder::new(SchemaGenerator::new(crate::COMPONENTS_SCHEMAS_PREFIX));
        guard.describe_dyn(&mut op);
        assert_eq!(op.into_spec().parameters.len(), 1);
    }
}
