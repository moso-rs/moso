//! `#[derive(Error)]` — an application's own error taxonomy.
//!
//! ```
//! use moso::prelude::*;
//! # /// The upstream call failed.
//! # #[derive(Debug)] pub struct GatewayError;
//! # impl std::fmt::Display for GatewayError {
//! #     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//! #         f.write_str("upstream unreachable")
//! #     }
//! # }
//! # impl std::error::Error for GatewayError {}
//! /// The failures this application's domain can produce.
//! #[derive(Debug, moso::Error)]
//! pub enum ShopError {
//!     /// Not enough stock to satisfy the order.
//!     #[error(status = 409, type = "https://shop.example/errors/out-of-stock")]
//!     #[error(detail = "Only {available} left in stock")]
//!     OutOfStock {
//!         /// Which product.
//!         sku: String,
//!         /// How many remain.
//!         available: u32,
//!     },
//!
//!     /// An upstream service failed; detail suppressed automatically.
//!     #[error(status = 500)]
//!     Gateway(#[from] GatewayError),
//! }
//! # fn main() {
//! let out_of_stock = ShopError::OutOfStock { sku: "A1".to_owned(), available: 2 };
//! assert_eq!(Error::from(out_of_stock).detail(), Some("Only 2 left in stock"));
//!
//! // A 5xx never carries the template text on the wire.
//! assert_eq!(Error::from(ShopError::Gateway(GatewayError)).detail(), None);
//! # }
//! ```
//!
//! The derive emits five things:
//!
//! - `Display`, from the `detail` template with `{field}` interpolation;
//! - `core::error::Error`, whose `source()` is the `#[from]`/`#[source]` field;
//! - `From<Inner>` for every `#[from]` field, so `?` reaches the variant;
//! - `From<Self> for moso::Error`, which is what `?` in a handler uses;
//! - `Describe` plus a `variants()` descriptor, so `#[endpoint(errors = T)]`
//!   can put the responses in the OpenAPI document.
//!
//! **Detail is suppressed for any status ≥ 500.** `Problem::from_error` would
//! suppress it at render time anyway, but not attaching it in the first place
//! means a `detail` template naming an internal host or an SQL fragment cannot
//! be reached by an operator flipping `http.expose_internal_errors` on to debug
//! something else. The text still reaches the log, as the error's `source`.
//!
//! See `docs/01-http/16-errors.md`.

use std::collections::BTreeMap;

use darling::util::SpannedValue;
use darling::{FromDeriveInput, FromField, FromVariant, ast};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{DeriveInput, Ident};

use crate::config::support::{Diagnostic, moso, reject_generics, sentence_case};
use crate::util::attrs::did_you_mean;

// ---------------------------------------------------------------------------
// The status taxonomy
// ---------------------------------------------------------------------------

/// Every status `moso::ErrorKind` can represent, with its variant.
///
/// `Error::status()` reads the status *from the kind*: there is no
/// `Error::with_status`, so a `#[error(status = ..)]` the taxonomy has no entry
/// for cannot be produced at all. Rather than round a 402 down to a 400 and lie
/// on the wire, the derive refuses and says which statuses it can spell.
///
/// This mirrors `moso_core::ErrorKind` by hand, because `moso-macros` depends on
/// no runtime Moso crate and so cannot name the enum. The two lists are kept in
/// step by `every_response_kind_is_spellable_by_the_derive` in the facade's
/// `tests/macro_surface.rs`, a crate that *can* see both: it derives one variant
/// per unique response status and asserts the derived set equals
/// `ErrorKind::RESPONSE_KINDS`, so adding a kind in `moso-core` fails that test
/// until this table gains its status.
const STATUS_KINDS: &[(u16, &str)] = &[
    (400, "BadRequest"),
    (401, "Unauthenticated"),
    (403, "Forbidden"),
    (404, "NotFound"),
    (405, "MethodNotAllowed"),
    (406, "NotAcceptable"),
    (409, "Conflict"),
    (410, "Gone"),
    (412, "PreconditionFailed"),
    (413, "PayloadTooLarge"),
    (414, "UriTooLong"),
    (415, "UnsupportedMedia"),
    (416, "RangeNotSatisfiable"),
    (422, "Validation"),
    (423, "Locked"),
    (429, "TooManyRequests"),
    (431, "HeaderFieldsTooLarge"),
    (500, "Internal"),
    (501, "NotImplemented"),
    (502, "BadGateway"),
    (503, "Unavailable"),
    (504, "GatewayTimeout"),
];

