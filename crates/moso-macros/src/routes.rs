//! `routes!` and `ep!` — registering endpoints by name.
//!
//! `#[endpoint]` puts an operation's metadata on a companion type called
//! `__moso_op_<name>`, because Rust cannot attach an associated type to a `fn`
//! item. These two macros are the sugar that hides that name:
//!
//! ```
//! use moso::prelude::*;
//! # /// A user.
//! # #[derive(Schema)] pub struct UserOut { /// Identifier.
//! #     pub id: u64 }
//! # /// List users.
//! # #[endpoint] pub async fn list() -> Result<Json<Vec<UserOut>>> { Ok(Json(vec![])) }
//! # /// Create a user.
//! # #[endpoint] pub async fn create() -> Result<Created<UserOut>> {
//! #     Ok(Created::at("/users/1", UserOut { id: 1 })) }
//! # /// Handlers that live in another module.
//! # pub mod users {
//! #     use moso::prelude::*;
//! #     use super::UserOut;
//! #     /// Show a user.
//! #     #[endpoint] pub async fn show(Path(id): Path<u64>) -> Result<Json<UserOut>> {
//! #         Ok(Json(UserOut { id })) }
//! # }
//! # fn main() {
//! let table = moso::routes! {
//!     GET    "/users"      => list,
//!     POST   "/users"      => create,
//!     GET    "/users/{id}" => users::show,
//! }
//! .tag("users");
//!
//! let one = Router::new().get("/users", moso::ep!(list));
//! # assert_eq!((table.len(), one.len()), (3, 1));
//! # }
//! ```
//!
//! # What `routes!` expands to
//!
//! ```text
//! ::moso::__private::Router::new()
//!     .endpoint::<__moso_op_list>(
//!         ::moso::__private::HttpMethod::Get,
//!         ::moso::__private::route_path!("/users"),
//!     )
//!     .endpoint::<__moso_op_create>(
//!         ::moso::__private::HttpMethod::Post,
//!         ::moso::__private::route_path!("/users"),
//!     )
//! ```
//!
//! It is the builder chain, written as a table. Acceptance criterion 5 of
//! `01-http/11` says the two must produce byte-identical OpenAPI documents,
//! and the cheapest way to guarantee that is for one to *be* the other.
//!
//! The path literal is checked twice, on purpose. [`crate::path::validate`]
//! runs here, during expansion, and owns the *message*: it has the literal's
//! span, so it can point at the user's own quotes and offer their own path with
//! the mistake corrected. `route_path!` then re-checks the same rules in a
//! `const`, which costs nothing and catches a path that reached the router by
//! another road; `Router::endpoint` checks a third time at registration, so a
//! path built at runtime still fails loudly rather than 404ing in staging.
//!
//! # Path preservation
//!
//! Only the **last** segment of a handler's path is rewritten:
//! `users::list` becomes `users::__moso_op_list`. The module qualification is
//! the user's, and a macro that flattened it would break every table that
//! registers handlers from more than one module.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{Error, Ident, LitStr, Path, PathArguments, Token};

use crate::endpoint::suggestion;

/// The HTTP methods a `routes!` table may name.
///
/// `ANY` is not an HTTP method: it is shorthand for registering the same
/// endpoint under every method in this list, which is what a catch-all route
/// (a proxy, a webhook receiver, a legacy shim) actually needs.
const METHOD_NAMES: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "ANY",
];

/// The methods `ANY` expands to, in `HttpMethod::ALL` order so that the route
/// table and the OpenAPI document come out deterministically.
const ANY_METHODS: &[&str] = &["GET", "PUT", "POST", "DELETE", "OPTIONS", "HEAD", "PATCH"];

// ---------------------------------------------------------------------------
// The shared naming rule
// ---------------------------------------------------------------------------

/// The companion type `#[endpoint]` generates for a handler called `name`.
///
/// The single source of truth for the `__moso_op_` prefix: `#[endpoint]` emits
/// it, `routes!` and `ep!` resolve it, and all three call this function.
///
/// The span is the caller's, so `routes! { GET "/u" => lst }` underlines `lst`
/// rather than pointing into the macro.
pub(crate) fn op_ident(name: &Ident) -> Ident {
    format_ident!("__moso_op_{}", name, span = name.span())
}

