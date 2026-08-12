//! `permissions!`, `roles!`, `#[requires]` and `#[public]` — the authorization
//! macros.
//!
//! # Why these are proc macros
//!
//! `posts.read = "View posts"` has to become a variant called `PostsRead`, and
//! `macro_rules!` cannot build an identifier out of pieces. Everything else
//! about them could have been declarative; the case conversion is what forces
//! the issue, and having forced it, the same pass also flattens role
//! inheritance and rejects a cycle by name.
//!
//! # What each one emits
//!
//! ```text
//! moso::permissions! { posts.read = "View posts", posts.publish = "Publish posts" }
//!
//! #[repr(u16)]
//! #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
//! pub enum Perm { PostsRead = 0, PostsPublish = 1 }
//!
//! impl Perm {
//!     pub const ALL: &'static [Perm] = &[Perm::PostsRead, Perm::PostsPublish];
//!     pub const NAMES: &'static [&'static str] = &["posts.read", "posts.publish"];
//!     pub const fn as_str(self) -> &'static str { Self::NAMES[self as usize] }
//!     pub const fn description(self) -> &'static str { /* … */ }
//!     pub const fn group(self) -> &'static str { /* … */ }
//!     pub fn parse(name: &str) -> Option<Perm> { /* … */ }
//! }
//!
//! const _: () = assert!(Perm::ALL.len() <= ::moso::__private::MAX_PERMISSIONS);
//! impl ::moso::__private::Permission for Perm { /* delegates to the consts */ }
//! ```
//!
//! ```text
//! moso::roles! {
//!     Viewer = [posts.read],
//!     Editor = Viewer + [posts.publish],
//! }
//!
//! #[repr(u8)]
//! pub enum Role { Viewer = 0, Editor = 1 }
//!
//! impl Role {
//!     // Inheritance is flattened *here*, so a static role's permissions are a
//!     // constant and resolving one at runtime costs nothing.
//!     pub const fn permissions(self) -> ::moso::__private::PermSet<Perm> { /* … */ }
//! }
//! ```
//!
//! ```text
//! #[requires(Perm::PostsCreate)]
//! #[endpoint]
//! async fn create(Json(body): Json<CreatePost>) -> Result<Created<PostOut>> { … }
//!
//! #[doc(hidden)] pub struct __moso_authz_create;
//! impl ::moso::__private::Requirement for __moso_authz_create {
//!     type Perm = Perm;
//!     const NAMES: &'static [&'static str] = &[Perm::PostsCreate.as_str()];
//! }
//!
//! #[endpoint]
//! async fn create(
//!     _moso_authz: ::moso::__private::Required<__moso_authz_create>,
//!     Json(body): Json<CreatePost>,
//! ) -> Result<Created<PostOut>> { … }
//! ```
//!
//! # Attribute order
//!
//! `#[requires]` and `#[public]` must be written **above** `#[endpoint]`. Rust
//! expands the outermost attribute first, so an `#[endpoint]` that ran already
//! has generated the companion type and the extraction glue against the
//! signature it saw; a parameter added afterwards would not be passed. Both
//! macros check for `#[endpoint]` below them and say so — with the corrected
//! order — when it is missing.

use std::collections::HashMap;

use proc_macro2::{Span, TokenStream};
use quote::{ToTokens, format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Error, Ident, ItemFn, LitStr, Path, Token, bracketed};

use crate::util::attrs::levenshtein;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// `posts` + `read` → `PostsRead`.
///
/// `heck` and not a hand-rolled loop, because `api_key` has to become `ApiKey`
/// and not `ApiKey` by accident.
fn variant_name(parts: &[&str]) -> String {
    use heck::ToUpperCamelCase as _;
    parts.join("_").to_upper_camel_case()
}

/// The closest candidate to `input`, when one is close enough.
///
/// The same budget the rest of Moso's diagnostics use, so `posts.pubish`
/// suggests `posts.publish` and `xyzzy` suggests nothing.
fn suggest(input: &str, candidates: &[String]) -> Option<String> {
    let budget = (input.chars().count() / 3).max(1) + 1;
    candidates
        .iter()
        .map(|candidate| (levenshtein(input, candidate), candidate))
        .filter(|(distance, _)| *distance <= budget)
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| candidate.clone())
}

/// The doc comment on a declaration, joined into one line.
fn doc_text(attrs: &[Attribute]) -> Option<String> {
    let lines: Vec<String> = attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .filter_map(|attr| match &attr.meta {
            syn::Meta::NameValue(nv) => match &nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(text),
                    ..
                }) => Some(text.value().trim().to_owned()),
                _ => None,
            },
            _ => None,
        })
        .filter(|line| !line.is_empty())
        .collect();
    (!lines.is_empty()).then(|| lines.join(" "))
}

// ---------------------------------------------------------------------------
// permissions!
// ---------------------------------------------------------------------------