/// The default `type` URI of each kind, mirroring `ErrorKind::type_uri`.
///
/// Duplicated as text because `ErrorKind::type_uri` is a method, and the
/// `variants()` descriptor is a `const`. A unit test asserts the two lists stay
/// the same length; `docs/01-http/16-errors.md` pins the format.
const ERROR_TYPE_BASE: &str = "https://moso.rs/errors/";

/// The `ErrorKind` variant for `status`, or `None` when the taxonomy has none.
pub(crate) fn kind_for(status: u16) -> Option<&'static str> {
    STATUS_KINDS
        .iter()
        .find(|(code, _)| *code == status)
        .map(|(_, kind)| *kind)
}

/// The slug half of a kind's default `type` URI: `BadRequest` → `bad-request`.
fn slug_for(kind: &str) -> String {
    let mut slug = String::with_capacity(kind.len() + 3);
    for (index, character) in kind.char_indices() {
        if character.is_ascii_uppercase() && index > 0 {
            slug.push('-');
        }
        slug.push(character.to_ascii_lowercase());
    }
    // `UriTooLong` is spelled `uri-too-long`; no acronym in the taxonomy needs
    // special handling, and a unit test pins every entry.
    slug
}

/// `OutOfStock` → `out-of-stock`, for a `type_base`-derived URI.
fn kebab(name: &str) -> String {
    slug_for(name)
}

// ---------------------------------------------------------------------------
// Attribute model
// ---------------------------------------------------------------------------

/// One field of an error variant.
#[derive(Debug, FromField)]
#[darling(attributes(error), forward_attrs(from, source))]
struct ErrorField {
    /// The field's name, or `None` for a tuple field.
    ident: Option<Ident>,
    /// The declared type.
    ty: syn::Type,
    /// Forwarded `#[from]` / `#[source]` markers.
    attrs: Vec<syn::Attribute>,
}

impl ErrorField {
    /// Whether the field carries `#[from]`.
    fn is_from(&self) -> bool {
        self.attrs.iter().any(|attr| attr.path().is_ident("from"))
    }

    /// Whether the field is the variant's source — `#[from]` implies it.
    fn is_source(&self) -> bool {
        self.is_from() || self.attrs.iter().any(|attr| attr.path().is_ident("source"))
    }
}

/// One variant of an error enum.
#[derive(Debug, FromVariant)]
#[darling(attributes(error), forward_attrs(doc))]
struct ErrorVariant {
    /// The variant's name.
    ident: Ident,
    /// Its fields.
    fields: ast::Fields<ErrorField>,
    /// Forwarded `///` documentation.
    #[allow(dead_code)]
    attrs: Vec<syn::Attribute>,
    /// `#[error(status = 409)]`.
    #[darling(default)]
    status: Option<SpannedValue<u16>>,
    /// `#[error(type = "https://…")]`.
    #[darling(default, rename = "type")]
    type_uri: Option<String>,
    /// `#[error(title = "Out of stock")]`.
    #[darling(default)]
    title: Option<String>,
    /// `#[error(detail = "Only {available} left")]`.
    #[darling(default)]
    detail: Option<SpannedValue<String>>,
}

/// The `#[derive(Error)]` input.
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(error), supports(enum_any, struct_any), forward_attrs(doc))]
struct ErrorInput {
    /// The type's name.
    ident: Ident,
    /// Generics, which the derive rejects.
    generics: syn::Generics,
    /// Variants (enum) or fields (struct).
    data: ast::Data<ErrorVariant, ErrorField>,
    /// Forwarded `///` documentation.
    #[allow(dead_code)]
    attrs: Vec<syn::Attribute>,
    /// `#[error(status = 500)]` on the type — the fallback for every variant.
    #[darling(default)]
    status: Option<SpannedValue<u16>>,
    /// `#[error(type = "…")]` on the type. Only meaningful on a struct.
    #[darling(default, rename = "type")]
    type_uri: Option<String>,
    /// `#[error(title = "…")]` on the type.
    #[darling(default)]
    title: Option<String>,
    /// `#[error(detail = "…")]` on the type.
    #[darling(default)]
    detail: Option<SpannedValue<String>>,
    /// `#[error(type_base = "https://shop.example/errors/")]` — every variant
    /// without an explicit `type` gets `type_base` plus its kebab-case name.
    #[darling(default)]
    type_base: Option<String>,
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

/// Expand `#[derive(Error)]`.
///
/// Wire this up from `lib.rs` with
/// `#[proc_macro_derive(Error, attributes(error, from, source))]` — the bare
/// `#[from]` and `#[source]` markers must be declared there or rustc rejects
/// them as unknown attributes before this code ever runs.
pub(crate) fn expand(input: DeriveInput) -> TokenStream {
    let ident = input.ident.clone();

    let parsed = match ErrorInput::from_derive_input(&input) {
        Ok(parsed) => parsed,
        Err(error) => return with_placeholder(&ident, error.write_errors()),
    };

    match build(&parsed) {
        Ok(tokens) => tokens,
        Err(error) => with_placeholder(&ident, error.to_compile_error()),
    }
}

/// The impls that keep one bad attribute from becoming a page of errors.
///
/// Without them, every `?` on the type in every handler adds "the trait bound
/// `moso::Error: From<ShopError>` is not satisfied" on top of the real error.
fn with_placeholder(ident: &Ident, errors: TokenStream) -> TokenStream {
    let moso = moso();
    quote! {
        #errors

        #[automatically_derived]
        impl ::core::fmt::Display for #ident {
            fn fmt(&self, _f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::unimplemented!()
            }
        }

        #[automatically_derived]
        impl ::core::error::Error for #ident {}

        #[automatically_derived]
        impl ::core::convert::From<#ident> for #moso::Error {
            fn from(_value: #ident) -> Self {
                ::core::unimplemented!()
            }
        }