/// Rewrite a handler path to its companion type's path.
///
/// `list` becomes `__moso_op_list`; `users::list` becomes
/// `users::__moso_op_list`; `crate::routes::users::list` keeps every segment
/// but the last.
pub(crate) fn rewrite_path(path: &Path) -> Result<Path, Error> {
    let mut rewritten = path.clone();

    let Some(last) = rewritten.segments.last_mut() else {
        return Err(Error::new(
            path.span(),
            "expected a handler name\n\n\
             help: write the function's name, as in `GET \"/users\" => list`",
        ));
    };

    if !matches!(last.arguments, PathArguments::None) {
        return Err(Error::new(
            last.arguments.span(),
            "a handler name may not carry generic arguments\n\n\
             note: handlers are not generic — `#[endpoint]` rejects a generic `async fn`\n\
             help: write the plain name, as in `GET \"/users\" => list`",
        ));
    }

    last.ident = op_ident(&last.ident);
    Ok(rewritten)
}

// ---------------------------------------------------------------------------
// routes!
// ---------------------------------------------------------------------------

/// One row of a `routes!` table: a method, a path literal and a handler.
struct Route {
    /// The canonical, upper-case method name — or `ANY`.
    method: String,
    /// The span of the method token, for the error that names it.
    method_span: Span,
    /// The path template, kept as a literal so `route_path!` can check it.
    path: LitStr,
    /// The handler's path, already rewritten to its companion type.
    handler: Path,
}

impl Parse for Route {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let method: Ident = input.parse().map_err(|error| {
            Error::new(
                error.span(),
                "expected an HTTP method\n\n\
                 help: write a row as `GET \"/users\" => list`\n\
                 help: methods are `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`, \
                 `TRACE` and `ANY`",
            )
        })?;
        let method_span = method.span();
        let name = method.to_string().to_ascii_uppercase();
        if !METHOD_NAMES.contains(&name.as_str()) {
            let hint = suggestion(&name, METHOD_NAMES)
                .unwrap_or_else(|| format!("note: `{method}` is not an HTTP method"));
            return Err(Error::new(
                method_span,
                format!(
                    "unknown HTTP method `{method}`\n\n{hint}\n\
                     help: methods are `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, \
                     `OPTIONS`, `TRACE` and `ANY`"
                ),
            ));
        }

        let path: LitStr = input.parse().map_err(|error| {
            Error::new(
                error.span(),
                "expected a path template in quotes\n\n\
                 help: write a row as `GET \"/users/{id}\" => show`\n\
                 note: paths use OpenAPI syntax — `{id}`, not `:id`",
            )
        })?;
        // Checked here rather than left to `route_path!`: a const-evaluation
        // panic cannot carry a `note:` or a `help:`, and it points into
        // `moso-core` instead of at the literal the user typed.
        crate::path::validate(&path)?;

        input.parse::<Token![=>]>().map_err(|error| {
            Error::new(
                error.span(),
                "expected `=>` between the path and the handler\n\n\
                 help: write a row as `GET \"/users\" => list`",
            )
        })?;

        let handler: Path = input.parse().map_err(|error| {
            Error::new(
                error.span(),
                "expected a handler name\n\n\
                 help: write the function's name, as in `GET \"/users\" => list`\n\
                 help: a handler in another module keeps its path: `users::list`",
            )
        })?;

        Ok(Route {
            method: name,
            method_span,
            path,
            handler: rewrite_path(&handler)?,
        })
    }
}

/// A whole `routes!` table.
struct Table {
    routes: Vec<Route>,
}

impl Parse for Table {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut routes = Vec::new();
        while !input.is_empty() {
            routes.push(input.parse()?);
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>().map_err(|error| {
                Error::new(
                    error.span(),
                    "expected `,` between routes\n\n\
                     help: separate the rows of the table with commas:\n    \
                     GET  \"/users\" => list,\n    \
                     POST \"/users\" => create,",
                )
            })?;
        }
        Ok(Table { routes })
    }
}

