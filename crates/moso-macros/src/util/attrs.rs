#![allow(
    dead_code,
    reason = "a shared toolkit: every macro in this crate uses a different subset, and an \
              unused helper is not a defect"
)]
//! Shared attribute parsing, diagnostics and span helpers.
//!
//! Every macro in this crate parses attributes the same way and reports
//! mistakes in the same shape, because a user does not care which macro was
//! responsible for a message they are reading at 2am. The rules are the ones in
//! `docs/04-devex/41-diagnostics.md`:
//!
//! * plain language, no jargon, no blame;
//! * the span points at the **user's** token, never at generated code;
//! * every error carries a `help:` line that is code the user can paste;
//! * an unknown key gets a "did you mean" suggestion computed with
//!   [`did_you_mean`];
//! * one error per mistake — accumulate with [`Diagnostics`] and emit them all
//!   at once, next to a well-typed placeholder, so downstream code does not
//!   cascade.
//!
//! # Using it from a macro
//!
//! These are `pub(crate)` items, so this is a sketch rather than a doctest:
//!
//! ```text
//! let mut errors = Diagnostics::new();
//! let spec = parse_container(&input, &mut errors);
//! let expansion = expand(&spec);
//! errors.finish(expansion)      // compile errors first, placeholder after
//! ```
//!
//! [`Diagnostics`] is built on [`syn::Error`] because that is what carries a
//! span through to rustc unchanged, and converts both ways with
//! [`darling::Error`] ([`Diagnostics::into_darling`],
//! [`Diagnostics::push_darling`]) so a macro that parses with `darling` derives
//! can pour its errors into the same accumulator.

use std::fmt::Display;

use proc_macro2::{Span, TokenStream};
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::{
    Attribute, Expr, ExprLit, ExprPath, GenericArgument, Lit, Meta, Path, PathArguments, Type,
};

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// An accumulator for compile errors.
///
/// Collecting rather than bailing is the whole point: a struct with three
/// misspelled attributes should report three mistakes, not the first one three
/// times over three edit-compile cycles.
#[derive(Default)]
pub(crate) struct Diagnostics {
    errors: Vec<syn::Error>,
}

impl Diagnostics {
    /// An empty accumulator.
    pub(crate) fn new() -> Self {
        Self { errors: Vec::new() }
    }

    /// True when nothing has gone wrong.
    pub(crate) fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// How many distinct mistakes have been recorded.
    pub(crate) fn len(&self) -> usize {
        self.errors.len()
    }

    /// Record a ready-made [`syn::Error`].
    pub(crate) fn push(&mut self, error: syn::Error) {
        self.errors.push(error);
    }

    /// Record `message` at `span`.
    pub(crate) fn error(&mut self, span: Span, message: impl Display) {
        self.errors.push(syn::Error::new(span, message.to_string()));
    }

    /// Record `message` at the span of a syntax node.
    pub(crate) fn error_at(&mut self, node: &impl ToTokens, message: impl Display) {
        self.errors
            .push(syn::Error::new_spanned(node, message.to_string()));
    }

    /// Record `message` at `span` with one or more `help:` lines.
    ///
    /// The style guide requires a fix with every error, so this — not
    /// [`Diagnostics::error`] — is the common case.
    pub(crate) fn help(&mut self, span: Span, message: impl Display, help: impl Display) {
        self.errors.push(syn::Error::new(
            span,
            with_helps(&message, &[help.to_string()]),
        ));
    }

    /// Record `message` at the span of a syntax node, with a `help:` line.
    pub(crate) fn help_at(
        &mut self,
        node: &impl ToTokens,
        message: impl Display,
        help: impl Display,
    ) {
        self.errors.push(syn::Error::new_spanned(
            node,
            with_helps(&message, &[help.to_string()]),
        ));
    }

    /// Record `message` at `span` with several `help:` lines.
    pub(crate) fn helps(&mut self, span: Span, message: impl Display, helps: &[String]) {
        self.errors
            .push(syn::Error::new(span, with_helps(&message, helps)));
    }

