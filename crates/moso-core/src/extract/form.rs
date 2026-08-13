//! `Form<T>` — `application/x-www-form-urlencoded` bodies.
//!
//! Same contract as [`Json<T>`](crate::extract::Json): read with a hard cap,
//! deserialise with a field pointer, then validate. The differences are the
//! content type, the flat key space (a form has no nesting beyond the bracket
//! convention [`Query`](crate::extract::Query) uses), and that a `GET` form
//! submission arrives as a query string instead — which is why `Form` is a body
//! extractor only.
//!
//! ```
//! use moso::prelude::*;
//! use moso::response::Redirect;
//!
//! /// The browser form this endpoint accepts.
//! #[derive(Schema)]
//! pub struct LoginForm {
//!     /// Who is signing in.
//!     pub email: Email,
//!     /// Their password.
//!     #[schema(len = 8..=128)]
//!     pub password: String,
//! }
//!
//! /// Sign in and send the browser onwards.
//! #[endpoint]
//! async fn login(Form(creds): Form<LoginForm>) -> Result<Redirect> {
//!     let _ = creds.email;
//!     Ok(Redirect::to("/dashboard"))
//! }
//! # fn main() { assert_eq!(Router::new().post("/login", moso::ep!(login)).len(), 1); }
//! ```
//!
//! # Booleans
//!
//! An HTML checkbox submits `name=on` when ticked and nothing at all when not,
//! so a `bool` field accepts `on`, `true`, `1`, `yes` as true and treats
//! absence as `false`. This is one of the few places Moso is deliberately lax:
//! the alternative is that every form in every browser fails validation.
//!
//! Absence only means `false` for a field that opts into it —
//! `#[serde(default)]`, which `#[derive(Schema)]` emits for a `bool` — because
//! a silently-defaulted field is otherwise indistinguishable from one the
//! client forgot.
//!
//! # Pointers
//!
//! A form *is* the request body, so its pointers are rooted at the document
//! root: `/email`, not `/form/email`. That is what RFC 6901 addresses and what
//! a browser-side form library expects.

use moso_openapi::{ContentType, OperationBuilder, ResponseSpec};
use moso_schema::Schema;

use crate::ctx::RequestCtx;
use crate::error::{Error, Result};
use crate::extract::ExtractBody;
use crate::extract::body::read_limited;
use crate::extract::query::{DeOptions, QueryMap, identity_name};
use crate::response::{Describe, IntoResponse};
use crate::{Request, Response};

/// A urlencoded request body carrying `T`.
///
/// The `application/x-www-form-urlencoded` counterpart of [`Json`](crate::extract::Json):
/// same byte cap, same validation, same `422`. Use it for browser form posts.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::Redirect;
///
/// /// The browser form this endpoint accepts.
/// #[derive(Schema)]
/// pub struct LoginForm {
///     /// Who is signing in.
///     pub email: Email,
///     /// Whether to keep the session alive.
///     pub remember: bool,
/// }
///
/// /// Sign in.
/// #[endpoint]
/// async fn login(Form(creds): Form<LoginForm>) -> Result<Redirect> {
///     let _ = (creds.email, creds.remember);
///     Ok(Redirect::to("/dashboard"))
/// }
/// # fn main() { assert_eq!(Router::new().post("/login", moso::ep!(login)).len(), 1); }
/// ```
///
/// Checkbox values are read leniently — `on`, `true`, `yes` and `1` are all `true`
/// — because that is what browsers actually send.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Form<T>(pub T);

impl<T> Form<T> {
    /// The wrapped value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> core::ops::Deref for Form<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// The JSON Pointer root a form failure is reported under: the document root.
const FORM_POINTER_ROOT: &str = "";

/// The nesting depth a form body is allowed, independent of the URI limit.
///
/// A form arrives in the body rather than the request target, so
/// `http.query_depth_max` — which exists to bound URI parsing — does not
/// apply. The same number is used because the shape is the same.
const FORM_DEPTH_MAX: usize = 8;

impl<T: Schema> ExtractBody for Form<T> {
    fn describe(op: &mut OperationBuilder) {
        op.request_body_of::<T>(ContentType::Form, true);
        op.response(400, ResponseSpec::problem("Malformed form body"));
        op.response(
            413,
            ResponseSpec::problem("The body exceeded `http.body_max`"),
        );
        op.response(
            415,
            ResponseSpec::problem("The `Content-Type` is not a form encoding"),
        );
        if T::HAS_CONSTRAINTS {
            op.response(422, ResponseSpec::validation_problem_of::<T>());
            op.mark_validated();
        }
    }