/// Expand `routes! { … }` into the equivalent builder chain.
pub(crate) fn expand_routes(input: TokenStream) -> TokenStream {
    let table: Table = match syn::parse2(input) {
        Ok(table) => table,
        // A malformed table cannot produce a `Router`, and a second error
        // saying so would bury the first. `Router::new()` is the well-typed
        // placeholder that keeps `.tag("users")` from failing as well.
        Err(error) => {
            let error = error.to_compile_error();
            return quote! {{
                #error
                ::moso::__private::Router::new()
            }};
        }
    };

    let registrations = table.routes.iter().flat_map(|route| {
        let handler = &route.handler;
        let path = &route.path;
        let methods: Vec<&str> = if route.method == "ANY" {
            ANY_METHODS.to_vec()
        } else {
            vec![route.method.as_str()]
        };
        methods
            .into_iter()
            .map(|method| {
                let variant = Ident::new(&method_variant(method), route.method_span);
                quote! {
                    .endpoint::<#handler>(
                        ::moso::__private::HttpMethod::#variant,
                        ::moso::__private::route_path!(#path),
                    )
                }
            })
            .collect::<Vec<_>>()
    });

    quote! {
        ::moso::__private::Router::new()
            #(#registrations)*
    }
}

/// `GET` becomes `Get`: the spelling of `moso_openapi::path::HttpMethod`.
fn method_variant(method: &str) -> String {
    let mut chars = method.chars();
    let first: String = chars
        .by_ref()
        .take(1)
        .flat_map(char::to_uppercase)
        .collect();
    let rest: String = chars.flat_map(char::to_lowercase).collect();
    format!("{first}{rest}")
}

// ---------------------------------------------------------------------------
// ep!
// ---------------------------------------------------------------------------

