//! `#[endpoint]` — the one annotation.
//!
//! The macro leaves the user's `async fn` **exactly as written** and emits a
//! companion unit struct beside it (an expansion sketch, not a program):
//!
//! ```text
//! #[doc(hidden)] #[derive(Clone, Copy, Default)] pub struct __moso_op_create;
//! impl ::moso::__private::Endpoint  for __moso_op_create { /* the description */ }
//! impl ::moso::__private::HandlerFn for __moso_op_create { /* the extraction glue */ }
//! const _: () = { /* the assertions */ };
//! ```
//!
//! Rust cannot attach an associated type to a `fn` item, so the metadata has to
//! live on a type. `routes!` and `ep!` rewrite a handler's name to that type's
//! name, which is the whole reason the three macros exist together.
//!
//! # The three generated items, and why each one is shaped as it is
//!
//! - **`Endpoint::spec`** runs once per route at `App::build()`. It writes the
//!   summary, description, `operationId` and source location, then delegates to
//!   each parameter type's `describe`. Nothing is invented: every OpenAPI member
//!   comes from a type or from the doc comment.
//! - **`HandlerFn::invoke`** is **one concrete, non-generic async block**. It is
//!   monomorphised once per handler however many times the handler is
//!   registered, which is rule A2 of the compile-time architecture: erase early.
//! - **The `const _: () = { … }` assertion block** is what
//!   `#[axum::debug_handler]` does opt-in. Moso does it unconditionally, because
//!   "the error is bad unless you remembered to add an attribute" is not a
//!   developer experience. Each assertion's turbofish carries the span of the
//!   user's parameter type, so a missing `Extract` impl underlines the
//!   parameter rather than a line inside a `tower` blanket impl.
//!
//! # Which parameter consumes the body
//!
//! The glue has to decide, at expansion time, whether the last parameter is
//! extracted with [`Extract`](::moso::__private::Extract) or with
//! [`ExtractBody`](::moso::__private::ExtractBody). A proc macro sees tokens,
//! not types, so this is decided by [`is_body_extractor`] — a **name-based
//! heuristic** over the outermost path segment of the parameter's type.
//!
//! The heuristic decides *which trait is named*; the trait bound is what
//! actually enforces the rule. A type the heuristic does not recognise is
//! treated as a parts extractor, and if it is really a body extractor the
//! failure is `ExtractBody is implemented but Extract is not`, whose
//! `#[diagnostic::on_unimplemented]` note says so. A type the heuristic
//! recognises wrongly fails the other way round. Both messages are hand-written
//! in `moso-core`; neither is trait-resolution vomit.

use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
use quote::{ToTokens, format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Attribute, Error, Expr, FnArg, Ident, ItemFn, LitInt, LitStr, ReturnType, Type};

use crate::routes::op_ident;

/// The largest number of parameters a handler may have.
///
/// Mirrors `moso_core::handler::MAX_HANDLER_PARAMS`. The two cannot be shared:
/// a proc-macro crate must not depend on a runtime Moso crate. They are checked
/// against each other by a unit test in `moso-core`, and the message names the
/// number so a mismatch is visible rather than silent.
const MAX_HANDLER_PARAMS: usize = 16;

/// Every key `#[endpoint(…)]` accepts, in the order the help line lists them.
const KNOWN_ARGS: &[&str] = &[
    "operation_id",
    "tag",
    "hidden",
    "deprecated",
    "response",
    "example",
    "errors",
];

/// The `help:` line that follows any argument mistake.
const ARGS_HELP: &str = "help: valid arguments are `operation_id = \"…\"`, `tag = \"…\"`, `hidden`, \
                         `deprecated` and `errors = Type`\n\
                         help: and the two compound ones, `response(409, \"…\")` and \
                         `example(request = \"…\", response = \"…\")`";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Expand `#[endpoint]` over one `async fn`.
///
/// Never panics and never returns nothing: on any error the user's function is
/// re-emitted unchanged, followed by one `compile_error!` per distinct mistake
/// and — where it is legal to do so — a well-typed placeholder, so that a
/// `routes!` table naming this handler does not produce a second, derived
/// error. Two misplaced parameters are one mistake and one message; a misplaced
/// parameter and a misspelt attribute key are two.
pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let func: ItemFn = match syn::parse2(item.clone()) {
        Ok(func) => func,
        Err(error) => {
            let error = Error::new(
                error.span(),
                "`#[endpoint]` may only be applied to an `async fn`\n\n\
                 help: move the attribute onto the handler itself:\n    \
                 #[endpoint]\n    async fn list() -> Result<NoContent> { /* … */ }",
            );
            return join(item, error.to_compile_error());
        }
    };

    let mut errors: Vec<Error> = Vec::new();
    let args = parse_args(attr, &mut errors);

    // A `self` receiver is the one mistake whose placeholder would itself be a
    // syntax error: the generated struct cannot live inside an `impl` block.
    if let Some(receiver) = func.sig.inputs.iter().find_map(|input| match input {
        FnArg::Receiver(receiver) => Some(receiver),
        FnArg::Typed(_) => None,
    }) {
        let error = Error::new_spanned(
            receiver,
            "handlers must be free functions, not methods\n\n\
             note: a handler is registered by name, and a method has no name a router can reach\n\
             help: move the function out of the `impl` block and take what it needs as \
             parameters: `Inject<T>` for a provider, `Depends<T>` for a request-scoped value",
        );
        return join(func.to_token_stream(), error.to_compile_error());
    }

    check_signature(&func, &mut errors);

    let params = classify_parameters(&func, &mut errors);
    let return_type = return_type(&func, &mut errors);

    if !errors.is_empty() {
        let reported = combine(errors).to_compile_error();
        let placeholder = placeholder(&func);
        return join(join(func.to_token_stream(), reported), placeholder);
    }

    let Some(return_type) = return_type else {
        // `check_signature`/`return_type` always push an error when they return
        // `None`, so this arm is unreachable; belt and braces beat a panic.
        return func.to_token_stream();
    };

    generate(&func, &args, &params, &return_type)
}

// ---------------------------------------------------------------------------
// Signature checks
// ---------------------------------------------------------------------------

/// `async`, non-generic, no `impl Trait` in argument position.
fn check_signature(func: &ItemFn, errors: &mut Vec<Error>) {
    let name = &func.sig.ident;

    if func.sig.asyncness.is_none() {
        errors.push(Error::new(
            func.sig.fn_token.span(),
            format!(
                "handlers must be `async fn`\n\n\
                 note: the extraction glue awaits every parameter, so the handler itself must \
                 be awaitable\n\
                 help: write `async fn {name}(…)`"
            ),
        ));
    }

    let generics = &func.sig.generics;
    if !generics.params.is_empty() {
        errors.push(Error::new_spanned(
            &generics.params,
            "handlers may not be generic; use a concrete type or a trait object\n\n\
             note: a route stores one erased handler, so there is no call site at which a type \
             parameter could be chosen\n\
             help: name the concrete type, or take the dependency behind a trait object: \
             `Inject<Arc<dyn Mailer>>`",
        ));
    } else if let Some(where_clause) = &generics.where_clause {
        errors.push(Error::new_spanned(
            where_clause,
            "handlers may not be generic; use a concrete type or a trait object\n\n\
             help: delete the `where` clause — with no type parameters it constrains nothing",
        ));
    }

    // `impl Trait` in argument position makes the function generic without
    // showing up in `sig.generics`, and cannot be named in the assertions.
    for input in &func.sig.inputs {
        let FnArg::Typed(typed) = input else { continue };
        if matches!(&*typed.ty, Type::ImplTrait(_)) {
            errors.push(Error::new_spanned(
                &typed.ty,
                "handlers may not be generic; use a concrete type or a trait object\n\n\
                 note: `impl Trait` in a parameter is a type parameter in disguise, and the \
                 generated `Endpoint` has to name every parameter type\n\
                 help: name the extractor: `Inject<Db>`, `Json<CreateUser>`, `Depends<Actor>`",
            ));
        }
    }
}

