//! `#[derive(Responder)]` — a response type that controls its own status and
//! headers.
//!
//! ```
//! use moso::prelude::*;
//!
//! /// A user that has just been created.
//! #[derive(Schema, Responder)]
//! #[responder(status = 201, header(location = "self.url"))]
//! pub struct UserCreated {
//!     /// Sent as `Location`, not in the body.
//!     #[serde(skip)]
//!     pub url: String,
//!     /// Stable identifier.
//!     pub id: u64,
//!     /// Contact address.
//!     pub email: Email,
//! }
//! # fn main() {
//! let response = UserCreated {
//!     url: "/users/7".to_owned(),
//!     id: 7,
//!     email: "ada@example.com".parse().unwrap(),
//! }
//! .into_response();
//! assert_eq!(response.status(), 201);
//! # }
//! ```
//!
//! The derive emits `IntoResponse` and `Describe` from the same attribute, so
//! the status a handler *sends* and the status the OpenAPI document *claims*
//! cannot disagree — the property that makes the whole zero-annotation
//! document trustworthy.
//!
//! Without it, a non-response type in return position gets the hand-written
//! `Describe` diagnostic in `moso-core`, which names this derive as one of
//! three fixes (`docs/01-http/12-extractors-responses.md`).

use darling::util::SpannedValue;
use darling::{FromDeriveInput, FromMeta, ast};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{DeriveInput, Expr, Ident};

use crate::config::support::{Diagnostic, moso, sentence_case};
use crate::util::attrs::doc_text;

/// The status codes `http::StatusCode` names with an associated constant.
///
/// Generated code says `StatusCode::CREATED` rather than
/// `StatusCode::from_u16(201).unwrap()`: it is what a reader of `cargo expand`
/// wants to see, and it removes a `Result` from a path that cannot fail.
const STATUS_CONSTANTS: &[(u16, &str)] = &[
    (100, "CONTINUE"),
    (101, "SWITCHING_PROTOCOLS"),
    (200, "OK"),
    (201, "CREATED"),
    (202, "ACCEPTED"),
    (203, "NON_AUTHORITATIVE_INFORMATION"),
    (204, "NO_CONTENT"),
    (205, "RESET_CONTENT"),
    (206, "PARTIAL_CONTENT"),
    (300, "MULTIPLE_CHOICES"),
    (301, "MOVED_PERMANENTLY"),
    (302, "FOUND"),
    (303, "SEE_OTHER"),
    (304, "NOT_MODIFIED"),
    (307, "TEMPORARY_REDIRECT"),
    (308, "PERMANENT_REDIRECT"),
    (400, "BAD_REQUEST"),
    (401, "UNAUTHORIZED"),
    (402, "PAYMENT_REQUIRED"),
    (403, "FORBIDDEN"),
    (404, "NOT_FOUND"),
    (405, "METHOD_NOT_ALLOWED"),
    (406, "NOT_ACCEPTABLE"),
    (409, "CONFLICT"),
    (410, "GONE"),
    (412, "PRECONDITION_FAILED"),
    (413, "PAYLOAD_TOO_LARGE"),
    (415, "UNSUPPORTED_MEDIA_TYPE"),
    (422, "UNPROCESSABLE_ENTITY"),
    (429, "TOO_MANY_REQUESTS"),
    (500, "INTERNAL_SERVER_ERROR"),
    (501, "NOT_IMPLEMENTED"),
    (502, "BAD_GATEWAY"),
    (503, "SERVICE_UNAVAILABLE"),
    (504, "GATEWAY_TIMEOUT"),
];

/// Whether RFC 9110 forbids a body at this status.
///
/// `moso_core::response::json_response` drops the body anyway; knowing it here
/// is what lets `Describe` document the response as empty rather than claiming
/// a schema nobody will ever receive.
fn forbids_body(status: u16) -> bool {
    status == 204 || status == 304 || (100..200).contains(&status)
}