/// One `posts.read = "View posts"` line.
struct PermissionEntry {
    /// Doc comments written above it, which become the variant's own docs.
    attrs: Vec<Attribute>,
    /// The part before the dot.
    group: Ident,
    /// The part after it.
    name: Ident,
    /// The human description.
    description: LitStr,
}

impl Parse for PermissionEntry {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let group: Ident = input.parse().map_err(|error| {
            Error::new(
                error.span(),
                "expected a permission, written `group.name = \"description\"`\n\n\
                 help: `moso::permissions! { posts.read = \"View posts\" }`",
            )
        })?;
        input.parse::<Token![.]>().map_err(|error| {
            Error::new(
                error.span(),
                "a permission is written `group.name`, with exactly one dot\n\n\
                 note: the group is what the admin's role editor renders as a section heading\n\
                 help: `posts.read = \"View posts\"`",
            )
        })?;
        let name: Ident = input.parse()?;
        input.parse::<Token![=]>()?;
        let description: LitStr = input.parse().map_err(|error| {
            Error::new(
                error.span(),
                "a permission needs a description, as a string literal\n\n\
                 note: the description is shown in the admin's role editor and in the 403 an \
                 endpoint documents, so it is not optional\n\
                 help: `posts.read = \"View posts\"`",
            )
        })?;
        Ok(Self {
            attrs,
            group,
            name,
            description,
        })
    }
}

/// The whole `permissions!` body.
struct PermissionsInput {
    /// Every declaration, in order. The order is the bit order.
    entries: Punctuated<PermissionEntry, Token![,]>,
}

impl Parse for PermissionsInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        Ok(Self {
            entries: Punctuated::parse_terminated(input)?,
        })
    }
}