        #[automatically_derived]
        impl #moso::Describe for #ident {
            fn describe(_op: &mut #moso::OperationBuilder) {}
        }
    }
}

/// One arm of every generated `match`.
struct Case {
    /// The variant's name, for `variants()` and for the default title.
    name: String,
    /// The path a pattern names it by: `ShopError::OutOfStock`, or `ShopError`.
    ///
    /// The type is spelled out rather than written `Self`, because one of the
    /// generated impls is `From<ShopError> for moso::Error`, where `Self` is
    /// `moso::Error` and `Self::OutOfStock` names nothing.
    path: TokenStream,
    /// The fields, in declaration order.
    fields: Vec<FieldPlan>,
    /// The HTTP status.
    status: u16,
    /// The `ErrorKind` variant that carries that status.
    kind: Ident,
    /// The `type` URI.
    type_uri: String,
    /// The `title`.
    title: String,
    /// The `detail` template, already rewritten to name bindings.
    detail: Option<Detail>,
    /// The index of the `#[from]`/`#[source]` field, if there is one.
    source: Option<usize>,
    /// The index of the `#[from]` field, if there is one.
    from: Option<usize>,
}

impl Case {
    /// Whether the client may see the detail.
    fn discloses(&self) -> bool {
        self.status < 500
    }
}

/// One field of a case.
struct FieldPlan {
    /// The local the pattern binds it to.
    binding: Ident,
    /// The field's name, or its index as text for a tuple field.
    key: String,
    /// The declared type.
    ty: syn::Type,
    /// Whether the field is named.
    named: Option<Ident>,
}

/// A `detail` template, rewritten so every placeholder names a binding.
struct Detail {
    /// The rewritten `format!` string.
    format: String,
    /// The indices of the fields the template reads.
    used: Vec<usize>,
    /// The template exactly as the user wrote it, for the OpenAPI description.
    original: String,
}

/// Build every impl, or the first problem found.
fn build(input: &ErrorInput) -> syn::Result<TokenStream> {
    let moso = moso();
    let ident = &input.ident;

    if let Some(error) = reject_generics(
        &input.generics,
        "Error",
        ident,
        "the derive emits `impl From<Self> for moso::Error` and a `const` table of the variants, \
         neither of which can be written once for every instantiation",
    ) {
        return Err(error);
    }

    let cases = collect_cases(input)?;
    if cases.is_empty() {
        return Err(
            Diagnostic::new("`#[derive(Error)]` needs at least one variant")
                .note("an error type with no variants can never be constructed or documented")
                .help_code(
                    "add a variant:",
                    format!("pub enum {ident} {{\n    #[error(status = 404)]\n    NotFound,\n}}"),
                )
                .at(ident),
        );
    }

    let display = display_impl(ident, &cases);
    let std_error = std_error_impl(ident, &cases);
    let froms = from_impls(ident, &cases);
    let into_moso = into_moso_impl(ident, &cases, &moso);
    let describe = describe_impl(ident, &cases, &moso);
    let variants = variants_impl(ident, &cases);
    let assertions = assertions(ident, &moso);

    Ok(quote! {
        #display
        #std_error
        #(#froms)*
        #into_moso
        #describe
        #variants
        #assertions
    })
}

