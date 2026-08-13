//! `#[middleware]` — the function-shaped Tower layer.
//!
//! Writing a `Layer`, a `Service` and a hand-rolled `Future` is the single
//! most-cited Tower papercut, and 90% of the time the middleware is one
//! `async fn`. This module turns
//!
//! ```
//! use moso::prelude::*;
//! use moso::middleware::Next;
//! use moso::{Request, Response};
//! # /// One customer's slice of the system.
//! # #[derive(Clone)] pub struct Tenant(&'static str);
//! /// Resolve the tenant before anything downstream runs.
//! #[moso::middleware]
//! async fn tenant(mut req: Request, next: Next) -> Result<Response> {
//!     req.extensions_mut().insert(Tenant("acme"));
//!     Ok(next.run(req).await)
//! }
//! # fn main() { assert_eq!(TenantLayer::NAME, "tenant"); }
//! ```
//!
//! into a named, `Clone` `TenantLayer` / `TenantService<S>` pair, exactly as
//! `docs/06-reference/62-macro-reference.md` specifies.
//!
//! # Shape of the expansion
//!
//! An outline of the generated items, not a program:
//!
//! ```text
//! async fn tenant(mut req: Request, next: Next) -> Result<Response> { /* unchanged */ }
//!
//! #[derive(Clone, Copy, Debug, Default)]
//! pub(crate) struct TenantLayer;
//!
//! impl TenantLayer {
//!     pub const NAME: &'static str = "tenant";
//!     pub const PROVIDER_REQ: &'static [ProviderReq] = concat_reqs!();
//!     pub const fn new() -> Self { TenantLayer }
//!     pub const fn required_providers() -> &'static [ProviderReq] { Self::PROVIDER_REQ }
//! }
//!
//! impl<S> tower::Layer<S> for TenantLayer {
//!     type Service = TenantService<S>;
//!     fn layer(&self, inner: S) -> TenantService<S> { TenantService { inner } }
//! }
//!
//! pub(crate) struct TenantService<S> { inner: S }
//! impl<S: Clone> Clone for TenantService<S> { /* … */ }
//! impl<S> Debug for TenantService<S> { /* … */ }
//! impl<S> tower::Service<Request> for TenantService<S> where /* the Route bounds */ {
//!     fn call(&mut self, req: Request) -> Self::Future {
//!         let inner = /* the polled-ready instance */;
//!         Box::pin(async move {
//!             let next = Next::new(inner);
//!             Ok(match tenant(req, next).await {
//!                 Ok(response) => response,
//!                 Err(error)   => error.into_response(),
//!             })
//!         })
//!     }
//! }
//! ```
//!
//! The `Service` impl is generic over `S`, but every Moso registration point
//! (`Router::layer`, `MiddlewareStack::insert_after`, …) erases the inner
//! service to `moso::Route` first, so exactly one instantiation is compiled no
//! matter how many routes the layer is applied to.
//!
//! # Leading extractor parameters
//!
//! ```
//! use moso::prelude::*;
//! use moso::middleware::Next;
//! use moso::{Request, Response};
//! # /// A database handle.
//! # #[derive(Default)] pub struct Db;
//! /// Resolve the tenant, with the database already in hand.
//! #[moso::middleware]
//! async fn tenant(Inject(db): Inject<Db>, req: Request, next: Next) -> Result<Response> {
//!     let _ = db;
//!     Ok(next.run(req).await)
//! }
//! # fn main() { assert_eq!(TenantLayer::PROVIDER_REQ.len(), 1); }
//! ```
//!
//! Every parameter before `req`/`next` is extracted with `Extract` before the
//! function is called, and its `PROVIDER_REQ` is folded into
//! `TenantLayer::PROVIDER_REQ` so boot validation can see it.
//!
//! Extraction needs a `RequestCtx`, and middleware runs before the router
//! builds one, so the generated code calls **`::moso::__private::middleware_ctx`
//! — a re-export the facade must provide**. It recovers a context an inner
//! layer already installed, or builds one over the `Arc<AppState>` in the
//! request extensions. Only middleware with leading parameters emits the call;
//! the plain `(req, next)` shape touches neither.
//!
//! `Depends<T>` is rejected outright: middleware runs before extraction, so a
//! request dependency does not exist yet. The message is the one
//! `docs/01-http/17-middleware.md` publishes, word for word.

