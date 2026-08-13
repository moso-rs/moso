//! [`Redacted<T>`] — the response that honours a decision's obligations.
//!
//! An [`Obligation`](crate::Obligation) is only worth having if something acts
//! on it. This is that something: the handler returns `Redacted<PostOut>`
//! instead of `Json<PostOut>`, and the fields the policy said to remove are
//! removed on the way out.
//!
//! That is what makes "managers see salaries, peers do not" one policy and one
//! response type instead of two DTOs and a branch — and, more importantly, what
//! makes it impossible to forget: the obligation travels with the decision that
//! produced it, from [`Authorized`](crate::Authorized) to the wire.

use moso_core::response::Describe;
use moso_core::{IntoResponse, Response};
use moso_openapi::{OperationBuilder, ResponseSpec};
use moso_schema::Schema;

use crate::Decision;

/// A response body with a decision's obligations applied.
///
/// ```text
/// #[endpoint]
/// async fn show(post: Authorized<Read, Post>) -> Result<Redacted<PostOut>> {
///     let (post, decision) = post.into_parts();
///     Ok(Redacted::new(PostOut::from(post), decision))
/// }
/// ```
///
/// # Why the whole body is serialised twice
///
/// Applying a JSON Pointer needs a JSON tree, so the value is serialised to
/// `serde_json::Value`, edited, and written out. A response with no obligations
/// skips that entirely and serialises once, which is the overwhelmingly common
/// path — the cost is paid only by the responses that actually redact.
pub struct Redacted<T> {
    /// The body, before redaction.
    value: T,
    /// The decision whose obligations apply.
    decision: Decision,
    /// The status to return. 200 unless the handler says otherwise.
    status: http::StatusCode,
}

impl<T> Redacted<T> {
    /// A body carrying a decision's obligations.
    ///
    /// ```
    /// use moso_authz::{Decision, Obligation, Redacted};
    ///
    /// let decision = Decision::allow("peer").with_obligation(Obligation::redact("/salary"));
    /// let _ = Redacted::new("body", decision);
    /// ```
    #[must_use]
    pub fn new(value: T, decision: Decision) -> Self {
        Self {
            value,
            decision,
            status: http::StatusCode::OK,
        }
    }

    /// A body with no obligations, for the branch where none apply.
    ///
    /// ```
    /// use moso_authz::Redacted;
    ///
    /// let _ = Redacted::plain("body");
    /// ```
    #[must_use]
    pub fn plain(value: T) -> Self {
        Self::new(value, Decision::allow("no obligations"))
    }

    /// Return a status other than 200.
    ///
    /// ```
    /// use moso_authz::Redacted;
    ///
    /// let response = Redacted::plain("body").with_status(http::StatusCode::CREATED);
    /// assert_eq!(response.status(), http::StatusCode::CREATED);
    /// ```
    #[must_use]
    pub fn with_status(mut self, status: http::StatusCode) -> Self {
        self.status = status;
        self
    }

    /// The status this will return.
    ///
    /// ```
    /// use moso_authz::Redacted;
    ///
    /// assert_eq!(Redacted::plain(1).status(), http::StatusCode::OK);
    /// ```
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        self.status
    }

    /// The decision whose obligations apply.
    ///
    /// ```
    /// use moso_authz::Redacted;
    ///
    /// assert!(Redacted::plain(1).decision().allowed());
    /// ```
    #[must_use]
    pub fn decision(&self) -> &Decision {
        &self.decision
    }
}

impl<T: Schema> Redacted<T> {
    /// The body as it will be sent, with the obligations already applied.
    ///
    /// Exposed so a test can assert on the redaction without going through a
    /// whole request — which is what makes the snapshot test in this crate's
    /// suite short enough to read.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] when the body cannot be serialised, which for a
    /// `Schema` type means a `Serialize` impl that fails.
    pub fn to_json(&self) -> Result<serde_json::Value, serde_json::Error> {
        let mut body = serde_json::to_value(&self.value)?;
        self.decision.apply_obligations(&mut body);
        Ok(body)
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Redacted")
            .field("value", &self.value)
            .field("obligations", &self.decision.obligations())
            .finish()
    }
}

impl<T: Schema> IntoResponse for Redacted<T> {
    fn into_response(self) -> Response {
        if self.decision.obligations().is_empty() {
            return moso_core::response::json_response(self.status, &self.value);
        }
        match self.to_json() {
            Ok(body) => {
                let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
                (
                    self.status,
                    [(
                        http::header::CONTENT_TYPE,
                        http::HeaderValue::from_static("application/json"),
                    )],
                    bytes,
                )
                    .into_response()
            }
            Err(error) => moso_core::Error::internal(error)
                .with_detail("the response body could not be serialised")
                .into_response(),
        }
    }
}

impl<T: Schema> Describe for Redacted<T> {
    fn describe(op: &mut OperationBuilder) {
        op.response(200, ResponseSpec::json_of::<T>().description(
            "The resource. Fields the authorization policy attached a redaction obligation to are \
             absent from the body; which ones depends on the caller.",
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Obligation;

    /// A body with a field a peer must not see.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct Employee {
        name: String,
        salary: u32,
        card: String,
    }

    impl moso_schema::Validate for Employee {
        fn validate(
            &self,
            _ctx: &mut moso_schema::ValidationCtx,
        ) -> Result<(), moso_schema::ValidationErrors> {
            Ok(())
        }
    }

    impl Schema for Employee {
        fn schema_name() -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("Employee")
        }

        fn json_schema(
            _generator: &mut moso_schema::json_schema::SchemaGenerator,
        ) -> moso_schema::json_schema::SchemaNode {
            // The shape does not matter here: what is under test is what the
            // *body* looks like after the obligations run.
            moso_schema::json_schema::SchemaNode::any()
        }
    }

    fn employee() -> Employee {
        Employee {
            name: "Ada".to_owned(),
            salary: 120_000,
            card: "4242424242424242".to_owned(),
        }
    }

    #[test]
    fn a_redaction_removes_the_field_rather_than_nulling_it() {
        let decision = Decision::allow("peer").with_obligation(Obligation::redact("/salary"));
        let body = Redacted::new(employee(), decision).to_json().expect("json");

        assert!(
            body.get("salary").is_none(),
            "a null still says the field exists: {body}"
        );
        assert_eq!(body["name"], "Ada");
    }

    #[test]
    fn a_mask_keeps_the_documented_number_of_trailing_characters() {
        let decision = Decision::allow("self").with_obligation(Obligation::mask("/card", 4));
        let body = Redacted::new(employee(), decision).to_json().expect("json");

        assert_eq!(body["card"], "••••••••••••4242");
    }

    #[test]
    fn an_obligation_on_an_absent_field_is_satisfied_vacuously() {
        let decision = Decision::allow("peer").with_obligation(Obligation::redact("/bonus"));
        let body = Redacted::new(employee(), decision).to_json().expect("json");

        assert_eq!(body["salary"], 120_000);
    }

    #[test]
    fn a_custom_obligation_changes_nothing_on_the_wire() {
        let decision = Decision::allow("peer").with_obligation(Obligation::Custom {
            key: "reauthenticate".to_owned(),
            value: serde_json::json!({ "within": "5m" }),
        });
        let body = Redacted::new(employee(), decision).to_json().expect("json");

        assert_eq!(body["salary"], 120_000);
    }

    #[test]
    fn a_response_with_no_obligations_is_the_plain_body() {
        let body = Redacted::plain(employee()).to_json().expect("json");
        assert_eq!(body["salary"], 120_000);
    }
}