    /// Report an unrecognised attribute key, suggesting the closest known one.
    ///
    /// `attribute` is the outer attribute's name (`schema`, `endpoint`, …) and
    /// `known` is its full vocabulary, which is also printed when no suggestion
    /// is close enough to be useful.
    pub(crate) fn unknown_key(&mut self, span: Span, attribute: &str, key: &str, known: &[&str]) {
        let message = format!("unknown `{attribute}` attribute `{key}`");
        match did_you_mean(key, known) {
            Some(suggestion) => self.help(span, message, format!("did you mean `{suggestion}`?")),
            None => self.help(
                span,
                message,
                format!("the accepted keys are: {}", list(known)),
            ),
        }
    }

    /// Report a key that is known but not allowed in this position.
    pub(crate) fn misplaced_key(&mut self, span: Span, key: &str, allowed_on: &str, help: &str) {
        self.help(
            span,
            format!("`{key}` cannot be used here; it applies to {allowed_on}"),
            help,
        );
    }

    /// Report a key given twice.
    pub(crate) fn duplicate_key(&mut self, span: Span, key: &str) {
        self.help(
            span,
            format!("`{key}` is set twice"),
            format!("remove one of the two `{key}` entries"),
        );
    }

    /// Absorb a [`darling::Error`], preserving its span.
    pub(crate) fn push_darling(&mut self, error: darling::Error) {
        self.errors.push(error.into());
    }

    /// Every recorded error, combined into one [`syn::Error`].
    pub(crate) fn into_error(self) -> Option<syn::Error> {
        let mut iter = self.errors.into_iter();
        let mut first = iter.next()?;
        for error in iter {
            first.combine(error);
        }
        Some(first)
    }

    /// Every recorded error as a [`darling::Error`], for callers that speak
    /// darling's dialect.
    pub(crate) fn into_darling(self) -> Option<darling::Error> {
        if self.errors.is_empty() {
            return None;
        }
        Some(darling::Error::multiple(
            self.errors.into_iter().map(darling::Error::from).collect(),
        ))
    }

    /// Every recorded error as `compile_error!` invocations.
    pub(crate) fn into_compile_errors(self) -> Option<TokenStream> {
        self.into_error().map(|e| e.into_compile_error())
    }

    /// The macro's final output: the errors, then `expansion`.
    ///
    /// `expansion` should be a *well-typed placeholder* when parsing failed —
    /// the impls the user's downstream code expects, with stub bodies — so one
    /// mistake produces one error instead of a cascade of "trait not
    /// implemented" errors at every use site.
    pub(crate) fn finish(self, expansion: TokenStream) -> TokenStream {
        match self.into_compile_errors() {
            Some(errors) => {
                let mut out = errors;
                out.extend(expansion);
                out
            }
            None => expansion,
        }
    }
}

impl Extend<syn::Error> for Diagnostics {
    fn extend<T: IntoIterator<Item = syn::Error>>(&mut self, iter: T) {
        self.errors.extend(iter);
    }
}

impl From<syn::Error> for Diagnostics {
    fn from(error: syn::Error) -> Self {
        Self {
            errors: vec![error],
        }
    }
}

impl From<darling::Error> for Diagnostics {
    fn from(error: darling::Error) -> Self {
        Self {
            errors: error.into_iter().map(Into::into).collect(),
        }
    }
}

/// Format a message with its `help:` lines.
///
/// rustc prints a multi-line macro error verbatim under the span, so the shape
/// in `41-diagnostics.md` is reproduced by putting the helps in the message
/// itself; a proc macro on stable has no other way to attach them.
fn with_helps(message: &impl Display, helps: &[String]) -> String {
    let mut out = message.to_string();
    for help in helps {
        out.push_str("\n\nhelp: ");
        out.push_str(help);
    }
    out
}

/// `` `a`, `b` or `c` `` — a human list of accepted keys.
pub(crate) fn list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => format!("`{only}`"),
        [rest @ .., last] => {
            let mut out = String::with_capacity(items.len() * 10);
            for (index, item) in rest.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push('`');
                out.push_str(item);
                out.push('`');
            }
            out.push_str(" or `");
            out.push_str(last);
            out.push('`');
            out
        }
    }
}

// ---------------------------------------------------------------------------
// "did you mean"
// ---------------------------------------------------------------------------