use heck::ToUpperCamelCase;
use proc_macro2::{Ident, TokenStream};
use quote::{ToTokens, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{FnArg, ItemFn, PatType, ReturnType, Type, Visibility};

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------
//
// Every message follows the style guide in `docs/04-devex/41-diagnostics.md`:
// plain language, then `= note:` for the rule and `= help:` for a fix the
// reader can paste. `compile_error!` prints the message verbatim, so the
// `note`/`help` prefixes are written into the string.

/// The canonical signature, quoted in almost every message.
///
/// Only referenced by the test that guards the messages against drifting apart
/// from it; the messages themselves have to be `const` strings, which cannot be
/// built by interpolation.
#[cfg(test)]
const SIGNATURE: &str = "async fn tenant(req: Request, next: Next) -> Result<Response>";

const NOT_A_FUNCTION: &str = "\
`#[middleware]` can only be applied to a function
  = note: it generates a Tower `Layer` and `Service` that call the function once per request
  = help: write `#[moso::middleware] async fn tenant(req: Request, next: Next) -> Result<Response>`";

const NOT_ASYNC: &str = "\
middleware must be an `async fn`
  = note: the generated service awaits the function, so its body has to be a future
  = help: add `async`: `async fn tenant(req: Request, next: Next) -> Result<Response>`";

const IS_GENERIC: &str = "\
middleware may not be generic; use a concrete type or a trait object
  = note: the layer is boxed once at registration, and a generic function has no single body to box
  = help: name the concrete type, or take the erased form: `Inject(mailer): Inject<dyn Mailer>`";

const HAS_RECEIVER: &str = "\
middleware must be a free function, not a method
  = note: the generated layer calls the function by name, so there is no `self` to call it on
  = help: move it out of the `impl` block: `async fn tenant(req: Request, next: Next) -> Result<Response>`";

const IS_VARIADIC: &str = "\
middleware may not be variadic
  = help: write `async fn tenant(req: Request, next: Next) -> Result<Response>`";

const WRONG_ARITY: &str = "\
middleware must take the request and the rest of the stack as its last two parameters
  = note: `Next` is the rest of the stack; `next.run(req).await` calls it
  = help: write `async fn tenant(req: Request, next: Next) -> Result<Response>`
  = help: values from the provider map go first: `async fn tenant(Inject(db): Inject<Db>, req: Request, next: Next) -> Result<Response>`";

const LAST_NOT_NEXT: &str = "\
the last parameter of a middleware must be `next: Next`
  = note: `Next` is the rest of the stack; without it the middleware could never call inwards
  = help: write `async fn tenant(req: Request, next: Next) -> Result<Response>`";

const SECOND_LAST_NOT_REQUEST: &str = "\
the second-to-last parameter of a middleware must be `req: Request`
  = note: middleware sees the whole request, before any extractor has run
  = help: write `async fn tenant(req: Request, next: Next) -> Result<Response>`";

const NO_RETURN_TYPE: &str = "\
middleware must return `Result<Response>`
  = note: returning `Err` short-circuits the stack with a problem document, so `?` works here
  = help: write `-> Result<Response>` and end the body with `Ok(next.run(req).await)`";

/// The message `docs/01-http/17-middleware.md` publishes, and the one
/// `tests/ui/extract/depends_in_middleware.rs` asserts.
fn depends_message(rendered: &str) -> String {
    format!(
        "\
`{rendered}` cannot be used in middleware
  = note: middleware runs before extractors, so request dependencies are not yet available
  = help: read a middleware-inserted value with `req.extensions()`, or move this logic into
          a `Dependency` impl and use it in the handler"
    )
}

fn body_extractor_message(rendered: &str) -> String {
    format!(
        "\
`{rendered}` cannot be used in middleware
  = note: middleware runs before the body is read, and taking it here would consume it
  = help: take the body in the handler, or read it from the request: `let body = req.into_body();`"
    )
}

/// Runtime detail of the "no `RequestCtx`" arm, kept short on purpose: it is a
/// 500 body, not a compile error.
fn missing_context_message(name: &str) -> String {
    format!(
        "the `{name}` middleware injects a provider, but no `RequestCtx` is available at this \
         point in the stack. `Inject` needs the application's provider map; register this \
         middleware through `App::with_middleware` or `Router::layer` on a router an `App` \
         mounted, not on a bare `axum::Router`."
    )
}

/// Body extractors, rejected by name so the message beats trait resolution.
const BODY_EXTRACTORS: &[&str] = &["Json", "Form", "Multipart", "RawBody", "BodyStream"];

/// The keys `#[middleware(..)]` accepts.
const KEYS: &[&str] = &["name", "vis", "layer", "service"];

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Expand `#[middleware]`.
///
/// Always emits the user's function unchanged, so a mistake in the attribute
/// never turns into "cannot find function `tenant`" everywhere it is called.
/// When the signature is wrong the layer and service types are still emitted,
/// as pass-through placeholders, for the same reason.
pub(crate) fn expand(args: TokenStream, item: TokenStream) -> TokenStream {
    let func: ItemFn = match syn::parse2(item.clone()) {
        Ok(parsed) => parsed,
        Err(error) => {
            let reported = syn::Error::new(error.span(), NOT_A_FUNCTION).to_compile_error();
            return quote! { #reported #item };
        }
    };

    let mut errors: Vec<syn::Error> = Vec::new();
    let options = Options::parse(args, &mut errors);
    let plan = Plan::new(&func, &options, &mut errors);

    let generated = plan.emit();
    let reported = errors.iter().map(syn::Error::to_compile_error);

    quote! {
        #func
        #(#reported)*
        #generated
    }
}

// ---------------------------------------------------------------------------
// Attribute arguments
// ---------------------------------------------------------------------------

/// `#[middleware(name = "…", vis = "…", layer = "…", service = "…")]`.
#[derive(Default)]
struct Options {
    /// The name `moso middleware` prints. Defaults to the function's name.
    name: Option<String>,
    /// Visibility of the generated types. Defaults to the function's, widened
    /// to `pub(crate)` when the function is private.
    vis: Option<Visibility>,
    /// Overrides the `…Layer` identifier.
    layer: Option<Ident>,
    /// Overrides the `…Service` identifier.
    service: Option<Ident>,
}

impl Options {
    fn parse(args: TokenStream, errors: &mut Vec<syn::Error>) -> Self {
        let mut out = Options::default();
        if args.is_empty() {
            return out;
        }

        let parser = syn::meta::parser(|meta| {
            let key = match meta.path.get_ident() {
                Some(ident) => ident.to_string(),
                None => {
                    return Err(meta.error(unknown_key_message(
                        &meta.path.to_token_stream().to_string(),
                    )));
                }
            };
            match key.as_str() {
                "name" => {
                    let literal: syn::LitStr = meta.value()?.parse()?;
                    out.name = Some(literal.value());
                    Ok(())
                }
                "vis" => {
                    let literal: syn::LitStr = meta.value()?.parse()?;
                    out.vis = Some(literal.parse()?);
                    Ok(())
                }
                "layer" => {
                    let literal: syn::LitStr = meta.value()?.parse()?;
                    out.layer = Some(literal.parse()?);
                    Ok(())
                }
                "service" => {
                    let literal: syn::LitStr = meta.value()?.parse()?;
                    out.service = Some(literal.parse()?);
                    Ok(())
                }
                other => Err(meta.error(unknown_key_message(other))),
            }
        });

        if let Err(error) = syn::parse::Parser::parse2(parser, args) {
            errors.push(error);
        }
        out
    }
}

/// "unknown argument `naem`… did you mean `name`?" — rule 3 of the style guide
/// says always give a fix, so the suggestion is a `help:` line and the full set
/// is a `note:`.
fn unknown_key_message(key: &str) -> String {
    let listed = KEYS
        .iter()
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>()
        .join(", ");
    match closest(key, KEYS) {
        Some(suggestion) => format!(
            "\
unknown `#[middleware]` argument `{key}`
  = help: did you mean `{suggestion}`?
  = note: the arguments are {listed}"
        ),
        None => format!(
            "\
unknown `#[middleware]` argument `{key}`
  = note: the arguments are {listed}
  = help: `#[middleware]` with no arguments is the common case"
        ),
    }
}

/// The nearest candidate by Levenshtein distance, if one is near enough to be
/// worth suggesting.
fn closest<'a>(input: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let limit = (input.chars().count() / 3).max(2);
    candidates
        .iter()
        .map(|candidate| (levenshtein(input, candidate), *candidate))
        .filter(|(distance, _)| *distance <= limit)
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| candidate)
}

