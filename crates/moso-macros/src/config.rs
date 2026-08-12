//! `#[derive(Config)]` — typed, layered, discoverable configuration.
//!
//! The derive emits three things from one struct:
//!
//! 1. `Config::descriptor()` — a `&'static ConfigDescriptor` naming every
//!    field, its type, its doc comment, its default, whether it is secret and
//!    which environment variable aliases it. This is what `moso config` prints
//!    and what `.env.example` is generated from, so neither can rot.
//! 2. `Config::load_nested()` — one `ConfigLoader::field` call per leaf and one
//!    `ConfigLoader::section` call per `#[config(nested)]`, all of them
//!    executed before the first `?`, so a run reports *every* bad field rather
//!    than the first.
//! 3. The per-field `FieldSpec` that carries the two levels of defaults the
//!    loader cannot know: `#[config(default = ..)]` (level 8) and the
//!    `#[config(profile(..))]` entry for the active profile (level 7).
//!
//! See `docs/01-http/18-configuration.md` for the model and
//! `docs/06-reference/62-macro-reference.md` for the expansion contract.

use darling::util::{Flag, SpannedValue};
use darling::{FromDeriveInput, FromField, FromMeta, ast};
use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::spanned::Spanned;
use syn::{DeriveInput, Expr, Ident, Lit, Type};

use crate::config::support::{Diagnostic, moso, reject_generics, type_display};
use crate::util::attrs::{doc_text, option_inner, type_ident};

// ---------------------------------------------------------------------------
// Shared support
// ---------------------------------------------------------------------------

/// The four helpers the support derives share and `util::attrs` does not have.
///
/// Everything `util::attrs` already provides — `doc_text`, `option_inner`,
/// `generic_inner`, `type_ident`, `did_you_mean`, `levenshtein` — is used from
/// there rather than repeated here.
///
/// **Placement note for the integration agent.** These four belong in
/// `util/attrs.rs` too. They live here because the support derives own no
/// shared file; moving the module is a one-line change at each `use` site.
/// `Diagnostic` is a *builder* over one `syn::Error` rather than an
/// accumulator like `attrs::Diagnostics`, because a derive that has found a
/// contradictory attribute has nothing useful to say about the rest of the
/// type — and the style guide asks for one error, not a cascade.
pub(crate) mod support {
    use proc_macro2::{Span, TokenStream};
    use quote::{ToTokens, quote};
    use syn::spanned::Spanned;
    use syn::{Generics, Ident, Type};

    /// The path every generated item resolves against.
    ///
    /// Generated code never names a runtime crate: `::moso::__private` is a
    /// `#[doc(hidden)]` façade, so `moso-core` can be refactored, split or
    /// renamed without touching a single macro or breaking a user's expanded
    /// code (`docs/06-reference/62-macro-reference.md`).
    pub(crate) fn moso() -> TokenStream {
        quote!(::moso::__private)
    }

    /// A diagnostic in the house style (`docs/04-devex/41-diagnostics.md`).
    ///
    /// `compile_error!` renders one message, so the `note:`/`help:` lines the
    /// style guide requires are folded into that message with the same
    /// prefixes rustc would use. The span always points at the *user's* token.
    #[derive(Debug, Clone)]
    pub(crate) struct Diagnostic {
        /// What is wrong, in plain language.
        message: String,
        /// Why the rule exists, and what to do about it.
        lines: Vec<String>,
    }

    impl Diagnostic {
        /// Start a diagnostic whose headline is `message`.
        pub(crate) fn new(message: impl Into<String>) -> Self {
            Self {
                message: message.into(),
                lines: Vec::new(),
            }
        }

        /// Add a `note:` line — one sentence saying why the rule exists.
        #[must_use]
        pub(crate) fn note(mut self, note: impl Into<String>) -> Self {
            self.lines.push(format!("note: {}", note.into()));
            self
        }

        /// Add a `help:` line. Every diagnostic must carry at least one, and it
        /// must be code the reader can paste.
        #[must_use]
        pub(crate) fn help(mut self, help: impl Into<String>) -> Self {
            self.lines.push(format!("help: {}", help.into()));
            self
        }

        /// Add a `help:` line followed by an indented block of code.
        #[must_use]
        pub(crate) fn help_code(
            mut self,
            help: impl Into<String>,
            code: impl Into<String>,
        ) -> Self {
            self.lines.push(format!("help: {}", help.into()));
            for line in code.into().lines() {
                self.lines.push(format!("          {line}"));
            }
            self
        }

        /// Attach the diagnostic to the span of `tokens`.
        pub(crate) fn at(self, tokens: &impl Spanned) -> syn::Error {
            self.at_span(tokens.span())
        }

        /// Attach the diagnostic to an explicit span.
        pub(crate) fn at_span(self, span: Span) -> syn::Error {
            let mut rendered = self.message;
            for line in &self.lines {
                rendered.push('\n');
                rendered.push_str(line);
            }
            syn::Error::new(span, rendered)
        }
    }

    /// A type rendered for a human: `Option<u32>`, `SecretString`.
    ///
    /// `to_token_stream().to_string()` pads every token with spaces, so the
    /// punctuation is closed up again: a space survives only between two word
    /// characters (`dyn Mailer`, `&'static str`) and after a comma. Never used
    /// for code generation — only for diagnostics and for the `type_name` a
    /// boot report prints.
    pub(crate) fn type_display(ty: &Type) -> String {
        /// Whether a space is meaningful next to this character.
        fn word(character: char) -> bool {
            character.is_alphanumeric() || character == '_' || character == '\''
        }

        let rendered = ty.to_token_stream().to_string();
        let mut out = String::with_capacity(rendered.len());
        let mut pending_space = false;

        for character in rendered.chars() {
            if character == ' ' {
                pending_space = true;
                continue;
            }
            let keep = pending_space
                && (out.ends_with(',')
                    || (out.chars().next_back().is_some_and(word) && word(character)));
            if keep {
                out.push(' ');
            }
            out.push(character);
            pending_space = false;
        }
        out
    }

