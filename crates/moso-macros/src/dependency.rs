//! `#[derive(Dependency)]` — a value resolved once per request and cached.
//!
//! Two shapes, from `docs/01-http/15-dependency-injection.md`:
//!
//! ```
//! use moso::prelude::*;
//! # /// Who the request acts as.
//! # #[derive(Clone, Debug)] pub struct CurrentUser { pub is_admin: bool }
//! # impl Dependency for CurrentUser {
//! #     const PROVIDER_REQ: &'static [moso::ProviderReq] = &[];
//! #     async fn resolve(_: &RequestCtx) -> Result<Self> { Ok(CurrentUser { is_admin: true }) }
//! # }
//! # /// One customer's slice of the system.
//! # #[derive(Clone, Debug)] pub struct Tenant(String);
//! # impl Dependency for Tenant {
//! #     const PROVIDER_REQ: &'static [moso::ProviderReq] = &[];
//! #     async fn resolve(_: &RequestCtx) -> Result<Self> { Ok(Tenant("acme".to_owned())) }
//! # }
//! /// Composition: every field is itself a dependency.
//! #[derive(Dependency, Clone)]
//! pub struct Editing {
//!     /// Who is editing.
//!     user: CurrentUser,
//!     /// Which tenant they are editing in.
//!     tenant: Tenant,
//! }
//!
//! /// Wrap-and-check, the common case.
//! #[derive(Dependency, Clone)]
//! #[depends(from = CurrentUser, check = "is_admin", error = "admin required")]
//! pub struct AdminUser(pub CurrentUser);
//! # fn main() {}
//! ```
//!
//! Both compute `PROVIDER_REQ` as the union of what the fields need, which is
//! what makes a missing provider a *boot* error rather than a 500: `App::build`
//! walks every operation's `required_providers()` and that union is how a
//! transitive requirement two dependencies deep gets into it.
//!
//! # Why the derive always writes `resolve`
//!
//! A tempting third shape is "the derive writes `PROVIDER_REQ` and `describe`,
//! and forwards `resolve` to an inherent `Self::resolve` the user writes". It
//! is a trap: if the user forgets the inherent method and `Dependency` is in
//! scope — which it is, through the prelude — then `Self::resolve(ctx)` resolves
//! to the *trait* method and the generated body calls itself forever. There is
//! no syntax for "the inherent one only". `#[depends(manual)]` covers the case
//! honestly instead: it emits no trait impl at all, so the hand-written
//! `impl Dependency for T` beside it is the only one.

use darling::util::{Flag, SpannedValue};
use darling::{FromDeriveInput, FromField, ast};
use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::{DeriveInput, Expr, Ident, Path, Type};

use crate::config::support::{Diagnostic, moso, reject_generics, type_display};
use crate::error::{kind_for, unmapped_status};
use crate::util::attrs::generic_inner;

/// The status a failed `check` produces when the attribute does not say.
const DEFAULT_CHECK_STATUS: u16 = 403;

/// The message a failed `check` produces when the attribute does not say.
const DEFAULT_CHECK_MESSAGE: &str = "not permitted";

// ---------------------------------------------------------------------------
// Attribute model
// ---------------------------------------------------------------------------

/// One field of a derived dependency.
#[derive(Debug, FromField)]
#[darling(attributes(depends))]
struct DependencyField {
    /// The field's name, or `None` for a tuple field.
    ident: Option<Ident>,
    /// The declared type.
    ty: Type,
    /// `#[depends(default)]` — fill the field with `Default::default()`
    /// instead of resolving it.
    #[darling(default)]
    default: Flag,
    /// `#[depends(provider)]` — an `Arc<T>` read from the provider map, which
    /// adds `ProviderReq::of::<T>()` to `PROVIDER_REQ`.
    #[darling(default)]
    provider: Flag,
}