/// Plain Levenshtein distance over `char`s, two rows at a time.
fn levenshtein(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current: Vec<usize> = vec![0; right.len() + 1];

    for (i, l) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, r) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(l != *r);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        core::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// Everything the emitter needs, resolved once.
struct Plan {
    /// Visibility of the generated types.
    vis: Visibility,
    /// `TenantLayer`.
    layer: Ident,
    /// `TenantService`.
    service: Ident,
    /// The name `moso middleware` prints.
    name: String,
    /// The user's function, called from `Service::call`.
    func: Ident,
    /// Leading parameters, extracted before the call.
    injected: Vec<Type>,
    /// The declared return type, for the assertion block.
    output: Option<Type>,
    /// Set when the signature did not check out: the emitter then produces a
    /// pass-through service rather than one that calls a function it cannot
    /// call correctly.
    placeholder: bool,
}

impl Plan {
    fn new(func: &ItemFn, options: &Options, errors: &mut Vec<syn::Error>) -> Self {
        let before = errors.len();

        let base = func.sig.ident.to_string();
        let base = base
            .strip_prefix("r#")
            .unwrap_or(&base)
            .to_upper_camel_case();
        let span = func.sig.ident.span();

        check_shape(func, errors);
        let injected = check_parameters(func, errors);
        let output = match &func.sig.output {
            ReturnType::Type(_, ty) => Some((**ty).clone()),
            ReturnType::Default => {
                errors.push(syn::Error::new(
                    func.sig.paren_token.span.join(),
                    NO_RETURN_TYPE,
                ));
                None
            }
        };

        Plan {
            vis: options.vis.clone().unwrap_or_else(|| widen(&func.vis)),
            layer: options
                .layer
                .clone()
                .unwrap_or_else(|| Ident::new(&format!("{base}Layer"), span)),
            service: options
                .service
                .clone()
                .unwrap_or_else(|| Ident::new(&format!("{base}Service"), span)),
            name: options
                .name
                .clone()
                .unwrap_or_else(|| func.sig.ident.to_string()),
            func: func.sig.ident.clone(),
            injected,
            output,
            placeholder: errors.len() != before,
        }
    }
}

/// A private function still has to be registrable from `main.rs`, so its layer
/// is widened to `pub(crate)`. Anything the author wrote explicitly is kept.
fn widen(vis: &Visibility) -> Visibility {
    match vis {
        Visibility::Inherited => syn::parse_quote!(pub(crate)),
        other => other.clone(),
    }
}

/// `async`, not generic, not a method, not variadic.
fn check_shape(func: &ItemFn, errors: &mut Vec<syn::Error>) {
    if func.sig.asyncness.is_none() {
        errors.push(syn::Error::new(func.sig.fn_token.span(), NOT_ASYNC));
    }
    if !func.sig.generics.params.is_empty() {
        errors.push(syn::Error::new_spanned(
            &func.sig.generics.params,
            IS_GENERIC,
        ));
    } else if let Some(clause) = &func.sig.generics.where_clause {
        errors.push(syn::Error::new_spanned(clause, IS_GENERIC));
    }
    if let Some(FnArg::Receiver(receiver)) = func.sig.inputs.first() {
        errors.push(syn::Error::new_spanned(receiver, HAS_RECEIVER));
    }
    if let Some(variadic) = &func.sig.variadic {
        errors.push(syn::Error::new_spanned(variadic, IS_VARIADIC));
    }
}