    /// Reject generic parameters on a derive that cannot support them.
    ///
    /// `Config` and `Dependency` hang a process-wide `static` off the deriving
    /// type, and a `static` inside a generic function is shared by **every**
    /// instantiation — a generic version would silently hand `Foo<A>` the
    /// descriptor built for `Foo<B>`. `Error` has its own reason. Refusing is
    /// the honest answer, and `reason` is what the diagnostic says.
    pub(crate) fn reject_generics(
        generics: &Generics,
        derive: &str,
        ident: &Ident,
        reason: &str,
    ) -> Option<syn::Error> {
        if generics.params.is_empty() {
            return None;
        }
        Some(
            Diagnostic::new(format!(
                "`#[derive({derive})]` cannot be used on a generic type"
            ))
            .note(reason)
            .help_code(
                "define a concrete type instead:",
                format!("pub struct {ident} {{ /* … */ }}"),
            )
            .at(&generics.params),
        )
    }

    /// `OutOfStock` → `Out of stock`.
    ///
    /// The default `title` of a `#[derive(Error)]` variant and of a
    /// `#[derive(Responder)]` response description. `heck` has no sentence
    /// case, and title case (`Out Of Stock`) reads like a headline rather than
    /// like the human-readable summary RFC 9457 asks a `title` to be.
    pub(crate) fn sentence_case(name: &str) -> String {
        let characters: Vec<char> = name.chars().collect();
        let mut words: Vec<String> = Vec::new();
        let mut word = String::new();

        for (index, character) in characters.iter().copied().enumerate() {
            if character == '_' || character == '-' {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                continue;
            }
            let previous_upper = index > 0 && characters[index - 1].is_ascii_uppercase();
            let next_lower = characters
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_lowercase());
            // Break before an upper-case letter that starts a new word — either
            // because the previous letter was lower case (`OutOf`), or because
            // it is the last letter of an acronym (`URIToo`).
            let boundary = character.is_ascii_uppercase()
                && (!previous_upper || next_lower)
                && !word.is_empty();
            if boundary {
                words.push(std::mem::take(&mut word));
            }
            word.push(character);
        }
        if !word.is_empty() {
            words.push(word);
        }

        let mut sentence = String::new();
        for (index, word) in words.iter().enumerate() {
            if index > 0 {
                sentence.push(' ');
            }
            // An acronym stays as written: `URITooLong` reads worse as
            // `U R I too long` than as `URI too long`.
            let acronym = word.len() > 1 && word.chars().all(|c| c.is_ascii_uppercase());
            if index == 0 || acronym {
                sentence.push_str(word);
            } else {
                sentence.push_str(&word.to_lowercase());
            }
        }
        if let Some(first) = sentence.chars().next()
            && first.is_ascii_lowercase()
        {
            sentence.replace_range(0..first.len_utf8(), &first.to_ascii_uppercase().to_string());
        }
        sentence
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sentence_case_reads_like_a_sentence() {
            assert_eq!(sentence_case("OutOfStock"), "Out of stock");
            assert_eq!(sentence_case("PaymentRequired"), "Payment required");
            assert_eq!(sentence_case("Gateway"), "Gateway");
            assert_eq!(sentence_case("URITooLong"), "URI too long");
            assert_eq!(sentence_case("out_of_stock"), "Out of stock");
        }

        #[test]
        fn types_render_without_token_padding() {
            let ty: syn::Type = syn::parse_quote!(Option<std::string::String>);
            assert_eq!(type_display(&ty), "Option<std::string::String>");
            let ty: syn::Type = syn::parse_quote!(Reloadable<String>);
            assert_eq!(type_display(&ty), "Reloadable<String>");
        }
    }
}

// ---------------------------------------------------------------------------
// Attribute model
// ---------------------------------------------------------------------------

/// A `#[config(default = ..)]` value, rendered exactly as a configuration
/// source would have supplied it.
///
/// Defaults are *text*, not typed values: level 7 and level 8 of the precedence
/// stack sit under the same `Coerce` call as the environment and the TOML, so
/// `default = false` and `BIND=false` cannot disagree about what `false` means.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rendered(String);

impl FromMeta for Rendered {
    fn from_value(value: &Lit) -> darling::Result<Self> {
        Ok(Self(match value {
            Lit::Str(text) => text.value(),
            Lit::Bool(flag) => flag.value().to_string(),
            Lit::Int(number) => number.base10_digits().to_owned(),
            Lit::Float(number) => number.base10_digits().to_owned(),
            Lit::Char(character) => character.value().to_string(),
            other => return Err(darling::Error::unexpected_lit_type(other)),
        }))
    }

    fn from_expr(expr: &Expr) -> darling::Result<Self> {
        match expr {
            Expr::Lit(literal) => Self::from_value(&literal.lit),
            // `default = -1`: a negative number is a unary expression, not a
            // literal, and a configuration default of `-1` is ordinary.
            Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => {
                let Self(inner) = Self::from_expr(&unary.expr)?;
                Ok(Self(format!("-{inner}")))
            }
            Expr::Group(group) => Self::from_expr(&group.expr),
            other => Err(darling::Error::unexpected_expr_type(other)),
        }
    }
}