/// Expand `moso::permissions! { … }`.
pub(crate) fn permissions(input: TokenStream) -> TokenStream {
    let parsed: PermissionsInput = match syn::parse2(input) {
        Ok(parsed) => parsed,
        Err(error) => return error.to_compile_error(),
    };

    let entries: Vec<&PermissionEntry> = parsed.entries.iter().collect();
    if entries.is_empty() {
        return Error::new(
            Span::call_site(),
            "`permissions!` needs at least one permission\n\n\
             note: an empty registry makes every `#[requires]` a compile error, which is not the \
             failure anyone wants to debug\n\
             help: `moso::permissions! { posts.read = \"View posts\" }`",
        )
        .to_compile_error();
    }

    let mut errors: Vec<Error> = Vec::new();
    let mut seen: HashMap<String, Span> = HashMap::new();
    for entry in &entries {
        let wire = format!("{}.{}", entry.group, entry.name);
        if let Some(first) = seen.get(&wire) {
            let mut error = Error::new(
                entry.name.span(),
                format!(
                    "`{wire}` is declared twice\n\n\
                     note: a permission's position in the list is its bit, so two entries with \
                     one name would give the same capability two bits and make a stored \
                     `PermSet` ambiguous\n\
                     help: remove one of them"
                ),
            );
            error.combine(Error::new(
                *first,
                format!("`{wire}` is first declared here"),
            ));
            errors.push(error);
            continue;
        }
        seen.insert(wire, entry.name.span());
    }

    if !errors.is_empty() {
        return combine(errors).to_compile_error();
    }

    let variants: Vec<Ident> = entries
        .iter()
        .map(|entry| {
            format_ident!(
                "{}",
                variant_name(&[&entry.group.to_string(), &entry.name.to_string()]),
                span = entry.name.span()
            )
        })
        .collect();
    let wires: Vec<String> = entries
        .iter()
        .map(|entry| format!("{}.{}", entry.group, entry.name))
        .collect();
    let descriptions: Vec<&LitStr> = entries.iter().map(|entry| &entry.description).collect();
    let groups: Vec<String> = entries
        .iter()
        .map(|entry| entry.group.to_string())
        .collect();
    let docs: Vec<TokenStream> = entries
        .iter()
        .zip(&wires)
        .zip(&descriptions)
        .map(|((entry, wire), description)| {
            let written = doc_text(&entry.attrs);
            let text = match written {
                Some(text) => format!("{text} — `{wire}`: {}", description.value()),
                None => format!("`{wire}` — {}", description.value()),
            };
            quote!(#[doc = #text])
        })
        .collect();

    let indices: Vec<u16> = (0..entries.len() as u16).collect();

    quote! {
        /// The application's permission registry, generated by `moso::permissions!`.
        ///
        /// A permission is a capability, not a row: the whole set is knowable at
        /// boot, which is what lets the admin render it, the OpenAPI document
        /// describe it, and a typo in `#[requires]` be caught instead of
        /// silently never matching.
        #[repr(u16)]
        #[derive(
            ::core::clone::Clone,
            ::core::marker::Copy,
            ::core::fmt::Debug,
            ::core::cmp::PartialEq,
            ::core::cmp::Eq,
            ::core::hash::Hash,
            ::core::cmp::PartialOrd,
            ::core::cmp::Ord,
        )]
        #[allow(unreachable_pub, dead_code)]
        pub enum Perm {
            #( #docs #variants = #indices, )*
        }

        #[allow(unreachable_pub, dead_code)]
        impl Perm {
            /// Every permission, in declaration order. Index `i` is `ALL[i]`,
            /// and that index is the bit a `PermSet` sets.
            pub const ALL: &'static [Perm] = &[ #( Perm::#variants ),* ];

            /// Every wire name, in the same order as [`Perm::ALL`].
            pub const NAMES: &'static [&'static str] = &[ #( #wires ),* ];

            /// Every description, in the same order as [`Perm::ALL`].
            pub const DESCRIPTIONS: &'static [&'static str] = &[ #( #descriptions ),* ];

            /// The wire name, e.g. `"posts.read"`.
            ///
            /// Inherent and `const`, which is what a `match` arm and a
            /// `#[requires]` expansion need; the `Permission` trait method
            /// delegates to it.
            pub const fn as_str(self) -> &'static str {
                Self::NAMES[self as usize]
            }

            /// The human description, from the declaration.
            pub const fn description(self) -> &'static str {
                Self::DESCRIPTIONS[self as usize]
            }

            /// The group — the part before the dot.
            pub const fn group(self) -> &'static str {
                match self {
                    #( Perm::#variants => #groups, )*
                }
            }

            /// Parse a wire name back into a permission, for a database round
            /// trip or an API key's stored scope list.
            pub fn parse(name: &str) -> ::core::option::Option<Perm> {
                match name {
                    #( #wires => ::core::option::Option::Some(Perm::#variants), )*
                    _ => ::core::option::Option::None,
                }
            }
        }

        // The cap is checked here rather than at the first `PermSet` operation,
        // so a registry that outgrew the bitset says so at the declaration.
        const _: () = ::core::assert!(
            Perm::ALL.len() <= ::moso::__private::MAX_PERMISSIONS,
            "a `permissions!` registry may declare at most 256 permissions; see \
             `moso_authz::MAX_PERMISSIONS`",
        );

        impl ::moso::__private::Permission for Perm {
            const ALL: &'static [Self] = Perm::ALL;
            const FINGERPRINT: u64 = ::moso::__private::fingerprint_of(Perm::NAMES);

            fn index(self) -> u16 {
                self as u16
            }

            fn from_index(index: u16) -> ::core::option::Option<Self> {
                match Perm::ALL.get(index as usize) {
                    ::core::option::Option::Some(permission) => {
                        ::core::option::Option::Some(*permission)
                    }
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }

            fn as_str(self) -> &'static str {
                Perm::as_str(self)
            }

            fn description(self) -> &'static str {
                Perm::description(self)
            }

            fn group(self) -> &'static str {
                Perm::group(self)
            }

            fn parse(name: &str) -> ::core::option::Option<Self> {
                Perm::parse(name)
            }
        }

        impl ::core::fmt::Display for Perm {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(Perm::as_str(*self))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// roles!
// ---------------------------------------------------------------------------

/// One `Editor = Viewer + [posts.create, posts.update]` line.
struct RoleEntry {
    /// Doc comments, which become the role's description.
    attrs: Vec<Attribute>,
    /// The variant name.
    name: Ident,
    /// Roles this one inherits from, in written order.
    parents: Vec<Ident>,
    /// Permissions granted directly, as `Perm` variant identifiers.
    grants: Vec<Ident>,
}

impl Parse for RoleEntry {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let name: Ident = input.parse().map_err(|error| {
            Error::new(
                error.span(),
                "expected a role, written `Name = [permissions]`\n\n\
                 help: `moso::roles! { Viewer = [posts.read] }`",
            )
        })?;
        input.parse::<Token![=]>()?;

        let mut parents = Vec::new();
        let mut grants = Vec::new();
        loop {
            if input.peek(syn::token::Bracket) {
                let content;
                bracketed!(content in input);
                let listed: Punctuated<PermissionRef, Token![,]> =
                    Punctuated::parse_terminated(&content)?;
                grants.extend(listed.into_iter().map(|reference| reference.variant));
            } else {
                parents.push(input.parse::<Ident>().map_err(|error| {
                    Error::new(
                        error.span(),
                        "expected a role to inherit from, or a `[…]` list of permissions\n\n\
                         help: `Editor = Viewer + [posts.create]`",
                    )
                })?);
            }
            if input.peek(Token![+]) {
                input.parse::<Token![+]>()?;
                continue;
            }
            break;
        }

        Ok(Self {
            attrs,
            name,
            parents,
            grants,
        })
    }
}

/// A `posts.read` inside a `roles!` list, resolved to a `Perm` variant.
struct PermissionRef {
    /// The variant identifier, e.g. `PostsRead`.
    variant: Ident,
}