/// The return type, or `()` when the handler declares none.
///
/// `impl Trait` is rejected here rather than in [`check_signature`] because the
/// help line is different: a return position wants a response type, not a
/// trait object.
fn return_type(func: &ItemFn, errors: &mut Vec<Error>) -> Option<Type> {
    match &func.sig.output {
        ReturnType::Default => Some(syn::parse_quote!(())),
        ReturnType::Type(_, ty) => {
            if matches!(&**ty, Type::ImplTrait(_)) {
                errors.push(Error::new_spanned(
                    ty,
                    "a handler's return type must be a named type\n\n\
                     note: `#[endpoint]` documents the response from the return type, and \
                     `impl Trait` has no name to document\n\
                     help: name it: `Result<Created<UserOut>>`, `Result<Page<UserOut>>`, \
                     `Result<NoContent>`",
                ));
                return None;
            }
            Some((**ty).clone())
        }
    }
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// One handler parameter, reduced to what codegen needs.
#[derive(Clone)]
struct Param {
    /// The declared type, verbatim, spans intact.
    ty: Type,
    /// Whether the glue should call `ExtractBody::extract_body` for it.
    body: bool,
}

/// Split the parameter list, decide which one consumes the body, and report
/// every ordering mistake.
///
/// Reports at most one ordering error, per the "one error, not a cascade" rule:
/// a handler with three misplaced body extractors is one mistake, not three.
fn classify_parameters(func: &ItemFn, errors: &mut Vec<Error>) -> Vec<Param> {
    let inputs: Vec<&syn::PatType> = func
        .sig
        .inputs
        .iter()
        .filter_map(|input| match input {
            FnArg::Typed(typed) => Some(typed),
            FnArg::Receiver(_) => None,
        })
        .collect();

    if inputs.len() > MAX_HANDLER_PARAMS {
        let offending = inputs[MAX_HANDLER_PARAMS];
        errors.push(Error::new_spanned(
            offending,
            format!(
                "handlers support at most {MAX_HANDLER_PARAMS} parameters; group them into a \
                 `Depends` struct\n\n\
                 note: this handler declares {count}\n\
                 help: group related parameters into a struct deriving `Dependency` and take it \
                 as one parameter:\n    \
                 #[derive(moso::Dependency)]\n    \
                 struct ListDeps {{ /* the grouped parameters */ }}",
                count = inputs.len()
            ),
        ));
    }

    let last = inputs.len().saturating_sub(1);
    let mut bodies: Vec<usize> = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        if is_body_extractor(&input.ty) {
            bodies.push(index);
        }
    }

    // One error, not a cascade. Two body extractors is the more specific
    // diagnosis, so it wins over "not last" — which would otherwise always fire
    // first, since with two bodies the first one cannot be last.
    if bodies.len() > 1 {
        let second = &inputs[bodies[1]];
        errors.push(Error::new_spanned(
            second,
            format!(
                "only one body extractor is allowed per handler\n\n\
                 note: `{first_type}` already consumes the request body, and a body can only be \
                 read once\n\
                 help: model the payload as one type and take it once, e.g. `Json<CreatePost>` \
                 deriving `moso::Schema`",
                first_type = short_type(&inputs[bodies[0]].ty)
            ),
        ));
    } else if let Some(&first) = bodies.first()
        && first != last
    {
        let body_type = short_type(&inputs[first].ty);
        let follower = &inputs[first + 1];
        errors.push(Error::new_spanned(
            follower,
            format!(
                "request body extractor must be the last parameter\n\n\
                 note: `{body_type}` consumes the request body, so no parameter may follow it\n\
                 note: only one body extractor is allowed per handler\n\
                 help: move `{body_type}` to the end of the parameter list"
            ),
        ));
    }

    inputs
        .iter()
        .enumerate()
        .map(|(index, input)| Param {
            ty: (*input.ty).clone(),
            body: index == last && bodies.first() == Some(&last),
        })
        .collect()
}

/// Type names that mean "this parameter consumes the request body".
///
/// Matched against the **outermost** path segment, so `Path<String>` is a path
/// parameter and only a bare `String` is a body. Documented as a heuristic on
/// the module header: it chooses which trait the glue names, and the trait
/// bound is the real enforcement.
const BODY_EXTRACTOR_NAMES: &[&str] = &[
    "Bytes",
    "Form",
    "Json",
    "Multipart",
    "Raw",
    "RawBody",
    "Stream",
    "String",
    "Text",
    "Upload",
    "Xml",
];

/// Whether `ty` names a request-body extractor.
///
/// Recognises the built-in extractors by name, plus two shape rules that catch
/// third-party ones: a name ending in `Body` (`RawBody`, `OpaqueBody`,
/// `ProtobufBody`) and a name beginning with `Body` (`BodyStream`). References,
/// parentheses and grouping are seen through; anything else is not a body.
fn is_body_extractor(ty: &Type) -> bool {
    let Some(name) = outer_type_name(ty) else {
        return false;
    };
    BODY_EXTRACTOR_NAMES.contains(&name.as_str())
        || (name.ends_with("Body") && name.len() > 4)
        || (name.starts_with("Body") && name.len() > 4)
        || name.ends_with("Multipart")
        || name.ends_with("Upload")
}

/// The last path segment of `ty`, seeing through `&`, `(…)` and grouping.
fn outer_type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(path) => path.path.segments.last().map(|s| s.ident.to_string()),
        Type::Reference(reference) => outer_type_name(&reference.elem),
        Type::Paren(paren) => outer_type_name(&paren.elem),
        Type::Group(group) => outer_type_name(&group.elem),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Code generation
// ---------------------------------------------------------------------------

/// Emit the function, the companion struct, the two impls and the assertions.
fn generate(func: &ItemFn, args: &EndpointArgs, params: &[Param], ret: &Type) -> TokenStream {
    let name = &func.sig.ident;
    let name_str = name.to_string();
    let op = op_ident(name);
    let cfgs = cfg_attributes(&func.attrs);
    let struct_definition = op_struct(&op, &name_str, &cfgs);

    let spec_body = spec_body(func, args, params, ret);
    let provider_reqs = provider_reqs(params);
    let invoke_body = invoke_body(name, params, ret);
    let assertions = assertions(params, ret, args, &cfgs);

    quote! {
        #func

        #struct_definition

        #(#cfgs)*
        impl ::moso::__private::Endpoint for #op {
            const NAME: &'static str = #name_str;

            fn spec(__moso_b: &mut ::moso::__private::OperationBuilder) {
                #spec_body
            }

            fn required_providers() -> &'static [::moso::__private::ProviderReq] {
                #provider_reqs
            }
        }

        #(#cfgs)*
        impl ::moso::__private::HandlerFn for #op {
            // A zero-parameter handler leaves `__moso_req` and `__moso_ctx`
            // untouched, and a handler the user marked `#[deprecated]` must not
            // warn at the one call site the macro generates for it.
            #[allow(unused_mut, unused_variables, deprecated)]
            fn invoke(
                __moso_req: ::moso::__private::Request,
                __moso_ctx: ::moso::__private::RequestCtx,
            ) -> ::moso::__private::BoxFuture<'static, ::moso::__private::Response> {
                #invoke_body
            }
        }

        #assertions
    }
}

/// The companion unit struct.
///
/// It carries a real doc comment as well as `#[doc(hidden)]`: an application
/// with `#![deny(missing_docs)]` would otherwise fail on macro output it never
/// wrote. The `allow`s cover the same class of problem for `unreachable_pub`
/// and `dead_code`, which fire on `pub` items inside a private `routes` module.
fn op_struct(op: &Ident, name: &str, cfgs: &[&Attribute]) -> TokenStream {
    let doc = format!("The [`Endpoint`] generated for `{name}` by `#[endpoint]`.");
    quote! {
        #[doc = #doc]
        #[doc(hidden)]
        #(#cfgs)*
        // `__moso_op_create` is a type with a function's name by construction, and
        // it is `pub` inside a private module whenever the handler is private, so
        // `unreachable_pub` and `dead_code` both fire on correct output.
        #[allow(non_camel_case_types, non_snake_case, unreachable_pub, dead_code)]
        #[derive(Clone, Copy, Default)]
        pub struct #op;
    }
}

/// The `#[cfg(…)]` attributes on the handler, to be copied onto everything the
/// macro generates.
///
/// Without this, `#[endpoint] #[cfg(feature = "admin")] async fn purge(…)`
/// compiles the companion type into a build where the function does not exist,
/// and the failure is a "cannot find function" pointing at generated tokens.
///
/// `cfg_attr` is deliberately **not** copied: it can expand to any attribute at
/// all, and duplicating one onto an item it was never written for would be a
/// silent behaviour change rather than a conditional compile.
fn cfg_attributes(attrs: &[Attribute]) -> Vec<&Attribute> {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg"))
        .collect()
}