/// The `#[config(profile(dev = .., test = .., production = ..))]` table.
///
/// Level 7 of the precedence stack. The derive picks the entry for the active
/// profile at load time and hands it to the loader as
/// `FieldSpec::profile_default`, so `moso config` can attribute the value to
/// "profile default (production)" rather than to the base default.
#[derive(Debug, Clone, Default, FromMeta)]
struct ProfileDefaults {
    /// The `dev` default.
    #[darling(default)]
    dev: Option<Rendered>,
    /// The `test` default.
    #[darling(default)]
    test: Option<Rendered>,
    /// The `production` default.
    #[darling(default)]
    production: Option<Rendered>,
}

impl ProfileDefaults {
    /// Whether any profile carries a default.
    fn is_empty(&self) -> bool {
        self.dev.is_none() && self.test.is_none() && self.production.is_none()
    }
}

/// A `#[config(range = 1..=1000)]` bound.
#[derive(Debug, Clone)]
struct RangeBound {
    /// The inclusive lower bound, if the range has one.
    start: Option<Expr>,
    /// The upper bound, if the range has one.
    end: Option<Expr>,
    /// Whether the upper bound is inclusive (`..=`).
    inclusive: bool,
}

impl RangeBound {
    /// The range as a person writes it: `1..=1000`.
    fn rendered(&self) -> String {
        let start = self
            .start
            .as_ref()
            .map(|expr| expr.to_token_stream().to_string().replace(' ', ""))
            .unwrap_or_default();
        let end = self
            .end
            .as_ref()
            .map(|expr| expr.to_token_stream().to_string().replace(' ', ""))
            .unwrap_or_default();
        let dots = if self.inclusive { "..=" } else { ".." };
        format!("{start}{dots}{end}")
    }