/// The parameter rules, and the extractor types the leading parameters imply.
///
/// `Depends<T>` wins over every positional complaint: a signature that has one
/// has exactly one mistake, and printing "the last parameter must be `next`"
/// underneath it would be the cascade rule 4 forbids.
fn check_parameters(func: &ItemFn, errors: &mut Vec<syn::Error>) -> Vec<Type> {
    let typed: Vec<&PatType> = func
        .sig
        .inputs
        .iter()
        .filter_map(|argument| match argument {
            FnArg::Typed(typed) => Some(typed),
            FnArg::Receiver(_) => None,
        })
        .collect();

    let mut rejected = false;
    for argument in &typed {
        if head_ident(&argument.ty).as_deref() == Some("Depends") {
            errors.push(syn::Error::new_spanned(
                &argument.ty,
                depends_message(&render_type(&argument.ty)),
            ));
            rejected = true;
        }
    }
    if rejected {
        return Vec::new();
    }

    if typed.len() < 2 {
        errors.push(syn::Error::new(
            func.sig.paren_token.span.join(),
            WRONG_ARITY,
        ));
        return Vec::new();
    }

    // At most one positional error: parameters in the wrong order is one
    // mistake, and two messages about it would be the cascade rule 4 forbids.
    let last = typed[typed.len() - 1];
    if head_ident(&last.ty).as_deref() != Some("Next") {
        errors.push(syn::Error::new_spanned(last, LAST_NOT_NEXT));
        return Vec::new();
    }
    let second_last = typed[typed.len() - 2];
    if head_ident(&second_last.ty).as_deref() != Some("Request") {
        errors.push(syn::Error::new_spanned(
            second_last,
            SECOND_LAST_NOT_REQUEST,
        ));
        return Vec::new();
    }

    let mut injected = Vec::with_capacity(typed.len() - 2);
    for argument in &typed[..typed.len() - 2] {
        match head_ident(&argument.ty) {
            Some(head) if BODY_EXTRACTORS.contains(&head.as_str()) => {
                errors.push(syn::Error::new_spanned(
                    &argument.ty,
                    body_extractor_message(&render_type(&argument.ty)),
                ));
            }
            _ => injected.push((*argument.ty).clone()),
        }
    }
    injected
}

/// The last path segment of a plain type path — `Inject` for
/// `moso::extract::Inject<Db>`. `None` for anything that is not a path.
fn head_ident(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(path) if path.qself.is_none() => {
            Some(path.path.segments.last()?.ident.to_string())
        }
        Type::Reference(reference) => head_ident(&reference.elem),
        Type::Group(group) => head_ident(&group.elem),
        Type::Paren(paren) => head_ident(&paren.elem),
        _ => None,
    }
}

/// A type as a human would write it, capped at 80 characters — rule 2 of the
/// style guide. `Depends < CurrentUser >` becomes `Depends<CurrentUser>`.
fn render_type(ty: &Type) -> String {
    /// Dropping a space because of the character before it.
    const TIGHT_AFTER: &[char] = &['<', '(', '[', '&', ':', '*', '!', '#'];
    /// Dropping a space because of the character after it.
    const TIGHT_BEFORE: &[char] = &['<', '>', ',', '(', ')', '[', ']', ';', ':'];

    let raw: Vec<char> = ty.to_token_stream().to_string().chars().collect();
    let mut out = String::with_capacity(raw.len());
    for (index, character) in raw.iter().enumerate() {
        if *character != ' ' {
            out.push(*character);
            continue;
        }
        let previous = out.chars().last().unwrap_or('\0');
        let next = raw.get(index + 1).copied().unwrap_or('\0');
        if TIGHT_AFTER.contains(&previous) || TIGHT_BEFORE.contains(&next) || next == '\0' {
            continue;
        }
        out.push(' ');
    }

    if out.chars().count() > 80 {
        out = out.chars().take(79).collect::<String>() + "…";
    }
    out
}

// ---------------------------------------------------------------------------
// Emission
// ---------------------------------------------------------------------------

/// The one path generated code resolves against.
fn private() -> TokenStream {
    quote!(::moso::__private)
}