/// The `#[derive(Dependency)]` input.
#[derive(Debug, FromDeriveInput)]
// No `supports(..)`: darling would print "Unsupported shape `enum`", which
// names no fix. The hand-written check in `build` names two.
#[darling(attributes(depends), forward_attrs(doc))]
struct DependencyInput {
    /// The type's name.
    ident: Ident,
    /// Generics, which the derive rejects.
    generics: syn::Generics,
    /// The fields.
    data: ast::Data<darling::util::Ignored, DependencyField>,
    /// Forwarded `///` documentation.
    #[allow(dead_code)]
    attrs: Vec<syn::Attribute>,
    /// `#[depends(from = CurrentUser)]` — the dependency this one wraps.
    ///
    /// A `Path` rather than a `Type`, because `from = CurrentUser` reaches
    /// darling as an *expression* and only a path shape can be read from one.
    /// A generic dependency is still reachable, quoted: `from = "Scoped<Foo>"`.
    #[darling(default)]
    from: Option<SpannedValue<Path>>,
    /// `#[depends(check = "is_admin")]` — a predicate over the wrapped value.
    #[darling(default)]
    check: Option<SpannedValue<String>>,
    /// `#[depends(error = "admin required")]` — the detail of a failed check.
    #[darling(default)]
    error: Option<SpannedValue<String>>,
    /// `#[depends(status = 403)]` — the status of a failed check.
    #[darling(default)]
    status: Option<SpannedValue<u16>>,
    /// `#[depends(unwrap = false)]` — keep the value `from` resolved to,
    /// rather than taking its `.0`.
    ///
    /// The default is inferred: a wrapper whose field type differs from `from`
    /// takes `.0` (`AdminUser(User)` from `CurrentUser(User)`), and one whose
    /// field type *is* `from` keeps it. A unit struct has no field to compare,
    /// so it takes `.0` — the shape only exists to run a `check` against the
    /// value inside. Set this when the inference guesses wrong.
    #[darling(default)]
    unwrap: Option<SpannedValue<bool>>,
    /// `#[depends(manual)]` — emit no trait impl; the user writes one.
    #[darling(default)]
    manual: Flag,
}

/// How one field of a composed dependency is produced.
enum Source {
    /// `ctx.depends::<T>().await?`.
    Dependency(Type),
    /// `ctx.provider::<T>()?`, for a field of type `Arc<T>`.
    Provider(Type),
    /// `Default::default()`.
    Default,
}

/// One field, planned.
struct FieldPlan {
    /// The local the value is bound to.
    binding: Ident,
    /// The field's name, or `None` for a tuple field.
    name: Option<Ident>,
    /// Where the value comes from.
    source: Source,
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

/// Expand `#[derive(Dependency)]`.
///
/// Wire this up from `lib.rs` with
/// `#[proc_macro_derive(Dependency, attributes(depends))]`.
pub(crate) fn expand(input: DeriveInput) -> TokenStream {
    let ident = input.ident.clone();

    let parsed = match DependencyInput::from_derive_input(&input) {
        Ok(parsed) => parsed,
        Err(error) => return with_placeholder(&ident, error.write_errors()),
    };

    match build(&parsed) {
        Ok(tokens) => tokens,
        Err(error) => with_placeholder(&ident, error.to_compile_error()),
    }
}

/// The impl that keeps one bad attribute from becoming a page of errors.
///
/// Without it, every `Depends<T>` parameter in every handler adds "the trait
/// bound `AdminUser: Dependency` is not satisfied" on top of the real error.
fn with_placeholder(ident: &Ident, errors: TokenStream) -> TokenStream {
    let moso = moso();
    quote! {
        #errors

        #[automatically_derived]
        impl #moso::Dependency for #ident {
            async fn resolve(_ctx: &#moso::RequestCtx) -> #moso::Result<Self> {
                ::core::unimplemented!()
            }
        }
    }
}