    async fn extract_body(req: Request, ctx: &RequestCtx) -> Result<Self> {
        if !is_form_content_type(req.headers()) {
            return Err(Error::unsupported_media(
                req.headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("(none)")
                    .to_owned(),
            ));
        }
        let bytes = read_limited(req, ctx.limits().body_max).await?;
        let text = core::str::from_utf8(bytes.as_slice())
            .map_err(|error| Error::bad_request(format!("the form body is not UTF-8: {error}")))?;
        let map = QueryMap::parse(text, FORM_DEPTH_MAX)?;
        let value: T = map.deserialize(DeOptions::FORM, FORM_POINTER_ROOT, identity_name)?;
        let mut validation = ctx.validation(FORM_POINTER_ROOT);
        value.validate(&mut validation).map_err(Error::validation)?;
        Ok(Form(value))
    }
}

impl<T: Schema> IntoResponse for Form<T> {
    fn into_response(self) -> Response {
        match serde_urlencoded::to_string(&self.0) {
            Ok(encoded) => (
                [(http::header::CONTENT_TYPE, ContentType::Form.as_str())],
                encoded,
            )
                .into_response(),
            Err(error) => Error::internal(error).into_response(),
        }
    }
}

impl<T: Schema> Describe for Form<T> {
    fn describe(op: &mut OperationBuilder) {
        op.response(
            200,
            ResponseSpec::deferred_content(ContentType::Form, |generator| {
                generator.subschema_for::<T>()
            })
            .description(format!("`{}`, form-encoded", T::schema_name())),
        );
    }
}

/// Whether a `Content-Type` names a form encoding this extractor reads.
///
/// A missing header is accepted for the same reason [`Json`](crate::extract::Json)
/// accepts one: clients omit it and refusing them buys nothing.
pub fn is_form_content_type(headers: &http::HeaderMap) -> bool {
    let Some(value) = headers.get(http::header::CONTENT_TYPE) else {
        return true;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let essence = value.split(';').next().unwrap_or("").trim();
    essence.is_empty() || essence.eq_ignore_ascii_case("application/x-www-form-urlencoded")
}

/// The values a form `bool` field accepts as true.
///
/// Absence is false; anything not in this list is a 422 rather than a silent
/// false, because a typo that silently disables a setting is worse than an error.
pub const TRUTHY_FORM_VALUES: &[&str] = &["on", "true", "1", "yes", "y"];

/// Whether a raw form value means `true`. Case-insensitive.
pub fn is_truthy(value: &str) -> bool {
    TRUTHY_FORM_VALUES
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn decode<T: serde::de::DeserializeOwned>(body: &str) -> Result<T> {
        let map = QueryMap::parse(body, FORM_DEPTH_MAX)?;
        map.deserialize(DeOptions::FORM, FORM_POINTER_ROOT, identity_name)
    }

    #[test]
    fn checkbox_values_are_truthy() {
        assert!(is_truthy("on"));
        assert!(is_truthy("TRUE"));
        assert!(!is_truthy("off"));
    }

    #[test]
    fn form_content_types_are_recognised() {
        let mut headers = http::HeaderMap::new();
        assert!(is_form_content_type(&headers));
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        assert!(is_form_content_type(&headers));
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        assert!(!is_form_content_type(&headers));
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Login {
        email: String,
        password: String,
        #[serde(default)]
        remember_me: bool,
    }

    #[test]
    fn a_ticked_checkbox_is_true_and_an_absent_one_is_false() {
        assert_eq!(
            decode::<Login>("email=a%40b.com&password=hunter2&remember_me=on").unwrap(),
            Login {
                email: "a@b.com".into(),
                password: "hunter2".into(),
                remember_me: true,
            }
        );
        assert!(
            !decode::<Login>("email=a%40b.com&password=hunter2")
                .unwrap()
                .remember_me
        );
    }

    #[test]
    fn a_form_bool_does_not_treat_an_empty_value_as_true() {
        // A query string's `?flag` means "set"; a form's `flag=` is a control
        // that submitted nothing, which is not the same claim.
        #[derive(Debug, Deserialize)]
        struct Flag {
            flag: bool,
        }
        assert!(!decode::<Flag>("flag=").unwrap().flag);
        assert!(decode::<Flag>("flag=on").unwrap().flag);
    }

    #[test]
    fn repeated_fields_collect_into_a_vec() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Selection {
            option: Vec<String>,
        }
        assert_eq!(
            decode::<Selection>("option=a&option=b").unwrap(),
            Selection {
                option: vec!["a".into(), "b".into()]
            }
        );
    }
}