impl Plan {
    fn emit(&self) -> TokenStream {
        let Plan {
            vis,
            layer,
            service,
            name,
            ..
        } = self;
        let p = private();

        let layer_doc = format!(
            " The Tower layer `#[moso::middleware]` generated for [`{func}`].\n\n\
             Register it with [`Router::layer`], [`MiddlewareStack::insert_after`] or any other \
             API that takes a `tower::Layer<moso::Route>`:\n\n\
             ```text\n\
             stack.insert_after(Slot::Trace, {layer}::NAME, {layer}::new());\n\
             ```\n\n\
             [`Router::layer`]: moso::Router::layer\n\
             [`MiddlewareStack::insert_after`]: moso::MiddlewareStack::insert_after",
            func = self.func,
            layer = layer,
        );
        let service_doc = format!(
            " The service [`{layer}`] wraps the rest of the stack in.\n\n\
             One instantiation is compiled per application, not per route: every Moso \
             registration point erases the inner service to `moso::Route` before applying the \
             layer."
        );
        let new_doc = format!(" Build a [`{layer}`].");
        let name_doc = format!(
            " The name `moso middleware` prints for this layer.\n\n\
             `\"{name}\"`, unless `#[middleware(name = \"…\")]` said otherwise."
        );
        let reqs_doc = " The providers this middleware injects.\n\n\
             Folded from the `PROVIDER_REQ` of every parameter before `req`, so a middleware that \
             takes `Inject<Db>` participates in boot validation exactly like a handler."
            .to_owned();
        let required_doc = " The providers this middleware injects, as a function.\n\n\
             The same slice as [`Self::PROVIDER_REQ`], spelled the way \
             `Endpoint::required_providers` is, so a boot check can treat both alike."
            .to_owned();

        let provider_reqs = self.provider_reqs();
        let call_body = self.call_body();
        let assertions = self.assertions();

        quote! {
            #[doc = #layer_doc]
            #[derive(
                ::core::clone::Clone,
                ::core::marker::Copy,
                ::core::fmt::Debug,
                ::core::default::Default,
            )]
            #vis struct #layer;

            impl #layer {
                #[doc = #name_doc]
                pub const NAME: &'static str = #name;

                #[doc = #reqs_doc]
                pub const PROVIDER_REQ: &'static [#p::ProviderReq] = #provider_reqs;

                #[doc = #new_doc]
                #[inline]
                pub const fn new() -> Self {
                    #layer
                }