/// The closest entry in `options` to `input`, if one is close enough to be a
/// plausible typo.
///
/// The threshold scales with the length of the input: a three-letter key gets
/// one edit of slack, a twelve-letter key gets four. A suggestion that is
/// further away than that is noise, and the caller prints the whole vocabulary
/// instead.
pub(crate) fn did_you_mean<'a>(input: &str, options: &[&'a str]) -> Option<&'a str> {
    let lowered = input.to_ascii_lowercase();
    let budget = (input.chars().count() / 3).max(1);
    options
        .iter()
        .copied()
        .filter_map(|candidate| {
            let folded = candidate.to_ascii_lowercase();
            // A shared prefix beats edit distance: `lenght` is three edits from
            // `len`, and `len` is still obviously what was meant.
            if folded.len() >= 3 && (lowered.starts_with(&folded) || folded.starts_with(&lowered)) {
                return Some((0, candidate));
            }
            let distance = levenshtein(&lowered, &folded);
            (distance <= budget).then_some((distance, candidate))
        })
        .min_by(|(a, left), (b, right)| a.cmp(b).then_with(|| left.len().cmp(&right.len())))
        .map(|(_, candidate)| candidate)
}

/// Damerau-Levenshtein edit distance (optimal string alignment).
///
/// Transpositions count as one edit rather than two, which matters: `ragne` for
/// `range` is one slip of the fingers, and a metric that scores it as two
/// refuses to suggest the obvious fix.
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    if a_chars.is_empty() {
        return b_chars.len();
    }
    if b_chars.is_empty() {
        return a_chars.len();
    }

    // Three rows: the one before last is what makes a transposition cost one.
    let mut before_last = vec![0usize; b_chars.len() + 1];
    let mut previous: Vec<usize> = (0..=b_chars.len()).collect();
    let mut current = vec![0usize; b_chars.len() + 1];

    for (i, &a_char) in a_chars.iter().enumerate() {
        current[0] = i + 1;
        for (j, &b_char) in b_chars.iter().enumerate() {
            let cost = usize::from(a_char != b_char);
            let mut best = (previous[j] + cost)
                .min(current[j] + 1)
                .min(previous[j + 1] + 1);
            if i > 0 && j > 0 && a_char == b_chars[j - 1] && a_chars[i - 1] == b_char {
                best = best.min(before_last[j - 1] + 1);
            }
            current[j + 1] = best;
        }
        std::mem::swap(&mut before_last, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }

    previous[b_chars.len()]
}

// ---------------------------------------------------------------------------
// Doc comments
// ---------------------------------------------------------------------------

/// The `///` lines on an item, with the leading space of each line removed.
pub(crate) fn doc_lines(attrs: &[Attribute]) -> Vec<String> {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let Meta::NameValue(nv) = &attr.meta else {
            continue;
        };
        let Expr::Lit(ExprLit {
            lit: Lit::Str(text),
            ..
        }) = &nv.value
        else {
            continue;
        };
        let value = text.value();
        lines.push(value.strip_prefix(' ').unwrap_or(&value).to_owned());
    }
    lines
}

/// The whole doc comment as one string, or `None` when there is none.
pub(crate) fn doc_text(attrs: &[Attribute]) -> Option<String> {
    let lines = doc_lines(attrs);
    let text = lines.join("\n");
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The first paragraph of the doc comment, and everything after it.
///
/// `#[endpoint]` maps the pair onto OpenAPI's `summary` and `description`;
/// `#[derive(Schema)]` uses the whole text as a `description`.
pub(crate) fn doc_summary_and_description(attrs: &[Attribute]) -> (Option<String>, Option<String>) {
    let lines = doc_lines(attrs);
    let mut summary: Vec<&str> = Vec::new();
    let mut rest: Vec<&str> = Vec::new();
    let mut in_summary = true;

    for line in &lines {
        if in_summary {
            if line.trim().is_empty() {
                if summary.is_empty() {
                    continue;
                }
                in_summary = false;
                continue;
            }
            summary.push(line);
        } else {
            rest.push(line);
        }
    }

    let summary = summary.join(" ").trim().to_owned();
    let rest = rest.join("\n").trim().to_owned();
    (
        (!summary.is_empty()).then_some(summary),
        (!rest.is_empty()).then_some(rest),
    )
}

// ---------------------------------------------------------------------------
// rename_all
// ---------------------------------------------------------------------------

/// The case conventions `rename_all` accepts, spelled exactly as serde spells
/// them so a user does not have to learn a second vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RenameRule {
    /// `lowercase`
    Lower,
    /// `UPPERCASE`
    Upper,
    /// `PascalCase`
    Pascal,
    /// `camelCase`
    Camel,
    /// `snake_case`
    Snake,
    /// `SCREAMING_SNAKE_CASE`
    ScreamingSnake,
    /// `kebab-case`
    Kebab,
    /// `SCREAMING-KEBAB-CASE`
    ScreamingKebab,
}