impl Parse for PermissionRef {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let group: Ident = input.parse().map_err(|error| {
            Error::new(
                error.span(),
                "expected a permission, written `group.name`\n\n\
                 help: `Viewer = [posts.read, users.read]`",
            )
        })?;
        if input.peek(Token![.]) {
            input.parse::<Token![.]>()?;
            let name: Ident = input.parse()?;
            let variant = format_ident!(
                "{}",
                variant_name(&[&group.to_string(), &name.to_string()]),
                span = name.span()
            );
            return Ok(Self { variant });
        }
        // A bare `PostsRead`, for somebody who prefers the variant name.
        Ok(Self { variant: group })
    }
}

/// The whole `roles!` body.
struct RolesInput {
    /// Every role, in order. The order is the bit order.
    entries: Punctuated<RoleEntry, Token![,]>,
}

impl Parse for RolesInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        Ok(Self {
            entries: Punctuated::parse_terminated(input)?,
        })
    }
}

/// Expand `moso::roles! { … }`.
pub(crate) fn roles(input: TokenStream) -> TokenStream {
    let parsed: RolesInput = match syn::parse2(input) {
        Ok(parsed) => parsed,
        Err(error) => return error.to_compile_error(),
    };

    let entries: Vec<&RoleEntry> = parsed.entries.iter().collect();
    if entries.is_empty() {
        return Error::new(
            Span::call_site(),
            "`roles!` needs at least one role\n\n\
             help: `moso::roles! { Viewer = [posts.read] }`",
        )
        .to_compile_error();
    }

    let names: Vec<String> = entries.iter().map(|entry| entry.name.to_string()).collect();
    let mut errors: Vec<Error> = Vec::new();

    let mut seen: HashMap<String, Span> = HashMap::new();
    for entry in &entries {
        let name = entry.name.to_string();
        if let Some(first) = seen.get(&name) {
            let mut error = Error::new(
                entry.name.span(),
                format!("`{name}` is declared twice\n\n help: remove one of them"),
            );
            error.combine(Error::new(
                *first,
                format!("`{name}` is first declared here"),
            ));
            errors.push(error);
        } else {
            seen.insert(name, entry.name.span());
        }
    }

    // Unknown parents, with a suggestion. Reported before the cycle check, so a
    // typo does not also produce "cycle through an unknown role".
    for entry in &entries {
        for parent in &entry.parents {
            let parent_name = parent.to_string();
            if !names.contains(&parent_name) {
                let hint = match suggest(&parent_name, &names) {
                    Some(candidate) => format!("\nhelp: did you mean `{candidate}`?"),
                    None => format!("\nnote: the roles declared here are {}", names.join(", ")),
                };
                errors.push(Error::new(
                    parent.span(),
                    format!("`{parent_name}` is not a role in this registry{hint}"),
                ));
            }
        }
    }

    if !errors.is_empty() {
        return combine(errors).to_compile_error();
    }

    // Flatten inheritance. Doing it here is what makes a static role's
    // permissions a `const PermSet` and role resolution free at runtime.
    let by_name: HashMap<String, &RoleEntry> = entries
        .iter()
        .map(|entry| (entry.name.to_string(), *entry))
        .collect();
    let mut flattened: HashMap<String, Vec<Ident>> = HashMap::new();
    for entry in &entries {
        let mut path = Vec::new();
        let mut granted = Vec::new();
        if let Err(error) = flatten(entry, &by_name, &mut path, &mut granted) {
            errors.push(error);
        } else {
            flattened.insert(entry.name.to_string(), granted);
        }
    }

    if !errors.is_empty() {
        return combine(errors).to_compile_error();
    }

    let variants: Vec<&Ident> = entries.iter().map(|entry| &entry.name).collect();
    let wires: Vec<String> = names.iter().map(|name| name.to_lowercase()).collect();
    let descriptions: Vec<String> = entries
        .iter()
        .map(|entry| doc_text(&entry.attrs).unwrap_or_else(|| entry.name.to_string()))
        .collect();
    let docs: Vec<TokenStream> = entries
        .iter()
        .zip(&descriptions)
        .map(|(entry, description)| {
            let count = flattened
                .get(&entry.name.to_string())
                .map_or(0, |granted| granted.len());
            let text = format!("{description} — grants {count} permission(s).");
            quote!(#[doc = #text])
        })
        .collect();
    let sets: Vec<TokenStream> = entries
        .iter()
        .map(|entry| {
            let granted = &flattened[&entry.name.to_string()];
            quote! {
                ::moso::__private::PermSet::empty()
                    #( .with_index(Perm::#granted as u16) )*
            }
        })
        .collect();
    let indices: Vec<u8> = (0..entries.len() as u8).collect();

    quote! {
        /// The application's role registry, generated by `moso::roles!`.
        ///
        /// Inheritance is flattened at expansion time, so a role's permissions
        /// are a constant and resolving them costs a copy of four words.
        #[repr(u8)]
        #[derive(
            ::core::clone::Clone,
            ::core::marker::Copy,
            ::core::fmt::Debug,
            ::core::cmp::PartialEq,
            ::core::cmp::Eq,
            ::core::hash::Hash,
            ::core::cmp::PartialOrd,
            ::core::cmp::Ord,
        )]
        #[allow(unreachable_pub, dead_code)]
        pub enum Role {
            #( #docs #variants = #indices, )*
        }

        #[allow(unreachable_pub, dead_code)]
        impl Role {
            /// Every role, in declaration order. Index `i` is `ALL[i]`.
            pub const ALL: &'static [Role] = &[ #( Role::#variants ),* ];

            /// Every wire name, in the same order as [`Role::ALL`].
            pub const NAMES: &'static [&'static str] = &[ #( #wires ),* ];

            /// The permissions this role grants, inheritance included.
            ///
            /// `const`, which is the whole point: resolving a static role is a
            /// copy, not a graph walk.
            pub const fn permissions(self) -> ::moso::__private::PermSet<Perm> {
                match self {
                    #( Role::#variants => #sets, )*
                }
            }

            /// The wire name, e.g. `"editor"`.
            pub const fn as_str(self) -> &'static str {
                Self::NAMES[self as usize]
            }

            /// The human description, from the doc comment.
            pub const fn description(self) -> &'static str {
                match self {
                    #( Role::#variants => #descriptions, )*
                }
            }

            /// Parse a wire name back into a role, for a database round trip.
            pub fn parse(name: &str) -> ::core::option::Option<Role> {
                match name {
                    #( #wires => ::core::option::Option::Some(Role::#variants), )*
                    _ => ::core::option::Option::None,
                }
            }
        }

        const _: () = ::core::assert!(
            Role::ALL.len() <= ::moso::__private::MAX_ROLES,
            "a `roles!` registry may declare at most 64 static roles; customer-defined roles go \
             through `RoleSource`, which is not bounded by this",
        );

        impl ::moso::__private::Role for Role {
            type Perm = Perm;

            const ALL: &'static [Self] = Role::ALL;

            fn index(self) -> u8 {
                self as u8
            }

            fn from_index(index: u8) -> ::core::option::Option<Self> {
                match Role::ALL.get(index as usize) {
                    ::core::option::Option::Some(role) => ::core::option::Option::Some(*role),
                    ::core::option::Option::None => ::core::option::Option::None,
                }
            }

            fn as_str(self) -> &'static str {
                Role::as_str(self)
            }

            fn description(self) -> &'static str {
                Role::description(self)
            }

            fn permissions(self) -> ::moso::__private::PermSet<Perm> {
                Role::permissions(self)
            }

            fn parse(name: &str) -> ::core::option::Option<Self> {
                Role::parse(name)
            }
        }

        impl ::core::fmt::Display for Role {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(Role::as_str(*self))
            }
        }
    }
}