                #[doc = #required_doc]
                #[inline]
                pub const fn required_providers() -> &'static [#p::ProviderReq] {
                    Self::PROVIDER_REQ
                }
            }

            #[automatically_derived]
            impl<__MosoInner> #p::tower::Layer<__MosoInner> for #layer {
                type Service = #service<__MosoInner>;

                #[inline]
                fn layer(&self, inner: __MosoInner) -> Self::Service {
                    #service { inner }
                }
            }

            #[doc = #service_doc]
            #vis struct #service<__MosoInner> {
                inner: __MosoInner,
            }

            #[automatically_derived]
            impl<__MosoInner> ::core::clone::Clone for #service<__MosoInner>
            where
                __MosoInner: ::core::clone::Clone,
            {
                #[inline]
                fn clone(&self) -> Self {
                    #service {
                        inner: ::core::clone::Clone::clone(&self.inner),
                    }
                }
            }

            #[automatically_derived]
            impl<__MosoInner> ::core::fmt::Debug for #service<__MosoInner> {
                fn fmt(
                    &self,
                    __moso_f: &mut ::core::fmt::Formatter<'_>,
                ) -> ::core::fmt::Result {
                    ::core::fmt::Formatter::write_str(__moso_f, ::core::stringify!(#service))
                }
            }

            #[automatically_derived]
            impl<__MosoInner> #p::tower::Service<#p::Request> for #service<__MosoInner>
            where
                __MosoInner: #p::tower::Service<
                        #p::Request,
                        Error = ::core::convert::Infallible,
                    >
                    + ::core::clone::Clone
                    + ::core::marker::Send
                    + ::core::marker::Sync
                    + 'static,
                <__MosoInner as #p::tower::Service<#p::Request>>::Response:
                    #p::IntoResponse + 'static,
                <__MosoInner as #p::tower::Service<#p::Request>>::Future:
                    ::core::marker::Send + 'static,
            {
                type Response = #p::Response;
                type Error = ::core::convert::Infallible;
                type Future = #p::BoxFuture<
                    'static,
                    ::core::result::Result<#p::Response, ::core::convert::Infallible>,
                >;

                #[inline]
                fn poll_ready(
                    &mut self,
                    __moso_cx: &mut ::core::task::Context<'_>,
                ) -> ::core::task::Poll<
                    ::core::result::Result<(), ::core::convert::Infallible>,
                > {
                    <__MosoInner as #p::tower::Service<#p::Request>>::poll_ready(
                        &mut self.inner,
                        __moso_cx,
                    )
                }

                fn call(&mut self, __moso_req: #p::Request) -> Self::Future {
                    #call_body
                }
            }

            #assertions
        }
    }

    /// `concat_reqs!` over the leading parameters' `PROVIDER_REQ`.
    fn provider_reqs(&self) -> TokenStream {
        let p = private();
        if self.placeholder || self.injected.is_empty() {
            return quote!(#p::concat_reqs!());
        }
        let each = self
            .injected
            .iter()
            .map(|ty| quote_spanned!(ty.span()=> <#ty as #p::Extract>::PROVIDER_REQ));
        quote!(#p::concat_reqs!(#(#each,)*))
    }

    /// The body of `Service::call`.
    fn call_body(&self) -> TokenStream {
        let p = private();

        // The instance that was polled ready is the one that must be called;
        // its clone takes its place for the next request. Getting this wrong is
        // the classic Tower bug, so it is spelled out rather than inferred.
        let take_inner = quote! {
            let __moso_ready = ::core::clone::Clone::clone(&self.inner);
            let __moso_inner = ::core::mem::replace(&mut self.inner, __moso_ready);
        };

        if self.placeholder {
            // The signature did not check out, so the function is not called:
            // the layer becomes a transparent pass-through and the one real
            // error stands alone.
            return quote! {
                let __moso_ready = ::core::clone::Clone::clone(&self.inner);
                let mut __moso_inner = ::core::mem::replace(&mut self.inner, __moso_ready);
                let __moso_future = <__MosoInner as #p::tower::Service<#p::Request>>::call(
                    &mut __moso_inner,
                    __moso_req,
                );
                ::std::boxed::Box::pin(async move {
                    match __moso_future.await {
                        ::core::result::Result::Ok(__moso_response) => {
                            ::core::result::Result::Ok(
                                #p::IntoResponse::into_response(__moso_response),
                            )
                        }
                        // `Infallible`, so the compiler proves this arm dead
                        // rather than us asserting it.
                        ::core::result::Result::Err(__moso_never) => match __moso_never {},
                    }
                })
            };
        }

        let func = &self.func;
        let mut arguments: Vec<TokenStream> = Vec::with_capacity(self.injected.len() + 2);
        let mut prologue = TokenStream::new();

        if !self.injected.is_empty() {
            let missing = missing_context_message(&self.name);
            prologue.extend(quote! {
                let (mut __moso_parts, __moso_body) = __moso_req.into_parts();
                // `Inject` reads the application's provider map through the
                // context, which `middleware_ctx` recovers from the request
                // extensions — an already-built one if an inner layer put it
                // there, otherwise a fresh one over the application state. A
                // middleware on a router that no `App` mounted has neither, and
                // says so rather than panicking.
                let __moso_ctx = match #p::middleware_ctx(&__moso_parts) {
                    ::core::result::Result::Ok(__moso_found) => __moso_found,
                    ::core::result::Result::Err(_) => {
                        return ::core::result::Result::Ok(
                            #p::IntoResponse::into_response(
                                #p::Error::internal_msg(#missing),
                            ),
                        );
                    }
                };
            });

            for (index, ty) in self.injected.iter().enumerate() {
                let binding = Ident::new(&format!("__moso_arg{index}"), ty.span());
                prologue.extend(quote_spanned! { ty.span()=>
                    let #binding = match <#ty as #p::Extract>::extract(
                        &mut __moso_parts,
                        &__moso_ctx,
                    ).await {
                        ::core::result::Result::Ok(__moso_value) => __moso_value,
                        ::core::result::Result::Err(__moso_error) => {
                            return ::core::result::Result::Ok(
                                #p::IntoResponse::into_response(__moso_error),
                            );
                        }
                    };
                });
                arguments.push(quote!(#binding));
            }

            prologue.extend(quote! {
                let __moso_req = #p::Request::from_parts(__moso_parts, __moso_body);
            });
        }

        arguments.push(quote!(__moso_req));
        arguments.push(quote!(__moso_next));

        quote! {
            #take_inner
            ::std::boxed::Box::pin(async move {
                #prologue
                let __moso_next = #p::Next::new(__moso_inner);
                ::core::result::Result::Ok(match #func(#(#arguments),*).await {
                    ::core::result::Result::Ok(__moso_response) => __moso_response,
                    ::core::result::Result::Err(__moso_error) => {
                        #p::IntoResponse::into_response(__moso_error)
                    }
                })
            })
        }
    }

    /// Assertion codegen — tool 3 in `docs/04-devex/41-diagnostics.md`.
    ///
    /// Moves the span of a bound failure onto the user's parameter or return
    /// type instead of onto generated tokens deep inside `Service::call`.
    fn assertions(&self) -> TokenStream {
        if self.placeholder {
            return TokenStream::new();
        }
        let p = private();

        let checks = self
            .injected
            .iter()
            .map(|ty| quote_spanned!(ty.span()=> __moso_assert_extract::<#ty>();));

        let output = match &self.output {
            Some(ty) => {
                let span = ty.span();
                quote_spanned! { span=>
                    fn __moso_assert_output(__moso_value: #ty) {
                        __moso_assert_result(__moso_value);
                    }
                }
            }
            None => TokenStream::new(),
        };

        quote! {
            #[doc(hidden)]
            // As in `endpoint.rs`: these exist to be type-checked, not called.
            #[allow(dead_code, non_snake_case)]
            const _: () = {
                fn __moso_assert_extract<__MosoT: #p::Extract>() {}

                fn __moso_assert_result<__MosoE: #p::IntoResponse>(
                    _: ::core::result::Result<#p::Response, __MosoE>,
                ) {
                }

                #output

                fn __moso_check() {
                    #(#checks)*
                }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand_str(args: &str, item: &str) -> String {
        let args: TokenStream = args.parse().expect("attribute arguments");
        let item: TokenStream = item.parse().expect("item");
        expand(args, item).to_string()
    }

    fn expand_ok(args: &str, item: &str) -> String {
        let rendered = expand_str(args, item);
        assert!(
            !rendered.contains("compile_error"),
            "unexpected error in expansion: {rendered}"
        );
        rendered
    }

    /// Whatever else it does, the output has to be a parseable Rust file —
    /// otherwise every downstream error is a syntax error at generated tokens.
    fn assert_parses(rendered: &str) {
        let tokens: TokenStream = rendered.parse().expect("re-lex");
        syn::parse2::<syn::File>(tokens).expect("expansion is a valid file");
    }

    const SIMPLE: &str = "async fn tenant(mut req: Request, next: Next) -> Result<Response> { \
                          Ok(next.run(req).await) }";

    #[test]
    fn it_names_the_layer_and_the_service_after_the_function() {
        let rendered = expand_ok("", SIMPLE);
        assert_parses(&rendered);
        assert!(rendered.contains("struct TenantLayer"));
        assert!(rendered.contains("struct TenantService"));
        assert!(rendered.contains("\"tenant\""));
    }

    #[test]
    fn snake_case_becomes_upper_camel_case() {
        let rendered = expand_ok(
            "",
            "async fn require_api_key(req: Request, next: Next) -> Result<Response> { todo!() }",
        );
        assert!(rendered.contains("struct RequireApiKeyLayer"));
        assert!(rendered.contains("struct RequireApiKeyService"));
    }

    #[test]
    fn the_users_function_survives_unchanged() {
        let rendered = expand_ok("", SIMPLE);
        assert!(rendered.contains("async fn tenant"));
    }

    #[test]
    fn a_private_function_gets_a_pub_crate_layer() {
        let rendered = expand_ok("", SIMPLE);
        assert!(rendered.contains("pub (crate) struct TenantLayer"));
    }

    #[test]
    fn a_pub_function_gets_a_pub_layer() {
        let rendered = expand_ok(
            "",
            "pub async fn tenant(req: Request, next: Next) -> Result<Response> { todo!() }",
        );
        assert!(rendered.contains("pub struct TenantLayer"));
        assert!(!rendered.contains("pub (crate) struct TenantLayer"));
    }

    #[test]
    fn vis_overrides_the_derived_visibility() {
        let rendered = expand_ok("vis = \"pub(super)\"", SIMPLE);
        assert!(rendered.contains("pub (super) struct TenantLayer"));
    }

    #[test]
    fn layer_and_service_names_can_be_overridden() {
        let rendered = expand_ok("layer = \"Tenancy\", service = \"TenancySvc\"", SIMPLE);
        assert!(rendered.contains("struct Tenancy ;"));
        assert!(rendered.contains("struct TenancySvc"));
    }

    #[test]
    fn name_overrides_the_printed_name() {
        let rendered = expand_ok("name = \"tenant-resolver\"", SIMPLE);
        assert!(rendered.contains("\"tenant-resolver\""));
    }

    #[test]
    fn without_injection_the_request_is_passed_straight_through() {
        let rendered = expand_ok("", SIMPLE);
        assert!(!rendered.contains("into_parts"));
        assert!(!rendered.contains("RequestCtx"));
        assert!(rendered.contains("concat_reqs ! ()"));
    }

    #[test]
    fn injected_parameters_are_extracted_and_declared() {
        let rendered = expand_ok(
            "",
            "async fn tenant(Inject(db): Inject<Db>, req: Request, next: Next) \
             -> Result<Response> { todo!() }",
        );
        assert_parses(&rendered);
        assert!(rendered.contains("Inject < Db > as :: moso :: __private :: Extract"));
        assert!(rendered.contains("PROVIDER_REQ"));
        assert!(rendered.contains("into_parts"));
        assert!(rendered.contains("from_parts"));
        // The one `__private` item this macro needs that `#[endpoint]` does
        // not: middleware runs before the router builds a context.
        assert!(rendered.contains(":: moso :: __private :: middleware_ctx"));
    }

    #[test]
    fn two_injected_parameters_are_bound_in_order() {
        let rendered = expand_ok(
            "",
            "async fn m(Inject(a): Inject<A>, Inject(b): Inject<B>, req: Request, next: Next) \
             -> Result<Response> { todo!() }",
        );
        let first = rendered.find("__moso_arg0").expect("first binding");
        let second = rendered.find("__moso_arg1").expect("second binding");
        assert!(first < second);
    }

    // ── diagnostics ───────────────────────────────────────────────────────

    fn error_of(args: &str, item: &str) -> String {
        let rendered = expand_str(args, item);
        assert!(rendered.contains("compile_error"), "expected an error");
        rendered
    }

    #[test]
    fn depends_is_the_documented_compile_error() {
        let rendered = error_of(
            "",
            "async fn tenant(Depends(user): Depends<CurrentUser>, req: Request, next: Next) \
             -> Result<Response> { todo!() }",
        );
        assert!(rendered.contains("`Depends<CurrentUser>` cannot be used in middleware"));
        assert!(rendered.contains(
            "middleware runs before extractors, so request dependencies are not yet available"
        ));
        assert!(rendered.contains("read a middleware-inserted value with `req.extensions()`"));
        assert!(rendered.contains("a `Dependency` impl and use it in the handler"));
    }

    #[test]
    fn depends_suppresses_the_positional_complaints() {
        // `Depends` last is one mistake, not two.
        let rendered = error_of(
            "",
            "async fn tenant(req: Request, next: Next, Depends(u): Depends<CurrentUser>) \
             -> Result<Response> { todo!() }",
        );
        assert!(rendered.contains("cannot be used in middleware"));
        assert!(!rendered.contains("the last parameter of a middleware"));
        assert_eq!(rendered.matches("compile_error").count(), 1);
    }

    #[test]
    fn a_non_async_function_is_rejected() {
        let rendered = error_of(
            "",
            "fn tenant(req: Request, next: Next) -> Result<Response> {}",
        );
        assert!(rendered.contains("middleware must be an `async fn`"));
    }

    #[test]
    fn a_generic_function_is_rejected() {
        let rendered = error_of(
            "",
            "async fn tenant<T>(req: Request, next: Next) -> Result<Response> { todo!() }",
        );
        assert!(rendered.contains("middleware may not be generic"));
    }

    #[test]
    fn a_missing_next_is_rejected() {
        let rendered = error_of(
            "",
            "async fn tenant(req: Request) -> Result<Response> { todo!() }",
        );
        assert!(rendered.contains("last two parameters"));
    }

    #[test]
    fn a_swapped_request_and_next_are_rejected() {
        let rendered = error_of(
            "",
            "async fn tenant(next: Next, req: Request) -> Result<Response> { todo!() }",
        );
        assert!(rendered.contains("the last parameter of a middleware must be `next: Next`"));
    }

    #[test]
    fn a_missing_return_type_is_rejected() {
        let rendered = error_of("", "async fn tenant(req: Request, next: Next) {}");
        assert!(rendered.contains("middleware must return `Result<Response>`"));
    }

    #[test]
    fn a_body_extractor_is_rejected_by_name() {
        let rendered = error_of(
            "",
            "async fn tenant(Json(body): Json<CreateUser>, req: Request, next: Next) \
             -> Result<Response> { todo!() }",
        );
        assert!(rendered.contains("`Json<CreateUser>` cannot be used in middleware"));
        assert!(rendered.contains("middleware runs before the body is read"));
    }

    #[test]
    fn an_unknown_argument_suggests_the_nearest_key() {
        let rendered = error_of("naem = \"tenant\"", SIMPLE);
        assert!(rendered.contains("unknown `#[middleware]` argument `naem`"));
        assert!(rendered.contains("did you mean `name`?"));
    }

    #[test]
    fn a_wild_argument_lists_the_keys_without_guessing() {
        let rendered = error_of("queue = \"mail\"", SIMPLE);
        assert!(rendered.contains("unknown `#[middleware]` argument `queue`"));
        assert!(!rendered.contains("did you mean"));
    }

    #[test]
    fn a_non_function_item_gets_one_error_and_keeps_its_tokens() {
        let rendered = error_of("", "struct Tenant;");
        assert!(rendered.contains("`#[middleware]` can only be applied to a function"));
        assert!(rendered.contains("struct Tenant"));
        assert_eq!(rendered.matches("compile_error").count(), 1);
    }

    #[test]
    fn a_broken_signature_still_defines_the_layer_types() {
        // Rule 4: one error, and a well-typed placeholder so the registration
        // site does not also fail with "cannot find type `TenantLayer`".
        let rendered = error_of(
            "",
            "fn tenant(req: Request, next: Next) -> Result<Response> {}",
        );
        assert_parses(&rendered);
        assert!(rendered.contains("struct TenantLayer"));
        assert!(rendered.contains("struct TenantService"));
        assert!(!rendered.contains("tenant (__moso_req"));
    }

    #[test]
    fn a_method_is_rejected() {
        let rendered = error_of(
            "",
            "async fn tenant(&self, req: Request, next: Next) -> Result<Response> { todo!() }",
        );
        assert!(rendered.contains("free function, not a method"));
    }

    // ── helpers ───────────────────────────────────────────────────────────

    #[test]
    fn types_render_the_way_a_human_writes_them() {
        let render = |source: &str| render_type(&syn::parse_str::<Type>(source).expect("type"));
        assert_eq!(render("Depends<CurrentUser>"), "Depends<CurrentUser>");
        assert_eq!(render("Inject<dyn Mailer>"), "Inject<dyn Mailer>");
        assert_eq!(
            render("moso::extract::Inject<Db>"),
            "moso::extract::Inject<Db>"
        );
        assert_eq!(
            render("HashMap<String, Vec<u8>>"),
            "HashMap<String, Vec<u8>>"
        );
        assert_eq!(render("&'a str"), "&'a str");
    }

    #[test]
    fn a_very_long_type_is_capped_at_eighty_characters() {
        let long = format!("Depends<{}>", "A".repeat(200));
        let rendered = render_type(&syn::parse_str::<Type>(&long).expect("type"));
        assert_eq!(rendered.chars().count(), 80);
        assert!(rendered.ends_with('…'));
    }

    #[test]
    fn levenshtein_is_the_usual_edit_distance() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("name", "name"), 0);
        assert_eq!(levenshtein("naem", "name"), 2);
        assert_eq!(levenshtein("vis", "visible"), 4);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn suggestions_are_only_offered_when_they_are_close() {
        assert_eq!(closest("naem", KEYS), Some("name"));
        assert_eq!(closest("nam", KEYS), Some("name"));
        assert_eq!(closest("servce", KEYS), Some("service"));
        assert_eq!(closest("queue", KEYS), None);
    }

    #[test]
    fn the_head_of_a_type_path_is_its_last_segment() {
        let head = |source: &str| head_ident(&syn::parse_str::<Type>(source).expect("type"));
        assert_eq!(head("Next").as_deref(), Some("Next"));
        assert_eq!(head("moso::Next").as_deref(), Some("Next"));
        assert_eq!(head("Inject<Db>").as_deref(), Some("Inject"));
        assert_eq!(head("(u8, u8)"), None);
    }

    #[test]
    fn the_canonical_signature_is_the_one_the_messages_quote() {
        // Guards against the messages and the doc drifting apart.
        assert!(NOT_ASYNC.contains(SIGNATURE));
        assert!(WRONG_ARITY.contains(SIGNATURE));
        assert!(LAST_NOT_NEXT.contains(SIGNATURE));
    }
}