impl RenameRule {
    /// Every accepted spelling, for the "did you mean" list.
    pub(crate) const NAMES: &'static [&'static str] = &[
        "lowercase",
        "UPPERCASE",
        "PascalCase",
        "camelCase",
        "snake_case",
        "SCREAMING_SNAKE_CASE",
        "kebab-case",
        "SCREAMING-KEBAB-CASE",
    ];

    /// Parse one of [`RenameRule::NAMES`].
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "lowercase" => Self::Lower,
            "UPPERCASE" => Self::Upper,
            "PascalCase" => Self::Pascal,
            "camelCase" => Self::Camel,
            "snake_case" => Self::Snake,
            "SCREAMING_SNAKE_CASE" => Self::ScreamingSnake,
            "kebab-case" => Self::Kebab,
            "SCREAMING-KEBAB-CASE" => Self::ScreamingKebab,
            _ => return None,
        })
    }

    /// Apply the rule to an identifier.
    ///
    /// Works from either direction — `snake_case` fields and `PascalCase`
    /// variants both land where serde would put them — because the conversion
    /// goes through a word split rather than a character rewrite.
    pub(crate) fn apply(self, ident: &str) -> String {
        use heck::{
            ToKebabCase, ToLowerCamelCase, ToShoutyKebabCase, ToShoutySnakeCase, ToSnakeCase,
            ToUpperCamelCase,
        };
        match self {
            Self::Lower => ident.to_lowercase(),
            Self::Upper => ident.to_uppercase(),
            Self::Pascal => ident.to_upper_camel_case(),
            Self::Camel => ident.to_lower_camel_case(),
            Self::Snake => ident.to_snake_case(),
            Self::ScreamingSnake => ident.to_shouty_snake_case(),
            Self::Kebab => ident.to_kebab_case(),
            Self::ScreamingKebab => ident.to_shouty_kebab_case(),
        }
    }
}

// ---------------------------------------------------------------------------
// Attribute and literal helpers
// ---------------------------------------------------------------------------

/// Every attribute called `name`.
pub(crate) fn attrs_named<'a>(
    attrs: &'a [Attribute],
    name: &'a str,
) -> impl Iterator<Item = &'a Attribute> + 'a {
    attrs.iter().filter(move |a| a.path().is_ident(name))
}

/// True when the item carries `#[name]` in any form.
pub(crate) fn has_attr(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|a| a.path().is_ident(name))
}

/// The string value of a literal, or an error pointing at it.
pub(crate) fn lit_str(lit: &Lit, key: &str) -> syn::Result<String> {
    match lit {
        Lit::Str(s) => Ok(s.value()),
        other => Err(syn::Error::new(
            other.span(),
            with_helps(
                &format!("`{key}` needs a string"),
                &[format!("write it as `{key} = \"…\"`")],
            ),
        )),
    }
}

/// A path written either bare (`my_check`) or quoted (`"my_check"`).
///
/// Both spellings appear in the documentation, and rejecting either one would
/// be a papercut with no upside.
pub(crate) fn expr_as_path(expr: &Expr, key: &str) -> syn::Result<Path> {
    match expr {
        Expr::Path(ExprPath { path, .. }) => Ok(path.clone()),
        Expr::Lit(ExprLit {
            lit: Lit::Str(text),
            ..
        }) => text.parse::<Path>().map_err(|_| {
            syn::Error::new(
                text.span(),
                with_helps(
                    &format!("`{key}` is not a path"),
                    &[format!("write it as `{key} = my_function`")],
                ),
            )
        }),
        other => Err(syn::Error::new(
            other.span(),
            with_helps(
                &format!("`{key}` needs a function or type path"),
                &[format!("write it as `{key} = my_function`")],
            ),
        )),
    }
}