/// Turn the parsed input into one `Case` per wire shape.
fn collect_cases(input: &ErrorInput) -> syn::Result<Vec<Case>> {
    match &input.data {
        ast::Data::Enum(variants) => variants
            .iter()
            .map(|variant| {
                let name = variant.ident.to_string();
                let variant_ident = &variant.ident;
                let ident = &input.ident;
                case(
                    input,
                    &name,
                    quote!(#ident::#variant_ident),
                    variant.fields.iter(),
                    variant.status.as_ref().or(input.status.as_ref()),
                    variant.type_uri.as_ref().or(input.type_uri.as_ref()),
                    variant.title.as_ref(),
                    variant.detail.as_ref(),
                    variant_ident.span(),
                )
            })
            .collect(),
        ast::Data::Struct(fields) => {
            let ident = &input.ident;
            Ok(vec![case(
                input,
                &input.ident.to_string(),
                quote!(#ident),
                fields.iter(),
                input.status.as_ref(),
                input.type_uri.as_ref(),
                input.title.as_ref(),
                input.detail.as_ref(),
                input.ident.span(),
            )?])
        }
    }
}

/// Build one case, validating its attributes against its fields.
#[allow(clippy::too_many_arguments)]
fn case<'a>(
    input: &ErrorInput,
    name: &str,
    path: TokenStream,
    fields: impl Iterator<Item = &'a ErrorField>,
    status: Option<&SpannedValue<u16>>,
    type_uri: Option<&String>,
    title: Option<&String>,
    detail: Option<&SpannedValue<String>>,
    span: Span,
) -> syn::Result<Case> {
    let fields: Vec<&ErrorField> = fields.collect();

    let plans: Vec<FieldPlan> = fields
        .iter()
        .enumerate()
        .map(|(index, field)| FieldPlan {
            binding: format_ident!("__moso_f{}", index),
            key: field
                .ident
                .as_ref()
                .map_or_else(|| index.to_string(), std::string::ToString::to_string),
            ty: field.ty.clone(),
            named: field.ident.clone(),
        })
        .collect();

    // 500 is the safe default: a variant whose author forgot to say what it is
    // becomes an internal error whose detail is suppressed, not a 200.
    let status_value = status.map_or(500, |status| **status);
    let status_span = status.map_or(span, darling::util::SpannedValue::span);
    let Some(kind) = kind_for(status_value) else {
        return Err(unmapped_status(status_value, status_span));
    };
    let kind = Ident::new(kind, status_span);

    let source_indices: Vec<usize> = fields
        .iter()
        .enumerate()
        .filter(|(_, field)| field.is_source())
        .map(|(index, _)| index)
        .collect();
    if source_indices.len() > 1 {
        return Err(
            Diagnostic::new("a variant may have only one `#[from]` or `#[source]` field")
                .note("`std::error::Error::source` returns one error, so a second would be lost")
                .help("keep the field that caused the failure and drop `#[source]` from the other")
                .at_span(plans[source_indices[1]].ty.span()),
        );
    }
    let source = source_indices.first().copied();

    let from = fields
        .iter()
        .position(|field| field.is_from())
        .filter(|_| true);
    if from.is_some() && fields.len() != 1 {
        return Err(
            Diagnostic::new("a `#[from]` variant must have exactly one field")
                .note(
                    "the generated `From` conversion has only the source error to work with, and \
                     no way to fill the other fields in",
                )
                .help_code(
                    "carry the extra data in a separate variant, or drop `#[from]` and convert \
                     explicitly:",
                    format!("{name}(#[from] ::std::io::Error),"),
                )
                .at_span(span),
        );
    }

    let detail = detail
        .map(|template| parse_template(template, &plans, name))
        .transpose()?;

    let title = title.cloned().unwrap_or_else(|| sentence_case(name));

    let type_uri = match (type_uri, input.type_base.as_deref()) {
        (Some(explicit), _) => explicit.clone(),
        (None, Some(base)) => format!("{}{}", base, kebab(name)),
        (None, None) => format!("{ERROR_TYPE_BASE}{}", slug_for(&kind.to_string())),
    };

    Ok(Case {
        name: name.to_owned(),
        path,
        fields: plans,
        status: status_value,
        kind,
        type_uri,
        title,
        detail,
        source,
        from,
    })
}

/// The diagnostic for a status the taxonomy cannot represent.
pub(crate) fn unmapped_status(status: u16, span: Span) -> syn::Error {
    let supported = STATUS_KINDS
        .iter()
        .map(|(code, _)| code.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let nearest = STATUS_KINDS
        .iter()
        .map(|(code, _)| *code)
        .min_by_key(|code| code.abs_diff(status))
        .unwrap_or(500);

    Diagnostic::new(format!(
        "`status = {status}` is not in Moso's error taxonomy"
    ))
    .note(
        "a `moso::Error` takes its status from its `ErrorKind`, so a status with no kind \
             cannot be produced",
    )
    .help_code(
        format!("use the nearest kind that carries a status ({nearest}):"),
        format!("#[error(status = {nearest})]"),
    )
    .help(format!("the statuses with a kind are: {supported}"))
    .at_span(span)
}

// ---------------------------------------------------------------------------
// The `detail` template
// ---------------------------------------------------------------------------

/// Rewrite `"Only {available} left"` into a `format!` string over the bindings.
///
/// Every placeholder is checked against the variant's fields *here*, so an
/// `{availble}` typo produces one error with a "did you mean" pointing at the
/// user's string rather than a `format!` error inside generated tokens.
fn parse_template(
    template: &SpannedValue<String>,
    fields: &[FieldPlan],
    case: &str,
) -> syn::Result<Detail> {
    let text = template.as_str();
    let span = template.span();
    let mut format = String::with_capacity(text.len());
    let mut used: Vec<usize> = Vec::new();
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
                format.push_str("{{");
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
                format.push_str("}}");
            }
            '}' => {
                return Err(
                    Diagnostic::new("this `detail` template has a `}` that opens nothing")
                        .note("`{` and `}` delimit a field placeholder, as in `format!`")
                        .help("write `}}` if you meant a literal closing brace")
                        .at_span(span),
                );
            }
            '{' => {
                let mut body = String::new();
                let mut closed = false;
                for next in characters.by_ref() {
                    if next == '}' {
                        closed = true;
                        break;
                    }
                    body.push(next);
                }
                if !closed {
                    return Err(Diagnostic::new(
                        "this `detail` template has a `{` that is never closed",
                    )
                    .note("`{` and `}` delimit a field placeholder, as in `format!`")
                    .help("write `{{` if you meant a literal opening brace")
                    .at_span(span));
                }

                let (name, spec) = body
                    .split_once(':')
                    .map_or((body.as_str(), None), |(n, s)| (n, Some(s)));
                let name = name.trim();
                if name.is_empty() {
                    return Err(Diagnostic::new(
                        "a `detail` placeholder must name a field of the variant",
                    )
                    .note(
                        "the template is rendered from the variant's own fields, so there are no \
                         positional arguments to fall back on",
                    )
                    .help_code(
                        "name the field:",
                        format!("#[error(detail = \"… {{{}}} …\")]", first_key(fields)),
                    )
                    .at_span(span));
                }

                let index = fields
                    .iter()
                    .position(|field| field.key == name)
                    .ok_or_else(|| unknown_placeholder(name, fields, case, span))?;
                if !used.contains(&index) {
                    used.push(index);
                }

                let binding = &fields[index].binding;
                match spec {
                    Some(spec) => format.push_str(&format!("{{{binding}:{spec}}}")),
                    None => format.push_str(&format!("{{{binding}}}")),
                }
            }
            other => format.push(other),
        }
    }

    used.sort_unstable();
    Ok(Detail {
        format,
        used,
        original: text.to_owned(),
    })
}