/// Collect a role's permissions, following inheritance, rejecting a cycle.
///
/// The error names *both* roles in the cycle and prints the path, because
/// "cycle detected" without the path is a puzzle rather than a diagnosis.
fn flatten(
    entry: &RoleEntry,
    by_name: &HashMap<String, &RoleEntry>,
    path: &mut Vec<String>,
    granted: &mut Vec<Ident>,
) -> Result<(), Error> {
    let name = entry.name.to_string();
    if path.contains(&name) {
        path.push(name.clone());
        return Err(Error::new(
            entry.name.span(),
            format!(
                "`{name}` inherits from itself: {}\n\n\
                 note: inheritance is flattened at expansion time, so a cycle has no fixed \
                 point and cannot be resolved into a constant\n\
                 help: break the loop — a role that needs another's permissions can list them \
                 directly instead",
                path.join(" → ")
            ),
        ));
    }
    path.push(name);

    for parent in &entry.parents {
        let parent_entry = by_name
            .get(&parent.to_string())
            .expect("unknown parents are reported before flattening");
        flatten(parent_entry, by_name, path, granted)?;
    }
    for permission in &entry.grants {
        if !granted.iter().any(|held| held == permission) {
            granted.push(permission.clone());
        }
    }

    path.pop();
    Ok(())
}

// ---------------------------------------------------------------------------
// #[requires] and #[public]
// ---------------------------------------------------------------------------

/// One `#[requires(..)]` declaration, parsed.
#[derive(Default)]
struct RequiresArgs {
    /// Each permission, as the tokens that produce its wire name.
    names: Vec<TokenStream>,
    /// The permission type, inferred from the first `Perm::Variant` written.
    perm_type: Option<Path>,
    /// Whether any of them is enough.
    any: bool,
    /// Whether an allow is audited too.
    audit: bool,
    /// How many permissions were named, for the "at least one" check.
    count: usize,
}