    /// The boolean expression that holds when `binding` is inside the range.
    fn condition(&self, binding: &Ident) -> TokenStream {
        let mut checks: Vec<TokenStream> = Vec::new();
        if let Some(start) = &self.start {
            checks.push(quote!(*#binding >= #start));
        }
        if let Some(end) = &self.end {
            checks.push(if self.inclusive {
                quote!(*#binding <= #end)
            } else {
                quote!(*#binding < #end)
            });
        }
        if checks.is_empty() {
            return quote!(true);
        }
        quote!(#(#checks)&&*)
    }
}

impl FromMeta for RangeBound {
    fn from_expr(expr: &Expr) -> darling::Result<Self> {
        match expr {
            Expr::Range(range) => Ok(Self {
                start: range.start.as_deref().cloned(),
                end: range.end.as_deref().cloned(),
                inclusive: matches!(range.limits, syn::RangeLimits::Closed(_)),
            }),
            Expr::Group(group) => Self::from_expr(&group.expr),
            // `range = "1..=1000"` also parses, because a person who quotes it
            // is not making a mistake worth an error.
            Expr::Lit(literal) => Self::from_value(&literal.lit),
            other => Err(darling::Error::custom(
                "expected a range such as `1..=1000`, `0..`, or `..=100`",
            )
            .with_span(other)),
        }
    }

    fn from_string(value: &str) -> darling::Result<Self> {
        let parsed: Expr = syn::parse_str(value).map_err(|_| {
            darling::Error::custom("expected a range such as `1..=1000`, `0..`, or `..=100`")
        })?;
        Self::from_expr(&parsed)
    }
}

/// One `#[config(..)]` field.
#[derive(Debug, FromField)]
#[darling(attributes(config), forward_attrs(doc))]
struct ConfigField {
    /// The field's name. `None` only for a tuple struct, which the derive
    /// rejects.
    ident: Option<Ident>,
    /// The declared type.
    ty: Type,
    /// Forwarded `///` documentation.
    attrs: Vec<syn::Attribute>,
    /// `#[config(default = ..)]` — level 8.
    #[darling(default)]
    default: Option<Rendered>,
    /// `#[config(env = "RUST_LOG")]` — an explicit environment alias.
    #[darling(default)]
    env: Option<SpannedValue<String>>,
    /// `#[config(secret)]` — the value is redacted everywhere.
    #[darling(default)]
    secret: Flag,
    /// `#[config(nested)]` — the field is another `Config`.
    #[darling(default)]
    nested: Flag,
    /// `#[config(profile(..))]` — level 7.
    #[darling(default)]
    profile: Option<SpannedValue<ProfileDefaults>>,
    /// `#[config(range = 1..=1000)]` — a bound checked after coercion.
    #[darling(default)]
    range: Option<SpannedValue<RangeBound>>,
    /// `#[config(reloadable)]` — the value is re-read on `SIGHUP`.
    #[darling(default)]
    reloadable: Flag,
    /// `#[config(secret_from = "file")]` — where the secret comes from.
    #[darling(default)]
    secret_from: Option<SpannedValue<String>>,
    /// `#[config(parse)]` — coerce through `String` and `FromStr`.
    #[darling(default)]
    parse: Flag,
}

/// The `#[derive(Config)]` input.
#[derive(Debug, FromDeriveInput)]
#[darling(attributes(config), supports(struct_named), forward_attrs(doc))]
struct ConfigInput {
    /// The struct's name.
    ident: Ident,
    /// Generics, which the derive rejects.
    generics: syn::Generics,
    /// The fields.
    data: ast::Data<darling::util::Ignored, ConfigField>,
    /// Forwarded `///` documentation.
    #[allow(dead_code)]
    attrs: Vec<syn::Attribute>,
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

/// Expand `#[derive(Config)]`.
///
/// Wire this up from `lib.rs` with
/// `#[proc_macro_derive(Config, attributes(config))]`.
pub(crate) fn expand(input: DeriveInput) -> TokenStream {
    let ident = input.ident.clone();
    let generics = input.generics.clone();

    let parsed = match ConfigInput::from_derive_input(&input) {
        Ok(parsed) => parsed,
        Err(error) => return with_placeholder(&ident, &generics, error.write_errors()),
    };

    match build(&parsed) {
        Ok(tokens) => tokens,
        Err(error) => with_placeholder(&ident, &generics, error.to_compile_error()),
    }
}

/// A `Config` impl that type-checks but never runs, emitted beside an error.
///
/// Rule 4 of the diagnostics style guide: one error, not a cascade. Without
/// this, every `App::new(cfg)` and every `ctx.config::<T>()` in the program
/// would add a second, misleading "the trait bound `AppConfig: Config` is not
/// satisfied".
fn with_placeholder(ident: &Ident, generics: &syn::Generics, errors: TokenStream) -> TokenStream {
    let moso = moso();
    let (impl_generics, type_generics, where_clause) = generics.split_for_impl();
    quote! {
        #errors

        #[automatically_derived]
        impl #impl_generics #moso::Config for #ident #type_generics #where_clause {
            fn descriptor() -> &'static #moso::ConfigDescriptor {
                ::core::unimplemented!()
            }

            fn load_nested(
                _loader: &#moso::ConfigLoader,
                _prefix: &#moso::ConfigKey,
                _errors: &mut #moso::BootErrors,
            ) -> ::core::option::Option<Self> {
                ::core::unimplemented!()
            }
        }
    }
}

/// What the derive knows about one field once the attributes agree.
struct Plan<'a> {
    /// The parsed attributes and the field itself.
    field: &'a ConfigField,
    /// The field's name, which is also its key segment.
    name: &'a Ident,
    /// The local the loaded value is bound to.
    binding: Ident,
    /// The `type_name` a boot report prints for this field.
    type_name: TokenStream,
    /// The doc comment, as `Option<&'static str>` tokens.
    doc: TokenStream,
}

/// Build the two `Config` methods, or the first problem found.
fn build(input: &ConfigInput) -> syn::Result<TokenStream> {
    let moso = moso();
    let ident = &input.ident;

    if let Some(error) = reject_generics(
        &input.generics,
        "Config",
        ident,
        "the derive stores a process-wide descriptor for the type, and one `static` cannot \
         describe every instantiation",
    ) {
        return Err(error);
    }

    let ast::Data::Struct(fields) = &input.data else {
        // `supports(struct_named)` already rejected this; belt and braces so a
        // future darling change cannot turn it into a panic.
        return Err(
            Diagnostic::new("`#[derive(Config)]` needs a struct with named fields")
                .note(
                    "configuration keys are field names, so there is nothing to name a tuple field",
                )
                .help_code(
                    "give the fields names:",
                    format!("pub struct {ident} {{\n    pub name: String,\n}}"),
                )
                .at(ident),
        );
    };

    let mut plans: Vec<Plan<'_>> = Vec::with_capacity(fields.fields.len());
    for field in &fields.fields {
        plans.push(plan_field(field)?);
    }

    let descriptors = plans.iter().map(|plan| descriptor_entry(plan, &moso));
    let loads = plans
        .iter()
        .map(|plan| load_field(plan, &moso))
        .collect::<syn::Result<Vec<_>>>()?;
    let assignments = plans.iter().map(|plan| {
        let name = plan.name;
        let binding = &plan.binding;
        quote!(#name: #binding?)
    });

    let type_name = ident.to_string();

    Ok(quote! {
        #[automatically_derived]
        impl #moso::Config for #ident {
            fn descriptor() -> &'static #moso::ConfigDescriptor {
                static __MOSO_DESCRIPTOR: ::std::sync::OnceLock<#moso::ConfigDescriptor> =
                    ::std::sync::OnceLock::new();
                __MOSO_DESCRIPTOR.get_or_init(|| #moso::ConfigDescriptor {
                    type_name: #type_name,
                    // Leaked once per process: a nested section's descriptor is
                    // produced by a function call, so the field table cannot be
                    // a `const` and `&'static [FieldDescriptor]` has to come
                    // from somewhere that outlives every caller.
                    fields: ::std::boxed::Box::leak(
                        ::std::vec::Vec::into_boxed_slice(::std::vec![#(#descriptors),*])
                    ),
                })
            }

            fn load_nested(
                __loader: &#moso::ConfigLoader,
                __prefix: &#moso::ConfigKey,
                __errors: &mut #moso::BootErrors,
            ) -> ::core::option::Option<Self> {
                // Every field is read before the first `?`, so one run reports
                // every problem rather than the first.
                #(#loads)*
                ::core::option::Option::Some(Self { #(#assignments),* })
            }
        }
    })
}

/// Validate one field's attributes and work out how it loads.
fn plan_field(field: &ConfigField) -> syn::Result<Plan<'_>> {
    let name = field.ident.as_ref().ok_or_else(|| {
        Diagnostic::new("`#[derive(Config)]` needs a struct with named fields")
            .note("a configuration key is a field name, and a tuple field has none")
            .help("give the field a name")
            .at(&field.ty)
    })?;

    check_conflicts(field, name)?;
    let secret = field.secret.is_present() || field.secret_from.is_some();
    if secret {
        check_secret_type(field, name)?;
    }
    if field.reloadable.is_present() {
        check_reloadable_type(field, name)?;
    }
    if let Some(from) = &field.secret_from {
        check_secret_from(from)?;
    }

    let type_name = type_name_tokens(field);
    let doc = match doc_text(&field.attrs) {
        Some(doc) => quote!(::core::option::Option::Some(#doc)),
        None => quote!(::core::option::Option::None),
    };

    Ok(Plan {
        field,
        name,
        binding: format_ident!("__moso_field_{}", name),
        type_name,
        doc,
    })
}

/// Reject attribute combinations that cannot both be honoured.
fn check_conflicts(field: &ConfigField, name: &Ident) -> syn::Result<()> {
    if !field.nested.is_present() {
        if field.parse.is_present() && field.range.is_some() {
            return Err(Diagnostic::new(
                "`#[config(parse)]` and `#[config(range = ..)]` cannot be combined",
            )
            .note("a parsed value is produced by `FromStr`, which the range check cannot compare")
            .help("validate the bound inside the type's `FromStr`, and drop `range`")
            .at(&field.ty));
        }
        return Ok(());
    }

    // A nested section is a whole `Config`, so every scalar knob is meaningless
    // on it and silently ignoring one would be a trap.
    let mut offender: Option<(&str, proc_macro2::Span)> = None;
    if let Some(env) = &field.env {
        offender = Some(("env", env.span()));
    }
    if let Some(range) = &field.range {
        offender = Some(("range", range.span()));
    }
    if let Some(profile) = &field.profile {
        offender = Some(("profile", profile.span()));
    }
    if field.secret.is_present() {
        offender = Some(("secret", field.secret.span()));
    }
    if field.parse.is_present() {
        offender = Some(("parse", field.parse.span()));
    }
    if field.default.is_some() {
        offender = Some(("default", field.ty.span()));
    }
    if let Some((key, span)) = offender {
        return Err(Diagnostic::new(format!(
            "`#[config({key})]` cannot be combined with `#[config(nested)]`"
        ))
        .note(
            "a nested section is described by its own type, which carries the defaults, the \
               aliases and the secrecy of each of its fields",
        )
        .help_code(
            format!("move `{key}` onto the field inside the nested type, and leave this one bare:"),
            format!("#[config(nested)]\npub {name}: /* … */,"),
        )
        .at_span(span));
    }

    if field.reloadable.is_present() {
        return Err(Diagnostic::new(
            "`#[config(reloadable)]` cannot be combined with `#[config(nested)]`",
        )
        .note(
            "a battery reads its section once at boot — a reloaded database URL would not reach \
             the pool that was already built from the old one",
        )
        .help_code(
            "mark the individual values that can change instead:",
            "#[config(reloadable, default = \"info\")]\npub log: Reloadable<String>,",
        )
        .at_span(field.reloadable.span()));
    }

    Ok(())
}

/// `#[config(secret)]` on a field whose type is not a secret is a compile
/// error, per acceptance criterion 7 of `docs/01-http/18-configuration.md`.
fn check_secret_type(field: &ConfigField, name: &Ident) -> syn::Result<()> {
    let bare = option_inner(&field.ty).unwrap_or(&field.ty);
    if type_ident(bare).is_some_and(|ident| ident == "SecretString" || ident == "SecretBytes") {
        return Ok(());
    }

    let suggestion = if type_ident(bare).is_some_and(|ident| ident == "Vec") {
        "SecretBytes"
    } else {
        "SecretString"
    };
    let rendered = if option_inner(&field.ty).is_some() {
        format!("Option<{suggestion}>")
    } else {
        suggestion.to_owned()
    };

    Err(Diagnostic::new(format!(
        "`#[config(secret)]` needs a secret type, and `{}` is not one",
        type_display(&field.ty)
    ))
    .note(
        "a secret is a distinct type so that it cannot reach a log line, a `Debug` output or a \
         response by accident",
    )
    .help_code(
        "change the field's type:",
        format!("#[config(secret)]\npub {name}: {rendered},"),
    )
    .at(&field.ty))
}

/// `#[config(reloadable)]` needs a `Reloadable<T>`, because the indirection has
/// to be visible at every read.
fn check_reloadable_type(field: &ConfigField, name: &Ident) -> syn::Result<()> {
    if type_ident(&field.ty).is_some_and(|ident| ident == "Reloadable") {
        return Ok(());
    }
    Err(Diagnostic::new(format!(
        "`#[config(reloadable)]` needs a `Reloadable<T>`, and `{}` is not one",
        type_display(&field.ty)
    ))
    .note(
        "a reloadable value is read through `.get()` so the indirection is visible at every use \
         site rather than hidden behind a field access that used to be free",
    )
    .help_code(
        "wrap the type:",
        format!(
            "#[config(reloadable)]\npub {name}: Reloadable<{}>,",
            type_display(&field.ty)
        ),
    )
    .at(&field.ty))
}

/// Only the `file` convention is resolved during the synchronous load.
fn check_secret_from(from: &SpannedValue<String>) -> syn::Result<()> {
    match from.as_str() {
        "file" | "env" => Ok(()),
        other => Err(Diagnostic::new(format!(
            "`#[config(secret_from = \"{other}\")]` is not a source the derive can read"
        ))
        .note(
            "configuration is loaded synchronously at boot, and only `env` and `file` \
             (the `${KEY}_FILE` convention Docker and Kubernetes use) can be read without \
             an await",
        )
        .help_code(
            "read the secret with a provider instead, and leave the field bare `secret`:",
            "App::new(cfg).secret_provider(::std::sync::Arc::new(VaultSecrets::new(..)))",
        )
        .at_span(from.span())),
    }
}

/// The `type_name` a `missing configuration` error prints for this field.
fn type_name_tokens(field: &ConfigField) -> TokenStream {
    let moso = moso();
    if field.nested.is_present() {
        let rendered = type_display(&field.ty);
        return quote!(#rendered);
    }
    if let Some(range) = &field.range {
        // `integer in 1..=1000` — the same sentence the loader would have had
        // to invent, generated from the very attribute that enforces it.
        let noun = numeric_noun(&field.ty);
        let rendered = format!("{noun} in {}", range.rendered());
        return quote!(#rendered);
    }
    if field.parse.is_present() {
        let rendered = type_display(&field.ty);
        return quote!(#rendered);
    }
    let ty = &field.ty;
    quote!(<#ty as #moso::Coerce>::TYPE_NAME)
}

/// `integer` or `number`, from the field's spelling.
fn numeric_noun(ty: &Type) -> &'static str {
    let bare = option_inner(ty).unwrap_or(ty);
    match type_ident(bare)
        .map(std::string::ToString::to_string)
        .as_deref()
    {
        Some("f32" | "f64") => "number",
        Some(
            "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i32" | "i64" | "i128"
            | "isize",
        ) => "integer",
        _ => "value",
    }
}

/// One `FieldDescriptor` literal.
fn descriptor_entry(plan: &Plan<'_>, moso: &TokenStream) -> TokenStream {
    let field = plan.field;
    let name = plan.name.to_string();
    let type_name = &plan.type_name;
    let doc = &plan.doc;
    let secret = field.secret.is_present() || field.secret_from.is_some();
    let reloadable = field.reloadable.is_present();

    let default = match &field.default {
        Some(Rendered(value)) => quote!(::core::option::Option::Some(#value)),
        None => quote!(::core::option::Option::None),
    };
    let env_alias = match &field.env {
        Some(alias) => {
            let alias = alias.as_str();
            quote!(::core::option::Option::Some(#alias))
        }
        None => quote!(::core::option::Option::None),
    };
    let nested = if field.nested.is_present() {
        let ty = &field.ty;
        quote!(::core::option::Option::Some(<#ty as #moso::Config>::descriptor()))
    } else {
        quote!(::core::option::Option::None)
    };

    quote! {
        #moso::FieldDescriptor {
            name: #name,
            type_name: #type_name,
            doc: #doc,
            default: #default,
            secret: #secret,
            nested: #nested,
            env_alias: #env_alias,
            reloadable: #reloadable,
        }
    }
}

/// The statements that read one field into its local.
///
/// Nothing here uses a `let`-chain or any other post-2021 syntax: the tokens
/// land in the *user's* crate, whose edition the derive does not control.
fn load_field(plan: &Plan<'_>, moso: &TokenStream) -> syn::Result<TokenStream> {
    let field = plan.field;
    let binding = &plan.binding;
    let name = plan.name.to_string();
    let ty = &field.ty;

    if field.nested.is_present() {
        return Ok(quote! {
            let #binding = __loader.section::<#ty>(__prefix, #name, __errors);
        });
    }

    let spec_ident = format_ident!("__moso_spec_{}", plan.name);
    let spec = field_spec(plan, moso, &spec_ident);
    let optional = option_inner(ty);

    let read = if field.parse.is_present() {
        parse_read(plan, moso, optional, &spec_ident)
    } else if let Some(inner) = optional {
        quote!(__loader.optional_field::<#inner>(__prefix, &#spec_ident, __errors))
    } else {
        quote!(__loader.field::<#ty>(__prefix, &#spec_ident, __errors))
    };

    let range_check = match &field.range {
        Some(range) => range_check(plan, moso, range, optional.is_some(), &spec_ident),
        None => quote!(),
    };
    let mutability = if field.range.is_some() {
        quote!(mut)
    } else {
        quote!()
    };

    Ok(quote! {
        #spec
        let #mutability #binding = #read;
        #range_check
    })
}

/// The `FieldSpec` statement, including the profile-default selection.
fn field_spec(plan: &Plan<'_>, moso: &TokenStream, spec_ident: &Ident) -> TokenStream {
    let field = plan.field;
    let name = plan.name.to_string();
    let type_name = &plan.type_name;

    let mut builder = quote!(#moso::FieldSpec::new(#name, #type_name));
    if field.secret.is_present() || field.secret_from.is_some() {
        builder = quote!(#builder.secret());
    }
    if let Some(alias) = &field.env {
        let alias = alias.as_str();
        builder = quote!(#builder.env(#alias));
    }
    if let Some(Rendered(value)) = &field.default {
        builder = quote!(#builder.default_value(#value));
    }

    let profile = match &field.profile {
        Some(profiles) if !profiles.is_empty() => {
            let arms = [
                (quote!(Dev), profiles.dev.as_ref()),
                (quote!(Test), profiles.test.as_ref()),
                (quote!(Production), profiles.production.as_ref()),
            ]
            .into_iter()
            .filter_map(|(variant, rendered)| {
                let Rendered(value) = rendered?;
                Some(quote! {
                    #moso::Profile::#variant => #spec_ident.profile_default(#value),
                })
            })
            .collect::<Vec<_>>();
            quote! {
                let #spec_ident = match __loader.profile() {
                    #(#arms)*
                    _ => #spec_ident,
                };
            }
        }
        _ => quote!(),
    };

    quote! {
        let #spec_ident = #builder;
        #profile
    }
}

/// The `#[config(parse)]` read: text from the sources, then `FromStr`.
fn parse_read(
    plan: &Plan<'_>,
    moso: &TokenStream,
    optional: Option<&Type>,
    spec_ident: &Ident,
) -> TokenStream {
    let name = plan.name.to_string();
    let target = optional.unwrap_or(&plan.field.ty);
    let rendered_type = type_display(target);

    let wrap = if optional.is_some() {
        quote!(::core::option::Option::Some(::core::option::Option::Some(
            __moso_parsed
        )))
    } else {
        quote!(::core::option::Option::Some(__moso_parsed))
    };
    let absent = if optional.is_some() {
        quote!(::core::option::Option::Some(::core::option::Option::None))
    } else {
        quote!(::core::option::Option::None)
    };
    let read = if optional.is_some() {
        quote!(__loader.optional_field::<::std::string::String>(__prefix, &#spec_ident, __errors))
    } else {
        quote!(
            __loader
                .field::<::std::string::String>(__prefix, &#spec_ident, __errors)
                .map(::core::option::Option::Some)
        )
    };

    quote! {
        match #read {
            ::core::option::Option::Some(::core::option::Option::Some(__moso_text)) => {
                match <#target as ::core::str::FromStr>::from_str(__moso_text.as_str()) {
                    ::core::result::Result::Ok(__moso_parsed) => #wrap,
                    ::core::result::Result::Err(__moso_why) => {
                        let __moso_key = __prefix.child(#name);
                        __errors.push(#moso::BootError::InvalidConfig {
                            key: __moso_key.dotted(),
                            source: __loader
                                .value_for(&__moso_key, &#spec_ident)
                                .map_or_else(
                                    || ::std::string::String::from("default"),
                                    |__moso_found| ::std::string::ToString::to_string(
                                        &__moso_found.origin
                                    ),
                                ),
                            expected: ::std::string::String::from(#rendered_type),
                            found: ::std::format!(
                                "{:?} ({})",
                                __moso_text,
                                ::std::string::ToString::to_string(&__moso_why)
                            ),
                            note: ::core::option::Option::None,
                        });
                        ::core::option::Option::None
                    }
                }
            }
            ::core::option::Option::Some(::core::option::Option::None) => #absent,
            ::core::option::Option::None => ::core::option::Option::None,
        }
    }
}

/// The bound check a `#[config(range = ..)]` field runs after coercion.
///
/// A value that is present but out of range is recorded *and* cleared, so the
/// `?` in `load_nested` fails and the caller sees the problem in the same
/// report as every other one.
fn range_check(
    plan: &Plan<'_>,
    moso: &TokenStream,
    range: &RangeBound,
    optional: bool,
    spec_ident: &Ident,
) -> TokenStream {
    let binding = &plan.binding;
    let name = plan.name.to_string();
    let value = format_ident!("__moso_value");
    let condition = range.condition(&value);
    let expected = format!("{} in {}", numeric_noun(&plan.field.ty), range.rendered());

    let pattern = if optional {
        quote!(::core::option::Option::Some(::core::option::Option::Some(#value)))
    } else {
        quote!(::core::option::Option::Some(#value))
    };

    quote! {
        {
            let mut __moso_out_of_range = false;
            if let #pattern = &#binding {
                if !(#condition) {
                    let __moso_key = __prefix.child(#name);
                    __errors.push(#moso::BootError::InvalidConfig {
                        key: __moso_key.dotted(),
                        source: __loader
                            .value_for(&__moso_key, &#spec_ident)
                            .map_or_else(
                                || ::std::string::String::from("default"),
                                |__moso_found| ::std::string::ToString::to_string(
                                    &__moso_found.origin
                                ),
                            ),
                        expected: ::std::string::String::from(#expected),
                        found: ::std::string::ToString::to_string(#value),
                        note: ::core::option::Option::None,
                    });
                    __moso_out_of_range = true;
                }
            }
            if __moso_out_of_range {
                #binding = ::core::option::Option::None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expand a derive input and return the tokens as a searchable string.
    fn expand_str(input: proc_macro2::TokenStream) -> String {
        let parsed: DeriveInput = syn::parse2(input).expect("a derive input");
        expand(parsed).to_string()
    }

    #[test]
    fn a_plain_field_reads_through_coerce() {
        let out = expand_str(quote! {
            struct AppConfig {
                /// Human-readable service name.
                #[config(default = "shop")]
                pub name: String,
            }
        });
        assert!(out.contains("impl :: moso :: __private :: Config for AppConfig"));
        assert!(out.contains("FieldSpec :: new (\"name\""));
        assert!(out.contains(". default_value (\"shop\")"));
        assert!(out.contains("field :: < String >"));
        assert!(out.contains("Human-readable service name."), "{out}");
    }

    #[test]
    fn a_nested_field_becomes_a_section() {
        let out = expand_str(quote! {
            struct AppConfig {
                #[config(nested)]
                pub database: DatabaseConfig,
            }
        });
        assert!(out.contains("section :: < DatabaseConfig > (__prefix , \"database\""));
        assert!(
            out.contains("< DatabaseConfig as :: moso :: __private :: Config > :: descriptor ()")
        );
    }

    #[test]
    fn an_optional_field_uses_the_optional_reader() {
        let out = expand_str(quote! {
            struct AppConfig {
                pub port: Option<u16>,
            }
        });
        assert!(out.contains("optional_field :: < u16 >"), "{out}");
    }

    #[test]
    fn profile_defaults_are_selected_at_load_time() {
        let out = expand_str(quote! {
            struct AppConfig {
                #[config(default = false, profile(production = false, dev = true))]
                pub expose_docs: bool,
            }
        });
        assert!(out.contains("match __loader . profile ()"));
        assert!(
            out.contains("Profile :: Dev => __moso_spec_expose_docs . profile_default (\"true\")"),
            "{out}"
        );
        assert!(
            out.contains(
                "Profile :: Production => __moso_spec_expose_docs . profile_default (\"false\")"
            ),
            "{out}"
        );
        assert!(out.contains(". default_value (\"false\")"));
    }

    #[test]
    fn an_env_alias_reaches_both_the_spec_and_the_descriptor() {
        let out = expand_str(quote! {
            struct AppConfig {
                #[config(default = "info", env = "RUST_LOG")]
                pub log: String,
            }
        });
        assert!(out.contains(". env (\"RUST_LOG\")"));
        assert!(out.contains("env_alias : :: core :: option :: Option :: Some (\"RUST_LOG\")"));
    }

    #[test]
    fn a_secret_field_marks_the_spec_and_the_descriptor() {
        let out = expand_str(quote! {
            struct AppConfig {
                #[config(secret)]
                pub secret_key: SecretString,
            }
        });
        assert!(out.contains(". secret ()"));
        assert!(out.contains("secret : true"));
    }

    #[test]
    fn secret_on_a_plain_string_is_a_compile_error_with_a_fix() {
        let out = expand_str(quote! {
            struct AppConfig {
                #[config(secret)]
                pub secret_key: String,
            }
        });
        assert!(out.contains("compile_error !"), "{out}");
        assert!(out.contains("needs a secret type"), "{out}");
        assert!(out.contains("SecretString"), "{out}");
        // The placeholder keeps a downstream `App::new(cfg)` from adding a
        // second, misleading error.
        assert!(out.contains("impl :: moso :: __private :: Config for AppConfig"));
    }

    #[test]
    fn secret_on_an_optional_secret_is_accepted() {
        let out = expand_str(quote! {
            struct AppConfig {
                #[config(secret)]
                pub secret_key: Option<SecretString>,
            }
        });
        assert!(!out.contains("compile_error !"), "{out}");
    }

    #[test]
    fn reloadable_on_a_bare_type_is_a_compile_error() {
        let out = expand_str(quote! {
            struct AppConfig {
                #[config(reloadable, default = "info")]
                pub log: String,
            }
        });
        assert!(out.contains("compile_error !"));
        assert!(out.contains("pub log: Reloadable<String>,"), "{out}");
    }

    #[test]
    fn reloadable_on_a_nested_section_is_rejected() {
        let out = expand_str(quote! {
            struct AppConfig {
                #[config(nested, reloadable)]
                pub database: DatabaseConfig,
            }
        });
        assert!(out.contains("compile_error !"));
        assert!(
            out.contains("cannot be combined with `#[config(nested)]`"),
            "{out}"
        );
    }

    #[test]
    fn a_default_on_a_nested_section_is_rejected() {
        let out = expand_str(quote! {
            struct AppConfig {
                #[config(nested, default = "x")]
                pub database: DatabaseConfig,
            }
        });
        assert!(out.contains("compile_error !"));
    }

    #[test]
    fn a_range_renders_the_expected_type_and_checks_the_bound() {
        let out = expand_str(quote! {
            struct DatabaseConfig {
                #[config(default = 10, range = 1..=1000)]
                pub max_connections: u32,
            }
        });
        assert!(out.contains("\"integer in 1..=1000\""), "{out}");
        assert!(out.contains("BootError :: InvalidConfig"));
        assert!(out.contains("* __moso_value >= 1"), "{out}");
        assert!(out.contains("* __moso_value <= 1000"), "{out}");
    }

    #[test]
    fn an_exclusive_range_uses_a_strict_comparison() {
        let out = expand_str(quote! {
            struct C {
                #[config(range = 0..100)]
                pub ratio: u8,
            }
        });
        assert!(out.contains("* __moso_value < 100"), "{out}");
        assert!(out.contains("\"integer in 0..100\""), "{out}");
    }

    #[test]
    fn parse_goes_through_from_str() {
        let out = expand_str(quote! {
            struct C {
                #[config(parse, default = "1h")]
                pub window: Window,
            }
        });
        assert!(
            out.contains("< Window as :: core :: str :: FromStr > :: from_str"),
            "{out}"
        );
        assert!(
            out.contains("field :: < :: std :: string :: String >"),
            "{out}"
        );
    }

    #[test]
    fn an_unknown_key_is_rejected_with_a_suggestion() {
        let out = expand_str(quote! {
            struct C {
                #[config(defualt = "x")]
                pub name: String,
            }
        });
        assert!(out.contains("compile_error !"), "{out}");
        assert!(out.contains("Did you mean"), "{out}");
    }

    #[test]
    fn a_generic_config_is_rejected() {
        let out = expand_str(quote! {
            struct C<T> {
                pub name: T,
            }
        });
        assert!(out.contains("cannot be used on a generic type"), "{out}");
    }

    #[test]
    fn an_unsupported_secret_source_is_rejected() {
        let out = expand_str(quote! {
            struct C {
                #[config(secret_from = "vault")]
                pub key: SecretString,
            }
        });
        assert!(out.contains("secret_provider"), "{out}");
    }

    #[test]
    fn the_expansion_stays_within_twenty_lines_per_field() {
        // `docs/06-reference/62-macro-reference.md` budgets `#[derive(Config)]`
        // at 20 lines per field. Formatted output is what `xtask expand-size`
        // measures; token count per field is the proxy available here.
        let out = expand_str(quote! {
            struct AppConfig {
                #[config(default = "shop")]
                pub name: String,
                #[config(default = "0.0.0.0:3000")]
                pub bind: SocketAddr,
                pub public_url: Url,
                #[config(nested)]
                pub database: DatabaseConfig,
                #[config(secret)]
                pub secret_key: SecretString,
            }
        });
        let statements = out.matches(';').count();
        assert!(statements < 5 * 20, "{statements} statements for 5 fields");
    }
}