/// A type written either bare (`User`) or quoted (`"User"`).
pub(crate) fn expr_as_type(expr: &Expr, key: &str) -> syn::Result<Type> {
    match expr {
        Expr::Path(ExprPath { path, .. }) => Ok(Type::Path(syn::TypePath {
            qself: None,
            path: path.clone(),
        })),
        Expr::Lit(ExprLit {
            lit: Lit::Str(text),
            ..
        }) => text.parse::<Type>().map_err(|_| {
            syn::Error::new(
                text.span(),
                with_helps(
                    &format!("`{key}` is not a type"),
                    &[format!("write it as `{key} = MyType`")],
                ),
            )
        }),
        other => Err(syn::Error::new(
            other.span(),
            with_helps(
                &format!("`{key}` needs a type"),
                &[format!("write it as `{key} = MyType`")],
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// Type shape helpers
// ---------------------------------------------------------------------------

/// The `T` of an `Option<T>` written syntactically.
///
/// Syntactic and therefore fallible in the presence of an alias, which is
/// documented rather than papered over: `type Maybe = Option<u8>` is treated as
/// an opaque type, and the field is required.
pub(crate) fn option_inner(ty: &Type) -> Option<&Type> {
    generic_inner(ty, &["Option"])
}

/// The `T` of a `Vec<T>`, `HashSet<T>`, … written syntactically.
pub(crate) fn sequence_inner(ty: &Type) -> Option<&Type> {
    generic_inner(ty, &["Vec", "VecDeque", "HashSet", "BTreeSet"])
}

/// The single generic argument of `Wrapper<T>` when `Wrapper` is one of
/// `names`.
pub(crate) fn generic_inner<'a>(ty: &'a Type, names: &[&str]) -> Option<&'a Type> {
    let Type::Path(path) = ty else { return None };
    if path.qself.is_some() {
        return None;
    }
    let segment = path.path.segments.last()?;
    if !names.iter().any(|n| segment.ident == n) {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })
}

/// The final identifier of a type path — `Vec` for `std::vec::Vec<T>`.
pub(crate) fn type_ident(ty: &Type) -> Option<&syn::Ident> {
    match ty {
        Type::Path(path) => path.path.segments.last().map(|s| &s.ident),
        Type::Reference(reference) => type_ident(&reference.elem),
        _ => None,
    }
}

/// True when `ident` appears anywhere in `ty`.
///
/// Used to break the `const` cycle a self-referential field would otherwise
/// create: `Category { children: Vec<Category> }` must not compute
/// `HAS_CONSTRAINTS` from a type whose own `HAS_CONSTRAINTS` is what is being
/// computed.
pub(crate) fn type_mentions(ty: &Type, ident: &syn::Ident) -> bool {
    ty.to_token_stream().into_iter().any(|token| match token {
        proc_macro2::TokenTree::Ident(found) => found == *ident,
        proc_macro2::TokenTree::Group(group) => {
            group.stream().into_iter().any(|inner| match inner {
                proc_macro2::TokenTree::Ident(found) => found == *ident,
                _ => false,
            })
        }
        _ => false,
    }) || nested_mentions(ty, ident)
}

/// The `Vec<Category>` case: the identifier is inside angle brackets, which
/// `to_token_stream` yields as plain tokens rather than a group.
fn nested_mentions(ty: &Type, ident: &syn::Ident) -> bool {
    let mut found = false;
    for token in ty.to_token_stream() {
        if let proc_macro2::TokenTree::Ident(candidate) = token
            && candidate == *ident
        {
            found = true;
        }
    }
    found
}

/// RFC 6901 escaping for one JSON Pointer token: `~` → `~0`, `/` → `~1`.
///
/// Applied at macro time, so the generated pointer is a literal.
pub(crate) fn escape_pointer_token(token: &str) -> String {
    if !token.contains(['~', '/']) {
        return token.to_owned();
    }
    let mut out = String::with_capacity(token.len() + 4);
    for ch in token.chars() {
        match ch {
            '~' => out.push_str("~0"),
            '/' => out.push_str("~1"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[test]
    fn levenshtein_matches_known_distances() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("len", "len"), 0);
        assert_eq!(
            levenshtein("lenght", "length"),
            1,
            "a transposition is one edit"
        );
        assert_eq!(levenshtein("ragne", "range"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
    }

    #[test]
    fn suggestions_are_offered_for_plausible_typos() {
        let known = ["len", "range", "pattern", "format", "nested"];
        assert_eq!(did_you_mean("ragne", &known), Some("range"));
        assert_eq!(did_you_mean("Len", &known), Some("len"));
        assert_eq!(did_you_mean("patern", &known), Some("pattern"));
        assert_eq!(did_you_mean("formt", &known), Some("format"));
        // A shared prefix wins even when the edit distance is large.
        assert_eq!(did_you_mean("lenght", &known), Some("len"));
        assert_eq!(did_you_mean("nest", &known), Some("nested"));
    }

    #[test]
    fn nonsense_gets_no_suggestion() {
        let known = ["len", "range", "pattern"];
        assert_eq!(did_you_mean("completely_different", &known), None);
        assert_eq!(did_you_mean("zzzzz", &known), None);
    }

    #[test]
    fn the_shortest_of_two_equal_candidates_wins() {
        // Both are one edit away; the shorter one is the more likely intent.
        let known = ["len", "lens_and_more"];
        assert_eq!(did_you_mean("le", &known), Some("len"));
    }

    #[test]
    fn key_lists_read_as_english() {
        assert_eq!(list(&[]), "");
        assert_eq!(list(&["len"]), "`len`");
        assert_eq!(list(&["len", "range"]), "`len` or `range`");
        assert_eq!(
            list(&["len", "range", "pattern"]),
            "`len`, `range` or `pattern`"
        );
    }

    #[test]
    fn doc_comments_lose_their_leading_space() {
        let item: syn::ItemStruct = parse_quote! {
            /// Public handle.
            ///
            /// Lowercase only.
            struct S;
        };
        assert_eq!(
            doc_lines(&item.attrs),
            vec!["Public handle.", "", "Lowercase only."]
        );
        assert_eq!(
            doc_text(&item.attrs).as_deref(),
            Some("Public handle.\n\nLowercase only.")
        );
    }

    #[test]
    fn the_first_paragraph_is_the_summary() {
        let item: syn::ItemStruct = parse_quote! {
            /// Create a user.
            ///
            /// Sends a welcome email asynchronously.
            struct S;
        };
        let (summary, description) = doc_summary_and_description(&item.attrs);
        assert_eq!(summary.as_deref(), Some("Create a user."));
        assert_eq!(
            description.as_deref(),
            Some("Sends a welcome email asynchronously.")
        );
    }

    #[test]
    fn a_wrapped_summary_line_is_rejoined() {
        let item: syn::ItemStruct = parse_quote! {
            /// Create a user
            /// and send mail.
            struct S;
        };
        let (summary, description) = doc_summary_and_description(&item.attrs);
        assert_eq!(summary.as_deref(), Some("Create a user and send mail."));
        assert_eq!(description, None);
    }

    #[test]
    fn undocumented_items_produce_nothing() {
        let item: syn::ItemStruct = parse_quote! { struct S; };
        assert_eq!(doc_text(&item.attrs), None);
        assert_eq!(doc_summary_and_description(&item.attrs), (None, None));
    }

    #[test]
    fn rename_rules_match_serde() {
        let cases = [
            (RenameRule::Lower, "created_at", "created_at"),
            (RenameRule::Upper, "created_at", "CREATED_AT"),
            (RenameRule::Pascal, "created_at", "CreatedAt"),
            (RenameRule::Camel, "created_at", "createdAt"),
            (RenameRule::Snake, "CreatedAt", "created_at"),
            (RenameRule::ScreamingSnake, "CreatedAt", "CREATED_AT"),
            (RenameRule::Kebab, "CreatedAt", "created-at"),
            (RenameRule::ScreamingKebab, "CreatedAt", "CREATED-AT"),
        ];
        for (rule, input, expected) in cases {
            assert_eq!(rule.apply(input), expected, "{rule:?} on {input}");
        }
    }

    #[test]
    fn every_rename_rule_name_parses() {
        for name in RenameRule::NAMES {
            assert!(RenameRule::parse(name).is_some(), "{name}");
        }
        assert_eq!(RenameRule::parse("camel_case"), None);
    }

    #[test]
    fn option_is_unwrapped_syntactically() {
        let ty: Type = parse_quote!(Option<u8>);
        assert!(option_inner(&ty).is_some());
        let ty: Type = parse_quote!(::core::option::Option<Vec<String>>);
        assert!(option_inner(&ty).is_some());
        let ty: Type = parse_quote!(u8);
        assert!(option_inner(&ty).is_none());
    }

    #[test]
    fn sequences_are_recognised() {
        let ty: Type = parse_quote!(Vec<String>);
        assert!(sequence_inner(&ty).is_some());
        let ty: Type = parse_quote!(BTreeSet<u8>);
        assert!(sequence_inner(&ty).is_some());
        let ty: Type = parse_quote!(String);
        assert!(sequence_inner(&ty).is_none());
    }

    #[test]
    fn self_reference_is_detected_through_wrappers() {
        let ident: syn::Ident = parse_quote!(Category);
        let ty: Type = parse_quote!(Vec<Category>);
        assert!(type_mentions(&ty, &ident));
        let ty: Type = parse_quote!(Option<Box<Category>>);
        assert!(type_mentions(&ty, &ident));
        let ty: Type = parse_quote!(Vec<String>);
        assert!(!type_mentions(&ty, &ident));
    }

    #[test]
    fn pointer_tokens_are_escaped() {
        assert_eq!(escape_pointer_token("plain"), "plain");
        assert_eq!(escape_pointer_token("a/b"), "a~1b");
        assert_eq!(escape_pointer_token("a~b"), "a~0b");
    }

    #[test]
    fn diagnostics_accumulate_and_stay_empty_when_nothing_is_wrong() {
        let mut errors = Diagnostics::new();
        assert!(errors.is_empty());
        errors.unknown_key(Span::call_site(), "schema", "lenght", &["len", "range"]);
        errors.error(Span::call_site(), "second problem");
        assert_eq!(errors.len(), 2);
        let rendered = errors.into_error().expect("two errors").to_string();
        assert!(
            rendered.contains("unknown `schema` attribute `lenght`"),
            "{rendered}"
        );
        assert!(rendered.contains("help: did you mean `len`?"), "{rendered}");
    }

    #[test]
    fn an_unknown_key_with_no_neighbour_lists_the_vocabulary() {
        let mut errors = Diagnostics::new();
        errors.unknown_key(Span::call_site(), "schema", "wibble", &["len", "range"]);
        let rendered = errors.into_error().unwrap().to_string();
        assert!(
            rendered.contains("the accepted keys are: `len` or `range`"),
            "{rendered}"
        );
    }

    #[test]
    fn an_empty_accumulator_passes_the_expansion_through() {
        let errors = Diagnostics::new();
        let out = errors.finish(quote::quote!(
            struct X;
        ));
        assert_eq!(
            out.to_string(),
            quote::quote!(
                struct X;
            )
            .to_string()
        );
    }

    #[test]
    fn a_failed_parse_keeps_the_placeholder() {
        let mut errors = Diagnostics::new();
        errors.error(Span::call_site(), "boom");
        let out = errors
            .finish(quote::quote!(
                struct X;
            ))
            .to_string();
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("struct X"), "{out}");
    }

    #[test]
    fn darling_errors_round_trip() {
        let mut errors = Diagnostics::new();
        errors.push_darling(darling::Error::custom("from darling"));
        assert_eq!(errors.len(), 1);
        let back = errors.into_darling().expect("one error");
        assert!(back.to_string().contains("from darling"));
    }

    #[test]
    fn paths_are_accepted_bare_or_quoted() {
        let bare: Expr = parse_quote!(passwords_match);
        let quoted: Expr = parse_quote!("passwords_match");
        assert_eq!(
            expr_as_path(&bare, "check")
                .unwrap()
                .to_token_stream()
                .to_string(),
            expr_as_path(&quoted, "check")
                .unwrap()
                .to_token_stream()
                .to_string()
        );
    }

    #[test]
    fn a_non_path_expression_is_rejected_with_a_fix() {
        let expr: Expr = parse_quote!(1 + 1);
        let error = expr_as_path(&expr, "check").unwrap_err().to_string();
        assert!(
            error.contains("help: write it as `check = my_function`"),
            "{error}"
        );
    }
}