/// Expand `#[requires(..)]` over a handler.
pub(crate) fn requires(args: TokenStream, item: TokenStream) -> TokenStream {
    expand_over_endpoint(args, item, "requires", |args, func| {
        let parsed = match parse_requires(args) {
            Ok(parsed) => parsed,
            Err(error) => return Err(error),
        };
        if parsed.count == 0 {
            return Err(Error::new(
                Span::call_site(),
                "`#[requires]` needs at least one permission\n\n\
                 note: an empty requirement is satisfied by everybody, which is what `#[public]` \
                 says on purpose\n\
                 help: `#[requires(Perm::PostsCreate)]`, or `#[public]` if the endpoint is meant \
                 to be open",
            ));
        }

        let marker = format_ident!("__moso_authz_{}", func.sig.ident);
        let perm = parsed
            .perm_type
            .clone()
            .unwrap_or_else(|| syn::parse_quote!(Perm));
        let names = &parsed.names;
        let mode = if parsed.any {
            quote!(::moso::__private::RequireMode::Any)
        } else {
            quote!(::moso::__private::RequireMode::All)
        };
        let audit = parsed.audit;
        let doc = format!(
            "The `#[requires]` declaration on `{}`, generated by `#[requires]`.",
            func.sig.ident
        );

        Ok((
            quote! {
                #[doc = #doc]
                #[doc(hidden)]
                #[allow(non_camel_case_types, unreachable_pub, dead_code)]
                #[derive(
                    ::core::clone::Clone,
                    ::core::marker::Copy,
                    ::core::fmt::Debug,
                    ::core::default::Default,
                )]
                pub struct #marker;

                impl ::moso::__private::Requirement for #marker {
                    type Perm = #perm;
                    const NAMES: &'static [&'static str] = &[ #( #names ),* ];
                    const MODE: ::moso::__private::RequireMode = #mode;
                    const AUDIT: bool = #audit;
                }
            },
            quote!(::moso::__private::Required<#marker>),
        ))
    })
}

/// Expand `#[public]` over a handler.
pub(crate) fn public(args: TokenStream, item: TokenStream) -> TokenStream {
    if !args.is_empty() {
        return join(
            item,
            Error::new_spanned(
                args,
                "`#[public]` takes no arguments\n\n\
                 note: it means \"this endpoint needs no authorization, and that was a decision\"\n\
                 help: write it bare — `#[public]`",
            )
            .to_compile_error(),
        );
    }
    expand_over_endpoint(args, item, "public", |_args, _func| {
        Ok((TokenStream::new(), quote!(::moso::__private::Public)))
    })
}

/// The shared shape of `#[requires]` and `#[public]`.
///
/// Both check that `#[endpoint]` is still below them, emit some items, and
/// prepend one parameter to the handler. Prepending rather than appending
/// matters: the *last* parameter is the one `#[endpoint]` treats as the body
/// extractor, and stealing that position would break every handler that reads
/// a body.
fn expand_over_endpoint(
    args: TokenStream,
    item: TokenStream,
    attribute: &str,
    build: impl FnOnce(TokenStream, &ItemFn) -> Result<(TokenStream, TokenStream), Error>,
) -> TokenStream {
    let mut func: ItemFn = match syn::parse2(item.clone()) {
        Ok(func) => func,
        Err(error) => {
            return join(
                item,
                Error::new(
                    error.span(),
                    format!(
                        "`#[{attribute}]` may only be applied to a handler `async fn`\n\n\
                         help: move it onto the handler, above `#[endpoint]`:\n    \
                         #[{attribute}]\n    #[endpoint]\n    async fn create() -> \
                         Result<NoContent> {{ /* … */ }}"
                    ),
                )
                .to_compile_error(),
            );
        }
    };

    if !func.attrs.iter().any(is_endpoint) {
        return join(
            func.to_token_stream(),
            Error::new(
                func.sig.ident.span(),
                format!(
                    "`#[{attribute}]` must be written *above* `#[endpoint]`\n\n\
                     note: Rust expands the outermost attribute first, so by the time \
                     `#[{attribute}]` sees this function `#[endpoint]` has already generated the \
                     extraction glue for the signature it saw — a check added afterwards would \
                     never run\n\
                     help: swap the two attributes:\n    \
                     #[{attribute}]\n    #[endpoint]\n    async fn {name}(/* … */)",
                    name = func.sig.ident,
                ),
            )
            .to_compile_error(),
        );
    }

    let (items, parameter) = match build(args, &func) {
        Ok(built) => built,
        Err(error) => return join(func.to_token_stream(), error.to_compile_error()),
    };

    let binding = format_ident!("_moso_{attribute}");
    let injected: syn::FnArg = syn::parse_quote!(#binding: #parameter);
    func.sig.inputs.insert(0, injected);

    let mut out = items;
    out.extend(func.to_token_stream());
    out
}

/// Whether an attribute is `#[endpoint]`, however it is spelt.
fn is_endpoint(attr: &Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "endpoint")
}