// ---------------------------------------------------------------------------
// Attribute model
// ---------------------------------------------------------------------------

/// One `header(name = "expression")` group.
///
/// The value is a Rust expression with `self` in scope, so a header can be
/// built from a field the body does not serialise — which is exactly why
/// `UserCreated` carries `#[serde(skip)] url`.
#[derive(Debug, Default)]
struct HeaderGroup(Vec<(SpannedValue<String>, SpannedValue<String>)>);

impl FromMeta for HeaderGroup {
    fn from_list(items: &[ast::NestedMeta]) -> darling::Result<Self> {
        let mut headers = Vec::with_capacity(items.len());
        let mut errors = darling::Error::accumulator();

        for item in items {
            let ast::NestedMeta::Meta(syn::Meta::NameValue(pair)) = item else {
                errors.push(
                    darling::Error::custom(
                        "expected `name = \"expression\"`, as in `header(location = \"self.url\")`",
                    )
                    .with_span(item),
                );
                continue;
            };
            let Some(name) = pair.path.get_ident() else {
                errors.push(darling::Error::custom("expected a header name").with_span(&pair.path));
                continue;
            };
            let Expr::Lit(literal) = &pair.value else {
                errors.push(
                    darling::Error::custom(
                        "the value must be a string holding a Rust expression, as in \
                         `header(location = \"self.url\")`",
                    )
                    .with_span(&pair.value),
                );
                continue;
            };
            let syn::Lit::Str(text) = &literal.lit else {
                errors.push(darling::Error::unexpected_lit_type(&literal.lit).with_span(literal));
                continue;
            };
            headers.push((
                SpannedValue::new(name.to_string(), name.span()),
                SpannedValue::new(text.value(), text.span()),
            ));
        }

        errors.finish()?;
        Ok(Self(headers))
    }
}

/// The `#[derive(Responder)]` input.
#[derive(Debug, FromDeriveInput)]
// No `supports(..)`: darling's shape rejection would win the race and print
// "Unsupported shape `enum`", which names no fix. The hand-written check in
// `build` points at `Either` instead.
#[darling(attributes(responder), forward_attrs(doc))]
struct ResponderInput {
    /// The type's name.
    ident: Ident,
    /// Generics, passed through.
    generics: syn::Generics,
    /// The fields — unused, but darling needs somewhere to put them.
    #[allow(dead_code)]
    data: ast::Data<darling::util::Ignored, darling::util::Ignored>,
    /// Forwarded `///` documentation, which becomes the response description.
    attrs: Vec<syn::Attribute>,
    /// `#[responder(status = 201)]`. Defaults to 200.
    #[darling(default)]
    status: Option<SpannedValue<u16>>,
    /// `#[responder(description = "…")]` for the OpenAPI response.
    #[darling(default)]
    description: Option<String>,
    /// `#[responder(header(location = "self.url"))]`, repeatable.
    #[darling(default, multiple)]
    header: Vec<HeaderGroup>,
}

/// One header, validated.
struct Header {
    /// The wire name, lower case: `location`, `x-request-id`.
    name: String,
    /// The local the rendered value is bound to.
    binding: Ident,
    /// The expression producing the value.
    expression: Expr,
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

/// Expand `#[derive(Responder)]`.
///
/// Wire this up from `lib.rs` with
/// `#[proc_macro_derive(Responder, attributes(responder))]`.
pub(crate) fn expand(input: DeriveInput) -> TokenStream {
    let ident = input.ident.clone();
    let generics = input.generics.clone();

    let parsed = match ResponderInput::from_derive_input(&input) {
        Ok(parsed) => parsed,
        Err(error) => return with_placeholder(&ident, &generics, error.write_errors()),
    };

    match build(&parsed) {
        Ok(tokens) => tokens,
        Err(error) => with_placeholder(&ident, &generics, error.to_compile_error()),
    }
}

/// The impls that keep one bad attribute from becoming a page of errors.
fn with_placeholder(ident: &Ident, generics: &syn::Generics, errors: TokenStream) -> TokenStream {
    let moso = moso();
    let (impl_generics, type_generics, where_clause) = generics.split_for_impl();
    quote! {
        #errors

        #[automatically_derived]
        impl #impl_generics #moso::IntoResponse for #ident #type_generics #where_clause {
            fn into_response(self) -> #moso::Response {
                ::core::unimplemented!()
            }
        }