/// Build the `Dependency` impl, or the first problem found.
fn build(input: &DependencyInput) -> syn::Result<TokenStream> {
    let moso = moso();
    let ident = &input.ident;

    let ast::Data::Struct(fields) = &input.data else {
        return Err(Diagnostic::new("`#[derive(Dependency)]` needs a struct")
            .note(
                "a dependency resolves to one value per request; an enum would have to choose a \
                 variant, which is a decision only `resolve` can make",
            )
            .help_code(
                "wrap the choice in a struct, or write the impl by hand:",
                format!(
                    "impl Dependency for {ident} {{\n    async fn resolve(ctx: &RequestCtx) \
                         -> Result<Self> {{ /* … */ }}\n}}"
                ),
            )
            .at(ident));
    };

    if let Some(error) = reject_generics(
        &input.generics,
        "Dependency",
        ident,
        "`PROVIDER_REQ` is a `const` built from the fields, and a `const` cannot read the generic \
         parameters of the item around it",
    ) {
        return Err(error);
    }

    check_conflicts(input)?;

    if input.manual.is_present() {
        return Ok(manual_impl(input, fields, &moso));
    }

    let body = match &input.from {
        Some(from) => wrap_and_check(input, fields, from, &moso)?,
        None => compose(input, fields, &moso)?,
    };
    let assertions = assertions(ident, &moso);

    Ok(quote! {
        #body
        #assertions
    })
}