/// The body of `Endpoint::spec`.
fn spec_body(func: &ItemFn, args: &EndpointArgs, params: &[Param], ret: &Type) -> TokenStream {
    let name_str = func.sig.ident.to_string();
    let (summary, description) = doc_summary_and_description(&func.attrs);

    let summary = summary.map(|text| quote!(__moso_b.summary(#text);));
    let description = description.map(|text| quote!(__moso_b.description(#text);));

    let operation_id = match &args.operation_id {
        Some(literal) => quote!(__moso_b.operation_id(#literal);),
        // Module path + fn name, e.g. `blog::routes::users` + `create` =
        // `users_create`. At the crate root there is no module segment to
        // prefix with, and inventing one would produce `blog_create`.
        None => quote! {
            __moso_b.operation_id(match ::core::module_path!().rsplit_once("::") {
                ::core::option::Option::Some((_, __moso_module)) => {
                    ::std::format!("{}_{}", __moso_module, #name_str)
                }
                ::core::option::Option::None => ::std::string::String::from(#name_str),
            });
        },
    };

    // `line!()` is expanded by rustc at the span it is given, so pinning it to
    // the handler's own identifier makes `moso routes` point at the `async fn`
    // rather than at the attribute above it.
    let source_span = func.sig.ident.span();
    let source = quote_spanned! {source_span=>
        __moso_b.source(::core::file!(), ::core::line!());
    };

    let tags = args.tags.iter().map(|tag| quote!(__moso_b.tag(#tag);));

    let hidden = args.hidden.then(|| quote!(__moso_b.hidden();));
    let deprecated = (args.deprecated || has_deprecated_attribute(&func.attrs))
        .then(|| quote!(__moso_b.deprecated();));

    // Emitted *before* the describers so that an explicit attribute wins: the
    // builder's merge rule is first-writer-wins, and an escape hatch that lost
    // to the thing it is escaping would be useless.
    let extra_responses = args.responses.iter().map(|(status, description)| {
        let spec = if status.value >= 400 {
            quote!(::moso::__private::ResponseSpec::problem(#description))
        } else {
            quote!(::moso::__private::ResponseSpec::empty(#description))
        };
        let code = status.value;
        quote_spanned!(status.span=> __moso_b.response(#code, #spec);)
    });

    let describers = params.iter().map(|param| {
        let ty = &param.ty;
        let trait_path = if param.body {
            quote!(::moso::__private::ExtractBody)
        } else {
            quote!(::moso::__private::Extract)
        };
        quote_spanned!(ty.span()=> <#ty as #trait_path>::describe(__moso_b);)
    });

    // `HandlerReturn`, not `Describe`: the return type also has to reach
    // `IntoResponse` in `invoke`, and asking for the two separately makes a type
    // that has neither fail twice. One combined bound is one message.
    let response = quote_spanned! {ret.span()=>
        <#ret as ::moso::__private::HandlerReturn>::describe_response(__moso_b);
    };

    let errors = args.errors.iter().map(
        |ty| quote_spanned!(ty.span()=> <#ty as ::moso::__private::Describe>::describe(__moso_b);),
    );

    let examples = examples(args);

    quote! {
        #summary
        #description
        #operation_id
        #source
        #(#tags)*
        #hidden
        #deprecated
        #(#extra_responses)*
        #(#describers)*
        #response
        #(#errors)*
        #examples
    }
}

/// The `example(request = …, response = …)` escape hatch.
///
/// Written last, over what the describers produced: an example belongs to a
/// media type, and until the extractors have run there is no media type to
/// attach it to. Existing examples are never overwritten — a type that
/// documents its own example is more specific than an attribute.
fn examples(args: &EndpointArgs) -> Option<TokenStream> {
    if args.request_example.is_none() && args.response_example.is_none() {
        return None;
    }

    // A string that parses as JSON becomes that JSON; anything else becomes a
    // JSON string. Both are useful and neither can fail, so an example never
    // turns a documentation nicety into a boot error.
    let helper = quote! {
        fn __moso_example(__moso_text: &str) -> ::moso::__private::serde_json::Value {
            match ::moso::__private::serde_json::from_str(__moso_text) {
                ::core::result::Result::Ok(__moso_value) => __moso_value,
                ::core::result::Result::Err(_) => ::moso::__private::serde_json::Value::String(
                    ::std::string::String::from(__moso_text),
                ),
            }
        }
    };

    let request = args.request_example.as_ref().map(|expr| {
        quote_spanned! {expr.span()=>
            {
                let __moso_value = __moso_example(#expr);
                if let ::core::option::Option::Some(__moso_body) =
                    __moso_b.spec_mut().request_body.as_mut()
                {
                    for __moso_media in __moso_body.content.values_mut() {
                        if __moso_media.example.is_none() {
                            __moso_media.example =
                                ::core::option::Option::Some(__moso_value.clone());
                        }
                    }
                }
            }
        }
    });

    let response = args.response_example.as_ref().map(|expr| {
        quote_spanned! {expr.span()=>
            {
                let __moso_value = __moso_example(#expr);
                if let ::core::option::Option::Some((_, __moso_response)) = __moso_b
                    .spec_mut()
                    .responses
                    .iter_mut()
                    .find(|(__moso_key, _)| __moso_key.starts_with('2'))
                {
                    for __moso_media in __moso_response.content.values_mut() {
                        if __moso_media.example.is_none() {
                            __moso_media.example =
                                ::core::option::Option::Some(__moso_value.clone());
                        }
                    }
                }
            }
        }
    });

    Some(quote! { #helper #request #response })
}

/// `Endpoint::required_providers`, as a `const` slice concatenation.
fn provider_reqs(params: &[Param]) -> TokenStream {
    let reqs = params.iter().map(|param| {
        let ty = &param.ty;
        let trait_path = if param.body {
            quote!(::moso::__private::ExtractBody)
        } else {
            quote!(::moso::__private::Extract)
        };
        quote_spanned!(ty.span()=> <#ty as #trait_path>::PROVIDER_REQ)
    });
    quote!(::moso::__private::concat_reqs!(#(#reqs,)*))
}

/// The body of `HandlerFn::invoke`: one concrete async block.
fn invoke_body(name: &Ident, params: &[Param], ret: &Type) -> TokenStream {
    let bindings: Vec<Ident> = (0..params.len())
        .map(|index| format_ident!("__moso_a{}", index))
        .collect();

    let steps = params.iter().zip(&bindings).map(|(param, binding)| {
        let ty = &param.ty;
        if param.body {
            quote_spanned! {ty.span()=>
                let #binding = match <#ty as ::moso::__private::ExtractBody>::extract_body(
                    ::moso::__private::Request::from_parts(__moso_parts, __moso_body),
                    &__moso_ctx,
                )
                .await
                {
                    ::core::result::Result::Ok(__moso_value) => __moso_value,
                    ::core::result::Result::Err(__moso_error) => {
                        return ::moso::__private::IntoResponse::into_response(__moso_error);
                    }
                };
            }
        } else {
            quote_spanned! {ty.span()=>
                let #binding = match <#ty as ::moso::__private::Extract>::extract(
                    &mut __moso_parts,
                    &__moso_ctx,
                )
                .await
                {
                    ::core::result::Result::Ok(__moso_value) => __moso_value,
                    ::core::result::Result::Err(__moso_error) => {
                        return ::moso::__private::IntoResponse::into_response(__moso_error);
                    }
                };
            }
        }
    });

    // Spanned on the return type and routed through `HandlerReturn`, so that a
    // return type which is neither a response nor documentable produces exactly
    // the same diagnostic as `Endpoint::spec` does — rustc then prints it once.
    let call = quote_spanned! {ret.span()=>
        <_ as ::moso::__private::HandlerReturn>::into_handler_response(
            #name(#(#bindings),*).await,
        )
    };

    // `HandlerFuture::box_handler_future`, not `Box::pin`: the two compile to
    // the same thing, but a future that is not `Send` fails a named bound with
    // a hand-written headline instead of producing a cast error between two
    // 80-character `Pin<Box<…>>` types. Spanned on the handler's name so the
    // caret lands on `async fn list`, not on the `#[endpoint]` above it.
    let name_span = name.span();
    quote_spanned! {name_span=>
        ::moso::__private::HandlerFuture::box_handler_future(async move {
            let (mut __moso_parts, __moso_body) = __moso_req.into_parts();
            #(#steps)*
            #call
        })
    }
}

/// The always-on assertion block.
///
/// Every check carries the span of the user's own type. That single detail is
/// the difference between "the trait bound `Inject<Db>: Extract` is not
/// satisfied, underlining your parameter" and forty lines of `tower` internals.
///
/// # Why these are path expressions and not `fn assert<T: Extract>()` calls
///
/// The obvious spelling is a helper with the bound on it:
///
/// ```text
/// fn __moso_assert_extract<T: Extract>() {}
/// __moso_assert_extract::<Tenant>();
/// ```
///
/// It works, and it produces the error **twice**: once here and once from
/// `Endpoint::spec`, which names the same trait at the same span but through
/// `<Tenant as Extract>::describe`. rustc suppresses a diagnostic it has already
/// rendered verbatim, and those two do not render alike — the helper's adds
/// `note: required by a bound in __moso_assert_extract`, pointing at the
/// `#[endpoint]` attribute, which is a framework span the style guide forbids.
///
/// Referring to the associated item directly produces the identical rendering,
/// so the reader gets one error. The assertion still earns its keep: it is what
/// guarantees the check happens even for a parameter that codegen might one day
/// stop naming, and `let _: fn(…)` type-checks the *signature* too, so an
/// extractor whose `describe` has drifted is caught here rather than inside a
/// generated body.
fn assertions(
    params: &[Param],
    ret: &Type,
    args: &EndpointArgs,
    cfgs: &[&Attribute],
) -> TokenStream {
    let checks = params.iter().map(|param| {
        let ty = &param.ty;
        let trait_path = if param.body {
            quote!(::moso::__private::ExtractBody)
        } else {
            quote!(::moso::__private::Extract)
        };
        quote_spanned! {ty.span()=>
            let _: fn(&mut ::moso::__private::OperationBuilder) =
                <#ty as #trait_path>::describe;
        }
    });

    let response = quote_spanned! {ret.span()=>
        let _: fn(&mut ::moso::__private::OperationBuilder) =
            <#ret as ::moso::__private::HandlerReturn>::describe_response;
    };

    let errors = args.errors.iter().map(|ty| {
        quote_spanned! {ty.span()=>
            let _: fn(&mut ::moso::__private::OperationBuilder) =
                <#ty as ::moso::__private::Describe>::describe;
        }
    });

    quote! {
        #(#cfgs)*
        // The assertion block exists to be type-checked, never called, and its
        // helpers are named after the user's parameters so a failure underlines
        // the right token.
        #[allow(dead_code, non_snake_case)]
        const _: () = {
            fn __moso_check() {
                #(#checks)*
                #response
                #(#errors)*
            }
        };
    }
}

/// A companion type that compiles but describes nothing.
///
/// Emitted alongside the one error a broken handler produces, so that
/// `routes! { GET "/users" => list }` still resolves `__moso_op_list` and the
/// user reads one message instead of three.
fn placeholder(func: &ItemFn) -> TokenStream {
    let name_str = func.sig.ident.to_string();
    let op = op_ident(&func.sig.ident);
    let cfgs = cfg_attributes(&func.attrs);
    let struct_definition = op_struct(&op, &name_str, &cfgs);

    quote! {
        #struct_definition

        #(#cfgs)*
        impl ::moso::__private::Endpoint for #op {
            const NAME: &'static str = #name_str;

            fn spec(__moso_b: &mut ::moso::__private::OperationBuilder) {
                let _ = __moso_b;
            }

            fn required_providers() -> &'static [::moso::__private::ProviderReq] {
                &[]
            }
        }

        #(#cfgs)*
        impl ::moso::__private::HandlerFn for #op {
            fn invoke(
                __moso_req: ::moso::__private::Request,
                __moso_ctx: ::moso::__private::RequestCtx,
            ) -> ::moso::__private::BoxFuture<'static, ::moso::__private::Response> {
                let _ = (__moso_req, __moso_ctx);
                ::std::boxed::Box::pin(async move {
                    ::moso::__private::IntoResponse::into_response(::moso::__private::NoContent)
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Doc comments
// ---------------------------------------------------------------------------

/// Split a doc comment into an OpenAPI `summary` and `description`.
///
/// The first line is the summary; everything after it, with the blank line
/// between them removed, is the Markdown description. Both are optional.
fn doc_summary_and_description(attrs: &[Attribute]) -> (Option<String>, Option<String>) {
    let mut lines: Vec<String> = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let syn::Meta::NameValue(name_value) = &attr.meta else {
            continue;
        };
        let Expr::Lit(literal) = &name_value.value else {
            continue;
        };
        let syn::Lit::Str(text) = &literal.lit else {
            continue;
        };
        // `/// text` yields `" text"`; strip the one space rustdoc adds, not
        // the indentation of a fenced code block.
        for line in text.value().split('\n') {
            lines.push(line.strip_prefix(' ').unwrap_or(line).to_owned());
        }
    }

    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }

    if lines.is_empty() {
        return (None, None);
    }

    let summary = lines.remove(0).trim_end().to_owned();

    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }

    let description = if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    };

    (
        (!summary.is_empty()).then_some(summary),
        description.filter(|text| !text.trim().is_empty()),
    )
}

/// Whether the handler carries `#[deprecated]`.
fn has_deprecated_attribute(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident("deprecated"))
}

// ---------------------------------------------------------------------------
// Attribute arguments
// ---------------------------------------------------------------------------

/// An HTTP status code parsed at expansion time, with its literal's span.
struct Status {
    /// The code itself, already known to be in `100..=599`.
    value: u16,
    /// The literal's span, so the generated call underlines the user's number.
    span: Span,
}

/// Everything `#[endpoint(…)]` can say.
#[derive(Default)]
struct EndpointArgs {
    operation_id: Option<LitStr>,
    tags: Vec<LitStr>,
    hidden: bool,
    deprecated: bool,
    responses: Vec<(Status, LitStr)>,
    request_example: Option<Expr>,
    response_example: Option<Expr>,
    errors: Vec<Type>,
}

/// Parse `#[endpoint(…)]`, accumulating one error per mistaken argument.
fn parse_args(attr: TokenStream, errors: &mut Vec<Error>) -> EndpointArgs {
    let mut args = EndpointArgs::default();

    for chunk in split_top_level(attr) {
        let span = chunk_span(&chunk);
        let Some((key, rest)) = split_key(chunk.clone()) else {
            errors.push(Error::new(
                span,
                format!("expected a `#[endpoint]` argument name\n\n{ARGS_HELP}"),
            ));
            continue;
        };

        let name = key.to_string();
        match name.as_str() {
            "operation_id" => match value_of::<LitStr>(&name, rest, key.span()) {
                Ok(literal) => args.operation_id = Some(literal),
                Err(error) => errors.push(error),
            },
            "tag" => match value_of::<LitStr>(&name, rest, key.span()) {
                Ok(literal) => args.tags.push(literal),
                Err(error) => errors.push(error),
            },
            "errors" => match value_of::<Type>(&name, rest, key.span()) {
                Ok(ty) => args.errors.push(ty),
                Err(error) => errors.push(error),
            },
            "hidden" | "deprecated" => match rest {
                Rest::Word => {
                    if name == "hidden" {
                        args.hidden = true;
                    } else {
                        args.deprecated = true;
                    }
                }
                _ => errors.push(Error::new(
                    key.span(),
                    format!(
                        "the `{name}` argument takes no value\n\n\
                         help: write `#[endpoint({name})]`"
                    ),
                )),
            },
            "response" => match parse_response(rest, key.span()) {
                Ok(response) => args.responses.push(response),
                Err(error) => errors.push(error),
            },
            "example" => {
                if let Err(error) = parse_example(rest, key.span(), &mut args) {
                    errors.push(error);
                }
            }
            _ => errors.push(unknown_argument(&key)),
        }
    }

    args
}

/// What follows an argument's name.
#[derive(Clone)]
enum Rest {
    /// `hidden`
    Word,
    /// `tag = "users"`
    Value(TokenStream),
    /// `response(409, "…")`
    List(TokenStream, Span),
}

/// Split one argument into its name and whatever follows.
fn split_key(chunk: TokenStream) -> Option<(Ident, Rest)> {
    let mut trees = chunk.into_iter();
    let TokenTree::Ident(key) = trees.next()? else {
        return None;
    };

    match trees.next() {
        None => Some((key, Rest::Word)),
        Some(TokenTree::Punct(punct)) if punct.as_char() == '=' => {
            Some((key, Rest::Value(trees.collect())))
        }
        Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis => {
            Some((key, Rest::List(group.stream(), group.span())))
        }
        Some(other) => Some((
            key,
            Rest::Value(std::iter::once(other).chain(trees).collect()),
        )),
    }
}

/// Parse `key = <T>`.
fn value_of<T: syn::parse::Parse>(name: &str, rest: Rest, span: Span) -> Result<T, Error> {
    let Rest::Value(tokens) = rest else {
        return Err(Error::new(
            span,
            format!(
                "the `{name}` argument needs a value\n\n\
                 help: write `#[endpoint({name} = …)]`\n{ARGS_HELP}"
            ),
        ));
    };
    let value_span = chunk_span(&tokens);
    syn::parse2::<T>(tokens).map_err(|_| {
        Error::new(
            value_span,
            format!(
                "the `{name}` argument's value is not valid here\n\n\
                 help: `operation_id` and `tag` take a string literal; `errors` takes a type, \
                 as in `errors = BillingError`"
            ),
        )
    })
}

/// Parse `response(409, "Email already registered")`.
fn parse_response(rest: Rest, span: Span) -> Result<(Status, LitStr), Error> {
    let malformed = || {
        Error::new(
            span,
            "`response` takes a status code and a description\n\n\
             help: write `#[endpoint(response(409, \"Email already registered\"))]`",
        )
    };

    let Rest::List(tokens, list_span) = rest else {
        return Err(malformed());
    };

    let parts = split_top_level(tokens);
    let [status, description] = parts.as_slice() else {
        return Err(malformed());
    };

    let status_span = chunk_span(status);
    let literal: LitInt = syn::parse2(status.clone()).map_err(|_| malformed())?;
    let value: u16 = literal.base10_parse().map_err(|_| {
        Error::new(
            status_span,
            "a response status must be an HTTP status code between 100 and 599\n\n\
             help: write `response(409, \"…\")`",
        )
    })?;
    if !(100..=599).contains(&value) {
        return Err(Error::new(
            status_span,
            "a response status must be an HTTP status code between 100 and 599\n\n\
             help: write `response(409, \"…\")`",
        ));
    }

    let description: LitStr = syn::parse2(description.clone()).map_err(|_| {
        Error::new(
            list_span,
            "a response description must be a string literal\n\n\
             help: write `response(409, \"Email already registered\")`",
        )
    })?;

    Ok((
        Status {
            value,
            span: status_span,
        },
        description,
    ))
}

/// Parse `example(request = …, response = …)`.
fn parse_example(rest: Rest, span: Span, args: &mut EndpointArgs) -> Result<(), Error> {
    let malformed = || {
        Error::new(
            span,
            "`example` takes `request` and/or `response`\n\n\
             help: write `#[endpoint(example(request = r#\"{\"name\":\"ada\"}\"#))]`\n\
             help: the value is a string: literal JSON, or `include_str!(\"…/create.json\")`",
        )
    };

    let Rest::List(tokens, _) = rest else {
        return Err(malformed());
    };

    for chunk in split_top_level(tokens) {
        let Some((key, Rest::Value(value))) = split_key(chunk) else {
            return Err(malformed());
        };
        let expr: Expr = syn::parse2(value).map_err(|_| malformed())?;
        match key.to_string().as_str() {
            "request" => args.request_example = Some(expr),
            "response" => args.response_example = Some(expr),
            other => {
                let hint = suggestion(other, &["request", "response"])
                    .unwrap_or_else(|| "note: `example` takes `request` and `response`".to_owned());
                return Err(Error::new(
                    key.span(),
                    format!(
                        "unknown `example` field `{other}`\n\n{hint}\n\
                         help: write `example(request = \"…\", response = \"…\")`"
                    ),
                ));
            }
        }
    }

    Ok(())
}

/// The "unknown argument" error, with its Levenshtein suggestion.
fn unknown_argument(key: &Ident) -> Error {
    let name = key.to_string();
    let hint = suggestion(&name, KNOWN_ARGS)
        .unwrap_or_else(|| format!("note: `#[endpoint]` has no `{name}` argument"));
    Error::new(
        key.span(),
        format!("unknown `#[endpoint]` argument `{name}`\n\n{hint}\n{ARGS_HELP}"),
    )
}

/// `help: did you mean …?`, when a candidate is close enough to be a typo.
///
/// The threshold scales with the length of what the user typed, so `tags` is a
/// typo for `tag` but `tag` is not a typo for `errors`.
pub(crate) fn suggestion(input: &str, candidates: &[&str]) -> Option<String> {
    let budget = (input.chars().count() / 3).max(1) + 1;
    candidates
        .iter()
        .map(|candidate| (levenshtein(input, candidate), *candidate))
        .filter(|(distance, _)| *distance <= budget)
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| format!("help: did you mean `{candidate}`?"))
}

/// Edit distance, two rows at a time.
fn levenshtein(left: &str, right: &str) -> usize {
    let right_chars: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right_chars.len()).collect();
    let mut current = vec![0usize; right_chars.len() + 1];

    for (row, left_char) in left.chars().enumerate() {
        current[0] = row + 1;
        for (column, right_char) in right_chars.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_char != *right_char);
            let deletion = previous[column + 1] + 1;
            let insertion = current[column] + 1;
            current[column + 1] = substitution.min(deletion).min(insertion);
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[right_chars.len()]
}

// ---------------------------------------------------------------------------
// Token utilities
// ---------------------------------------------------------------------------

/// Split a token stream on top-level commas.
///
/// Groups are opaque, so `response(409, "…")` is one chunk and the comma inside
/// it is not a separator. Trailing commas produce no empty chunk.
pub(crate) fn split_top_level(tokens: TokenStream) -> Vec<TokenStream> {
    let mut chunks: Vec<TokenStream> = Vec::new();
    let mut current: Vec<TokenTree> = Vec::new();

    for tree in tokens {
        match &tree {
            TokenTree::Punct(punct) if punct.as_char() == ',' => {
                if !current.is_empty() {
                    chunks.push(current.drain(..).collect());
                }
            }
            _ => current.push(tree),
        }
    }
    if !current.is_empty() {
        chunks.push(current.into_iter().collect());
    }

    chunks
}

/// The span of a token stream, joined where the compiler allows it.
pub(crate) fn chunk_span(tokens: &TokenStream) -> Span {
    let mut trees = tokens.clone().into_iter();
    let Some(first) = trees.next() else {
        return Span::call_site();
    };
    let mut span = first.span();
    for tree in trees {
        span = span.join(tree.span()).unwrap_or(span);
    }
    span
}

/// Render a type for an error message, short enough to read.
///
/// Style guide rule 2: never print a type longer than 80 characters. Past that
/// the outermost name plus `<…>` says the same thing in a tenth of the width.
pub(crate) fn short_type(ty: &Type) -> String {
    let full = render_type(ty);
    if full.chars().count() <= 80 {
        return full;
    }
    match outer_type_name(ty) {
        Some(name) => format!("{name}<…>"),
        None => full.chars().take(77).chain("…".chars()).collect(),
    }
}

/// Turn a type back into something that looks like what the user wrote.
fn render_type(ty: &Type) -> String {
    let mut text = ty.to_token_stream().to_string();
    for (from, to) in [
        (" ::", "::"),
        (":: ", "::"),
        (" <", "<"),
        ("< ", "<"),
        (" >", ">"),
        ("> ", ">"),
        (" ,", ","),
        ("& ", "&"),
        (" (", "("),
        ("( ", "("),
        (" )", ")"),
        (" [", "["),
        ("[ ", "["),
        (" ]", "]"),
    ] {
        text = text.replace(from, to);
    }
    // The `>` rules glue `-> T` into `->T`; put the space back.
    text = text.replace("->", " -> ");
    while text.contains("  ") {
        text = text.replace("  ", " ");
    }
    text.trim().to_owned()
}

/// Concatenate two token streams.
fn join(first: TokenStream, second: TokenStream) -> TokenStream {
    let mut out = first;
    out.extend(second);
    out
}

/// Fold accumulated errors into one, so the user reads them in source order.
fn combine(errors: Vec<Error>) -> Error {
    let mut iterator = errors.into_iter();
    let mut first = iterator
        .next()
        .unwrap_or_else(|| Error::new(Span::call_site(), "invalid `#[endpoint]`"));
    for error in iterator {
        first.combine(error);
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    // ── the body-extractor heuristic ──────────────────────────────────────

    #[test]
    fn built_in_body_extractors_are_recognised() {
        for ty in [
            parse_quote!(Json<CreateUser>),
            parse_quote!(Form<Login>),
            parse_quote!(Bytes),
            parse_quote!(Text),
            parse_quote!(RawBody),
            parse_quote!(BodyStream),
            parse_quote!(Multipart),
            parse_quote!(OpaqueBody<axum::Json<T>>),
            parse_quote!(moso::extract::Json<CreateUser>),
        ] {
            let ty: Type = ty;
            assert!(is_body_extractor(&ty), "{}", render_type(&ty));
        }
    }

    #[test]
    fn parts_extractors_are_not_body_extractors() {
        for ty in [
            parse_quote!(Inject<Db>),
            parse_quote!(Depends<CurrentUser>),
            parse_quote!(Path<UserId>),
            parse_quote!(Path<String>),
            parse_quote!(Query<Filter>),
            parse_quote!(Headers<Auth>),
            parse_quote!(Cookies),
            parse_quote!(RequestId),
            parse_quote!(Opaque<OriginalUri>),
            parse_quote!(Somebody),
        ] {
            let ty: Type = ty;
            assert!(!is_body_extractor(&ty), "{}", render_type(&ty));
        }
    }

    #[test]
    fn a_bare_string_is_a_body_but_a_nested_one_is_not() {
        let bare: Type = parse_quote!(String);
        let nested: Type = parse_quote!(Path<String>);
        assert!(is_body_extractor(&bare));
        assert!(!is_body_extractor(&nested));
    }

    #[test]
    fn third_party_body_extractors_are_caught_by_shape() {
        for name in ["ProtobufBody", "MsgPackBody", "BodyStream", "FileUpload"] {
            let ty: Type = syn::parse_str(name).unwrap();
            assert!(is_body_extractor(&ty), "{name}");
        }
        // `Body` itself is too generic a word to claim; four characters is the
        // floor for both shape rules.
        let ty: Type = parse_quote!(Body);
        assert!(!is_body_extractor(&ty));
    }

    // ── position detection ────────────────────────────────────────────────

    fn classify(func: ItemFn) -> (Vec<bool>, Vec<String>) {
        let mut errors = Vec::new();
        let params = classify_parameters(&func, &mut errors);
        (
            params.iter().map(|param| param.body).collect(),
            errors.iter().map(|error| error.to_string()).collect(),
        )
    }

    #[test]
    fn a_trailing_body_extractor_is_accepted_and_marked() {
        let (bodies, errors) = classify(parse_quote! {
            async fn create(
                Inject(db): Inject<Db>,
                Depends(actor): Depends<CurrentUser>,
                Json(body): Json<CreateUser>,
            ) -> Result<Created<UserOut>> {}
        });
        assert_eq!(bodies, vec![false, false, true]);
        assert!(errors.is_empty());
    }

    #[test]
    fn a_handler_with_no_body_extractor_marks_nothing() {
        let (bodies, errors) = classify(parse_quote! {
            async fn list(Inject(db): Inject<Db>, Query(filter): Query<Filter>) -> Result<()> {}
        });
        assert_eq!(bodies, vec![false, false]);
        assert!(errors.is_empty());
    }

    #[test]
    fn a_zero_parameter_handler_is_fine() {
        let (bodies, errors) = classify(parse_quote! {
            async fn healthz() -> Result<NoContent> {}
        });
        assert!(bodies.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn a_body_extractor_that_is_not_last_is_the_documented_error() {
        let (_, errors) = classify(parse_quote! {
            async fn create(Json(body): Json<CreateUser>, Inject(db): Inject<Db>) -> Result<()> {}
        });
        assert_eq!(errors.len(), 1);
        let message = &errors[0];
        assert!(message.starts_with("request body extractor must be the last parameter"));
        assert!(message.contains("only one body extractor is allowed per handler"));
        assert!(message.contains("help: move `Json<CreateUser>` to the end of the parameter list"));
    }

    #[test]
    fn two_body_extractors_are_one_error_naming_the_first() {
        let (_, errors) = classify(parse_quote! {
            async fn create(Bytes(raw): Bytes, Json(body): Json<CreateUser>) -> Result<()> {}
        });
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("only one body extractor is allowed per handler"));
        assert!(errors[0].contains("`Bytes` already consumes the request body"));
    }

    #[test]
    fn two_body_extractors_win_over_the_position_rule() {
        let (_, errors) = classify(parse_quote! {
            async fn create(
                Inject(db): Inject<Db>,
                Json(a): Json<A>,
                Form(b): Form<B>,
            ) -> Result<()> {}
        });
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("only one body extractor is allowed per handler"));
        assert!(errors[0].contains("`Json<A>` already consumes the request body"));
    }

    #[test]
    fn seventeen_parameters_is_one_error() {
        let params = (0..17usize)
            .map(|index| {
                let name = format_ident!("p{}", index);
                quote!(#name: RequestId)
            })
            .collect::<Vec<_>>();
        let func: ItemFn = syn::parse2(quote! {
            async fn wide(#(#params),*) -> Result<()> {}
        })
        .unwrap();
        let (_, errors) = classify(func);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with(
            "handlers support at most 16 parameters; group them into a `Depends` struct"
        ));
        assert!(errors[0].contains("this handler declares 17"));
    }

    #[test]
    fn sixteen_parameters_is_accepted() {
        let params = (0..16usize)
            .map(|index| {
                let name = format_ident!("p{}", index);
                quote!(#name: RequestId)
            })
            .collect::<Vec<_>>();
        let func: ItemFn = syn::parse2(quote! {
            async fn wide(#(#params),*) -> Result<()> {}
        })
        .unwrap();
        let (_, errors) = classify(func);
        assert!(errors.is_empty());
    }

    // ── signature checks ──────────────────────────────────────────────────

    fn signature_errors(func: ItemFn) -> Vec<String> {
        let mut errors = Vec::new();
        check_signature(&func, &mut errors);
        errors.iter().map(|error| error.to_string()).collect()
    }

    #[test]
    fn a_blocking_handler_is_rejected_by_name() {
        let errors = signature_errors(parse_quote! {
            fn list() -> Result<NoContent> {}
        });
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("handlers must be `async fn`"));
        assert!(errors[0].contains("help: write `async fn list(…)`"));
    }

    #[test]
    fn a_generic_handler_is_rejected() {
        let errors = signature_errors(parse_quote! {
            async fn list<T: Send>(item: T) -> Result<NoContent> {}
        });
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .starts_with("handlers may not be generic; use a concrete type or a trait object")
        );
    }

    #[test]
    fn impl_trait_in_a_parameter_is_rejected_as_generic() {
        let errors = signature_errors(parse_quote! {
            async fn list(item: impl Extract) -> Result<NoContent> {}
        });
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .starts_with("handlers may not be generic; use a concrete type or a trait object")
        );
    }

    #[test]
    fn a_lifetime_parameter_is_generic_too() {
        let errors = signature_errors(parse_quote! {
            async fn list<'a>(item: &'a str) -> Result<NoContent> {}
        });
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn impl_trait_in_return_position_is_rejected_with_its_own_message() {
        let func: ItemFn = parse_quote! {
            async fn list() -> impl IntoResponse {}
        };
        let mut errors = Vec::new();
        assert!(return_type(&func, &mut errors).is_none());
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .starts_with("a handler's return type must be a named type")
        );
    }

    #[test]
    fn a_handler_without_a_return_type_returns_unit() {
        let func: ItemFn = parse_quote! { async fn ping() {} };
        let mut errors = Vec::new();
        let ty = return_type(&func, &mut errors).unwrap();
        assert!(errors.is_empty());
        assert_eq!(render_type(&ty), "()");
    }

    // ── doc comments ──────────────────────────────────────────────────────

    #[test]
    fn the_first_line_is_the_summary_and_the_rest_is_the_description() {
        let func: ItemFn = parse_quote! {
            /// Create a user.
            ///
            /// Sends a welcome email asynchronously.
            /// Emails are unique; conflicts return 409.
            async fn create() -> Result<()> {}
        };
        let (summary, description) = doc_summary_and_description(&func.attrs);
        assert_eq!(summary.as_deref(), Some("Create a user."));
        assert_eq!(
            description.as_deref(),
            Some("Sends a welcome email asynchronously.\nEmails are unique; conflicts return 409.")
        );
    }

    #[test]
    fn a_one_line_doc_comment_has_no_description() {
        let func: ItemFn = parse_quote! {
            /// List users.
            async fn list() -> Result<()> {}
        };
        let (summary, description) = doc_summary_and_description(&func.attrs);
        assert_eq!(summary.as_deref(), Some("List users."));
        assert_eq!(description, None);
    }

    #[test]
    fn an_undocumented_handler_contributes_nothing() {
        let func: ItemFn = parse_quote! { async fn list() -> Result<()> {} };
        assert_eq!(doc_summary_and_description(&func.attrs), (None, None));
    }

    #[test]
    fn markdown_indentation_inside_the_description_survives() {
        let func: ItemFn = parse_quote! {
            /// Publish a post.
            ///
            /// ```json
            ///     {"draft": false}
            /// ```
            async fn publish() -> Result<()> {}
        };
        let (_, description) = doc_summary_and_description(&func.attrs);
        assert!(description.unwrap().contains("    {\"draft\": false}"));
    }

    #[test]
    fn the_deprecated_attribute_is_noticed() {
        let func: ItemFn = parse_quote! {
            #[deprecated = "use v2"]
            async fn list() -> Result<()> {}
        };
        assert!(has_deprecated_attribute(&func.attrs));
        let plain: ItemFn = parse_quote! { async fn list() -> Result<()> {} };
        assert!(!has_deprecated_attribute(&plain.attrs));
    }

    // ── attribute arguments ───────────────────────────────────────────────

    fn args_of(tokens: TokenStream) -> (EndpointArgs, Vec<String>) {
        let mut errors = Vec::new();
        let args = parse_args(tokens, &mut errors);
        (args, errors.iter().map(|e| e.to_string()).collect())
    }

    #[test]
    fn the_bare_attribute_parses_to_nothing() {
        let (args, errors) = args_of(quote!());
        assert!(errors.is_empty());
        assert!(args.operation_id.is_none());
        assert!(args.tags.is_empty());
        assert!(!args.hidden);
    }

    #[test]
    fn every_documented_argument_parses() {
        let (args, errors) = args_of(quote! {
            operation_id = "users.create",
            tag = "users",
            hidden,
            deprecated,
            response(409, "Email already registered"),
            example(request = "{}", response = "{}"),
            errors = BillingError,
        });
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(args.operation_id.unwrap().value(), "users.create");
        assert_eq!(args.tags.len(), 1);
        assert!(args.hidden);
        assert!(args.deprecated);
        assert_eq!(args.responses.len(), 1);
        assert_eq!(args.responses[0].0.value, 409);
        assert!(args.request_example.is_some());
        assert!(args.response_example.is_some());
        assert_eq!(args.errors.len(), 1);
    }

    #[test]
    fn several_tags_accumulate() {
        let (args, errors) = args_of(quote!(tag = "users", tag = "admin"));
        assert!(errors.is_empty());
        assert_eq!(args.tags.len(), 2);
    }

    #[test]
    fn an_unknown_argument_suggests_the_closest_one() {
        let (_, errors) = args_of(quote!(tags = "users"));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("unknown `#[endpoint]` argument `tags`"));
        assert!(errors[0].contains("help: did you mean `tag`?"));
        assert!(errors[0].contains("help: valid arguments are"));
    }

    #[test]
    fn a_wildly_wrong_argument_still_lists_the_valid_ones() {
        let (_, errors) = args_of(quote!(zzzzzzzz = "x"));
        assert_eq!(errors.len(), 1);
        assert!(!errors[0].contains("did you mean"));
        assert!(errors[0].contains("help: valid arguments are"));
    }

    #[test]
    fn a_flag_given_a_value_says_so() {
        let (_, errors) = args_of(quote!(hidden = true));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("the `hidden` argument takes no value"));
        assert!(errors[0].contains("help: write `#[endpoint(hidden)]`"));
    }

    #[test]
    fn a_malformed_response_is_one_error() {
        let (_, errors) = args_of(quote!(response(409)));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("`response` takes a status code and a description"));
    }

    #[test]
    fn an_out_of_range_status_is_rejected() {
        let (_, errors) = args_of(quote!(response(9000, "nope")));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("between 100 and 599"));
    }

    #[test]
    fn an_unknown_example_field_suggests_the_closest_one() {
        let (_, errors) = args_of(quote!(example(requst = "{}")));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("help: did you mean `request`?"));
    }

    #[test]
    fn two_bad_arguments_are_two_errors_not_a_cascade() {
        let (_, errors) = args_of(quote!(tags = "users", hiden));
        assert_eq!(errors.len(), 2);
    }

    // ── suggestions ───────────────────────────────────────────────────────

    #[test]
    fn levenshtein_is_the_usual_edit_distance() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("tag", "tag"), 0);
        assert_eq!(levenshtein("tags", "tag"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "tag"), 3);
    }

    #[test]
    fn suggestions_are_offered_only_when_they_are_plausible() {
        assert!(suggestion("tags", KNOWN_ARGS).unwrap().contains("`tag`"));
        assert!(
            suggestion("operationid", KNOWN_ARGS)
                .unwrap()
                .contains("`operation_id`")
        );
        assert!(
            suggestion("responses", KNOWN_ARGS)
                .unwrap()
                .contains("`response`")
        );
        assert_eq!(suggestion("completely_unrelated", KNOWN_ARGS), None);
    }

    // ── rendering ─────────────────────────────────────────────────────────

    #[test]
    fn types_render_the_way_the_user_wrote_them() {
        let ty: Type = parse_quote!(Json<CreateUser>);
        assert_eq!(render_type(&ty), "Json<CreateUser>");
        let ty: Type = parse_quote!(moso::extract::Path<(String, u32)>);
        assert_eq!(render_type(&ty), "moso::extract::Path<(String, u32)>");
        let ty: Type = parse_quote!(Result<Created<UserOut>>);
        assert_eq!(render_type(&ty), "Result<Created<UserOut>>");
    }

    #[test]
    fn a_long_type_is_shortened_to_its_head() {
        let ty: Type = parse_quote!(
            Json<VeryLongTypeNameIndeed<AnotherVeryLongTypeName, AndAThirdOneForGoodMeasure>>
        );
        assert!(render_type(&ty).chars().count() > 80);
        assert_eq!(short_type(&ty), "Json<…>");
    }

    #[test]
    fn a_short_type_is_printed_in_full() {
        let ty: Type = parse_quote!(Json<CreateUser>);
        assert_eq!(short_type(&ty), "Json<CreateUser>");
    }

    // ── token utilities ───────────────────────────────────────────────────

    #[test]
    fn top_level_commas_split_and_nested_ones_do_not() {
        let chunks = split_top_level(quote!(a, response(409, "x"), b));
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].to_string(), "a");
        assert_eq!(chunks[2].to_string(), "b");
    }

    #[test]
    fn a_trailing_comma_produces_no_empty_chunk() {
        assert_eq!(split_top_level(quote!(a, b,)).len(), 2);
        assert_eq!(split_top_level(quote!()).len(), 0);
    }

    // ── expansion smoke tests ─────────────────────────────────────────────

    /// The expansion's text with every space removed.
    ///
    /// `TokenStream::to_string` may put a space between any two tokens and does
    /// not promise where, so these assertions compare the squashed form: what
    /// the expansion *is*, not how `proc-macro2` chose to print it. Spaces
    /// inside string literals are squashed too, hence `"Createauser."`.
    fn expand_str(attr: TokenStream, item: TokenStream) -> String {
        expand(attr, item)
            .to_string()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    #[test]
    fn the_expansion_contains_the_documented_items() {
        let out = expand_str(
            quote!(),
            quote! {
                /// Create a user.
                ///
                /// Sends a welcome email asynchronously.
                async fn create(
                    Inject(db): Inject<Db>,
                    Json(body): Json<CreateUser>,
                ) -> Result<Created<UserOut>> { todo!() }
            },
        );
        assert!(out.contains("pubstruct__moso_op_create;"));
        assert!(out.contains("impl::moso::__private::Endpointfor__moso_op_create"));
        assert!(out.contains("impl::moso::__private::HandlerFnfor__moso_op_create"));
        assert!(out.contains("constNAME:&'staticstr=\"create\""));
        assert!(out.contains("__moso_b.summary(\"Createauser.\")"));
        assert!(out.contains("__moso_b.description(\"Sendsawelcomeemailasynchronously.\")"));
        assert!(out.contains("<Inject<Db>as::moso::__private::Extract>::describe(__moso_b)"));
        assert!(
            out.contains("<Json<CreateUser>as::moso::__private::ExtractBody>::describe(__moso_b)")
        );
        assert!(out.contains(
            "<Result<Created<UserOut>>as::moso::__private::HandlerReturn>::describe_response(\
             __moso_b)"
        ));
        assert!(out.contains("::moso::__private::concat_reqs!"));
        // The assertion block names the same associated items, so that a
        // failure renders identically to the one from `spec` and rustc prints
        // it once.
        assert_eq!(
            out.matches("<Inject<Db>as::moso::__private::Extract>::describe")
                .count(),
            2
        );
        assert_eq!(
            out.matches("<Json<CreateUser>as::moso::__private::ExtractBody>::describe")
                .count(),
            2
        );
        assert_eq!(
            out.matches(
                "<Result<Created<UserOut>>as::moso::__private::HandlerReturn>::describe_response"
            )
            .count(),
            2
        );
        assert!(!out.contains("__moso_assert_extract"));
        assert!(out.contains("ExtractBody>::extract_body"));
        assert!(out.contains("create(__moso_a0,__moso_a1).await"));
        assert!(!out.contains("compile_error"));
    }

    #[test]
    fn a_broken_handler_expands_to_one_error_and_a_placeholder() {
        let out = expand_str(
            quote!(),
            quote! {
                async fn create(Json(body): Json<CreateUser>, Inject(db): Inject<Db>)
                    -> Result<()> { todo!() }
            },
        );
        assert_eq!(out.matches("compile_error").count(), 1);
        // The placeholder still exists, so `routes!` does not produce a second
        // error about a missing type.
        assert!(out.contains("pubstruct__moso_op_create;"));
        assert!(out.contains("impl::moso::__private::Endpointfor__moso_op_create"));
        // …but it does not try to describe the broken parameters.
        assert!(!out.contains("::describe("));
        assert!(!out.contains("__moso_assert_extract"));
    }

    #[test]
    fn a_method_gets_no_placeholder_because_it_could_not_compile() {
        let out = expand_str(
            quote!(),
            quote! {
                async fn create(&self, Json(body): Json<CreateUser>) -> Result<()> { todo!() }
            },
        );
        assert_eq!(out.matches("compile_error").count(), 1);
        assert!(out.contains("handlersmustbefreefunctions,notmethods"));
        assert!(!out.contains("struct__moso_op_create"));
    }

    #[test]
    fn a_non_function_item_is_refused_politely() {
        let out = expand_str(
            quote!(),
            quote!(
                struct NotAHandler;
            ),
        );
        assert_eq!(out.matches("compile_error").count(), 1);
        assert!(out.contains("mayonlybeappliedtoan`asyncfn`"));
        assert!(out.contains("structNotAHandler"));
    }

    #[test]
    fn an_explicit_operation_id_replaces_the_derived_one() {
        let out = expand_str(
            quote!(operation_id = "users.create"),
            quote!(
                async fn create() -> Result<()> {
                    todo!()
                }
            ),
        );
        assert!(out.contains("__moso_b.operation_id(\"users.create\")"));
        assert!(!out.contains("module_path"));
    }

    #[test]
    fn the_derived_operation_id_uses_the_module_path() {
        let out = expand_str(
            quote!(),
            quote!(
                async fn create() -> Result<()> {
                    todo!()
                }
            ),
        );
        assert!(out.contains("::core::module_path!()"));
        assert!(out.contains("rsplit_once(\"::\")"));
        assert!(out.contains("\"create\""));
    }

    #[test]
    fn the_source_location_is_always_recorded() {
        let out = expand_str(
            quote!(),
            quote!(
                async fn create() -> Result<()> {
                    todo!()
                }
            ),
        );
        assert!(out.contains("__moso_b.source(::core::file!(),::core::line!())"));
    }

    #[test]
    fn hidden_deprecated_and_tags_reach_the_builder() {
        let out = expand_str(
            quote!(hidden, deprecated, tag = "users"),
            quote!(
                async fn create() -> Result<()> {
                    todo!()
                }
            ),
        );
        assert!(out.contains("__moso_b.hidden()"));
        assert!(out.contains("__moso_b.deprecated()"));
        assert!(out.contains("__moso_b.tag(\"users\")"));
    }

    #[test]
    fn a_deprecated_attribute_reaches_the_builder_without_the_argument() {
        let out = expand_str(
            quote!(),
            quote! {
                #[deprecated = "use v2"]
                async fn create() -> Result<()> { todo!() }
            },
        );
        assert!(out.contains("__moso_b.deprecated()"));
        // The glue calls a deprecated function; the lint must not fire in the
        // user's crate over code the user did not write.
        assert!(out.contains("#[allow(unused_mut,unused_variables,deprecated)]"));
    }

    #[test]
    fn extra_responses_choose_a_problem_document_only_for_failures() {
        let failure = expand_str(
            quote!(response(409, "Email already registered")),
            quote!(
                async fn create() -> Result<()> {
                    todo!()
                }
            ),
        );
        assert!(
            failure.contains("__moso_b.response(409u16,::moso::__private::ResponseSpec::problem")
        );

        let success = expand_str(
            quote!(response(204, "Nothing to send")),
            quote!(
                async fn create() -> Result<()> {
                    todo!()
                }
            ),
        );
        assert!(
            success.contains("__moso_b.response(204u16,::moso::__private::ResponseSpec::empty")
        );
    }

    #[test]
    fn an_extra_response_is_written_before_the_describers_so_it_wins() {
        let out = expand_str(
            quote!(response(200, "Overridden")),
            quote!(
                async fn create() -> Result<NoContent> {
                    todo!()
                }
            ),
        );
        let response_at = out.find("__moso_b.response(200u16").unwrap();
        let describe_at = out.find("::describe_response(__moso_b)").unwrap();
        assert!(response_at < describe_at);
    }

    #[test]
    fn an_errors_type_is_described_and_asserted() {
        let out = expand_str(
            quote!(errors = BillingError),
            quote!(
                async fn create() -> Result<()> {
                    todo!()
                }
            ),
        );
        assert_eq!(
            out.matches("<BillingErroras::moso::__private::Describe>::describe")
                .count(),
            2
        );
    }

    #[test]
    fn examples_are_written_after_the_describers() {
        let out = expand_str(
            quote!(example(request = "{}", response = "[]")),
            quote! {
                async fn create(Json(body): Json<CreateUser>) -> Result<()> { todo!() }
            },
        );
        assert!(out.contains("__moso_example"));
        assert!(out.contains("request_body.as_mut()"));
        assert!(out.contains("responses.iter_mut()"));
        let describe_at = out.find("ExtractBody>::describe").unwrap();
        let example_at = out.find("__moso_example(").unwrap();
        assert!(describe_at < example_at);
    }

    #[test]
    fn no_example_argument_emits_no_example_machinery() {
        let out = expand_str(
            quote!(),
            quote!(
                async fn create(Json(b): Json<C>) -> Result<()> {
                    todo!()
                }
            ),
        );
        assert!(!out.contains("__moso_example"));
        assert!(!out.contains("serde_json"));
    }

    #[test]
    fn a_cfg_gated_handler_gates_everything_it_generates() {
        let out = expand_str(
            quote!(),
            quote! {
                #[cfg(feature = "admin")]
                async fn purge() -> Result<()> { todo!() }
            },
        );
        // Once on the function itself, then on the struct, both impls and the
        // assertion block — five in total.
        assert_eq!(out.matches("#[cfg(feature=\"admin\")]").count(), 5);
    }

    #[test]
    fn cfg_attr_is_not_copied() {
        let out = expand_str(
            quote!(),
            quote! {
                #[cfg_attr(test, allow(dead_code))]
                async fn purge() -> Result<()> { todo!() }
            },
        );
        assert_eq!(out.matches("cfg_attr").count(), 1);
    }

    #[test]
    fn a_broken_cfg_gated_handler_gates_its_placeholder_too() {
        let out = expand_str(
            quote!(),
            quote! {
                #[cfg(feature = "admin")]
                async fn purge(Json(b): Json<A>, _id: RequestId) -> Result<()> { todo!() }
            },
        );
        assert_eq!(out.matches("compile_error").count(), 1);
        assert_eq!(out.matches("#[cfg(feature=\"admin\")]").count(), 4);
    }

    #[test]
    fn a_zero_parameter_handler_still_produces_working_glue() {
        let out = expand_str(
            quote!(),
            quote!(
                async fn ping() -> Result<()> {
                    todo!()
                }
            ),
        );
        assert!(out.contains("__moso_req.into_parts()"));
        assert!(out.contains("into_handler_response(ping().await,)"));
        assert!(out.contains("::moso::__private::concat_reqs!()"));
    }

    #[test]
    fn a_handler_with_no_body_extractor_never_calls_extract_body() {
        let out = expand_str(
            quote!(),
            quote!(
                async fn list(Query(f): Query<Filter>) -> Result<()> {
                    todo!()
                }
            ),
        );
        // `ExtractBody` still appears, once, as the bound on the unused
        // assertion helper — but nothing is asserted or extracted through it.
        assert!(!out.contains("ExtractBody>::describe"));
        assert!(!out.contains("ExtractBody>::extract_body"));
        assert!(!out.contains("__moso_assert_body::<"));
        assert!(out.contains("<Query<Filter>as::moso::__private::Extract>::extract("));
    }
}