/// Expand `ep!(list)` into the companion type's path, as a value.
///
/// The generated type is a unit struct, so its path *is* an expression. That is
/// what lets `Router::get("/users", ep!(list))` reach the
/// `Handler<EndpointMarker>` impl — the same one `routes!` uses, which is why
/// the two spellings produce the same OpenAPI document.
pub(crate) fn expand_ep(input: TokenStream) -> TokenStream {
    if let Some(error) = looks_like_a_route(&input) {
        return error.to_compile_error();
    }

    let path: Path = match syn::parse2(input) {
        Ok(path) => path,
        Err(error) => {
            let error = Error::new(
                error.span(),
                "expected a handler name\n\n\
                 help: write `ep!(list)`, or `ep!(users::list)` for a handler in another module",
            );
            return error.to_compile_error();
        }
    };

    match rewrite_path(&path) {
        Ok(rewritten) => quote!(#rewritten),
        Err(error) => error.to_compile_error(),
    }
}

/// Catch `ep!(GET "/healthz" => healthz)` and say what to write instead.
///
/// `ep!` names a *type*; a route needs a path as well, and `Router::route`
/// takes the path separately. Someone who reaches for the table syntax here is
/// one keystroke from the right answer, and a bare "expected a handler name"
/// would not tell them which one.
fn looks_like_a_route(input: &TokenStream) -> Option<Error> {
    let mut trees = input.clone().into_iter();
    let proc_macro2::TokenTree::Ident(first) = trees.next()? else {
        return None;
    };
    if !METHOD_NAMES.contains(&first.to_string().to_ascii_uppercase().as_str()) {
        return None;
    }
    if !matches!(trees.next()?, proc_macro2::TokenTree::Literal(_)) {
        return None;
    }
    Some(Error::new(
        first.span(),
        "`ep!` takes a handler name, not a whole route\n\n\
         note: `ep!` names the type `#[endpoint]` generated; the path belongs to the router\n\
         help: write `Router::new().get(\"/healthz\", ep!(healthz))`\n\
         help: for several routes use the table: \
         `routes! { GET \"/healthz\" => healthz }`",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    // ── the naming rule ───────────────────────────────────────────────────

    #[test]
    fn a_handler_name_becomes_its_companion_type() {
        let name: Ident = parse_quote!(list);
        assert_eq!(op_ident(&name).to_string(), "__moso_op_list");
    }

    #[test]
    fn a_raw_identifier_keeps_working() {
        let name: Ident = parse_quote!(r#type);
        assert_eq!(op_ident(&name).to_string(), "__moso_op_type");
    }

    fn rewritten(path: Path) -> String {
        let out = rewrite_path(&path).unwrap();
        squash(&quote!(#out).to_string())
    }

    #[test]
    fn a_bare_name_is_rewritten_in_place() {
        assert_eq!(rewritten(parse_quote!(list)), "__moso_op_list");
    }

    #[test]
    fn a_module_path_is_preserved() {
        assert_eq!(
            rewritten(parse_quote!(users::list)),
            "users::__moso_op_list"
        );
        assert_eq!(
            rewritten(parse_quote!(crate::routes::users::show)),
            "crate::routes::users::__moso_op_show"
        );
        assert_eq!(
            rewritten(parse_quote!(super::posts::publish)),
            "super::posts::__moso_op_publish"
        );
    }

    #[test]
    fn a_leading_colon_path_is_preserved() {
        assert_eq!(
            rewritten(parse_quote!(::blog::routes::list)),
            "::blog::routes::__moso_op_list"
        );
    }

    #[test]
    fn only_the_last_segment_is_rewritten() {
        let out = rewritten(parse_quote!(list::list::list));
        assert_eq!(out, "list::list::__moso_op_list");
        assert_eq!(out.matches("__moso_op_").count(), 1);
    }

    #[test]
    fn a_generic_handler_name_is_refused() {
        let path: Path = parse_quote!(list::<u32>);
        let error = rewrite_path(&path).unwrap_err().to_string();
        assert!(error.starts_with("a handler name may not carry generic arguments"));
    }

    // ── method spelling ───────────────────────────────────────────────────

    #[test]
    fn methods_map_onto_the_http_method_variants() {
        assert_eq!(method_variant("GET"), "Get");
        assert_eq!(method_variant("POST"), "Post");
        assert_eq!(method_variant("OPTIONS"), "Options");
        assert_eq!(method_variant("DELETE"), "Delete");
        assert_eq!(method_variant("TRACE"), "Trace");
    }

    // ── routes! ───────────────────────────────────────────────────────────

    /// A token stream's text with every space removed.
    ///
    /// `TokenStream::to_string` is free to put a space anywhere between two
    /// tokens, and does not promise where. Comparing the squashed text asserts
    /// what the expansion *is* rather than how `proc-macro2` chose to print it.
    fn expand(input: TokenStream) -> String {
        squash(&expand_routes(input).to_string())
    }

    /// The unsquashed expansion, for assertions about `compile_error!` prose.
    fn expand_raw(input: TokenStream) -> String {
        expand_routes(input).to_string()
    }

    fn squash(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn the_documented_table_expands_to_the_builder_chain() {
        let out = expand(quote! {
            GET    "/users"      => list,
            POST   "/users"      => create,
            GET    "/users/{id}" => show,
            PATCH  "/users/{id}" => update,
            DELETE "/users/{id}" => destroy,
        });
        assert!(out.starts_with("::moso::__private::Router::new()"));
        assert!(out.contains(
            ".endpoint::<__moso_op_list>(::moso::__private::HttpMethod::Get,\
             ::moso::__private::route_path!(\"/users\"),)"
        ));
        assert!(out.contains("__moso_op_create"));
        assert!(out.contains("__moso_op_destroy"));
        assert_eq!(out.matches(".endpoint::<").count(), 5);
        assert_eq!(out.matches("route_path!").count(), 5);
        assert!(!out.contains("compile_error"));
    }

    #[test]
    fn module_paths_survive_the_table() {
        let out = expand(quote! {
            GET "/users" => users::list,
            GET "/posts" => crate::routes::posts::list,
        });
        assert!(out.contains("users::__moso_op_list"));
        assert!(out.contains("crate::routes::posts::__moso_op_list"));
    }

    #[test]
    fn a_trailing_comma_is_optional() {
        let with = expand(quote!(GET "/users" => list,));
        let without = expand(quote!(GET "/users" => list));
        assert_eq!(with, without);
    }

    #[test]
    fn an_empty_table_is_an_empty_router() {
        assert_eq!(expand(quote!()), "::moso::__private::Router::new()");
    }

    #[test]
    fn every_method_is_accepted() {
        for method in [
            "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE",
        ] {
            let ident = Ident::new(method, Span::call_site());
            let out = expand(quote!(#ident "/x" => handler));
            assert!(!out.contains("compile_error"), "{method}");
            assert_eq!(out.matches(".endpoint::<").count(), 1, "{method}");
            let variant = method_variant(method);
            assert!(out.contains(&format!("HttpMethod::{variant}")), "{method}");
        }
    }

    #[test]
    fn any_registers_every_method_once() {
        let out = expand(quote!(ANY "/webhook" => receive));
        assert_eq!(out.matches(".endpoint::<").count(), ANY_METHODS.len());
        for method in ANY_METHODS {
            let variant = method_variant(method);
            assert!(out.contains(&format!("HttpMethod::{variant}")), "{method}");
        }
        // Every registration names the same endpoint and the same path.
        assert_eq!(out.matches("__moso_op_receive").count(), ANY_METHODS.len());
        assert_eq!(out.matches("\"/webhook\"").count(), ANY_METHODS.len());
    }

    #[test]
    fn methods_are_accepted_in_any_case() {
        let out = expand(quote!(get "/users" => list));
        assert!(out.contains("HttpMethod::Get"));
        assert!(!out.contains("compile_error"));
    }

    #[test]
    fn an_unknown_method_suggests_the_closest_one() {
        let out = expand_raw(quote!(GTE "/users" => list));
        assert_eq!(out.matches("compile_error").count(), 1);
        assert!(out.contains("unknown HTTP method `GTE`"));
        assert!(out.contains("help: did you mean `GET`?"));
        // The placeholder keeps a trailing `.tag(\"users\")` compiling.
        assert!(squash(&out).contains("::moso::__private::Router::new()"));
    }

    #[test]
    fn a_missing_arrow_is_one_error_with_a_fix() {
        let out = expand_raw(quote!(GET "/users" list));
        assert_eq!(out.matches("compile_error").count(), 1);
        assert!(out.contains("expected `=>` between the path and the handler"));
        assert!(out.contains("help: write a row as `GET \\\"/users\\\" => list`"));
    }

    #[test]
    fn an_unquoted_path_is_one_error_with_a_fix() {
        let out = expand_raw(quote!(GET /users => list));
        assert_eq!(out.matches("compile_error").count(), 1);
        assert!(out.contains("expected a path template in quotes"));
        assert!(out.contains("`{id}`, not `:id`"));
    }

    #[test]
    fn a_missing_comma_is_one_error_with_a_fix() {
        let out = expand_raw(quote! {
            GET "/users" => list
            POST "/users" => create
        });
        assert_eq!(out.matches("compile_error").count(), 1);
        assert!(out.contains("expected `,` between routes"));
    }

    #[test]
    fn a_whole_route_passed_to_ep_says_what_to_write_instead() {
        let out = expand_ep(quote!(GET "/healthz" => healthz)).to_string();
        assert_eq!(out.matches("compile_error").count(), 1);
        assert!(out.contains("`ep!` takes a handler name, not a whole route"));
        assert!(out.contains("ep!(healthz)"));
    }

    #[test]
    fn the_path_literal_reaches_route_path_verbatim() {
        let out = expand(quote!(GET "/users/{id}/posts/{slug}" => show));
        assert!(out.contains("route_path!(\"/users/{id}/posts/{slug}\")"));
    }

    // ── ep! ───────────────────────────────────────────────────────────────

    #[test]
    fn ep_rewrites_a_bare_name() {
        assert_eq!(
            squash(&expand_ep(quote!(list)).to_string()),
            "__moso_op_list"
        );
    }

    #[test]
    fn ep_preserves_a_module_path() {
        assert_eq!(
            squash(&expand_ep(quote!(users::list)).to_string()),
            "users::__moso_op_list"
        );
    }

    #[test]
    fn ep_refuses_something_that_is_not_a_name() {
        let out = expand_ep(quote!("list")).to_string();
        assert!(out.contains("compile_error"));
        assert!(out.contains("expected a handler name"));
    }

    #[test]
    fn ep_and_routes_agree_on_the_name() {
        let from_ep = squash(&expand_ep(quote!(users::list)).to_string());
        let from_routes = expand(quote!(GET "/users" => users::list));
        assert!(from_routes.contains(&from_ep));
    }
}