/// Reject attribute combinations that cannot both be honoured.
fn check_conflicts(input: &DependencyInput) -> syn::Result<()> {
    if input.manual.is_present() {
        if let Some(from) = &input.from {
            return Err(Diagnostic::new(
                "`#[depends(manual)]` and `#[depends(from = ..)]` cannot be combined",
            )
            .note("`manual` means the derive writes no `resolve`, and `from` is a `resolve`")
            .help("drop `manual` to use the generated wrap-and-check `resolve`")
            .at_span(from.span()));
        }
        return Ok(());
    }

    if input.from.is_none() {
        if let Some(check) = &input.check {
            return Err(Diagnostic::new(
                "`#[depends(check = ..)]` needs a `#[depends(from = ..)]` to check",
            )
            .note("the predicate is evaluated against the value `from` resolves to")
            .help_code(
                "name the dependency being wrapped:",
                "#[depends(from = CurrentUser, check = \"is_admin\", error = \"admin required\")]",
            )
            .at_span(check.span()));
        }
        for (key, span) in [
            (
                "error",
                input.error.as_ref().map(darling::util::SpannedValue::span),
            ),
            (
                "status",
                input.status.as_ref().map(darling::util::SpannedValue::span),
            ),
        ] {
            if let Some(span) = span {
                return Err(Diagnostic::new(format!(
                    "`#[depends({key} = ..)]` describes a failed check, and there is no check"
                ))
                .note("without `check`, the generated `resolve` cannot fail on its own")
                .help_code(
                    "add the predicate:",
                    "#[depends(from = CurrentUser, check = \"is_admin\")]",
                )
                .at_span(span));
            }
        }
        return Ok(());
    }

    if input.check.is_none() {
        for (key, span) in [
            (
                "error",
                input.error.as_ref().map(darling::util::SpannedValue::span),
            ),
            (
                "status",
                input.status.as_ref().map(darling::util::SpannedValue::span),
            ),
        ] {
            if let Some(span) = span {
                return Err(Diagnostic::new(format!(
                    "`#[depends({key} = ..)]` describes a failed check, and there is no check"
                ))
                .note("a `from` without a `check` cannot fail, so nothing would produce this")
                .help_code(
                    "add the predicate:",
                    "#[depends(from = CurrentUser, check = \"is_admin\")]",
                )
                .at_span(span));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// `#[depends(manual)]`
// ---------------------------------------------------------------------------

/// Emit only the requirement table, leaving `impl Dependency` to the user.
fn manual_impl(
    input: &DependencyInput,
    fields: &ast::Fields<DependencyField>,
    moso: &TokenStream,
) -> TokenStream {
    let ident = &input.ident;
    let plans = fields
        .iter()
        .enumerate()
        .map(|(index, field)| plan_field(index, field))
        .collect::<syn::Result<Vec<_>>>()
        .unwrap_or_default();
    let requirements = provider_reqs(&plans, moso);

    quote! {
        #[automatically_derived]
        impl #ident {
            /// The providers this dependency's fields need.
            ///
            /// Generated by `#[derive(moso::Dependency)]` with
            /// `#[depends(manual)]`. Point the hand-written impl at it:
            /// `const PROVIDER_REQ: &'static [ProviderReq] = Self::MOSO_PROVIDER_REQ;`
            #[doc(hidden)]
            pub const MOSO_PROVIDER_REQ: &'static [#moso::ProviderReq] = #requirements;
        }
    }
}

// ---------------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------------

/// Every field resolves itself; `PROVIDER_REQ` is their union.
fn compose(
    input: &DependencyInput,
    fields: &ast::Fields<DependencyField>,
    moso: &TokenStream,
) -> syn::Result<TokenStream> {
    let ident = &input.ident;
    let plans = fields
        .iter()
        .enumerate()
        .map(|(index, field)| plan_field(index, field))
        .collect::<syn::Result<Vec<_>>>()?;

    let requirements = provider_reqs(&plans, moso);
    let describes = plans.iter().filter_map(|plan| match &plan.source {
        Source::Dependency(ty) => Some(quote!(<#ty as #moso::Dependency>::describe(__op);)),
        _ => None,
    });
    let resolves = plans.iter().map(|plan| {
        let binding = &plan.binding;
        match &plan.source {
            Source::Dependency(ty) => quote!(let #binding = __ctx.depends::<#ty>().await?;),
            Source::Provider(ty) => quote!(let #binding = __ctx.provider::<#ty>()?;),
            Source::Default => quote!(let #binding = ::core::default::Default::default();),
        }
    });
    let construct = construct(fields.style, &plans);

    Ok(quote! {
        #[automatically_derived]
        impl #moso::Dependency for #ident {
            const PROVIDER_REQ: &'static [#moso::ProviderReq] = #requirements;

            fn describe(__op: &mut #moso::OperationBuilder) {
                #(#describes)*
            }

            async fn resolve(__ctx: &#moso::RequestCtx) -> #moso::Result<Self> {
                #(#resolves)*
                ::core::result::Result::Ok(#construct)
            }
        }
    })
}

/// Work out where one field's value comes from.
fn plan_field(index: usize, field: &DependencyField) -> syn::Result<FieldPlan> {
    if field.default.is_present() && field.provider.is_present() {
        return Err(Diagnostic::new(
            "`#[depends(default)]` and `#[depends(provider)]` cannot be combined",
        )
        .note("a field is filled from exactly one place")
        .help("keep `provider` to read the application's value, or `default` for a fresh one")
        .at(&field.ty));
    }

    let source = if field.default.is_present() {
        Source::Default
    } else if field.provider.is_present() {
        let inner = generic_inner(&field.ty, &["Arc"]).ok_or_else(|| {
            Diagnostic::new(format!(
                "`#[depends(provider)]` needs an `Arc<T>`, and `{}` is not one",
                type_display(&field.ty)
            ))
            .note(
                "the provider map hands out `Arc`s, because a provider is shared by every request",
            )
            .help_code(
                "wrap the type:",
                format!(
                    "#[depends(provider)]\npub db: ::std::sync::Arc<{}>,",
                    type_display(&field.ty)
                ),
            )
            .at(&field.ty)
        })?;
        Source::Provider(inner.clone())
    } else {
        Source::Dependency(field.ty.clone())
    };

    Ok(FieldPlan {
        binding: format_ident!("__moso_d{}", index),
        name: field.ident.clone(),
        source,
    })
}

/// `concat_reqs!` over every field that needs something.
///
/// A `#[depends(provider)]` field contributes a one-element slice, and that
/// slice needs a `const` of its own: `&[ProviderReq::of::<Db>()]` is a
/// *function call*, and rvalue static promotion deliberately refuses to promote
/// one, so the inline form would be a temporary that dies at the end of the
/// statement.
fn provider_reqs(plans: &[FieldPlan], moso: &TokenStream) -> TokenStream {
    let mut definitions: Vec<TokenStream> = Vec::new();
    let mut slices: Vec<TokenStream> = Vec::new();

    for (index, plan) in plans.iter().enumerate() {
        match &plan.source {
            Source::Dependency(ty) => {
                slices.push(quote!(<#ty as #moso::Dependency>::PROVIDER_REQ));
            }
            Source::Provider(ty) => {
                let name = format_ident!("__MOSO_PROVIDER_{}", index);
                definitions.push(quote! {
                    const #name: &'static [#moso::ProviderReq] =
                        &[#moso::ProviderReq::of::<#ty>()];
                });
                slices.push(quote!(#name));
            }
            Source::Default => {}
        }
    }

    if definitions.is_empty() {
        return quote!(#moso::concat_reqs!(#(#slices),*));
    }
    quote! {
        {
            #(#definitions)*
            #moso::concat_reqs!(#(#slices),*)
        }
    }
}

/// `Self { a: __moso_d0 }`, `Self(__moso_d0)` or `Self`.
fn construct(style: ast::Style, plans: &[FieldPlan]) -> TokenStream {
    match style {
        ast::Style::Unit => quote!(Self),
        ast::Style::Tuple => {
            let bindings = plans.iter().map(|plan| &plan.binding);
            quote!(Self(#(#bindings),*))
        }
        ast::Style::Struct => {
            let bindings = plans.iter().map(|plan| {
                let name = plan.name.as_ref().expect("a named field");
                let binding = &plan.binding;
                quote!(#name: #binding)
            });
            quote!(Self { #(#bindings),* })
        }
    }
}

// ---------------------------------------------------------------------------
// Wrap and check
// ---------------------------------------------------------------------------

/// The `#[depends(from = .., check = .., error = ..)]` shape.
fn wrap_and_check(
    input: &DependencyInput,
    fields: &ast::Fields<DependencyField>,
    from: &SpannedValue<Path>,
    moso: &TokenStream,
) -> syn::Result<TokenStream> {
    let ident = &input.ident;
    let from_type: &Path = from;

    if fields.len() > 1 {
        return Err(Diagnostic::new(
            "`#[depends(from = ..)]` needs a struct with at most one field",
        )
        .note("the generated `resolve` has only the wrapped value, and nothing to fill the rest with")
        .help_code(
            "resolve the other fields as dependencies of their own, and drop `from`:",
            format!(
                "#[derive(Dependency, Clone)]\npub struct {ident} {{ user: CurrentUser, tenant: Tenant }}"
            ),
        )
        .at_span(from.span()));
    }

    // `AdminUser(User)` wrapping `CurrentUser(User)` takes the inner value;
    // `Admin(CurrentUser)` keeps the whole thing. Comparing the spellings is
    // all a derive can do, and it gets both documented shapes right.
    let field = fields.iter().next();
    let unwrap = input.unwrap.as_ref().map_or_else(
        || {
            field.is_none_or(|field| {
                field.ty.to_token_stream().to_string() != from_type.to_token_stream().to_string()
            })
        },
        |explicit| **explicit,
    );
    let bind = if unwrap {
        quote!(let this = __moso_from.0;)
    } else {
        quote!(let this = __moso_from;)
    };

    let construct = match field.map(|field| field.ident.clone()) {
        None => quote!(Self),
        Some(Some(name)) => quote!(Self { #name: this }),
        Some(None) => quote!(Self(this)),
    };

    let (check, describe_failure) = match &input.check {
        Some(check) => {
            let predicate = check_expression(check)?;
            let message = input
                .error
                .as_ref()
                .map_or(DEFAULT_CHECK_MESSAGE, |error| error.as_str());
            let status = input.status.as_ref().map_or(DEFAULT_CHECK_STATUS, |s| **s);
            let status_span = input
                .status
                .as_ref()
                .map_or_else(|| check.span(), darling::util::SpannedValue::span);
            let Some(kind) = kind_for(status) else {
                return Err(unmapped_status(status, status_span));
            };
            let kind = Ident::new(kind, status_span);
            // A 5xx never carries its detail to the client, and a dependency
            // check that answers 5xx is a bug rather than a policy; the detail
            // is attached either way and `Problem::from_error` suppresses it.
            (
                quote! {
                    if !(#predicate) {
                        return ::core::result::Result::Err(
                            #moso::Error::new(#moso::ErrorKind::#kind).with_detail(#message)
                        );
                    }
                },
                quote!(__op.response(#status, #moso::ResponseSpec::problem(#message));),
            )
        }
        None => (quote!(), quote!()),
    };

    Ok(quote! {
        #[automatically_derived]
        impl #moso::Dependency for #ident {
            const PROVIDER_REQ: &'static [#moso::ProviderReq] =
                <#from_type as #moso::Dependency>::PROVIDER_REQ;

            fn describe(__op: &mut #moso::OperationBuilder) {
                <#from_type as #moso::Dependency>::describe(__op);
                #describe_failure
            }

            async fn resolve(__ctx: &#moso::RequestCtx) -> #moso::Result<Self> {
                let __moso_from = __ctx.depends::<#from_type>().await?;
                #bind
                #check
                ::core::result::Result::Ok(#construct)
            }
        }
    })
}

/// Turn a `check = "…"` string into a predicate over `this`.
///
/// Three spellings, in order of how often they are wanted:
///
/// | Written | Becomes |
/// | --- | --- |
/// | `"is_admin"` | `this.is_admin` — a field |
/// | `"is_admin()"` | `this.is_admin()` — a method |
/// | anything else | verbatim, with `this` bound to the wrapped value |
fn check_expression(check: &SpannedValue<String>) -> syn::Result<TokenStream> {
    let parsed: Expr = syn::parse_str(check.as_str()).map_err(|error| {
        Diagnostic::new(format!("`check` is not a Rust expression: {error}"))
            .note("the predicate is evaluated with `this` bound to the value `from` resolved to")
            .help_code(
                "name a field, a method, or write the comparison out:",
                "#[depends(check = \"is_admin\")]\n#[depends(check = \"has_role()\")]\n\
                 #[depends(check = \"this.role == Role::Admin\")]",
            )
            .at_span(check.span())
    })?;

    Ok(match &parsed {
        Expr::Path(path) if path.qself.is_none() && path.path.get_ident().is_some() => {
            let member = path.path.get_ident().expect("a single-segment path");
            quote!(this.#member)
        }
        Expr::Call(call) => match call.func.as_ref() {
            Expr::Path(path)
                if path.qself.is_none()
                    && path.path.get_ident().is_some()
                    && call.args.is_empty() =>
            {
                let member = path.path.get_ident().expect("a single-segment path");
                quote!(this.#member())
            }
            _ => quote!(#parsed),
        },
        _ => quote!(#parsed),
    })
}

/// Assertion codegen: point a missing `Clone` at the user's type.
///
/// `Dependency: Clone + Send + Sync + 'static`, and forgetting `Clone` is the
/// most common mistake — every example in the docs writes
/// `#[derive(Dependency, Clone)]` for exactly this reason.
fn assertions(ident: &Ident, moso: &TokenStream) -> TokenStream {
    quote! {
        #[doc(hidden)]
        const _: () = {
            fn __moso_assert_dependency<
                T: ::core::clone::Clone
                    + ::core::marker::Send
                    + ::core::marker::Sync
                    + 'static,
            >() {}
            fn __moso_check() {
                __moso_assert_dependency::<#ident>();
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
    fn the_documented_wrap_and_check_expands() {
        let out = expand_str(quote! {
            #[depends(from = CurrentUser, check = "is_admin", error = "admin required")]
            pub struct AdminUser(pub User);
        });
        assert!(
            out.contains("depends :: < CurrentUser > () . await ?"),
            "{out}"
        );
        assert!(out.contains("let this = __moso_from . 0 ;"), "{out}");
        assert!(out.contains("if ! (this . is_admin)"), "{out}");
        assert!(out.contains("ErrorKind :: Forbidden"), "{out}");
        assert!(out.contains(". with_detail (\"admin required\")"), "{out}");
        assert!(out.contains("Ok (Self (this))"), "{out}");
        // The 403 is documented, and so is whatever `CurrentUser` documents.
        assert!(
            out.contains("< CurrentUser as :: moso :: __private :: Dependency > :: describe"),
            "{out}"
        );
        assert!(out.contains("__op . response (403u16"), "{out}");
        // Transitive requirements come from the wrapped dependency.
        assert!(
            out.contains("< CurrentUser as :: moso :: __private :: Dependency > :: PROVIDER_REQ"),
            "{out}"
        );
    }

    #[test]
    fn a_check_that_is_a_call_stays_a_call() {
        let out = expand_str(quote! {
            #[depends(from = CurrentUser, check = "is_admin()")]
            pub struct AdminUser(pub User);
        });
        assert!(out.contains("if ! (this . is_admin ())"), "{out}");
        assert!(out.contains("\"not permitted\""), "{out}");
    }

    #[test]
    fn a_check_that_mentions_this_is_used_verbatim() {
        let out = expand_str(quote! {
            #[depends(from = CurrentUser, check = "this.role == Role::Admin")]
            pub struct AdminUser(pub User);
        });
        assert!(out.contains("if ! (this . role == Role :: Admin)"), "{out}");
    }

    #[test]
    fn wrapping_the_same_type_does_not_unwrap() {
        let out = expand_str(quote! {
            #[depends(from = CurrentUser, check = "is_admin")]
            pub struct AdminUser(pub CurrentUser);
        });
        assert!(out.contains("let this = __moso_from ;"), "{out}");
        assert!(!out.contains("__moso_from . 0"), "{out}");
    }

    #[test]
    fn a_unit_struct_can_be_a_pure_check() {
        let out = expand_str(quote! {
            #[depends(from = CurrentUser, check = "is_admin", error = "admin required")]
            pub struct AdminOnly;
        });
        assert!(out.contains("Ok (Self)"), "{out}");
        // Nothing to carry, so the check runs against the wrapped value.
        assert!(out.contains("let this = __moso_from . 0 ;"), "{out}");
    }

    #[test]
    fn unwrap_can_be_overridden() {
        let out = expand_str(quote! {
            #[depends(from = Session, check = "is_fresh", unwrap = false)]
            pub struct FreshSession;
        });
        assert!(out.contains("let this = __moso_from ;"), "{out}");
    }

    #[test]
    fn a_named_wrapper_names_its_field() {
        let out = expand_str(quote! {
            #[depends(from = CurrentUser, check = "is_admin")]
            pub struct AdminUser { pub user: User }
        });
        assert!(out.contains("Ok (Self { user : this })"), "{out}");
    }

    #[test]
    fn a_custom_status_picks_its_kind() {
        let out = expand_str(quote! {
            #[depends(from = ApiKey, check = "is_live", status = 401, error = "live key required")]
            pub struct LiveKey(pub Key);
        });
        assert!(out.contains("ErrorKind :: Unauthenticated"), "{out}");
        assert!(out.contains("__op . response (401u16"), "{out}");
    }

    #[test]
    fn composition_resolves_every_field_and_unions_the_requirements() {
        let out = expand_str(quote! {
            pub struct Editing {
                user: CurrentUser,
                tenant: Tenant,
            }
        });
        assert!(
            out.contains("depends :: < CurrentUser > () . await ?"),
            "{out}"
        );
        assert!(out.contains("depends :: < Tenant > () . await ?"), "{out}");
        assert!(out.contains("concat_reqs !"), "{out}");
        assert!(
            out.contains("< CurrentUser as :: moso :: __private :: Dependency > :: PROVIDER_REQ"),
            "{out}"
        );
        assert!(
            out.contains("< Tenant as :: moso :: __private :: Dependency > :: PROVIDER_REQ"),
            "{out}"
        );
        assert!(
            out.contains("Ok (Self { user : __moso_d0 , tenant : __moso_d1 })"),
            "{out}"
        );
    }

    #[test]
    fn a_provider_field_adds_a_requirement() {
        let out = expand_str(quote! {
            pub struct WithDb {
                #[depends(provider)]
                db: ::std::sync::Arc<Db>,
            }
        });
        assert!(out.contains("provider :: < Db > () ?"), "{out}");
        assert!(out.contains("const __MOSO_PROVIDER_0"), "{out}");
        assert!(out.contains("ProviderReq :: of :: < Db > ()"), "{out}");
    }

    #[test]
    fn a_provider_field_that_is_not_an_arc_is_rejected() {
        let out = expand_str(quote! {
            pub struct WithDb {
                #[depends(provider)]
                db: Db,
            }
        });
        assert!(out.contains("needs an `Arc<T>`"), "{out}");
        assert!(out.contains("Arc<Db>"), "{out}");
    }

    #[test]
    fn a_default_field_contributes_nothing() {
        let out = expand_str(quote! {
            pub struct Scoped {
                user: CurrentUser,
                #[depends(default)]
                seen: bool,
            }
        });
        assert!(out.contains("Default :: default ()"), "{out}");
        // One slice in the union, not two.
        assert_eq!(out.matches(":: PROVIDER_REQ").count(), 1, "{out}");
    }

    #[test]
    fn a_unit_struct_composes_to_nothing() {
        let out = expand_str(quote! {
            pub struct Anonymous;
        });
        assert!(out.contains("concat_reqs ! ()"), "{out}");
        assert!(out.contains("Ok (Self)"), "{out}");
    }

    #[test]
    fn manual_emits_only_the_requirement_table() {
        let out = expand_str(quote! {
            #[depends(manual)]
            pub struct AdminUser(pub User);
        });
        assert!(
            !out.contains("impl :: moso :: __private :: Dependency"),
            "{out}"
        );
        assert!(out.contains("pub const MOSO_PROVIDER_REQ"), "{out}");
    }

    #[test]
    fn manual_and_from_are_rejected_together() {
        let out = expand_str(quote! {
            #[depends(manual, from = CurrentUser)]
            pub struct AdminUser(pub User);
        });
        assert!(out.contains("cannot be combined"), "{out}");
    }

    #[test]
    fn a_check_without_a_from_is_rejected() {
        let out = expand_str(quote! {
            #[depends(check = "is_admin")]
            pub struct AdminUser(pub User);
        });
        assert!(out.contains("needs a `#[depends(from = ..)]`"), "{out}");
    }

    #[test]
    fn an_error_without_a_check_is_rejected() {
        let out = expand_str(quote! {
            #[depends(from = CurrentUser, error = "nope")]
            pub struct AdminUser(pub User);
        });
        assert!(out.contains("there is no check"), "{out}");
    }

    #[test]
    fn from_with_two_fields_is_rejected() {
        let out = expand_str(quote! {
            #[depends(from = CurrentUser)]
            pub struct AdminUser { a: User, b: Tenant }
        });
        assert!(out.contains("at most one field"), "{out}");
    }

    #[test]
    fn an_enum_is_rejected_with_two_fixes() {
        let out = expand_str(quote! {
            pub enum Either { A, B }
        });
        assert!(out.contains("needs a struct"), "{out}");
        assert!(out.contains("impl Dependency for Either"), "{out}");
    }

    #[test]
    fn a_generic_dependency_is_rejected() {
        let out = expand_str(quote! {
            pub struct Scoped<T>(T);
        });
        assert!(out.contains("cannot be used on a generic type"), "{out}");
        // The placeholder keeps `Depends<Scoped<_>>` from adding a second error.
        assert!(
            out.contains("impl :: moso :: __private :: Dependency for Scoped"),
            "{out}"
        );
    }

    #[test]
    fn a_clone_assertion_points_at_the_users_type() {
        let out = expand_str(quote! {
            pub struct Editing { user: CurrentUser }
        });
        assert!(
            out.contains("__moso_assert_dependency :: < Editing > ()"),
            "{out}"
        );
    }

    #[test]
    fn an_unknown_key_is_rejected_with_a_suggestion() {
        let out = expand_str(quote! {
            #[depends(fron = CurrentUser)]
            pub struct AdminUser(pub User);
        });
        assert!(out.contains("compile_error !"), "{out}");
        assert!(out.contains("Did you mean"), "{out}");
    }
}