        #[automatically_derived]
        impl #impl_generics #moso::Describe for #ident #type_generics #where_clause {
            fn describe(_op: &mut #moso::OperationBuilder) {}
        }
    }
}

/// Build both impls, or the first problem found.
fn build(input: &ResponderInput) -> syn::Result<TokenStream> {
    let moso = moso();
    let ident = &input.ident;

    if !matches!(input.data, ast::Data::Struct(_)) {
        return Err(Diagnostic::new("`#[derive(Responder)]` needs a struct")
            .note("an enum would need one status per variant, which is a different response shape")
            .help_code(
                "return one of two shapes with `Either`, which documents as a `oneOf`:",
                "async fn get() -> Result<Either<UserOut, Redirect>> { /* … */ }",
            )
            .at(ident));
    }

    let status = input.status.as_ref().map_or(200, |status| **status);
    let status_span = input
        .status
        .as_ref()
        .map_or_else(|| ident.span(), darling::util::SpannedValue::span);
    if !(100..=999).contains(&status) {
        return Err(
            Diagnostic::new(format!("`{status}` is not an HTTP status code"))
                .note("a status is three digits, between 100 and 599 in practice")
                .help_code("use a real status:", "#[responder(status = 201)]")
                .at_span(status_span),
        );
    }

    let headers = collect_headers(input)?;
    let description = input
        .description
        .clone()
        .or_else(|| doc_text(&input.attrs))
        .unwrap_or_else(|| sentence_case(&ident.to_string()));

    let into_response = into_response_impl(input, status, &headers, &moso);
    let describe = describe_impl(input, status, &headers, &description, &moso);
    let assertions = assertions(input, status, &moso);

    Ok(quote! {
        #into_response
        #describe
        #assertions
    })
}

/// Validate every `header(..)` entry and parse its expression.
fn collect_headers(input: &ResponderInput) -> syn::Result<Vec<Header>> {
    let mut headers: Vec<Header> = Vec::new();

    for (index, (name, expression)) in input.header.iter().flat_map(|group| &group.0).enumerate() {
        // `x_request_id` is how a header is spelled inside an attribute, since
        // `x-request-id` is not an identifier. The wire name is the dashed one.
        let wire = name.replace('_', "-").to_ascii_lowercase();
        if let Some(problem) = invalid_header_name(&wire) {
            return Err(
                Diagnostic::new(format!("`{wire}` is not a valid header name"))
                    .note(problem)
                    .help_code(
                        "use a token header name, with `_` standing in for `-`:",
                        "#[responder(header(x_request_id = \"self.request_id\"))]",
                    )
                    .at_span(name.span()),
            );
        }
        if headers.iter().any(|header| header.name == wire) {
            return Err(Diagnostic::new(format!("the `{wire}` header is set twice"))
                .note("the second value would silently replace the first")
                .help("keep one `header(..)` entry per header name")
                .at_span(name.span()));
        }

        let parsed: Expr = syn::parse_str(expression).map_err(|error| {
            Diagnostic::new(format!(
                "the `{wire}` header value is not a Rust expression: {error}"
            ))
            .note("the value is evaluated with `self` in scope, before the body is serialised")
            .help_code(
                "read a field:",
                format!("#[responder(header({name} = \"self.url\"))]", name = **name),
            )
            .at_span(expression.span())
        })?;

        headers.push(Header {
            name: wire,
            binding: format_ident!("__moso_header_{}", index),
            expression: parsed,
        });
    }

    Ok(headers)
}