/// Parse `#[requires(Perm::A, any(Perm::B, Perm::C), audit)]`.
fn parse_requires(args: TokenStream) -> Result<RequiresArgs, Error> {
    struct Args(Punctuated<syn::Expr, Token![,]>);

    impl Parse for Args {
        fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
            Ok(Self(Punctuated::parse_terminated(input)?))
        }
    }

    let parsed: Args = syn::parse2(args).map_err(|error| {
        Error::new(
            error.span(),
            "`#[requires]` takes a comma-separated list of permissions\n\n\
             help: `#[requires(Perm::PostsCreate)]`\n\
             help: several, all needed — `#[requires(Perm::PostsRead, Perm::PostsUpdate)]`\n\
             help: several, any one enough — `#[requires(any(Perm::PostsRead, Perm::AdminAccess))]`\n\
             help: audit the allows too — `#[requires(Perm::UsersSuspend, audit)]`",
        )
    })?;

    let mut out = RequiresArgs::default();
    for expr in parsed.0 {
        match expr {
            // `audit`
            syn::Expr::Path(path) if path.path.is_ident("audit") && path.qself.is_none() => {
                out.audit = true;
            }
            // `any(a, b)`
            syn::Expr::Call(call) if is_named_call(&call, "any") => {
                out.any = true;
                for inner in call.args {
                    collect_permission(inner, &mut out)?;
                }
            }
            // `all(a, b)`, for symmetry with `any`
            syn::Expr::Call(call) if is_named_call(&call, "all") => {
                for inner in call.args {
                    collect_permission(inner, &mut out)?;
                }
            }
            other => collect_permission(other, &mut out)?,
        }
    }
    Ok(out)
}

/// Whether a call expression is `name(..)`.
fn is_named_call(call: &syn::ExprCall, name: &str) -> bool {
    matches!(&*call.func, syn::Expr::Path(path) if path.path.is_ident(name))
}