/// The first field name, for a "name the field" help line.
fn first_key(fields: &[FieldPlan]) -> String {
    fields
        .first()
        .map_or_else(|| "field".to_owned(), |field| field.key.clone())
}

/// The diagnostic for a placeholder that names nothing.
fn unknown_placeholder(name: &str, fields: &[FieldPlan], case: &str, span: Span) -> syn::Error {
    let keys: Vec<&str> = fields.iter().map(|field| field.key.as_str()).collect();
    let mut diagnostic = Diagnostic::new(format!("`{{{name}}}` does not name a field of `{case}`"))
        .note("a `detail` template interpolates the variant's own fields and nothing else");

    if let Some(suggestion) = did_you_mean(name, &keys) {
        diagnostic = diagnostic.help_code(
            "did you mean:",
            format!("#[error(detail = \"… {{{suggestion}}} …\")]"),
        );
    } else if keys.is_empty() {
        diagnostic = diagnostic.help("this variant has no fields; write the message as plain text");
    } else {
        diagnostic = diagnostic.help(format!("the fields are: {}", keys.join(", ")));
    }

    diagnostic.at_span(span)
}

// ---------------------------------------------------------------------------
// The impls
// ---------------------------------------------------------------------------

/// The pattern that binds exactly the fields `wanted` names.
fn pattern(case: &Case, wanted: &[usize]) -> TokenStream {
    let path = &case.path;
    if case.fields.is_empty() {
        return quote!(#path);
    }
    let named = case.fields.iter().all(|field| field.named.is_some());
    if named {
        let bindings = wanted.iter().map(|index| {
            let field = &case.fields[*index];
            let name = field.named.as_ref().expect("named field");
            let binding = &field.binding;
            quote!(#name: #binding)
        });
        return quote!(#path { #(#bindings,)* .. });
    }
    let bindings = case.fields.iter().enumerate().map(|(index, field)| {
        if wanted.contains(&index) {
            let binding = &field.binding;
            quote!(#binding)
        } else {
            quote!(_)
        }
    });
    quote!(#path( #(#bindings),* ))
}

/// `Display`, from the `detail` template or the title.
fn display_impl(ident: &Ident, cases: &[Case]) -> TokenStream {
    let arms = cases.iter().map(|case| {
        let (used, body) = match &case.detail {
            Some(detail) => {
                let format = &detail.format;
                (detail.used.clone(), quote!(::core::write!(__f, #format)))
            }
            None => {
                let title = &case.title;
                (Vec::new(), quote!(__f.write_str(#title)))
            }
        };
        let pattern = pattern(case, &used);
        quote!(#pattern => #body,)
    });

    quote! {
        #[automatically_derived]
        impl ::core::fmt::Display for #ident {
            fn fmt(&self, __f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                match self {
                    #(#arms)*
                }
            }
        }
    }
}

/// `core::error::Error`, whose `source` is the `#[from]`/`#[source]` field.
fn std_error_impl(ident: &Ident, cases: &[Case]) -> TokenStream {
    if cases.iter().all(|case| case.source.is_none()) {
        return quote! {
            #[automatically_derived]
            impl ::core::error::Error for #ident {}
        };
    }

    let arms = cases.iter().map(|case| match case.source {
        Some(index) => {
            let pattern = pattern(case, &[index]);
            let binding = &case.fields[index].binding;
            quote!(#pattern => ::core::option::Option::Some(#binding),)
        }
        None => {
            let pattern = pattern(case, &[]);
            quote!(#pattern => ::core::option::Option::None,)
        }
    });

    quote! {
        #[automatically_derived]
        impl ::core::error::Error for #ident {
            fn source(&self) -> ::core::option::Option<&(dyn ::core::error::Error + 'static)> {
                match self {
                    #(#arms)*
                }
            }
        }
    }
}

/// `From<Inner>` for each `#[from]` field, so `?` reaches the variant.
fn from_impls(ident: &Ident, cases: &[Case]) -> Vec<TokenStream> {
    cases
        .iter()
        .filter_map(|case| {
            let index = case.from?;
            let field = &case.fields[index];
            let ty = &field.ty;
            let path = &case.path;
            let construct = match &field.named {
                Some(name) => quote!(#path { #name: __value }),
                None => quote!(#path(__value)),
            };
            Some(quote! {
                #[automatically_derived]
                impl ::core::convert::From<#ty> for #ident {
                    fn from(__value: #ty) -> Self {
                        #construct
                    }
                }
            })
        })
        .collect()
}

/// `From<Self> for moso::Error` — the conversion every `?` in a handler uses.
fn into_moso_impl(ident: &Ident, cases: &[Case], moso: &TokenStream) -> TokenStream {
    let discloses = cases.iter().any(Case::discloses);

    let arms = cases.iter().map(|case| {
        let pattern = pattern(case, &[]);
        let kind = &case.kind;
        let type_uri = &case.type_uri;
        let title = &case.title;
        // A status of 500 or more never carries its detail to the client. The
        // text still reaches the operator: it is this error's `source`, and the
        // boundary logs the whole chain.
        let detail = if case.discloses() {
            quote!(.with_detail(__moso_detail))
        } else {
            quote!()
        };
        quote! {
            #pattern => #moso::Error::new(#moso::ErrorKind::#kind)
                .with_type(#type_uri)
                .with_title(#title)
                #detail,
        }
    });

    let detail = if discloses {
        quote!(let __moso_detail = ::std::string::ToString::to_string(&__value);)
    } else {
        quote!()
    };

    quote! {
        #[automatically_derived]
        impl ::core::convert::From<#ident> for #moso::Error {
            fn from(__value: #ident) -> Self {
                #detail
                let __moso_error = match &__value {
                    #(#arms)*
                };
                // The source chain is what an operator reads for a 5xx, where
                // the detail is suppressed. It is never serialised.
                __moso_error.with_source(__value)
            }
        }
    }
}

/// `Describe`, so `#[endpoint(errors = T)]` documents these responses.
///
/// Variants that share a status share one response: `OperationSpec` keys
/// responses by status and keeps the first, so emitting two would silently drop
/// the second description.
fn describe_impl(ident: &Ident, cases: &[Case], moso: &TokenStream) -> TokenStream {
    let mut grouped: BTreeMap<u16, Vec<String>> = BTreeMap::new();
    for case in cases {
        let description = case
            .detail
            .as_ref()
            .map_or_else(|| case.title.clone(), |detail| detail.original.clone());
        let bucket = grouped.entry(case.status).or_default();
        if !bucket.contains(&description) {
            bucket.push(description);
        }
    }

    let responses = grouped.into_iter().map(|(status, descriptions)| {
        let description = descriptions.join("; ");
        quote!(__op.response(#status, #moso::ResponseSpec::problem(#description));)
    });

    quote! {
        #[automatically_derived]
        impl #moso::Describe for #ident {
            fn describe(__op: &mut #moso::OperationBuilder) {
                #(#responses)*
            }
        }
    }
}

/// The `variants()` descriptor.
///
/// A tuple rather than a named struct because the shape has to be nameable in a
/// return type and `moso-core` has no `ErrorVariant` type to name. See the
/// agent report: promoting this to a struct in `::moso::__private` is a
/// mechanical change here.
fn variants_impl(ident: &Ident, cases: &[Case]) -> TokenStream {
    let entries = cases.iter().map(|case| {
        let name = &case.name;
        let status = case.status;
        let type_uri = &case.type_uri;
        let title = &case.title;
        quote!((#name, #status, #type_uri, #title))
    });
    let count = cases.len();

    quote! {
        #[automatically_derived]
        impl #ident {
            /// Every variant, as `(name, status, type URI, title)`.
            ///
            /// Generated by `#[derive(moso::Error)]`. `#[endpoint(errors = …)]`
            /// documents the responses through `Describe`; this table is what
            /// `moso check` and the error-code reference read.
            pub const VARIANTS: [(&'static str, u16, &'static str, &'static str); #count] =
                [#(#entries),*];

            /// The variants this error can produce. See [`Self::VARIANTS`].
            #[must_use]
            pub fn variants() -> &'static [(&'static str, u16, &'static str, &'static str)] {
                &Self::VARIANTS
            }
        }
    }
}

/// Assertion codegen: the span of a missing bound points at the user's type.
fn assertions(ident: &Ident, moso: &TokenStream) -> TokenStream {
    quote! {
        #[doc(hidden)]
        const _: () = {
            // `From<Self> for moso::Error` stores the value as the error's
            // source, which is what puts it in the one log line the boundary
            // emits. `Send + Sync` is not a Moso invention: a handler's future
            // must be `Send`, so an error it can return already is.
            fn __moso_assert_boxable<
                T: ::core::error::Error + ::core::marker::Send + ::core::marker::Sync + 'static,
            >() {}
            fn __moso_check() {
                __moso_assert_boxable::<#ident>();
                let _ = ::core::stringify!(#moso);
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
    fn every_kind_has_a_distinct_status_and_slug() {
        let mut statuses: Vec<u16> = STATUS_KINDS.iter().map(|(code, _)| *code).collect();
        statuses.sort_unstable();
        statuses.dedup();
        assert_eq!(statuses.len(), STATUS_KINDS.len());

        // Pins the spelling `ErrorKind::type_uri` uses.
        assert_eq!(slug_for("BadRequest"), "bad-request");
        assert_eq!(slug_for("UriTooLong"), "uri-too-long");
        assert_eq!(slug_for("Internal"), "internal");
        assert_eq!(slug_for("RangeNotSatisfiable"), "range-not-satisfiable");
    }

    #[test]
    fn the_documented_example_expands() {
        let out = expand_str(quote! {
            pub enum ShopError {
                #[error(status = 409, type = "https://shop.example/errors/out-of-stock")]
                #[error(detail = "Only {available} left in stock")]
                OutOfStock { sku: String, available: u32 },

                #[error(status = 403)]
                PaymentRequired,

                #[error(status = 500)]
                Gateway(#[from] ReqwestError),
            }
        });

        // Display interpolates the named field and ignores the other.
        assert!(
            out.contains("ShopError :: OutOfStock { available : __moso_f1 , .. }"),
            "{out}"
        );
        assert!(out.contains("\"Only {__moso_f1} left in stock\""), "{out}");
        // The 403 falls back to a sentence-cased title.
        assert!(out.contains("\"Payment required\""), "{out}");
        // `#[from]` produces the inbound conversion...
        assert!(out.contains("impl :: core :: convert :: From < ReqwestError > for ShopError"));
        // ...and the source.
        assert!(out.contains("fn source (& self)"));
        // The outbound conversion picks the kind from the status.
        assert!(out.contains("ErrorKind :: Conflict"));
        assert!(out.contains("ErrorKind :: Internal"));
        assert!(out.contains("with_type (\"https://shop.example/errors/out-of-stock\")"));
    }

    #[test]
    fn a_five_hundred_never_attaches_its_detail() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(status = 500, detail = "connection to {host} refused")]
                Gateway { host: String },
            }
        });
        assert!(!out.contains("with_detail"), "{out}");
        // The text is still reachable, as the error's `Display` and therefore
        // as its source in the log.
        assert!(
            out.contains("\"connection to {__moso_f0} refused\""),
            "{out}"
        );
    }

    #[test]
    fn a_four_hundred_attaches_its_detail() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(status = 409, detail = "{sku} is gone")]
                Gone { sku: String },
            }
        });
        assert!(out.contains(". with_detail (__moso_detail)"), "{out}");
    }

    #[test]
    fn an_unmapped_status_names_the_nearest_supported_one() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(status = 402)]
                PaymentRequired,
            }
        });
        assert!(out.contains("not in Moso's error taxonomy"), "{out}");
        assert!(out.contains("#[error(status = 401)]"), "{out}");
        // The placeholder keeps `?` sites from adding a second error.
        assert!(
            out.contains("impl :: core :: convert :: From < E > for :: moso :: __private :: Error")
        );
    }

    #[test]
    fn an_unknown_placeholder_suggests_the_field() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(status = 409, detail = "only {availble} left")]
                OutOfStock { available: u32 },
            }
        });
        assert!(out.contains("does not name a field"), "{out}");
        assert!(out.contains("{available}"), "{out}");
    }

    #[test]
    fn an_unbalanced_template_is_rejected_once() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(status = 409, detail = "only {available left")]
                OutOfStock { available: u32 },
            }
        });
        assert!(out.contains("never closed"), "{out}");
        assert_eq!(out.matches("compile_error !").count(), 1, "{out}");
    }

    #[test]
    fn a_tuple_placeholder_is_addressed_by_index() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(status = 409, detail = "{0} is taken")]
                Taken(String, u32),
            }
        });
        assert!(out.contains("E :: Taken (__moso_f0 , _)"), "{out}");
        assert!(out.contains("\"{__moso_f0} is taken\""), "{out}");
    }

    #[test]
    fn a_type_base_derives_a_uri_per_variant() {
        let out = expand_str(quote! {
            #[error(type_base = "https://shop.example/errors/")]
            pub enum ShopError {
                #[error(status = 409)]
                OutOfStock,
            }
        });
        assert!(
            out.contains("with_type (\"https://shop.example/errors/out-of-stock\")"),
            "{out}"
        );
    }

    #[test]
    fn without_a_type_the_kind_uri_is_used() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(status = 409)]
                Clash,
            }
        });
        assert!(
            out.contains("with_type (\"https://moso.rs/errors/conflict\")"),
            "{out}"
        );
    }

    #[test]
    fn variants_sharing_a_status_share_one_response() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(status = 409, detail = "already taken")]
                Taken,
                #[error(status = 409, detail = "already published")]
                Published,
            }
        });
        assert_eq!(out.matches("__op . response (409u16").count(), 1, "{out}");
        assert!(out.contains("already taken; already published"), "{out}");
    }

    #[test]
    fn the_variants_table_lists_every_variant() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(status = 409)]
                Taken,
                #[error(status = 404)]
                Missing,
            }
        });
        assert!(out.contains("pub const VARIANTS"), "{out}");
        assert!(out.contains("(\"Taken\" , 409u16"), "{out}");
        assert!(out.contains("(\"Missing\" , 404u16"), "{out}");
        assert!(out.contains("pub fn variants ()"), "{out}");
    }

    #[test]
    fn a_struct_error_works_too() {
        let out = expand_str(quote! {
            #[error(status = 429, detail = "retry in {seconds}s")]
            pub struct Throttled { seconds: u64 }
        });
        assert!(out.contains("ErrorKind :: TooManyRequests"), "{out}");
        assert!(
            out.contains("Throttled { seconds : __moso_f0 , .. }"),
            "{out}"
        );
    }

    #[test]
    fn two_source_fields_are_rejected() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(status = 500)]
                Both { #[source] a: A, #[source] b: B },
            }
        });
        assert!(
            out.contains("only one `#[from]` or `#[source]` field"),
            "{out}"
        );
    }

    #[test]
    fn a_from_variant_with_extra_fields_is_rejected() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(status = 500)]
                Gateway { #[from] inner: A, url: String },
            }
        });
        assert!(out.contains("exactly one field"), "{out}");
    }

    #[test]
    fn an_unknown_key_is_rejected_with_a_suggestion() {
        let out = expand_str(quote! {
            pub enum E {
                #[error(staus = 409)]
                Taken,
            }
        });
        assert!(out.contains("compile_error !"), "{out}");
        assert!(out.contains("Did you mean"), "{out}");
    }

    #[test]
    fn a_generic_error_is_rejected() {
        let out = expand_str(quote! {
            pub enum E<T> {
                #[error(status = 409)]
                Taken(T),
            }
        });
        assert!(out.contains("cannot be used on a generic type"), "{out}");
    }
}