/// Why a header name is unusable, or `None` when it is fine.
fn invalid_header_name(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("a header name cannot be empty");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Some("a header name is made of letters, digits and `-`");
    }
    None
}

/// The `StatusCode` expression for `status`.
fn status_tokens(status: u16, moso: &TokenStream) -> TokenStream {
    match STATUS_CONSTANTS
        .iter()
        .find(|(code, _)| *code == status)
        .map(|(_, name)| Ident::new(name, Span::call_site()))
    {
        Some(constant) => quote!(#moso::http::StatusCode::#constant),
        // A status the `http` crate does not name — legal, and rare enough that
        // one `unwrap_or` on a value the macro has already range-checked is a
        // better trade than a hand-maintained table of all 900 of them.
        None => quote! {
            #moso::http::StatusCode::from_u16(#status)
                .unwrap_or(#moso::http::StatusCode::INTERNAL_SERVER_ERROR)
        },
    }
}

/// `IntoResponse`: render the headers, serialise the body, set the headers.
fn into_response_impl(
    input: &ResponderInput,
    code: u16,
    headers: &[Header],
    moso: &TokenStream,
) -> TokenStream {
    let ident = &input.ident;
    let (impl_generics, type_generics, where_clause) = split_for_impl(input, code, moso);
    let status = status_tokens(code, moso);

    // Headers are rendered *before* the body is serialised, because
    // `json_response` borrows `self` and a header expression reading a
    // `#[serde(skip)]` field has to run while that field is still there.
    let renders = headers.iter().map(|header| {
        let binding = &header.binding;
        let expression = &header.expression;
        quote!(let #binding = ::std::string::ToString::to_string(&(#expression));)
    });
    let sets = headers.iter().map(|header| {
        let binding = &header.binding;
        let name = &header.name;
        quote! {
            #moso::set_header(
                &mut __moso_response,
                #moso::http::HeaderName::from_static(#name),
                &#binding,
            );
        }
    });
    let mutability = if headers.is_empty() {
        quote!()
    } else {
        quote!(mut)
    };
    // A 204 or a 304 carries no body by definition. `json_response` drops one
    // anyway, but going straight to `empty_response` means the type is not
    // required to implement `Schema` just to say "done".
    let body = if forbids_body(code) {
        quote!(#moso::empty_response(#status))
    } else {
        quote!(#moso::json_response(#status, &self))
    };

    quote! {
        #[automatically_derived]
        impl #impl_generics #moso::IntoResponse for #ident #type_generics #where_clause {
            fn into_response(self) -> #moso::Response {
                #(#renders)*
                let #mutability __moso_response = #body;
                #(#sets)*
                __moso_response
            }
        }
    }
}

/// `Describe`: the same status, the same headers, the body's schema.
fn describe_impl(
    input: &ResponderInput,
    status: u16,
    headers: &[Header],
    description: &str,
    moso: &TokenStream,
) -> TokenStream {
    let ident = &input.ident;
    let (impl_generics, type_generics, where_clause) = split_for_impl(input, status, moso);

    let base = if forbids_body(status) {
        quote!(#moso::ResponseSpec::empty(#description))
    } else {
        quote!(#moso::ResponseSpec::json_of::<Self>().description(#description))
    };

    let schema = if headers.is_empty() {
        quote!()
    } else {
        // One node, cloned per header: every header a responder sets is
        // rendered with `ToString`, so every one of them is a string.
        quote! {
            let __moso_header_schema =
                __op.generator().subschema_for::<::std::string::String>();
        }
    };
    let attach = headers.iter().map(|header| {
        let name = &header.name;
        quote!(.header(#name, ::core::clone::Clone::clone(&__moso_header_schema)))
    });

    quote! {
        #[automatically_derived]
        impl #impl_generics #moso::Describe for #ident #type_generics #where_clause {
            fn describe(__op: &mut #moso::OperationBuilder) {
                #schema
                let __moso_spec = #base #(#attach)*;
                __op.response(#status, __moso_spec);
            }
        }
    }
}

/// The impl generics, with `Self: Schema` added for a generic body type.
///
/// A non-generic responder is left bare so that a missing `#[derive(Schema)]`
/// produces the hand-written `Schema` diagnostic rather than "does not
/// implement `IntoResponse`", which names the wrong fix.
fn split_for_impl<'a>(
    input: &'a ResponderInput,
    status: u16,
    moso: &TokenStream,
) -> (
    syn::ImplGenerics<'a>,
    syn::TypeGenerics<'a>,
    Option<TokenStream>,
) {
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();
    let needs_bound = !input.generics.params.is_empty() && !forbids_body(status);
    let where_clause = match (where_clause, needs_bound) {
        (Some(existing), true) => Some(quote!(#existing, Self: #moso::Schema)),
        (Some(existing), false) => Some(quote!(#existing)),
        (None, true) => Some(quote!(where Self: #moso::Schema)),
        (None, false) => None,
    };
    (impl_generics, type_generics, where_clause)
}

/// Assertion codegen: point the missing-`Schema` error at the user's type.
fn assertions(input: &ResponderInput, status: u16, moso: &TokenStream) -> TokenStream {
    if !input.generics.params.is_empty() || forbids_body(status) {
        return quote!();
    }
    let ident = &input.ident;
    quote! {
        #[doc(hidden)]
        const _: () = {
            fn __moso_assert_schema<T: #moso::Schema>() {}
            fn __moso_check() {
                __moso_assert_schema::<#ident>();
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expand a derive input and return the tokens as a searchable string.
    fn expand_str(input: TokenStream) -> String {
        let parsed: DeriveInput = syn::parse2(input).expect("a derive input");
        expand(parsed).to_string()
    }

    #[test]
    fn the_documented_example_expands() {
        let out = expand_str(quote! {
            #[responder(status = 201, header(location = "self.url"))]
            struct UserCreated {
                #[serde(skip)]
                url: String,
                id: Uuid,
            }
        });
        assert!(out.contains("impl :: moso :: __private :: IntoResponse for UserCreated"));
        assert!(out.contains("StatusCode :: CREATED"), "{out}");
        assert!(
            out.contains("ToString :: to_string (& (self . url))"),
            "{out}"
        );
        assert!(
            out.contains("HeaderName :: from_static (\"location\")"),
            "{out}"
        );
        assert!(out.contains("__op . response (201u16"), "{out}");
        assert!(out.contains("json_of :: < Self > ()"), "{out}");
        assert!(out.contains(". header (\"location\""), "{out}");
    }

    #[test]
    fn the_header_is_rendered_before_the_body_is_serialised() {
        let out = expand_str(quote! {
            #[responder(status = 201, header(location = "self.url"))]
            struct C { url: String }
        });
        let render = out.find("__moso_header_0 =").expect("the render");
        let serialise = out.find("json_response").expect("the body");
        assert!(render < serialise, "{out}");
    }

    #[test]
    fn the_default_status_is_two_hundred() {
        let out = expand_str(quote! {
            struct UserOut { id: u32 }
        });
        assert!(out.contains("StatusCode :: OK"), "{out}");
        assert!(out.contains("__op . response (200u16"), "{out}");
    }

    #[test]
    fn a_status_without_a_body_documents_as_empty() {
        let out = expand_str(quote! {
            #[responder(status = 204)]
            struct Done;
        });
        assert!(out.contains("ResponseSpec :: empty"), "{out}");
        assert!(!out.contains("json_of"), "{out}");
        // And nothing asks the type for a body it cannot have.
        assert!(!out.contains("json_response"), "{out}");
        assert!(out.contains("empty_response"), "{out}");
        // No `Schema` assertion: there is no body to serialise.
        assert!(!out.contains("__moso_assert_schema"), "{out}");
    }

    #[test]
    fn an_underscore_becomes_a_dash_on_the_wire() {
        let out = expand_str(quote! {
            #[responder(header(x_request_id = "self.id"))]
            struct C { id: String }
        });
        assert!(out.contains("from_static (\"x-request-id\")"), "{out}");
        assert!(out.contains(". header (\"x-request-id\""), "{out}");
    }

    #[test]
    fn several_header_groups_accumulate() {
        let out = expand_str(quote! {
            #[responder(status = 201, header(location = "self.url"))]
            #[responder(header(etag = "self.etag"))]
            struct C { url: String, etag: String }
        });
        assert!(out.contains("from_static (\"location\")"), "{out}");
        assert!(out.contains("from_static (\"etag\")"), "{out}");
    }

    #[test]
    fn a_repeated_header_is_rejected() {
        let out = expand_str(quote! {
            #[responder(header(location = "self.a", location = "self.b"))]
            struct C { a: String, b: String }
        });
        assert!(out.contains("is set twice"), "{out}");
    }

    #[test]
    fn a_header_value_that_is_not_an_expression_is_rejected() {
        let out = expand_str(quote! {
            #[responder(header(location = "self."))]
            struct C { url: String }
        });
        assert!(out.contains("is not a Rust expression"), "{out}");
        assert_eq!(out.matches("compile_error !").count(), 1, "{out}");
    }

    #[test]
    fn an_impossible_status_is_rejected() {
        let out = expand_str(quote! {
            #[responder(status = 9001)]
            struct C { id: u32 }
        });
        assert!(out.contains("is not an HTTP status code"), "{out}");
    }

    #[test]
    fn an_unnamed_status_falls_back_to_from_u16() {
        let out = expand_str(quote! {
            #[responder(status = 299)]
            struct C { id: u32 }
        });
        assert!(out.contains("from_u16 (299u16)"), "{out}");
    }

    #[test]
    fn the_doc_comment_becomes_the_response_description() {
        let out = expand_str(quote! {
            /// The user, as created.
            struct UserCreated { id: u32 }
        });
        assert!(out.contains("\"The user, as created.\""), "{out}");
    }

    #[test]
    fn without_a_doc_comment_the_type_name_is_used() {
        let out = expand_str(quote! {
            struct UserCreated { id: u32 }
        });
        assert!(out.contains("\"User created\""), "{out}");
    }

    #[test]
    fn a_generic_responder_gains_a_schema_bound() {
        let out = expand_str(quote! {
            struct Wrapper<T> { inner: T }
        });
        assert!(
            out.contains("where Self : :: moso :: __private :: Schema"),
            "{out}"
        );
        assert!(!out.contains("__moso_assert_schema"), "{out}");
    }

    #[test]
    fn an_enum_is_rejected_with_the_either_fix() {
        let out = expand_str(quote! {
            enum C { A, B }
        });
        assert!(out.contains("compile_error !"), "{out}");
        assert!(out.contains("Either"), "{out}");
    }

    #[test]
    fn an_unknown_key_is_rejected_with_a_suggestion() {
        let out = expand_str(quote! {
            #[responder(statuss = 201)]
            struct C { id: u32 }
        });
        assert!(out.contains("compile_error !"), "{out}");
        assert!(out.contains("Did you mean"), "{out}");
        // The placeholder keeps a handler returning this type from adding a
        // second error.
        assert!(out.contains("impl :: moso :: __private :: IntoResponse for C"));
    }

    #[test]
    fn suggestions_come_from_the_shared_helper() {
        assert_eq!(
            crate::util::attrs::did_you_mean("statuss", &["status", "header"]),
            Some("status")
        );
    }
}