/// Record one permission, in either the typed or the string form.
fn collect_permission(expr: syn::Expr, out: &mut RequiresArgs) -> Result<(), Error> {
    match expr {
        syn::Expr::Path(path) => {
            // `Perm::PostsCreate` — the type is everything but the last segment.
            if path.path.segments.len() >= 2 && out.perm_type.is_none() {
                let mut owner = path.path.clone();
                owner.segments.pop();
                // `pop` leaves a trailing `::`, which does not parse as a type.
                if let Some(pair) = owner.segments.pop() {
                    owner.segments.push_value(pair.into_value());
                }
                out.perm_type = Some(owner);
            }
            let value = &path;
            out.names.push(quote!(#value.as_str()));
            out.count += 1;
            Ok(())
        }
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(name),
            ..
        }) => {
            out.names.push(quote!(#name));
            out.count += 1;
            Ok(())
        }
        other => Err(Error::new(
            other.span(),
            "expected a permission — an enum variant or its wire name\n\n\
             note: the enum form is compile-checked; the string form is checked against the \
             registry at boot, with a \"did you mean\"\n\
             help: `#[requires(Perm::PostsCreate)]` or `#[requires(\"posts.create\")]`",
        )),
    }
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// Fold several errors into one, so a mistake produces one message per mistake.
fn combine(errors: Vec<Error>) -> Error {
    let mut iterator = errors.into_iter();
    let mut first = iterator.next().expect("combine is only called with errors");
    for error in iterator {
        first.combine(error);
    }
    first
}

/// Concatenate two token streams.
fn join(first: TokenStream, second: TokenStream) -> TokenStream {
    let mut out = first;
    out.extend(second);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expanded(input: proc_macro2::TokenStream) -> String {
        permissions(input).to_string()
    }

    #[test]
    fn a_dotted_permission_becomes_an_upper_camel_variant() {
        let out = expanded(quote!(
            posts.read = "View posts",
            api.key_rotate = "Rotate keys"
        ));
        assert!(out.contains("PostsRead"), "{out}");
        assert!(out.contains("ApiKeyRotate"), "{out}");
        assert!(out.contains("\"api.key_rotate\""), "{out}");
    }

    #[test]
    fn the_variant_order_is_the_bit_order() {
        let out = expanded(quote!(a.one = "1", a.two = "2", b.three = "3"));
        let first = out.find("AOne").expect("first variant");
        let second = out.find("ATwo").expect("second variant");
        let third = out.find("BThree").expect("third variant");
        assert!(first < second && second < third, "{out}");
    }

    #[test]
    fn a_duplicate_permission_is_rejected_by_name() {
        let out = expanded(quote!(posts.read = "a", posts.read = "b"));
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("declared twice"), "{out}");
    }

    #[test]
    fn an_empty_registry_is_rejected() {
        let out = expanded(quote!());
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("at least one permission"), "{out}");
    }

    #[test]
    fn a_permission_without_a_dot_says_what_the_shape_is() {
        let out = expanded(quote!(read = "View"));
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("exactly one dot"), "{out}");
    }

    #[test]
    fn a_permission_without_a_description_says_why_it_is_required() {
        let out = expanded(quote!(posts.read =));
        assert!(out.contains("compile_error"), "{out}");
    }

    #[test]
    fn role_inheritance_is_flattened_at_expansion_time() {
        let out = roles(quote!(
            Viewer = [posts.read],
            Editor = Viewer + [posts.publish],
        ))
        .to_string();

        // `Editor` carries both bits, without any runtime graph walk.
        assert!(out.contains("PostsRead"), "{out}");
        assert!(out.contains("PostsPublish"), "{out}");
        assert!(!out.contains("parents"), "{out}");
    }

    #[test]
    fn a_role_cycle_names_both_roles_and_the_path() {
        let out = roles(quote!(A = B + [x.one], B = A + [x.two])).to_string();
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("inherits from itself"), "{out}");
    }

    #[test]
    fn an_unknown_parent_role_is_suggested() {
        let out = roles(quote!(Viewer = [x.one], Editor = Vewier + [x.two])).to_string();
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("did you mean `Viewer`"), "{out}");
    }

    #[test]
    fn a_role_name_becomes_its_lowercase_wire_name() {
        let out = roles(quote!(Owner = [x.one])).to_string();
        assert!(out.contains("\"owner\""), "{out}");
    }

    #[test]
    fn requires_must_sit_above_endpoint() {
        let out = requires(
            quote!(Perm::PostsCreate),
            quote!(
                async fn create() -> Result<()> {
                    Ok(())
                }
            ),
        )
        .to_string();

        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("above"), "{out}");
    }

    #[test]
    fn requires_injects_the_extractor_as_the_first_parameter() {
        let out = requires(
            quote!(Perm::PostsCreate),
            quote!(
                #[endpoint]
                async fn create(body: Json<CreatePost>) -> Result<()> {
                    Ok(())
                }
            ),
        )
        .to_string();

        assert!(out.contains("__moso_authz_create"), "{out}");
        assert!(out.contains("Required"), "{out}");
        // The body extractor has to stay last, which is why we prepend.
        let injected = out.find("_moso_requires").expect("injected parameter");
        let body = out.find("body").expect("original parameter");
        assert!(injected < body, "{out}");
    }

    #[test]
    fn requires_infers_the_permission_type_from_the_variant_path() {
        let out = requires(
            quote!(app::authz::Perm::PostsCreate),
            quote!(
                #[endpoint]
                async fn create() -> Result<()> {
                    Ok(())
                }
            ),
        )
        .to_string();

        assert!(out.contains("type Perm = app :: authz :: Perm"), "{out}");
    }

    #[test]
    fn any_sets_the_mode_and_audit_sets_the_flag() {
        let out = requires(
            quote!(any(Perm::A, Perm::B), audit),
            quote!(
                #[endpoint]
                async fn create() -> Result<()> {
                    Ok(())
                }
            ),
        )
        .to_string();

        assert!(out.contains("RequireMode :: Any"), "{out}");
        assert!(out.contains("const AUDIT : bool = true"), "{out}");
    }

    #[test]
    fn an_empty_requires_points_at_public() {
        let out = requires(
            quote!(),
            quote!(
                #[endpoint]
                async fn create() -> Result<()> {
                    Ok(())
                }
            ),
        )
        .to_string();

        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("#[public]"), "{out}");
    }

    #[test]
    fn the_string_form_is_kept_verbatim_for_the_boot_check() {
        let out = requires(
            quote!("posts.pubish"),
            quote!(
                #[endpoint]
                async fn create() -> Result<()> {
                    Ok(())
                }
            ),
        )
        .to_string();

        assert!(out.contains("\"posts.pubish\""), "{out}");
    }

    #[test]
    fn public_injects_its_marker_and_takes_no_arguments() {
        let good = public(
            quote!(),
            quote!(
                #[endpoint]
                async fn health() -> Result<()> {
                    Ok(())
                }
            ),
        )
        .to_string();
        assert!(good.contains("Public"), "{good}");
        assert!(!good.contains("compile_error"), "{good}");

        let bad = public(
            quote!(Perm::PostsRead),
            quote!(
                #[endpoint]
                async fn health() -> Result<()> {
                    Ok(())
                }
            ),
        )
        .to_string();
        assert!(bad.contains("compile_error"), "{bad}");
    }

    #[test]
    fn suggestions_are_offered_only_when_they_are_plausible() {
        let candidates = vec!["posts.publish".to_owned(), "admin.access".to_owned()];
        assert_eq!(
            suggest("posts.pubish", &candidates).as_deref(),
            Some("posts.publish")
        );
        assert_eq!(suggest("completely.unrelated", &candidates), None);
    }
}
