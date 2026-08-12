//! `#[derive(Schema)]` and `#[derive(Constrained)]`.
//!
//! One type definition doing three jobs: a serde model, a set of runtime
//! constraints, and a JSON Schema 2020-12 document. The point of the derive is
//! that jobs two and three are generated from the *same* `#[schema(...)]`
//! attribute, so the documented constraint and the enforced constraint cannot
//! drift apart — there is only one of them.
//!
//! # What is generated
//!
//! | Item | Notes |
//! | --- | --- |
//! | `Serialize` / `Deserialize` | delegated to `serde`'s own derive, see below |
//! | `Validate` | the runtime half of every `#[schema(...)]` constraint |
//! | `Schema` | the documented half, plus `HAS_CONSTRAINTS` |
//! | `Debug` | only when a field is `#[schema(secret)]`, and it redacts |
//! | `IntoResponse` + `Describe` | so a handler can return the type directly |
//! | `From<Other>` | only with `#[schema(from = Other)]` |
//!
//! # How serde is generated without reimplementing serde
//!
//! A derive macro cannot add attributes to the item it is applied to, so the
//! `#[schema(...)]` vocabulary cannot be rewritten into `#[serde(...)]` in
//! place. Instead the expansion declares a **shadow type** inside an anonymous
//! `const`, carrying the translated serde attributes and marked
//! `#[serde(remote = "TheUserType")]`. Serde's own derive then generates
//! `Shadow::serialize(&Real, S)` and `Shadow::deserialize(D) -> Real` as
//! inherent functions, and the two real impls are one-line delegations (an
//! outline of the expansion, not a program):
//!
//! ```text
//! const _: () = {
//!     #[derive(Serialize, Deserialize)]
//!     #[serde(remote = "CreateUser", rename_all = "camelCase")]
//!     struct __MosoSerde { #[serde(rename = "userName")] username: String }
//!
//!     impl Serialize for CreateUser { /* __MosoSerde::serialize(self, s) */ }
//!     impl<'de> Deserialize<'de> for CreateUser { /* __MosoSerde::deserialize(d) */ }
//! };
//! ```
//!
//! This buys every serde feature — flatten, the four enum representations,
//! defaults, `deny_unknown_fields` — with no reimplementation and no risk of
//! behaving subtly differently from the rest of the ecosystem. The shadow lives
//! in the same module, so private fields are reachable and no visibility is
//! widened. `#[schema(no_serde)]` opts out for a type that writes its own.
//!
//! # Diagnostics
//!
//! Every mistake is one error, spanned at the user's token, with a `help:` line
//! that is code they can paste — see `docs/04-devex/41-diagnostics.md`. Parsing
//! never aborts: unrecognised input is recorded and skipped, and the expansion
//! still emits every impl, so one typo produces one error rather than a cascade
//! of "trait not implemented" errors at every use site.

use proc_macro2::{Literal, Span, TokenStream};
use quote::{ToTokens, format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{
    Attribute, Data, DataEnum, DataStruct, DeriveInput, Expr, ExprLit, ExprRange, Fields, Generics,
    Ident, Lit, Path, RangeLimits, Type,
};

use crate::util::attrs::{
    Diagnostics, RenameRule, did_you_mean, doc_text, escape_pointer_token, expr_as_path,
    expr_as_type, list, option_inner,
};

// ---------------------------------------------------------------------------
// The vocabulary
// ---------------------------------------------------------------------------

/// Every key `#[schema(...)]` accepts on a struct or enum.
const CONTAINER_KEYS: &[&str] = &[
    "rename",
    "rename_all",
    "deny_unknown",
    "from",
    "check",
    "title",
    "description",
    "tag",
    "content",
    "untagged",
    "deprecated",
    "example",
    "no_serde",
    "no_response",
];

/// Every key `#[schema(...)]` accepts on a field.
const FIELD_KEYS: &[&str] = &[
    "len",
    "pattern",
    "format",
    "trim",
    "lowercase",
    "uppercase",
    "non_empty",
    "contains",
    "starts_with",
    "ends_with",
    "range",
    "multiple_of",
    "positive",
    "non_negative",
    "unique",
    "each",
    "nested",
    "default",
    "rename",
    "skip",
    "read_only",
    "write_only",
    "secret",
    "deprecated",
    "example",
    "flatten",
    "title",
    "description",
    "enum_values",
    "delimiter",
    "flatten_bracket",
];

/// Every key `each(...)` accepts. Structural keys are meaningless on an
/// element, so they are rejected with a message that says where they belong.
const EACH_KEYS: &[&str] = &[
    "len",
    "pattern",
    "format",
    "non_empty",
    "contains",
    "starts_with",
    "ends_with",
    "range",
    "multiple_of",
    "positive",
    "non_negative",
    "nested",
    "enum_values",
];

/// Every key `#[constrained(...)]` accepts.
const CONSTRAINED_KEYS: &[&str] = &[
    "inner",
    "name",
    "len",
    "pattern",
    "format",
    "trim",
    "lowercase",
    "uppercase",
    "non_empty",
    "contains",
    "starts_with",
    "ends_with",
    "range",
    "multiple_of",
    "positive",
    "non_negative",
    "check",
    "title",
    "description",
    "secret",
];

/// The `format` names [`moso_schema::checks::is_valid_format`] enforces.
///
/// A name outside this list is legal — JSON Schema treats an unknown format as
/// an annotation — so it is accepted silently. A name *close* to one of these
/// is a typo and is rejected, which is the only shape of this mistake worth an
/// error.
const KNOWN_FORMATS: &[&str] = &[
    "email",
    "uri",
    "uuid",
    "hostname",
    "ipv4",
    "ipv6",
    "date",
    "date-time",
    "time",
    "duration",
    "password",
    "byte",
    "binary",
    "int32",
    "int64",
    "float",
    "double",
    "json-pointer",
    "regex",
];

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Expand `#[derive(Schema)]`.
pub(crate) fn derive_schema(input: DeriveInput) -> TokenStream {
    let mut errors = Diagnostics::new();

    if let Some(lifetime) = input.generics.lifetimes().next() {
        errors.help(
            lifetime.span(),
            "a schema type cannot borrow",
            "`Schema` requires `'static`, because a document outlives the request that produced \
             it — own the data: `String` instead of `&str`, `Vec<T>` instead of `&[T]`",
        );
    }

    let container = Container::parse(&input, &mut errors);

    let expansion = match &input.data {
        Data::Struct(data) => expand_struct(&container, data, &mut errors),
        Data::Enum(data) => expand_enum(&container, data, &mut errors),
        Data::Union(_) => {
            errors.help(
                input.ident.span(),
                "`Schema` cannot be derived for a union",
                "a union has no JSON representation — use an enum:\n    #[derive(Schema)]\n    \
                 #[schema(tag = \"kind\")]\n    pub enum MyType { /* … */ }",
            );
            TokenStream::new()
        }
    };

    errors.finish(expansion)
}

/// Expand `#[derive(Constrained)]`.
pub(crate) fn derive_constrained(input: DeriveInput) -> TokenStream {
    let mut errors = Diagnostics::new();
    let expansion = expand_constrained(&input, &mut errors);
    errors.finish(expansion)
}

/// The path every generated item resolves against.
///
/// Generated code never names a runtime crate: `moso-core` can be refactored,
/// split or renamed without touching this file and without a user's expanded
/// code breaking.
fn private() -> TokenStream {
    quote!(::moso::__private)
}

// ---------------------------------------------------------------------------
// Container attributes
// ---------------------------------------------------------------------------

/// How an enum is represented on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Repr {
    /// serde's default: `{"created": {...}}`.
    External,
    /// `#[schema(tag = "kind")]` — `{"kind": "created", ...}`.
    Internal(String),
    /// `#[schema(tag = "kind", content = "data")]`.
    Adjacent(String, String),
    /// `#[schema(untagged)]`.
    Untagged,
}

/// The parsed `#[schema(...)]` attributes of a struct or enum.
struct Container {
    ident: Ident,
    generics: Generics,
    /// The name this type occupies in `components/schemas`.
    schema_name: String,
    /// The serde name, when `rename` was given.
    rename: Option<String>,
    title: Option<String>,
    description: Option<String>,
    rename_all: Option<RenameRule>,
    deny_unknown: Option<Span>,
    from: Vec<Type>,
    checks: Vec<Path>,
    tag: Option<(String, Span)>,
    content: Option<(String, Span)>,
    untagged: Option<Span>,
    deprecated: Option<Option<String>>,
    examples: Vec<Expr>,
    no_serde: bool,
    no_response: bool,
}

impl Container {
    fn parse(input: &DeriveInput, errors: &mut Diagnostics) -> Self {
        let mut this = Self {
            ident: input.ident.clone(),
            generics: input.generics.clone(),
            schema_name: input.ident.to_string(),
            rename: None,
            title: None,
            description: doc_text(&input.attrs),
            rename_all: None,
            deny_unknown: None,
            from: Vec::new(),
            checks: Vec::new(),
            tag: None,
            content: None,
            untagged: None,
            deprecated: None,
            examples: Vec::new(),
            no_serde: false,
            // `#[derive(Schema, Responder)]` is the documented way to return a
            // type with a status other than 200, and both derives generate
            // `IntoResponse` + `Describe` — so emitting ours unconditionally
            // makes the documented form a coherence error on a span the user
            // cannot act on. A derive cannot see its siblings, but it can see
            // the item's attributes, and `#[responder(..)]` is exactly the
            // marker that says "the other one is producing them". Explicit
            // `#[schema(no_response)]` still works, and is what a type with a
            // hand-written `IntoResponse` uses.
            no_response: input.attrs.iter().any(|a| a.path().is_ident("responder")),
        };

        for attr in input.attrs.iter().filter(|a| a.path().is_ident("schema")) {
            let result = attr.parse_nested_meta(|meta| {
                let Some(key) = meta.path.get_ident().map(ToString::to_string) else {
                    errors.help(
                        meta.path.span(),
                        "a `schema` attribute key must be a plain name",
                        format!("the accepted keys are: {}", list(CONTAINER_KEYS)),
                    );
                    return Ok(());
                };

                match key.as_str() {
                    "rename" => {
                        let value = string_value(&meta, "rename", errors);
                        if let Some(value) = value {
                            this.schema_name.clone_from(&value);
                            this.rename = Some(value);
                        }
                    }
                    "rename_all" => {
                        if let Some(value) = string_value(&meta, "rename_all", errors) {
                            match RenameRule::parse(&value) {
                                Some(rule) => this.rename_all = Some(rule),
                                None => {
                                    let message = format!("unknown case convention `{value}`");
                                    match did_you_mean(&value, RenameRule::NAMES) {
                                        Some(suggestion) => errors.help(
                                            meta.path.span(),
                                            message,
                                            format!("did you mean `{suggestion}`?"),
                                        ),
                                        None => errors.help(
                                            meta.path.span(),
                                            message,
                                            format!(
                                                "the conventions are: {}",
                                                list(RenameRule::NAMES)
                                            ),
                                        ),
                                    }
                                }
                            }
                        }
                    }
                    "deny_unknown" => this.deny_unknown = Some(meta.path.span()),
                    "no_serde" => this.no_serde = true,
                    "no_response" => this.no_response = true,
                    "untagged" => this.untagged = Some(meta.path.span()),
                    "tag" => {
                        if let Some(value) = string_value(&meta, "tag", errors) {
                            this.tag = Some((value, meta.path.span()));
                        }
                    }
                    "content" => {
                        if let Some(value) = string_value(&meta, "content", errors) {
                            this.content = Some((value, meta.path.span()));
                        }
                    }
                    "title" => this.title = string_value(&meta, "title", errors),
                    "description" => {
                        this.description = string_value(&meta, "description", errors);
                    }
                    "from" => {
                        if let Some(expr) = expr_value(&meta, "from", errors) {
                            match expr_as_type(&expr, "from") {
                                Ok(ty) => this.from.push(ty),
                                Err(error) => errors.push(error),
                            }
                        }
                    }
                    "check" => {
                        if let Some(expr) = expr_value(&meta, "check", errors) {
                            match expr_as_path(&expr, "check") {
                                Ok(path) => this.checks.push(path),
                                Err(error) => errors.push(error),
                            }
                        }
                    }
                    "deprecated" => {
                        this.deprecated = Some(if meta.input.peek(syn::Token![=]) {
                            string_value(&meta, "deprecated", errors)
                        } else {
                            None
                        });
                    }
                    "example" => {
                        if let Some(expr) = expr_value(&meta, "example", errors) {
                            this.examples.push(expr);
                        }
                    }
                    other => {
                        errors.unknown_key(meta.path.span(), "schema", other, CONTAINER_KEYS);
                        // Swallow a value if there is one, so the rest of the
                        // list still parses and the user sees every mistake.
                        let _ = meta.value().and_then(|v| v.parse::<Expr>());
                    }
                }
                Ok(())
            });
            if let Err(error) = result {
                errors.push(error);
            }
        }

        if this.content.is_some() && this.tag.is_none() {
            let span = this.content.as_ref().map_or_else(Span::call_site, |c| c.1);
            errors.help(
                span,
                "`content` needs a `tag`",
                "an adjacently tagged enum names both:\n    #[schema(tag = \"kind\", content = \
                 \"data\")]",
            );
        }
        if let (Some(span), Some(_)) = (this.untagged, &this.tag) {
            errors.help(
                span,
                "an enum cannot be both tagged and untagged",
                "keep `tag = \"…\"` for a discriminated union, or `untagged` for one matched by \
                 shape",
            );
        }

        this
    }

    /// The wire representation implied by `tag` / `content` / `untagged`.
    fn repr(&self) -> Repr {
        match (&self.tag, &self.content, self.untagged) {
            (_, _, Some(_)) => Repr::Untagged,
            (Some((tag, _)), Some((content, _)), _) => Repr::Adjacent(tag.clone(), content.clone()),
            (Some((tag, _)), None, _) => Repr::Internal(tag.clone()),
            _ => Repr::External,
        }
    }

    /// The `#[serde(...)]` container attributes the shadow type carries.
    fn serde_attrs(&self) -> TokenStream {
        let mut entries: Vec<TokenStream> = Vec::new();
        if let Some(rule) = self.rename_all {
            let name = rename_rule_name(rule);
            entries.push(quote!(rename_all = #name));
        }
        if let Some(rename) = &self.rename {
            entries.push(quote!(rename = #rename));
        }
        if self.deny_unknown.is_some() {
            entries.push(quote!(deny_unknown_fields));
        }
        match self.repr() {
            Repr::External => {}
            Repr::Internal(tag) => entries.push(quote!(tag = #tag)),
            Repr::Adjacent(tag, content) => {
                entries.push(quote!(tag = #tag, content = #content));
            }
            Repr::Untagged => entries.push(quote!(untagged)),
        }
        quote!(#(#entries),*)
    }

    /// `impl<T: Schema> … for Type<T>` — every generated impl shares these.
    ///
    /// One bound covers all four traits: `Schema` is a subtrait of
    /// `Serialize + DeserializeOwned + Validate + Send + Sync + 'static`, so
    /// `T: Schema` is exactly what a generic model needs and nothing more.
    fn split_generics(
        &self,
        extra: Option<TokenStream>,
    ) -> (TokenStream, TokenStream, TokenStream) {
        let p = private();
        let mut generics = self.generics.clone();
        for param in &mut generics.params {
            if let syn::GenericParam::Type(ty) = param {
                ty.bounds.push(syn::parse_quote!(#p::Schema));
                if let Some(extra) = &extra {
                    let bound: syn::TypeParamBound = syn::parse_quote!(#extra);
                    ty.bounds.push(bound);
                }
            }
        }
        let (impl_generics, _, where_clause) = generics.split_for_impl();
        let (_, ty_generics, _) = self.generics.split_for_impl();
        (
            impl_generics.to_token_stream(),
            ty_generics.to_token_stream(),
            where_clause.to_token_stream(),
        )
    }

    /// The `schema_name()` body: a literal, or the documented mangling for a
    /// generic type so `Page<UserOut>` is stably `Page_UserOut`.
    fn schema_name_body(&self) -> TokenStream {
        let p = private();
        let base = &self.schema_name;
        let params: Vec<&Ident> = self
            .generics
            .params
            .iter()
            .filter_map(|param| match param {
                syn::GenericParam::Type(ty) => Some(&ty.ident),
                _ => None,
            })
            .collect();
        if params.is_empty() {
            quote!(::std::borrow::Cow::Borrowed(#base))
        } else {
            quote! {
                #p::generic_schema_name(#base, &[#(<#params as #p::Schema>::schema_name()),*])
            }
        }
    }
}

/// The serde spelling of a case convention.
fn rename_rule_name(rule: RenameRule) -> &'static str {
    match rule {
        RenameRule::Lower => "lowercase",
        RenameRule::Upper => "UPPERCASE",
        RenameRule::Pascal => "PascalCase",
        RenameRule::Camel => "camelCase",
        RenameRule::Snake => "snake_case",
        RenameRule::ScreamingSnake => "SCREAMING_SNAKE_CASE",
        RenameRule::Kebab => "kebab-case",
        RenameRule::ScreamingKebab => "SCREAMING-KEBAB-CASE",
    }
}

// ---------------------------------------------------------------------------
// Nested-meta helpers
// ---------------------------------------------------------------------------

/// `key = "value"`.
fn string_value(
    meta: &syn::meta::ParseNestedMeta<'_>,
    key: &str,
    errors: &mut Diagnostics,
) -> Option<String> {
    let expr = expr_value(meta, key, errors)?;
    match &expr {
        Expr::Lit(ExprLit {
            lit: Lit::Str(text),
            ..
        }) => Some(text.value()),
        other => {
            errors.help(
                other.span(),
                format!("`{key}` needs a string"),
                format!("write it as `{key} = \"…\"`"),
            );
            None
        }
    }
}

/// `key = <expr>`.
fn expr_value(
    meta: &syn::meta::ParseNestedMeta<'_>,
    key: &str,
    errors: &mut Diagnostics,
) -> Option<Expr> {
    match meta.value().and_then(|stream| stream.parse::<Expr>()) {
        Ok(expr) => Some(expr),
        Err(error) => {
            errors.help(
                error.span(),
                format!("`{key}` needs a value"),
                format!("write it as `{key} = …`"),
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Constraints — parsed once, emitted twice
// ---------------------------------------------------------------------------

/// A numeric literal from a range or a `multiple_of`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Num {
    /// An integer literal, possibly negative.
    Int(i128),
    /// A floating-point literal.
    Float(f64),
}

impl Num {
    /// The value as `f64`, for a float-typed field.
    fn as_f64(self) -> f64 {
        match self {
            Self::Int(v) => v as f64,
            Self::Float(v) => v,
        }
    }

    /// The value as `i128`, or `None` when it has a fractional part.
    fn as_int(self) -> Option<i128> {
        match self {
            Self::Int(v) => Some(v),
            Self::Float(v) if v.fract() == 0.0 => Some(v as i128),
            Self::Float(_) => None,
        }
    }

    /// The literal as a `serde_json::Number`, for the JSON Schema keyword.
    fn to_json_number(self) -> TokenStream {
        let p = private();
        match self {
            Self::Int(v) => {
                let lit = Literal::i64_suffixed(
                    v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
                );
                quote!(::core::option::Option::Some(#p::serde_json::Number::from(#lit)))
            }
            Self::Float(v) => {
                let lit = Literal::f64_suffixed(v);
                quote!(#p::serde_json::Number::from_f64(#lit))
            }
        }
    }
}

/// An inclusive size range, in characters or in elements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LenRange {
    min: Option<u64>,
    max: Option<u64>,
}

/// A numeric range, carrying whichever bounds are exclusive.
#[derive(Clone, Copy, Debug)]
struct NumRange {
    min: Option<Num>,
    max: Option<Num>,
    /// Rust range syntax cannot express an exclusive *lower* bound; that only
    /// ever comes from `positive`, which [`effective_range`] folds in.
    exclusive_max: bool,
    span: Span,
}

/// A compile-time-validated regular expression.
#[derive(Clone, Debug)]
struct Pattern {
    source: String,
    span: Span,
}

/// Everything one `#[schema(...)]` list says about one value.
///
/// Parsed once; [`emit_checks`] turns it into the runtime half and
/// [`emit_schema_constraints`] into the documented half. That is the whole
/// design: the two cannot disagree because they read the same struct.
#[derive(Clone, Debug, Default)]
struct Constraints {
    len: Option<(LenRange, Span)>,
    range: Option<NumRange>,
    pattern: Option<Pattern>,
    format: Option<(String, Span)>,
    contains: Option<(String, Span)>,
    starts_with: Option<(String, Span)>,
    ends_with: Option<(String, Span)>,
    non_empty: Option<Span>,
    trim: bool,
    lowercase: bool,
    uppercase: bool,
    multiple_of: Option<(Num, Span)>,
    positive: Option<Span>,
    non_negative: Option<Span>,
    unique: Option<Span>,
    enum_values: Option<(Vec<Expr>, Span)>,
    nested: Option<Span>,
}

impl Constraints {
    /// True when nothing here reaches the wire — the input for
    /// `HAS_CONSTRAINTS`.
    fn is_empty(&self) -> bool {
        self.len.is_none()
            && self.range.is_none()
            && self.pattern.is_none()
            && self.format.is_none()
            && self.contains.is_none()
            && self.starts_with.is_none()
            && self.ends_with.is_none()
            && self.non_empty.is_none()
            && self.multiple_of.is_none()
            && self.positive.is_none()
            && self.non_negative.is_none()
            && self.unique.is_none()
            && self.enum_values.is_none()
            && self.nested.is_none()
    }

    /// True when a value has to be rewritten on the way in.
    fn normalises(&self) -> bool {
        self.trim || self.lowercase || self.uppercase
    }

    /// The bitmask [`emit_normalise_trait`] understands.
    fn normalise_mask(&self) -> u8 {
        u8::from(self.trim) | (u8::from(self.lowercase) << 1) | (u8::from(self.uppercase) << 2)
    }

    /// The sentence appended to a normalising field's description, because the
    /// JSON Schema vocabulary has no keyword for "the server trims this".
    fn normalise_note(&self) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        if self.trim {
            parts.push("trimmed of leading and trailing whitespace");
        }
        if self.lowercase {
            parts.push("converted to lowercase");
        }
        if self.uppercase {
            parts.push("converted to uppercase");
        }
        match parts.as_slice() {
            [] => None,
            [one] => Some(format!("The value is {one} when it is received.")),
            [rest @ .., last] => Some(format!(
                "The value is {} and {last} when it is received.",
                rest.join(", ")
            )),
        }
    }

    /// Parse one key into `self`. Returns `false` when the key is not part of
    /// the constraint vocabulary, so the caller can try its own keys.
    fn parse_key(
        &mut self,
        key: &str,
        meta: &syn::meta::ParseNestedMeta<'_>,
        errors: &mut Diagnostics,
    ) -> bool {
        let span = meta.path.span();
        match key {
            "len" => {
                if let Some(range) = parse_len(meta, errors) {
                    self.len = Some((range, span));
                }
            }
            "range" => {
                if let Some(range) = parse_range(meta, errors) {
                    self.range = Some(range);
                }
            }
            "pattern" => {
                if let Some(pattern) = parse_pattern(meta, errors) {
                    self.pattern = Some(pattern);
                }
            }
            "format" => {
                if let Some(value) = string_value(meta, "format", errors) {
                    if !KNOWN_FORMATS.contains(&value.as_str())
                        && let Some(suggestion) = did_you_mean(&value, KNOWN_FORMATS)
                    {
                        errors.help(
                            span,
                            format!("unknown format `{value}`"),
                            format!("did you mean `{suggestion}`?"),
                        );
                    }
                    self.format = Some((value, span));
                }
            }
            "contains" => {
                if let Some(value) = string_value(meta, "contains", errors) {
                    self.contains = Some((value, span));
                }
            }
            "starts_with" => {
                if let Some(value) = string_value(meta, "starts_with", errors) {
                    self.starts_with = Some((value, span));
                }
            }
            "ends_with" => {
                if let Some(value) = string_value(meta, "ends_with", errors) {
                    self.ends_with = Some((value, span));
                }
            }
            "non_empty" => self.non_empty = Some(span),
            "trim" => self.trim = true,
            "lowercase" => self.lowercase = true,
            "uppercase" => self.uppercase = true,
            "multiple_of" => {
                if let Some(expr) = expr_value(meta, "multiple_of", errors)
                    && let Some(num) = literal_number(&expr, errors)
                {
                    self.multiple_of = Some((num, span));
                }
            }
            "positive" => self.positive = Some(span),
            "non_negative" => self.non_negative = Some(span),
            "unique" => self.unique = Some(span),
            "nested" => self.nested = Some(span),
            "enum_values" => {
                if let Some(expr) = expr_value(meta, "enum_values", errors) {
                    match &expr {
                        Expr::Array(array) => {
                            self.enum_values = Some((array.elems.iter().cloned().collect(), span));
                        }
                        other => errors.help(
                            other.span(),
                            "`enum_values` needs a list",
                            "write it as `enum_values = [\"draft\", \"published\"]`",
                        ),
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// Reject combinations that cannot both be true, before they become two
    /// contradictory keywords in the document.
    fn validate(&self, errors: &mut Diagnostics) {
        if let (Some(positive), Some(_)) = (self.positive, self.non_negative) {
            errors.help(
                positive,
                "`positive` and `non_negative` say different things",
                "`positive` is `> 0`, `non_negative` is `>= 0` — keep the one you meant",
            );
        }
        if let (Some((range, span)), Some(_)) = (self.len, self.non_empty)
            && range.min.is_some()
        {
            errors.help(
                span,
                "`non_empty` is already implied by `len`",
                format!(
                    "`len = {}..` says the same thing — remove `non_empty`",
                    range.min.unwrap_or(1)
                ),
            );
        }
        if let Some((range, span)) = self.len
            && let (Some(min), Some(max)) = (range.min, range.max)
            && min > max
        {
            errors.help(
                span,
                format!("this length range is empty: {min} is greater than {max}"),
                format!("did you mean `len = {max}..={min}`?"),
            );
        }
        if let Some(range) = &self.range
            && let (Some(min), Some(max)) = (range.min, range.max)
            && min.as_f64() > max.as_f64()
        {
            errors.help(
                range.span,
                "this range is empty: the lower bound is above the upper bound",
                "swap the two bounds",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Range parsing — real Rust syntax, so a bound cannot be a malformed string
// ---------------------------------------------------------------------------

/// `len = 3..=32`, `len = 1..`, `len = ..=10`, `len = 5`.
///
/// A half-open upper bound is lowered to its inclusive equivalent (`1..10`
/// becomes a maximum of 9), because a length is a whole number of characters
/// and JSON Schema's `maxLength` is inclusive.
fn parse_len(meta: &syn::meta::ParseNestedMeta<'_>, errors: &mut Diagnostics) -> Option<LenRange> {
    let expr = expr_value(meta, "len", errors)?;
    match &expr {
        Expr::Range(range) => {
            let (min, max, exclusive_max) = range_bounds(range, "len", errors)?;
            let min = match min {
                Some(n) => Some(non_negative_size(n, "len", errors)?),
                None => None,
            };
            let max = match max {
                Some(n) => Some(non_negative_size(n, "len", errors)?),
                None => None,
            };
            let max = match (max, exclusive_max) {
                (Some(0), true) => {
                    errors.help(
                        range.span(),
                        "this length range is empty",
                        "`..0` excludes every length — write `len = ..=0` for \"must be empty\"",
                    );
                    Some(0)
                }
                (Some(v), true) => Some(v - 1),
                (other, _) => other,
            };
            Some(LenRange { min, max })
        }
        other => {
            let num = literal_number(other, errors)?;
            let exact = non_negative_size(num, "len", errors)?;
            Some(LenRange {
                min: Some(exact),
                max: Some(exact),
            })
        }
    }
}

/// `range = 1..=100`, `range = 0.0..1.0`, `range = 13..`.
fn parse_range(
    meta: &syn::meta::ParseNestedMeta<'_>,
    errors: &mut Diagnostics,
) -> Option<NumRange> {
    let expr = expr_value(meta, "range", errors)?;
    match &expr {
        Expr::Range(range) => {
            let span = range.span();
            let (min, max, exclusive_max) = range_bounds(range, "range", errors)?;
            Some(NumRange {
                min,
                max,
                exclusive_max,
                span,
            })
        }
        other => {
            errors.help(
                other.span(),
                "`range` needs a range",
                "write it as `range = 1..=100`, `range = 1..` or `range = ..=100`",
            );
            None
        }
    }
}

/// The two endpoints of a range expression, and whether the upper one is
/// exclusive.
fn range_bounds(
    range: &ExprRange,
    key: &str,
    errors: &mut Diagnostics,
) -> Option<(Option<Num>, Option<Num>, bool)> {
    if range.start.is_none() && range.end.is_none() {
        errors.help(
            range.span(),
            format!("`{key}` needs at least one bound"),
            format!("write it as `{key} = 1..=10`, `{key} = 1..` or `{key} = ..=10`"),
        );
        return None;
    }
    let start = match &range.start {
        Some(expr) => Some(literal_number(expr, errors)?),
        None => None,
    };
    let end = match &range.end {
        Some(expr) => Some(literal_number(expr, errors)?),
        None => None,
    };
    Some((start, end, matches!(range.limits, RangeLimits::HalfOpen(_))))
}

/// A numeric literal, with `-` allowed in front of it.
fn literal_number(expr: &Expr, errors: &mut Diagnostics) -> Option<Num> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Int(int), ..
        }) => match int.base10_parse::<i128>() {
            Ok(value) => Some(Num::Int(value)),
            Err(_) => {
                errors.help(
                    int.span(),
                    "this number is too large to be a bound",
                    "bounds are limited to the range of `i64`",
                );
                None
            }
        },
        Expr::Lit(ExprLit {
            lit: Lit::Float(float),
            ..
        }) => match float.base10_parse::<f64>() {
            Ok(value) => Some(Num::Float(value)),
            Err(_) => {
                errors.error(float.span(), "this is not a number a bound can use");
                None
            }
        },
        Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => {
            match literal_number(&unary.expr, errors)? {
                Num::Int(value) => Some(Num::Int(-value)),
                Num::Float(value) => Some(Num::Float(-value)),
            }
        }
        Expr::Group(group) => literal_number(&group.expr, errors),
        Expr::Paren(paren) => literal_number(&paren.expr, errors),
        other => {
            errors.help(
                other.span(),
                "a bound must be a literal number",
                "write the number itself — a `const` cannot be read from an attribute:\n    \
                 #[schema(range = 1..=100)]",
            );
            None
        }
    }
}

/// A size bound: whole, and not negative.
fn non_negative_size(num: Num, key: &str, errors: &mut Diagnostics) -> Option<u64> {
    let Some(value) = num.as_int() else {
        errors.help(
            Span::call_site(),
            format!("`{key}` counts whole characters or elements"),
            format!("write `{key} = 1..=10`, not a fraction"),
        );
        return None;
    };
    match u64::try_from(value) {
        Ok(value) => Some(value),
        Err(_) => {
            errors.help(
                Span::call_site(),
                format!("`{key}` cannot be negative"),
                format!("a length starts at zero: `{key} = 0..=10`"),
            );
            None
        }
    }
}

/// `pattern = r"^[a-z0-9_]+$"`, compiled by the macro so an invalid expression
/// is a compile error at the literal rather than a panic on the first request.
fn parse_pattern(
    meta: &syn::meta::ParseNestedMeta<'_>,
    errors: &mut Diagnostics,
) -> Option<Pattern> {
    let expr = expr_value(meta, "pattern", errors)?;
    let Expr::Lit(ExprLit {
        lit: Lit::Str(text),
        ..
    }) = &expr
    else {
        errors.help(
            expr.span(),
            "`pattern` needs a string literal",
            "write it as `pattern = r\"^[a-z0-9_]+$\"` — a raw string keeps the backslashes",
        );
        return None;
    };

    let source = text.value();
    if let Err(error) = regex::Regex::new(&source) {
        errors.help(
            text.span(),
            format!(
                "this regular expression does not compile: {}",
                regex_detail(&error.to_string())
            ),
            "fix the expression, or use `format = \"…\"` if a named format says the same thing:\n    \
             #[schema(format = \"email\")]",
        );
        return None;
    }

    Some(Pattern {
        source,
        span: text.span(),
    })
}

/// The one-line reason out of a `regex` parse error.
///
/// `regex::Error`'s `Display` is a four-line block — a banner, the offending
/// expression, a caret, and the reason — laid out for a terminal that owns the
/// whole width. Inside a `syn::Error` it is re-indented and the caret no longer
/// lines up with anything, so only the last line is worth keeping. rustc has
/// already underlined the literal; what it cannot say is *why*.
fn regex_detail(rendered: &str) -> String {
    rendered
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("error: "))
        .map_or_else(
            || {
                rendered
                    .lines()
                    .next_back()
                    .unwrap_or("the expression is not valid")
                    .trim()
                    .to_owned()
            },
            str::to_owned,
        )
}

// ---------------------------------------------------------------------------
// Fields
// ---------------------------------------------------------------------------

/// What a field's type looks like, as far as the macro can tell syntactically.
///
/// Syntax is not types — `type Name = String` is opaque here — so the mapping
/// is deliberately conservative and every branch that could guess wrong instead
/// produces a hand-written error naming the field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// `String`, and anything unrecognised: the checks go through `AsRef<str>`.
    Text,
    /// `Vec<T>`, `VecDeque<T>`, `HashSet<T>`, `BTreeSet<T>`, `[T; N]`.
    Sequence,
    /// `HashMap<K, V>`, `BTreeMap<K, V>`, `IndexMap<K, V>`.
    Map,
    /// A signed integer primitive.
    SignedInt,
    /// An unsigned integer primitive.
    UnsignedInt,
    /// `f32` or `f64`.
    Float,
    /// `bool`.
    Bool,
}

impl Shape {
    /// Classify a field type, seeing through `Option<T>` and `Box<T>`.
    fn of(ty: &Type) -> Self {
        let ty = peel(ty);
        if matches!(ty, Type::Array(_) | Type::Slice(_)) {
            return Self::Sequence;
        }
        let Some(ident) = crate::util::attrs::type_ident(ty) else {
            return Self::Text;
        };
        let name = ident.to_string();
        match name.as_str() {
            "Vec" | "VecDeque" | "HashSet" | "BTreeSet" | "IndexSet" => Self::Sequence,
            "HashMap" | "BTreeMap" | "IndexMap" => Self::Map,
            "i8" | "i16" | "i32" | "i64" | "i128" | "isize" => Self::SignedInt,
            "u8" | "u16" | "u32" | "u64" | "u128" | "usize" => Self::UnsignedInt,
            "f32" | "f64" => Self::Float,
            "bool" => Self::Bool,
            _ => Self::Text,
        }
    }

    /// True when the value has a length rather than a magnitude.
    fn is_collection(self) -> bool {
        matches!(self, Self::Sequence | Self::Map)
    }

    /// True when a numeric constraint can be lowered to a cast and a check.
    fn is_numeric(self) -> bool {
        matches!(self, Self::SignedInt | Self::UnsignedInt | Self::Float)
    }

    /// The English name used in error messages.
    fn describe(self) -> &'static str {
        match self {
            Self::Text => "a string",
            Self::Sequence => "a collection",
            Self::Map => "a map",
            Self::SignedInt | Self::UnsignedInt => "an integer",
            Self::Float => "a number",
            Self::Bool => "a boolean",
        }
    }
}

/// Strip the wrappers that do not change how a value is validated.
fn peel(ty: &Type) -> &Type {
    let mut current = ty;
    loop {
        let next = crate::util::attrs::generic_inner(current, &["Option", "Box", "Arc", "Rc"]);
        match next {
            Some(inner) => current = inner,
            None => return current,
        }
    }
}

// ---------------------------------------------------------------------------
// Secrecy is a property of the type
// ---------------------------------------------------------------------------

/// Types that cannot print, log or serialise themselves by accident.
///
/// Matched by name because a proc macro sees syntax, not types. A user newtype
/// is not on the list and is not rejected either: [`check_secret_type`] only
/// complains about the types it can *prove* are leaky.
const SECRET_TYPES: &[&str] = &["Password", "SecretString", "SecretBytes", "Secret"];

/// Types whose whole purpose is to print themselves.
///
/// `#[schema(secret)]` on one of these is the mistake this check exists for: it
/// redacts the derived `Debug` of the *containing* struct and nothing else, so
/// the value still reaches the first log line that formats the field directly.
const LEAKY_TYPES: &[&str] = &["String", "str", "Cow", "Box", "Vec", "PathBuf", "Path"];

/// `#[schema(secret)]` on a type that can still print itself is a compile
/// error, for the same reason `#[config(secret)]` is (`18-configuration.md`,
/// acceptance criterion 7): the marker is a documentation and `Debug` hint, not
/// a containment boundary.
fn check_secret_type(ty: &Type, name: &str, errors: &mut Diagnostics) {
    let bare = peel(ty);
    let Some(ident) = crate::util::attrs::type_ident(bare) else {
        return;
    };
    let rendered = ident.to_string();
    if SECRET_TYPES.contains(&rendered.as_str()) || !LEAKY_TYPES.contains(&rendered.as_str()) {
        return;
    }

    // `Password` is the inbound shape (it has no `Display` and no
    // `AsRef<str>`, so `len`/`pattern` still work through `expose()`);
    // `SecretBytes` is the one for a `Vec<u8>`.
    let suggestion = if rendered == "Vec" {
        "SecretBytes"
    } else {
        "Password"
    };
    let wrapped = if option_inner(ty).is_some() {
        format!("Option<{suggestion}>")
    } else {
        suggestion.to_owned()
    };
    errors.help(
        ty.span(),
        format!(
            "`#[schema(secret)]` needs a secret type, and `{rendered}` is not one\n\n\
             note: `secret` redacts this struct's `Debug`; the `{rendered}` itself still prints \
             everywhere else\n\
             note: `Password` is the inbound shape; `SecretString` is the one to hold and compare"
        ),
        format!("change the field's type:\n    #[schema(secret)]\n    pub {name}: {wrapped},"),
    );
}

/// A default value: the flag form, or an expression.
#[derive(Clone, Debug)]
enum Default_ {
    /// `#[schema(default)]` — `Default::default()`.
    Trait,
    /// `#[schema(default = expr)]`.
    Expr(Box<Expr>),
}

/// One field of a struct, or of an enum variant.
struct FieldSpec {
    /// `None` for a tuple field.
    ident: Option<Ident>,
    /// The position, used to name a tuple field's accessor.
    index: usize,
    ty: Type,
    /// The name on the wire, after `rename` and `rename_all`.
    wire_name: String,
    /// The JSON Pointer token, RFC 6901 escaped.
    pointer: String,
    description: Option<String>,
    title: Option<String>,
    shape: Shape,
    optional: bool,
    skip: Option<Span>,
    flatten: Option<Span>,
    default: Option<Default_>,
    read_only: bool,
    write_only: bool,
    secret: Option<Span>,
    deprecated: Option<Option<String>>,
    examples: Vec<Expr>,
    delimiter: Option<(char, Span)>,
    flatten_bracket: bool,
    constraints: Constraints,
    each: Option<(Constraints, Span)>,
    /// The user's own attributes that are not `#[schema(...)]` or `#[doc]`,
    /// forwarded to the shadow so `#[cfg(...)]` keeps working.
    forwarded: Vec<Attribute>,
}

impl FieldSpec {
    fn parse(
        field: &syn::Field,
        index: usize,
        rename_all: Option<RenameRule>,
        errors: &mut Diagnostics,
    ) -> Self {
        let raw_name = field
            .ident
            .as_ref()
            .map_or_else(|| index.to_string(), ToString::to_string);
        let mut this = Self {
            ident: field.ident.clone(),
            index,
            ty: field.ty.clone(),
            wire_name: rename_all.map_or_else(|| raw_name.clone(), |rule| rule.apply(&raw_name)),
            pointer: String::new(),
            description: doc_text(&field.attrs),
            title: None,
            shape: Shape::of(&field.ty),
            optional: option_inner(&field.ty).is_some(),
            skip: None,
            flatten: None,
            default: None,
            read_only: false,
            write_only: false,
            secret: None,
            deprecated: None,
            examples: Vec::new(),
            delimiter: None,
            flatten_bracket: false,
            constraints: Constraints::default(),
            each: None,
            forwarded: field
                .attrs
                .iter()
                .filter(|a| !a.path().is_ident("schema") && !a.path().is_ident("doc"))
                .cloned()
                .collect(),
        };

        for attr in field.attrs.iter().filter(|a| a.path().is_ident("schema")) {
            let result = attr.parse_nested_meta(|meta| {
                let Some(key) = meta.path.get_ident().map(ToString::to_string) else {
                    errors.help(
                        meta.path.span(),
                        "a `schema` attribute key must be a plain name",
                        format!("the accepted keys are: {}", list(FIELD_KEYS)),
                    );
                    return Ok(());
                };
                let span = meta.path.span();

                if this.constraints.parse_key(&key, &meta, errors) {
                    return Ok(());
                }

                match key.as_str() {
                    "each" => {
                        let mut inner = Constraints::default();
                        let result = meta.parse_nested_meta(|inner_meta| {
                            let Some(inner_key) =
                                inner_meta.path.get_ident().map(ToString::to_string)
                            else {
                                errors.error(inner_meta.path.span(), "expected a plain name");
                                return Ok(());
                            };
                            if EACH_KEYS.contains(&inner_key.as_str()) {
                                inner.parse_key(&inner_key, &inner_meta, errors);
                            } else if FIELD_KEYS.contains(&inner_key.as_str()) {
                                errors.help(
                                    inner_meta.path.span(),
                                    format!(
                                        "`{inner_key}` applies to the field, not to each element"
                                    ),
                                    format!("move it out of `each(…)`: `#[schema({inner_key})]`"),
                                );
                            } else {
                                errors.unknown_key(
                                    inner_meta.path.span(),
                                    "each",
                                    &inner_key,
                                    EACH_KEYS,
                                );
                            }
                            Ok(())
                        });
                        if let Err(error) = result {
                            errors.push(error);
                        }
                        this.each = Some((inner, span));
                    }
                    "rename" => {
                        if let Some(value) = string_value(&meta, "rename", errors) {
                            this.wire_name = value;
                        }
                    }
                    "title" => this.title = string_value(&meta, "title", errors),
                    "description" => {
                        this.description = string_value(&meta, "description", errors);
                    }
                    "skip" => this.skip = Some(span),
                    "flatten" => this.flatten = Some(span),
                    "flatten_bracket" => this.flatten_bracket = true,
                    "read_only" => this.read_only = true,
                    "write_only" => this.write_only = true,
                    "secret" => {
                        this.secret = Some(span);
                        this.write_only = true;
                    }
                    "default" => {
                        this.default = Some(if meta.input.peek(syn::Token![=]) {
                            match expr_value(&meta, "default", errors) {
                                Some(expr) => Default_::Expr(Box::new(unquote_expr(expr))),
                                None => Default_::Trait,
                            }
                        } else {
                            Default_::Trait
                        });
                    }
                    "example" => {
                        if let Some(expr) = expr_value(&meta, "example", errors) {
                            this.examples.push(unquote_expr(expr));
                        }
                    }
                    "deprecated" => {
                        this.deprecated = Some(if meta.input.peek(syn::Token![=]) {
                            string_value(&meta, "deprecated", errors)
                        } else {
                            None
                        });
                    }
                    "delimiter" => {
                        if let Some(value) = string_value(&meta, "delimiter", errors) {
                            let mut chars = value.chars();
                            match (chars.next(), chars.next()) {
                                (Some(c), None) if matches!(c, ',' | '|' | ' ') => {
                                    this.delimiter = Some((c, span));
                                }
                                _ => errors.help(
                                    span,
                                    format!("`{value}` is not a delimiter Moso can split on"),
                                    "the delimiters are `\",\"`, `\"|\"` and `\" \"`",
                                ),
                            }
                        }
                    }
                    other => {
                        errors.unknown_key(span, "schema", other, FIELD_KEYS);
                        let _ = meta.value().and_then(|v| v.parse::<Expr>());
                    }
                }
                Ok(())
            });
            if let Err(error) = result {
                errors.push(error);
            }
        }

        this.pointer = format!("/{}", escape_pointer_token(&this.wire_name));
        this.constraints.validate(errors);
        if let Some((each, _)) = &this.each {
            each.validate(errors);
        }
        this.check_shape(field, errors);
        this
    }

    /// Reject a constraint that cannot mean anything for this field's type,
    /// naming the field and offering the constraint that does.
    ///
    /// A rejected constraint is also *removed*, so the expansion does not go on
    /// to emit code that cannot compile: one mistake produces one error, not an
    /// error plus a page of trait-bound noise from generated tokens.
    fn check_shape(&mut self, field: &syn::Field, errors: &mut Diagnostics) {
        let name = self.display_name();
        let mut poisoned: Vec<&str> = Vec::new();
        let c = &self.constraints;

        if let Some(span) = c.unique
            && self.shape != Shape::Sequence
        {
            errors.help(
                span,
                format!(
                    "`unique` needs a list; `{name}` is {}",
                    self.shape.describe()
                ),
                "apply it to a `Vec<T>`, or remove it",
            );
            poisoned.push("unique");
        }

        if let Some((_, span)) = &self.each
            && self.shape != Shape::Sequence
        {
            let (message, help) = if self.shape == Shape::Map {
                (
                    format!("`each(…)` cannot address the entries of `{name}`"),
                    "a map's errors need a pointer per key, which the derive cannot build —                      constrain the value type instead, with a constrained newtype",
                )
            } else {
                (
                    format!(
                        "`each(…)` needs a list; `{name}` is {}",
                        self.shape.describe()
                    ),
                    "apply the rules to the field itself: `#[schema(len = 1..=24)]`",
                )
            };
            errors.help(*span, message, help);
            poisoned.push("each");
        }

        let numeric_span = c
            .range
            .map(|r| r.span)
            .or(c.multiple_of.map(|(_, span)| span))
            .or(c.positive)
            .or(c.non_negative);
        if let Some(span) = numeric_span
            && !self.shape.is_numeric()
        {
            errors.help(
                span,
                format!(
                    "a numeric constraint needs a number; `{name}` is {}",
                    self.shape.describe()
                ),
                "use `len = …` for a string or a list\n\
                 help: a numeric bound needs a numeric primitive — a type alias is opaque here",
            );
            poisoned.push("numeric");
        }

        if let Some(range) = &c.range
            && self.shape == Shape::UnsignedInt
            && range.min.is_some_and(|m| m.as_f64() < 0.0)
        {
            errors.help(
                range.span,
                format!("`{name}` is unsigned, so a negative bound can never bind"),
                "start the range at zero, or make the field signed",
            );
        }

        // A whole float bound on an integer field is harmless — `1.0` and `1`
        // admit the same values — but a fractional one cannot be met by any
        // value the field can hold, which is always a mistake.
        if let Some(range) = &c.range
            && matches!(self.shape, Shape::SignedInt | Shape::UnsignedInt)
            && (range.min.is_some_and(|m| m.as_int().is_none())
                || range.max.is_some_and(|m| m.as_int().is_none()))
        {
            errors.help(
                range.span,
                format!("`{name}` holds whole numbers, so a fractional bound is misleading"),
                "round the bound to a whole number",
            );
        }

        let text_span = c
            .pattern
            .as_ref()
            .map(|p| p.span)
            .or(c.format.as_ref().map(|(_, span)| *span))
            .or(c.contains.as_ref().map(|(_, span)| *span))
            .or(c.starts_with.as_ref().map(|(_, span)| *span))
            .or(c.ends_with.as_ref().map(|(_, span)| *span));
        if let Some(span) = text_span
            && !matches!(self.shape, Shape::Text)
        {
            errors.help(
                span,
                format!(
                    "a text constraint needs a string; `{name}` is {}",
                    self.shape.describe()
                ),
                "apply it to each element instead: `#[schema(each(pattern = \"…\"))]`",
            );
            poisoned.push("text");
        }

        if c.normalises() && !matches!(self.shape, Shape::Text | Shape::Sequence) {
            errors.help(
                field.ty.span(),
                format!(
                    "`trim`, `lowercase` and `uppercase` rewrite text; `{name}` is {}",
                    self.shape.describe()
                ),
                "remove the normalisation, or make the field a `String`",
            );
            poisoned.push("normalise");
        }

        if self.secret.is_some() && self.default.is_some() {
            errors.help(
                self.secret.unwrap_or_else(Span::call_site),
                format!("`{name}` is secret, so it cannot have a documented default"),
                "remove `default` — a default value would be published in the OpenAPI document",
            );
        }

        if self.secret.is_some() {
            check_secret_type(&field.ty, &name, errors);
        }

        if self.skip.is_some() && !self.constraints.is_empty() {
            errors.help(
                self.skip.unwrap_or_else(Span::call_site),
                format!("`{name}` is skipped, so its constraints can never run"),
                "remove `skip`, or remove the constraints",
            );
        }

        for rejected in poisoned {
            match rejected {
                "unique" => self.constraints.unique = None,
                "each" => self.each = None,
                "numeric" => {
                    self.constraints.range = None;
                    self.constraints.multiple_of = None;
                    self.constraints.positive = None;
                    self.constraints.non_negative = None;
                }
                "text" => {
                    self.constraints.pattern = None;
                    self.constraints.format = None;
                    self.constraints.contains = None;
                    self.constraints.starts_with = None;
                    self.constraints.ends_with = None;
                }
                _ => {
                    self.constraints.trim = false;
                    self.constraints.lowercase = false;
                    self.constraints.uppercase = false;
                }
            }
        }
    }

    /// How the field is named in a message.
    fn display_name(&self) -> String {
        self.ident
            .as_ref()
            .map_or_else(|| self.index.to_string(), ToString::to_string)
    }

    /// The expression that reads this field out of `self`.
    fn access(&self) -> TokenStream {
        match &self.ident {
            Some(ident) => quote!(self.#ident),
            None => {
                let index = syn::Index::from(self.index);
                quote!(self.#index)
            }
        }
    }

    /// True when the field is absent from both serde and the schema.
    fn is_skipped(&self) -> bool {
        self.skip.is_some()
    }

    /// True when a client may omit the field.
    fn is_optional(&self) -> bool {
        self.optional || self.default.is_some() || self.read_only
    }

    /// The identifier of the generated `serde(default = "…")` function.
    ///
    /// Qualified by the type (and, in an enum, the variant) it belongs to,
    /// because the function lives at module scope: two models in one module
    /// with a defaulted field of the same name must not collide.
    fn default_fn(&self, prefix: &str) -> Ident {
        format_ident!(
            "__moso_default_{}_{}",
            prefix,
            self.display_name(),
            span = Span::call_site()
        )
    }

    /// The `#[serde(...)]` attributes this field's shadow carries.
    fn serde_attrs(&self, container_rename_all: Option<RenameRule>, prefix: &str) -> TokenStream {
        let mut entries: Vec<TokenStream> = Vec::new();
        // `rename_all` is set on the shadow container, so only an explicit
        // rename — or a name the rule would not produce — needs spelling out.
        let implied = container_rename_all.map_or_else(
            || self.display_name(),
            |rule| rule.apply(&self.display_name()),
        );
        if self.wire_name != implied {
            let name = &self.wire_name;
            entries.push(quote!(rename = #name));
        }
        if self.is_skipped() {
            entries.push(quote!(skip));
        } else {
            if self.flatten.is_some() {
                entries.push(quote!(flatten));
            }
            if self.write_only {
                entries.push(quote!(skip_serializing));
            }
            if self.read_only {
                entries.push(quote!(skip_deserializing));
            }
            match &self.default {
                Some(Default_::Trait) => entries.push(quote!(default)),
                Some(Default_::Expr(_)) => {
                    let path = self.default_fn(prefix).to_string();
                    entries.push(quote!(default = #path));
                }
                None => {}
            }
            if let Some((delimiter, _)) = self.delimiter {
                let helper = match delimiter {
                    ',' => "::moso::__private::comma_delimited",
                    '|' => "::moso::__private::pipe_delimited",
                    _ => "::moso::__private::space_delimited",
                };
                entries.push(quote!(deserialize_with = #helper));
            }
        }
        if entries.is_empty() {
            TokenStream::new()
        } else {
            quote!(#[serde(#(#entries),*)])
        }
    }
}

/// `default = "Locale::EN"` is code; `default = "hello"` is a string.
///
/// Both spellings appear in the documentation. The rule is mechanical and
/// documented: a string literal whose contents parse as a *path with a `::`*
/// or as a *call* is code, and anything else is the string it looks like.
fn unquote_expr(expr: Expr) -> Expr {
    let Expr::Lit(ExprLit {
        lit: Lit::Str(text),
        ..
    }) = &expr
    else {
        return expr;
    };
    let Ok(parsed) = text.parse::<Expr>() else {
        return expr;
    };
    let is_code = match &parsed {
        Expr::Path(path) => path.path.segments.len() > 1,
        Expr::Call(_) | Expr::MethodCall(_) | Expr::Macro(_) => true,
        _ => false,
    };
    if is_code { parsed } else { expr }
}

// ---------------------------------------------------------------------------
// The runtime half
// ---------------------------------------------------------------------------

/// Where one set of constraints is being applied.
struct Site {
    /// An expression of type `&T`.
    value: TokenStream,
    /// An expression of type `&str`.
    pointer: TokenStream,
    shape: Shape,
    /// How the value is read as text; see [`text_accessor`].
    text: TokenStream,
    /// The `ValidationErrors` binding.
    errors: TokenStream,
    /// The `ValidationCtx` binding.
    ctx: TokenStream,
}

/// How a value is read as `&str` for a text check.
///
/// `AsRef<str>` covers `String` and every constrained string newtype. The one
/// documented exception is [`Password`], which deliberately implements neither
/// `Display` nor `AsRef<str>` so that it cannot be printed by accident; its
/// documented accessor is `expose()`, and `#[schema(secret, len = 12..)]` on a
/// `Password` is in the reference example, so it has to work.
fn text_accessor(ty: &Type, value: &TokenStream) -> TokenStream {
    if crate::util::attrs::type_ident(peel(ty)).is_some_and(|ident| ident == "Password") {
        quote!((#value).expose())
    } else {
        quote!(::core::convert::AsRef::<str>::as_ref(#value))
    }
}

/// The numeric bounds actually enforced, after folding in `positive` and
/// `non_negative`.
fn effective_range(c: &Constraints) -> Option<(Option<Num>, Option<Num>, bool, bool)> {
    let mut min = c.range.and_then(|r| r.min);
    let max = c.range.and_then(|r| r.max);
    let mut exclusive_min = false;
    let exclusive_max = c.range.is_some_and(|r| r.exclusive_max);

    if c.positive.is_some() && min.is_none_or(|m| m.as_f64() <= 0.0) {
        min = Some(Num::Int(0));
        exclusive_min = true;
    } else if c.non_negative.is_some() && min.is_none_or(|m| m.as_f64() < 0.0) {
        min = Some(Num::Int(0));
        exclusive_min = false;
    }

    (min.is_some() || max.is_some()).then_some((min, max, exclusive_min, exclusive_max))
}

/// The `Bounds` value a range check is given.
fn bounds_tokens(exclusive_min: bool, exclusive_max: bool) -> TokenStream {
    let p = private();
    match (exclusive_min, exclusive_max) {
        (false, false) => quote!(#p::Bounds::INCLUSIVE),
        (false, true) => quote!(#p::Bounds::EXCLUSIVE_MAX),
        (true, false) => quote!(#p::Bounds::EXCLUSIVE_MIN),
        (true, true) => quote!(#p::Bounds {
            exclusive_min: true,
            exclusive_max: true
        }),
    }
}

/// `Some(3usize)` / `::core::option::Option::None`.
fn opt_usize(value: Option<u64>) -> TokenStream {
    match value {
        Some(v) => {
            let lit = Literal::usize_suffixed(usize::try_from(v).unwrap_or(usize::MAX));
            quote!(::core::option::Option::Some(#lit))
        }
        None => quote!(::core::option::Option::None),
    }
}

/// A numeric bound rendered in the width the check takes.
fn opt_num(value: Option<Num>, shape: Shape) -> TokenStream {
    let Some(num) = value else {
        return quote!(::core::option::Option::None);
    };
    let lit = match shape {
        Shape::UnsignedInt => Literal::u64_suffixed(
            u64::try_from(num.as_int().unwrap_or(0).max(0)).unwrap_or(u64::MAX),
        ),
        Shape::Float => Literal::f64_suffixed(num.as_f64()),
        _ => Literal::i64_suffixed(
            num.as_int()
                .unwrap_or_else(|| num.as_f64() as i128)
                .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        ),
    };
    quote!(::core::option::Option::Some(#lit))
}

/// The runtime half of one constraint set: a sequence of `check_*` calls.
///
/// Every helper takes `&mut ValidationErrors`, a concrete type, so a validation
/// body does not monomorphise per call site (rule A4 in `04-devex/42`).
fn emit_checks(c: &Constraints, site: &Site, statics: &mut usize) -> TokenStream {
    let p = private();
    let Site {
        value,
        pointer,
        shape,
        text,
        errors,
        ctx,
    } = site;
    let shape = *shape;
    let mut out = TokenStream::new();
    let length = quote!((#value).len());

    if let Some((range, _)) = c.len {
        let min = opt_usize(range.min);
        let max = opt_usize(range.max);
        out.extend(if shape.is_collection() {
            quote!(#p::check_len_seq(#length, #min, #max, #pointer, &mut #errors);)
        } else {
            quote!(#p::check_len_str(#text, #min, #max, #pointer, &mut #errors);)
        });
    }

    if c.non_empty.is_some() && c.len.is_none_or(|(range, _)| range.min.is_none()) {
        out.extend(if shape.is_collection() {
            quote!(#p::check_non_empty_seq(#length, #pointer, &mut #errors);)
        } else {
            quote!(#p::check_non_empty_str(#text, #pointer, &mut #errors);)
        });
    }

    if let Some(pattern) = &c.pattern {
        let source = &pattern.source;
        let name = format_ident!("__MOSO_RE_{}", *statics);
        *statics += 1;
        out.extend(quote_spanned! {pattern.span=>
            static #name: ::std::sync::OnceLock<#p::regex::Regex> =
                ::std::sync::OnceLock::new();
            #p::check_pattern(
                #text,
                #name.get_or_init(|| match #p::regex::Regex::new(#source) {
                    ::core::result::Result::Ok(__re) => __re,
                    // Unreachable: the derive compiled this expression at
                    // compile time, and a pattern that does not compile is a
                    // compile error at the literal.
                    ::core::result::Result::Err(__error) => {
                        ::core::panic!("moso: invalid pattern: {}", __error)
                    }
                }),
                #source,
                #pointer,
                &mut #errors,
            );
        });
    }

    if let Some((format, span)) = &c.format {
        out.extend(quote_spanned! {*span=>
            #p::check_format(#text, #format, #pointer, &mut #errors);
        });
    }
    if let Some((needle, span)) = &c.contains {
        out.extend(quote_spanned! {*span=>
            #p::check_contains(#text, #needle, #pointer, &mut #errors);
        });
    }
    if let Some((prefix, span)) = &c.starts_with {
        out.extend(quote_spanned! {*span=>
            #p::check_starts_with(#text, #prefix, #pointer, &mut #errors);
        });
    }
    if let Some((suffix, span)) = &c.ends_with {
        out.extend(quote_spanned! {*span=>
            #p::check_ends_with(#text, #suffix, #pointer, &mut #errors);
        });
    }

    if let Some((min, max, exclusive_min, exclusive_max)) = effective_range(c) {
        // On an unsigned field `>= 0` is not a constraint, only documentation.
        let vacuous = shape == Shape::UnsignedInt
            && max.is_none()
            && !exclusive_min
            && min.is_some_and(|m| m.as_f64() <= 0.0);
        if !vacuous {
            let bounds = bounds_tokens(exclusive_min, exclusive_max);
            let min = opt_num(min, shape);
            let max = opt_num(max, shape);
            out.extend(match shape {
                Shape::UnsignedInt => quote! {
                    #p::check_range_u64(*#value as u64, #min, #max, #bounds, #pointer, &mut #errors);
                },
                Shape::Float => quote! {
                    #p::check_range_f64(*#value as f64, #min, #max, #bounds, #pointer, &mut #errors);
                },
                _ => quote! {
                    #p::check_range_i64(*#value as i64, #min, #max, #bounds, #pointer, &mut #errors);
                },
            });
        }
    }

    if let Some((divisor, span)) = c.multiple_of {
        out.extend(if shape == Shape::Float {
            let lit = Literal::f64_suffixed(divisor.as_f64());
            quote_spanned! {span=>
                #p::check_multiple_of_f64(*#value as f64, #lit, #pointer, &mut #errors);
            }
        } else {
            let lit = Literal::i64_suffixed(divisor.as_int().unwrap_or(1) as i64);
            quote_spanned! {span=>
                #p::check_multiple_of_i64(*#value as i64, #lit, #pointer, &mut #errors);
            }
        });
    }

    if let Some(span) = c.unique {
        out.extend(quote_spanned! {span=>
            #p::check_unique(&(#value)[..], #pointer, &mut #errors);
        });
    }

    if let Some((values, span)) = &c.enum_values {
        out.extend(match enum_values_kind(values) {
            EnumValues::Strings => quote_spanned! {*span=>
                #p::check_one_of_str(#text, &[#(#values),*], #pointer, &mut #errors);
            },
            EnumValues::Integers => quote_spanned! {*span=>
                #p::check_one_of_i64(*#value as i64, &[#(#values),*], #pointer, &mut #errors);
            },
            EnumValues::Other => quote_spanned! {*span=>
                #p::check_one_of(#value, &[#(#values),*], #pointer, &mut #errors);
            },
        });
    }

    if let Some(span) = c.nested {
        out.extend(quote_spanned! {span=>
            #p::check_nested(#value, #pointer, #ctx, &mut #errors);
        });
    }

    out
}

/// Which `check_one_of_*` helper a list of permitted values needs.
enum EnumValues {
    Strings,
    Integers,
    Other,
}

fn enum_values_kind(values: &[Expr]) -> EnumValues {
    let all = |f: fn(&Lit) -> bool| {
        !values.is_empty()
            && values.iter().all(|expr| match expr {
                Expr::Lit(ExprLit { lit, .. }) => f(lit),
                _ => false,
            })
    };
    if all(|lit| matches!(lit, Lit::Str(_))) {
        EnumValues::Strings
    } else if all(|lit| matches!(lit, Lit::Int(_))) {
        EnumValues::Integers
    } else {
        EnumValues::Other
    }
}

// ---------------------------------------------------------------------------
// The documented half
// ---------------------------------------------------------------------------

/// The JSON Schema keywords for one constraint set, applied to `node`.
///
/// Generated from the same [`Constraints`] as [`emit_checks`]: `check_len_str`
/// and `minLength` are two readings of one attribute and cannot disagree.
fn emit_schema_constraints(c: &Constraints, node: &TokenStream) -> TokenStream {
    let p = private();
    let mut out = TokenStream::new();

    let len = c.len.map(|(range, _)| range).or_else(|| {
        c.non_empty.is_some().then_some(LenRange {
            min: Some(1),
            max: None,
        })
    });
    if let Some(range) = len {
        let min = match range.min {
            Some(v) => {
                let lit = Literal::u64_suffixed(v);
                quote!(::core::option::Option::Some(#lit))
            }
            None => quote!(::core::option::Option::None),
        };
        let max = match range.max {
            Some(v) => {
                let lit = Literal::u64_suffixed(v);
                quote!(::core::option::Option::Some(#lit))
            }
            None => quote!(::core::option::Option::None),
        };
        // `apply_len` picks `minLength`/`minItems`/`minProperties` from the
        // node's own type, so the documented keyword is right even where the
        // macro could only guess at the runtime one.
        out.extend(quote!(#node.apply_len(#min, #max);));
    }

    if let Some(pattern) = &c.pattern {
        let source = &pattern.source;
        out.extend(quote!(#node.pattern = ::core::option::Option::Some(
            ::std::borrow::Cow::Borrowed(#source)
        );));
    } else if let Some((needle, _)) = &c.contains {
        let escaped = regex::escape(needle);
        out.extend(quote!(#node.pattern = ::core::option::Option::Some(
            ::std::borrow::Cow::Borrowed(#escaped)
        );));
    } else if let Some((prefix, _)) = &c.starts_with {
        let escaped = format!("^{}", regex::escape(prefix));
        out.extend(quote!(#node.pattern = ::core::option::Option::Some(
            ::std::borrow::Cow::Borrowed(#escaped)
        );));
    } else if let Some((suffix, _)) = &c.ends_with {
        let escaped = format!("{}$", regex::escape(suffix));
        out.extend(quote!(#node.pattern = ::core::option::Option::Some(
            ::std::borrow::Cow::Borrowed(#escaped)
        );));
    }

    if let Some((format, _)) = &c.format {
        out.extend(quote!(#node.format = ::core::option::Option::Some(
            ::std::borrow::Cow::Borrowed(#format)
        );));
    }

    if let Some((min, max, exclusive_min, exclusive_max)) = effective_range(c) {
        if let Some(min) = min {
            let number = min.to_json_number();
            let slot = if exclusive_min {
                quote!(exclusive_minimum)
            } else {
                quote!(minimum)
            };
            out.extend(quote!(#node.#slot = #number;));
        }
        if let Some(max) = max {
            let number = max.to_json_number();
            let slot = if exclusive_max {
                quote!(exclusive_maximum)
            } else {
                quote!(maximum)
            };
            out.extend(quote!(#node.#slot = #number;));
        }
    }

    if let Some((divisor, _)) = c.multiple_of {
        let number = divisor.to_json_number();
        out.extend(quote!(#node.multiple_of = #number;));
    }

    if c.unique.is_some() {
        out.extend(quote!(#node.unique_items = true;));
    }

    if let Some((values, _)) = &c.enum_values {
        out.extend(quote! {
            #node.enumeration = ::std::vec![
                #(#p::serde_json::to_value(&#values)
                    .unwrap_or(#p::serde_json::Value::Null)),*
            ];
        });
    }

    out
}

/// The in-place normalisation `trim`, `lowercase` and `uppercase` perform.
///
/// Emitted once per derive, inside the same anonymous `const` as the serde
/// shadow, and only when a field asks for it. The blanket impls for `Option<T>`
/// and `Vec<T>` are what make `#[schema(trim)]` work on
/// `Option<Vec<String>>` without a second attribute.
fn emit_normalise_trait() -> TokenStream {
    quote! {
        trait __MosoNormalise {
            fn __moso_normalise(&mut self, __ops: u8);
        }

        impl __MosoNormalise for ::std::string::String {
            fn __moso_normalise(&mut self, __ops: u8) {
                if __ops & 1 != 0 {
                    let __trimmed = self.trim();
                    if __trimmed.len() != self.len() {
                        *self = ::std::borrow::ToOwned::to_owned(__trimmed);
                    }
                }
                if __ops & 2 != 0 {
                    *self = self.to_lowercase();
                }
                if __ops & 4 != 0 {
                    *self = self.to_uppercase();
                }
            }
        }

        impl<__T: __MosoNormalise> __MosoNormalise for ::core::option::Option<__T> {
            fn __moso_normalise(&mut self, __ops: u8) {
                if let ::core::option::Option::Some(__inner) = self {
                    __inner.__moso_normalise(__ops);
                }
            }
        }

        impl<__T: __MosoNormalise> __MosoNormalise for ::std::vec::Vec<__T> {
            fn __moso_normalise(&mut self, __ops: u8) {
                for __inner in self.iter_mut() {
                    __inner.__moso_normalise(__ops);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared impl emitters
// ---------------------------------------------------------------------------

/// The attributes every generated impl carries.
///
/// Generated code is not read, so a lint that fires inside it costs a user time
/// and teaches them nothing; the allow list is deliberately narrow, covering
/// only shapes the expansion genuinely produces.
fn generated_allows() -> TokenStream {
    quote! {
        #[automatically_derived]
        // Generated code is not the user's to lint: a constraint-free field leaves
        // its binding unused, and `clippy::pedantic` has opinions about shapes the
        // expansion produces deliberately.
        #[allow(
            unused_variables,
            unused_mut,
            unused_qualifications,
            clippy::all,
            clippy::pedantic
        )]
    }
}

/// `impl Schema` — the name, the document and `HAS_CONSTRAINTS`.
fn emit_schema_impl(
    container: &Container,
    body: TokenStream,
    has_constraints: TokenStream,
) -> TokenStream {
    let p = private();
    let ident = &container.ident;
    let (ig, tg, wc) = container.split_generics(None);
    let name = container.schema_name_body();
    let allows = generated_allows();
    quote! {
        #allows
        impl #ig #p::Schema for #ident #tg #wc {
            fn schema_name() -> ::std::borrow::Cow<'static, str> {
                #name
            }

            fn json_schema(__generator: &mut #p::SchemaGenerator) -> #p::SchemaNode {
                #body
            }

            const HAS_CONSTRAINTS: bool = #has_constraints;
        }
    }
}

/// `impl Validate` — the runtime half.
fn emit_validate_impl(container: &Container, body: TokenStream) -> TokenStream {
    let p = private();
    let ident = &container.ident;
    let (ig, tg, wc) = container.split_generics(None);
    let checks: Vec<TokenStream> = container
        .checks
        .iter()
        .map(|path| {
            quote_spanned! {path.span()=>
                if let ::core::result::Result::Err(__inner) = #path(self, __ctx) {
                    __errors.merge(__inner);
                }
            }
        })
        .collect();
    let allows = generated_allows();
    quote! {
        #allows
        impl #ig #p::Validate for #ident #tg #wc {
            fn validate(
                &self,
                __ctx: &mut #p::ValidationCtx,
            ) -> ::core::result::Result<(), #p::ValidationErrors> {
                let mut __errors = __ctx.errors();
                #body
                #(#checks)*
                __errors.into_result()
            }
        }
    }
}

/// `impl IntoResponse` and `impl Describe`.
///
/// A blanket `impl<T: Schema> IntoResponse for T` would be an orphan violation
/// in every application crate, so "return a bare `T: Schema` from a handler"
/// only works because the derive emits these two.
fn emit_response_impls(container: &Container) -> TokenStream {
    if container.no_response {
        return TokenStream::new();
    }
    let p = private();
    let ident = &container.ident;
    let (ig, tg, wc) = container.split_generics(None);
    let allows = generated_allows();
    quote! {
        #allows
        impl #ig #p::IntoResponse for #ident #tg #wc {
            fn into_response(self) -> #p::Response {
                #p::json_response(#p::http::StatusCode::OK, &self)
            }
        }

        #allows
        impl #ig #p::Describe for #ident #tg #wc {
            fn describe(__operation: &mut #p::OperationBuilder) {
                #p::describe_json::<Self>(__operation, 200u16);
            }
        }
    }
}

/// `impl From<Other>` for every `#[schema(from = Other)]`.
///
/// Field-name matching, one `Into::into` per field, each spanned at the field
/// it converts — so a missing or mistyped field names *that* field rather than
/// the whole impl.
fn emit_from_impls(container: &Container, fields: &[FieldSpec], named: bool) -> TokenStream {
    let ident = &container.ident;
    let (ig, tg, wc) = container.split_generics(None);
    let allows = generated_allows();
    let mut out = TokenStream::new();

    for source in &container.from {
        let body = if named {
            let entries: Vec<TokenStream> = fields
                .iter()
                .map(|field| {
                    let name = field.ident.as_ref().expect("named field");
                    quote_spanned! {name.span()=>
                        #name: ::core::convert::Into::into(__source.#name)
                    }
                })
                .collect();
            quote!(Self { #(#entries),* })
        } else {
            let entries: Vec<TokenStream> = fields
                .iter()
                .map(|field| {
                    let index = syn::Index::from(field.index);
                    quote!(::core::convert::Into::into(__source.#index))
                })
                .collect();
            quote!(Self(#(#entries),*))
        };
        out.extend(quote_spanned! {source.span()=>
            #allows
            impl #ig ::core::convert::From<#source> for #ident #tg #wc {
                fn from(__source: #source) -> Self {
                    #body
                }
            }
        });
    }
    out
}

/// The redacting `Debug` a struct with a secret field gets.
///
/// Generated *only* when a field is secret: a type without one keeps whatever
/// `Debug` its author derived, and a type with one cannot derive `Debug` at all
/// — which is the point, because that derive would print the secret.
fn emit_debug_impl(container: &Container, fields: &[FieldSpec], named: bool) -> TokenStream {
    if !fields.iter().any(|f| f.secret.is_some()) {
        return TokenStream::new();
    }
    let ident = &container.ident;
    let name = ident.to_string();
    let (ig, tg, wc) = container.split_generics(Some(quote!(::core::fmt::Debug)));
    let allows = generated_allows();

    let body = if named {
        let entries: Vec<TokenStream> = fields
            .iter()
            .map(|field| {
                let label = field.display_name();
                if field.secret.is_some() {
                    quote!(.field(#label, &#REDACTED))
                } else {
                    let access = field.access();
                    quote!(.field(#label, &#access))
                }
            })
            .collect();
        quote! {
            __f.debug_struct(#name) #(#entries)* .finish()
        }
    } else {
        let entries: Vec<TokenStream> = fields
            .iter()
            .map(|field| {
                if field.secret.is_some() {
                    quote!(.field(&#REDACTED))
                } else {
                    let access = field.access();
                    quote!(.field(&#access))
                }
            })
            .collect();
        quote! {
            __f.debug_tuple(#name) #(#entries)* .finish()
        }
    };

    quote! {
        #allows
        impl #ig ::core::fmt::Debug for #ident #tg #wc {
            fn fmt(&self, __f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                #body
            }
        }
    }
}

/// What a secret field prints as. Matches the redaction marker the tracing
/// layer and the problem renderer use.
const REDACTED: &str = "[redacted]";

/// `const HAS_CONSTRAINTS` — true when a 422 is reachable.
///
/// An attribute makes it true; so does a field whose *type* can reject a value,
/// which is why `email: Email` counts even with no attribute on it. A field
/// whose type mentions the deriving type is excluded: `Category { children:
/// Vec<Category> }` would otherwise be a `const` evaluation cycle.
fn emit_has_constraints(container: &Container, fields: &[&FieldSpec], base: bool) -> TokenStream {
    let p = private();
    let terms: Vec<TokenStream> = fields
        .iter()
        .filter(|field| {
            !field.is_skipped() && !crate::util::attrs::type_mentions(&field.ty, &container.ident)
        })
        .map(|field| {
            let ty = &field.ty;
            quote!(<#ty as #p::Schema>::HAS_CONSTRAINTS)
        })
        .collect();
    let base = base || !container.checks.is_empty() || container.deny_unknown.is_some();
    quote!(#base #(|| #terms)*)
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// The three struct shapes, which serde and JSON Schema treat differently.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StructKind {
    /// `struct S { a: A }` — a JSON object.
    Named,
    /// `struct S(A);` — transparent: the value *is* the inner value.
    Newtype,
    /// `struct S(A, B);` — a JSON array with `prefixItems`.
    Tuple,
    /// `struct S;` — JSON `null`.
    Unit,
}

fn expand_struct(
    container: &Container,
    data: &DataStruct,
    errors: &mut Diagnostics,
) -> TokenStream {
    if let Some((_, span)) = &container.tag {
        errors.help(
            *span,
            "`tag` describes how an enum is represented",
            "remove it — a struct is always a JSON object",
        );
    }
    if let Some(span) = container.untagged {
        errors.help(
            span,
            "`untagged` describes how an enum is represented",
            "remove it — a struct is always a JSON object",
        );
    }

    let kind = match &data.fields {
        Fields::Named(_) => StructKind::Named,
        Fields::Unnamed(f) if f.unnamed.len() == 1 => StructKind::Newtype,
        Fields::Unnamed(_) => StructKind::Tuple,
        Fields::Unit => StructKind::Unit,
    };

    let mut fields: Vec<FieldSpec> = data
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| FieldSpec::parse(field, index, container.rename_all, errors))
        .collect();

    // A newtype's value is the whole document, so its errors are reported at
    // the root pointer rather than at `/0`.
    if kind == StructKind::Newtype {
        fields[0].pointer = String::new();
    }
    for field in &mut fields {
        if field.flatten.is_some() {
            field.pointer = String::new();
        }
    }

    if container.deny_unknown.is_some()
        && let Some(field) = fields.iter().find(|f| f.flatten.is_some())
    {
        errors.help(
            field.flatten.unwrap_or_else(Span::call_site),
            "`deny_unknown` and `flatten` cannot both be used",
            "a flattened field absorbs unknown keys, so there is nothing left to deny — drop \
             one of the two",
        );
    }

    let live: Vec<&FieldSpec> = fields.iter().filter(|f| !f.is_skipped()).collect();

    let serde = emit_struct_serde(container, &fields, kind);
    let validate = emit_validate_impl(container, emit_field_validations(&fields));
    let base_constraints = fields.iter().any(|f| {
        !f.is_skipped()
            && (!f.constraints.is_empty() || f.each.as_ref().is_some_and(|(c, _)| !c.is_empty()))
    });
    let has_constraints = emit_has_constraints(container, &live, base_constraints);
    let schema = emit_schema_impl(
        container,
        emit_struct_schema_body(container, &fields, kind),
        has_constraints,
    );
    let debug = emit_debug_impl(container, &fields, kind == StructKind::Named);
    let responses = emit_response_impls(container);
    let from = emit_from_impls(container, &fields, kind == StructKind::Named);

    quote! {
        #serde
        #validate
        #schema
        #debug
        #responses
        #from
    }
}

/// The anonymous `const` holding the serde shadow and the two delegations.
fn emit_struct_serde(container: &Container, fields: &[FieldSpec], kind: StructKind) -> TokenStream {
    let p = private();
    let ident = &container.ident;
    let remote = ident.to_string();
    let prefix = remote.clone();
    let prefix = prefix.as_str();
    let defaults = emit_default_fns(prefix, fields);
    if container.no_serde {
        // The default functions are still needed: the `Schema` impl reads them.
        return defaults;
    }
    let container_attrs = container.serde_attrs();
    let generics = &container.generics;
    let where_clause = container.generics.where_clause.as_ref();

    let declaration = match kind {
        StructKind::Unit => quote!(struct __MosoSerde #generics #where_clause;),
        StructKind::Named => {
            let entries: Vec<TokenStream> = fields
                .iter()
                .map(|field| {
                    let attrs = field.serde_attrs(container.rename_all, prefix);
                    let forwarded = forwarded_attrs(field);
                    let name = field.ident.as_ref().expect("named field");
                    let ty = &field.ty;
                    quote!(#(#forwarded)* #attrs #name: #ty)
                })
                .collect();
            quote!(struct __MosoSerde #generics #where_clause { #(#entries),* })
        }
        StructKind::Newtype | StructKind::Tuple => {
            let entries: Vec<TokenStream> = fields
                .iter()
                .map(|field| {
                    let attrs = field.serde_attrs(None, prefix);
                    let forwarded = forwarded_attrs(field);
                    let ty = &field.ty;
                    quote!(#(#forwarded)* #attrs #ty)
                })
                .collect();
            quote!(struct __MosoSerde #generics (#(#entries),*) #where_clause;)
        }
    };

    let normalise = emit_normalisations(fields);
    let (serialize, deserialize) = serde_delegations(container, normalise);

    quote! {
        #defaults

        #[doc(hidden)]
        // The serde shadow type mirrors the user's field names and is never
        // named outside this block.
        #[allow(non_camel_case_types, non_snake_case, dead_code, clippy::all, clippy::pedantic)]
        const _: () = {
            use #p::serde as _serde;

            #[derive(_serde::Serialize, _serde::Deserialize)]
            #[serde(crate = "_serde")]
            #[serde(remote = #remote)]
            #[serde(#container_attrs)]
            #declaration

            #serialize
            #deserialize
        };
    }
}

/// `#[cfg(...)]` and `#[cfg_attr(...)]` follow a field everywhere it is
/// mentioned, so a conditionally compiled field does not leave the shadow, the
/// validation body and the schema body disagreeing about how many fields exist.
fn forwarded_attrs(field: &FieldSpec) -> Vec<&Attribute> {
    field
        .forwarded
        .iter()
        .filter(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"))
        .collect()
}

/// One `fn` per `#[schema(default = expr)]`, because serde's `default` takes a
/// path rather than an expression.
///
/// They live at module scope rather than inside the serde `const` block so the
/// `Schema` impl can call the *same* function serde calls: the documented
/// default and the deserialised one are then one value by construction rather
/// than two evaluations of one expression.
fn emit_default_fns(prefix: &str, fields: &[FieldSpec]) -> TokenStream {
    let mut out = TokenStream::new();
    for field in fields {
        let Some(Default_::Expr(expr)) = &field.default else {
            continue;
        };
        let name = field.default_fn(prefix);
        let ty = &field.ty;
        let forwarded = forwarded_attrs(field);
        // A string literal almost never already has the field's type:
        // `default = "x"` on a `String` field means `String::from("x")`. Other
        // literals do have it, and converting those would leave the target
        // type ambiguous.
        let value = if matches!(
            &**expr,
            Expr::Lit(ExprLit {
                lit: Lit::Str(_),
                ..
            })
        ) {
            quote_spanned!(expr.span()=> ::core::convert::Into::into(#expr))
        } else {
            quote_spanned!(expr.span()=> #expr)
        };
        out.extend(quote_spanned! {expr.span()=>
            #[doc(hidden)]
            // A `#[schema(default = ..)]` thunk, named after the field it serves and
            // holding whatever expression the user wrote.
            #[allow(non_snake_case, clippy::all, clippy::pedantic)]
            #(#forwarded)*
            fn #name() -> #ty {
                #value
            }
        });
    }
    out
}

/// The in-place normalisation applied after deserialising.
fn emit_normalisations(fields: &[FieldSpec]) -> TokenStream {
    let mut calls = TokenStream::new();
    for field in fields {
        if field.is_skipped() || !field.constraints.normalises() {
            continue;
        }
        let access = field.ident.as_ref().map_or_else(
            || {
                let index = syn::Index::from(field.index);
                quote!(#index)
            },
            |ident| quote!(#ident),
        );
        let mask = Literal::u8_suffixed(field.constraints.normalise_mask());
        let forwarded = forwarded_attrs(field);
        calls.extend(quote! {
            #(#forwarded)*
            __MosoNormalise::__moso_normalise(&mut __value.#access, #mask);
        });
    }
    calls
}

/// The two one-line impls that hand serde's remote derive the real work.
fn serde_delegations(container: &Container, normalise: TokenStream) -> (TokenStream, TokenStream) {
    let ident = &container.ident;
    let (ig, tg, wc) = container.split_generics(None);
    let de_generics = deserialize_generics(container);

    let serialize = quote! {
        impl #ig _serde::Serialize for #ident #tg #wc {
            fn serialize<__S>(&self, __serializer: __S)
                -> ::core::result::Result<__S::Ok, __S::Error>
            where
                __S: _serde::Serializer,
            {
                __MosoSerde::serialize(self, __serializer)
            }
        }
    };

    let deserialize = if normalise.is_empty() {
        quote! {
            impl #de_generics _serde::Deserialize<'de> for #ident #tg #wc {
                fn deserialize<__D>(__deserializer: __D)
                    -> ::core::result::Result<Self, __D::Error>
                where
                    __D: _serde::Deserializer<'de>,
                {
                    __MosoSerde::deserialize(__deserializer)
                }
            }
        }
    } else {
        let helpers = emit_normalise_trait();
        quote! {
            #helpers

            impl #de_generics _serde::Deserialize<'de> for #ident #tg #wc {
                fn deserialize<__D>(__deserializer: __D)
                    -> ::core::result::Result<Self, __D::Error>
                where
                    __D: _serde::Deserializer<'de>,
                {
                    let mut __value = __MosoSerde::deserialize(__deserializer)?;
                    #normalise
                    ::core::result::Result::Ok(__value)
                }
            }
        }
    };

    (serialize, deserialize)
}

/// `impl<'de, T: Schema>` — the generics of the `Deserialize` impl.
fn deserialize_generics(container: &Container) -> TokenStream {
    let p = private();
    let mut generics = container.generics.clone();
    for param in &mut generics.params {
        if let syn::GenericParam::Type(ty) = param {
            ty.bounds.push(syn::parse_quote!(#p::Schema));
        }
    }
    generics
        .params
        .insert(0, syn::GenericParam::Lifetime(syn::parse_quote!('de)));
    let (impl_generics, _, _) = generics.split_for_impl();
    impl_generics.to_token_stream()
}

// ---------------------------------------------------------------------------
// Per-field bodies
// ---------------------------------------------------------------------------

/// The element type of a sequence, for `each(...)`.
fn element_type(ty: &Type) -> Option<&Type> {
    let ty = peel(ty);
    match ty {
        Type::Array(array) => Some(&array.elem),
        Type::Slice(slice) => Some(&slice.elem),
        other => crate::util::attrs::sequence_inner(other),
    }
}

/// Every field's runtime checks, in declaration order.
fn emit_field_validations(fields: &[FieldSpec]) -> TokenStream {
    let mut statics = 0usize;
    let mut out = TokenStream::new();
    for field in fields {
        if field.is_skipped() {
            continue;
        }
        let access = field.access();
        let access = quote!(&#access);
        let block = emit_one_field_validation(field, &access, &mut statics);
        if block.is_empty() {
            continue;
        }
        let forwarded = forwarded_attrs(field);
        out.extend(quote!(#(#forwarded)* { #block }));
    }
    out
}

/// One field's checks, reading the value from `access` — `&self.username` for a
/// struct field, the pattern binding for an enum variant's.
fn emit_one_field_validation(
    field: &FieldSpec,
    access: &TokenStream,
    statics: &mut usize,
) -> TokenStream {
    let pointer = &field.pointer;
    let site = Site {
        value: quote!(__value),
        pointer: quote!(#pointer),
        shape: field.shape,
        text: text_accessor(&field.ty, &quote!(__value)),
        errors: quote!(__errors),
        ctx: quote!(__ctx),
    };
    let checks = emit_checks(&field.constraints, &site, statics);
    let each = emit_each_validation(field, statics);
    if checks.is_empty() && each.is_empty() {
        return TokenStream::new();
    }

    if field.optional {
        quote! {
            if let ::core::option::Option::Some(__value) = #access {
                #checks
                #each
            }
        }
    } else {
        quote! {
            let __value = #access;
            #checks
            #each
        }
    }
}

/// The `each(...)` loop: the inner rules applied to every element, with the
/// element's index in the pointer so a client can highlight `/tags/2`.
fn emit_each_validation(field: &FieldSpec, statics: &mut usize) -> TokenStream {
    let Some((each, _)) = &field.each else {
        return TokenStream::new();
    };
    if each.is_empty() {
        return TokenStream::new();
    }
    let element = element_type(&field.ty);
    let shape = element.map_or(Shape::Text, Shape::of);
    let text = element.map_or_else(
        || quote!(::core::convert::AsRef::<str>::as_ref(__element)),
        |ty| text_accessor(ty, &quote!(__element)),
    );
    let site = Site {
        value: quote!(__element),
        pointer: quote!(&__element_pointer),
        shape,
        text,
        errors: quote!(__errors),
        ctx: quote!(__ctx),
    };
    let checks = emit_checks(each, &site, statics);
    let format = format!(
        "{}/{{}}",
        field.pointer.replace('{', "{{").replace('}', "}}")
    );
    quote! {
        for (__index, __element) in
            ::core::iter::IntoIterator::into_iter(__value).enumerate()
        {
            if __ctx.is_full(&__errors) {
                break;
            }
            let __element_pointer = ::std::format!(#format, __index);
            #checks
        }
    }
}

/// The annotations that are not constraints: descriptions, defaults, examples
/// and the OpenAPI flags.
fn emit_node_annotations(field: &FieldSpec, prefix: &str, node: &TokenStream) -> TokenStream {
    let p = private();
    let mut out = TokenStream::new();

    let mut description = field.description.clone();
    if let Some(note) = field.constraints.normalise_note() {
        description = Some(match description {
            Some(existing) => format!("{existing}\n\n{note}"),
            None => note,
        });
    }
    if let Some(Some(note)) = &field.deprecated {
        description = Some(match description {
            Some(existing) => format!("{existing}\n\nDeprecated: {note}"),
            None => format!("Deprecated: {note}"),
        });
    }
    if let Some(description) = description {
        out.extend(quote!(#node.description = ::core::option::Option::Some(
            ::std::borrow::Cow::Borrowed(#description)
        );));
    }
    if let Some(title) = &field.title {
        out.extend(quote!(#node.title = ::core::option::Option::Some(
            ::std::borrow::Cow::Borrowed(#title)
        );));
    }
    if field.read_only {
        out.extend(quote!(#node.read_only = true;));
    }
    if field.write_only {
        out.extend(quote!(#node.write_only = true;));
    }
    if field.deprecated.is_some() {
        out.extend(quote!(#node.deprecated = true;));
    }
    match &field.default {
        Some(Default_::Expr(expr)) => {
            let default = field.default_fn(prefix);
            out.extend(quote_spanned! {expr.span()=>
                #node.default = #p::serde_json::to_value(&#default()).ok();
            });
        }
        Some(Default_::Trait) => {
            let ty = &field.ty;
            out.extend(quote! {
                #node.default = #p::serde_json::to_value(
                    &<#ty as ::core::default::Default>::default()
                ).ok();
            });
        }
        None => {}
    }
    for example in &field.examples {
        out.extend(quote_spanned! {example.span()=>
            if let ::core::result::Result::Ok(__example) = #p::serde_json::to_value(&#example) {
                #node.examples.push(__example);
            }
        });
    }
    out
}

/// One field's complete schema node: its type's schema, then its constraints,
/// then its annotations.
fn emit_field_node(field: &FieldSpec, prefix: &str) -> TokenStream {
    let ty = &field.ty;
    let node = quote!(__field);
    let constraints = emit_schema_constraints(&field.constraints, &node);
    let annotations = emit_node_annotations(field, prefix, &node);
    let each = match &field.each {
        Some((each, _)) if !each.is_empty() => {
            let items = quote!(__items);
            let inner = emit_schema_constraints(each, &items);
            quote! {
                if let ::core::option::Option::Some(__items) = __field.items.as_deref_mut() {
                    #inner
                }
            }
        }
        _ => TokenStream::new(),
    };
    quote_spanned! {ty.span()=>
        let mut __field = __generator.subschema_for::<#ty>();
        #constraints
        #each
        #annotations
    }
}

/// The container's own annotations, applied to the finished node.
fn emit_container_annotations(container: &Container, node: &TokenStream) -> TokenStream {
    let p = private();
    let mut out = TokenStream::new();

    let mut description = container.description.clone();
    if let Some(Some(note)) = &container.deprecated {
        description = Some(match description {
            Some(existing) => format!("{existing}\n\nDeprecated: {note}"),
            None => format!("Deprecated: {note}"),
        });
    }
    if let Some(description) = description {
        out.extend(quote!(#node.description = ::core::option::Option::Some(
            ::std::borrow::Cow::Borrowed(#description)
        );));
    }
    if let Some(title) = &container.title {
        out.extend(quote!(#node.title = ::core::option::Option::Some(
            ::std::borrow::Cow::Borrowed(#title)
        );));
    }
    if container.deprecated.is_some() {
        out.extend(quote!(#node.deprecated = true;));
    }
    for example in &container.examples {
        out.extend(quote_spanned! {example.span()=>
            if let ::core::result::Result::Ok(__example) = #p::serde_json::to_value(&#example) {
                #node.examples.push(__example);
            }
        });
    }
    out
}

fn emit_struct_schema_body(
    container: &Container,
    fields: &[FieldSpec],
    kind: StructKind,
) -> TokenStream {
    let p = private();
    let prefix = container.ident.to_string();
    let prefix = prefix.as_str();
    let node = quote!(__node);
    let annotations = emit_container_annotations(container, &node);

    match kind {
        StructKind::Unit => quote! {
            let mut __node = #p::SchemaNode::null();
            #annotations
            __node
        },
        StructKind::Newtype => {
            let field = &fields[0];
            let body = emit_field_node(field, prefix);
            quote! {
                let mut __node = { #body __field };
                #annotations
                __node
            }
        }
        StructKind::Tuple => {
            let items: Vec<TokenStream> = fields
                .iter()
                .map(|field| {
                    let body = emit_field_node(field, prefix);
                    quote!(.prefix_item({ #body __field }))
                })
                .collect();
            let count = Literal::u64_suffixed(fields.len() as u64);
            quote! {
                let mut __node = #p::ArrayBuilder::new()
                    #(#items)*
                    .min_items(#count)
                    .max_items(#count)
                    .build();
                #annotations
                __node
            }
        }
        StructKind::Named => {
            let properties: Vec<TokenStream> = fields
                .iter()
                .filter(|field| !field.is_skipped())
                .map(|field| {
                    let body = emit_field_node(field, prefix);
                    let forwarded = forwarded_attrs(field);
                    if field.flatten.is_some() {
                        quote! {
                            #(#forwarded)*
                            {
                                #body
                                __object = __object.all_of(__field);
                            }
                        }
                    } else {
                        let name = &field.wire_name;
                        let required = !field.is_optional();
                        quote! {
                            #(#forwarded)*
                            {
                                #body
                                __object = __object.property(#name, __field, #required);
                            }
                        }
                    }
                })
                .collect();
            let deny = container
                .deny_unknown
                .map(|_| quote!(__object = __object.additional_properties(false);));
            quote! {
                let mut __object = __generator.object(<Self as #p::Schema>::schema_name());
                #(#properties)*
                #deny
                let mut __node = __object.build();
                #annotations
                __node
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Every key `#[schema(...)]` accepts on an enum variant.
const VARIANT_KEYS: &[&str] = &["rename", "skip", "title", "description", "deprecated"];

/// The four variant shapes, which the four representations combine with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum VariantKind {
    Unit,
    Newtype,
    Tuple,
    Struct,
}

struct VariantSpec {
    ident: Ident,
    wire_name: String,
    description: Option<String>,
    title: Option<String>,
    deprecated: Option<Option<String>>,
    kind: VariantKind,
    fields: Vec<FieldSpec>,
    skip: Option<Span>,
    forwarded: Vec<Attribute>,
    span: Span,
}

impl VariantSpec {
    fn parse(
        variant: &syn::Variant,
        rename_all: Option<RenameRule>,
        errors: &mut Diagnostics,
    ) -> Self {
        let raw = variant.ident.to_string();
        let mut this = Self {
            ident: variant.ident.clone(),
            wire_name: rename_all.map_or_else(|| raw.clone(), |rule| rule.apply(&raw)),
            description: doc_text(&variant.attrs),
            title: None,
            deprecated: None,
            kind: match &variant.fields {
                Fields::Unit => VariantKind::Unit,
                Fields::Unnamed(f) if f.unnamed.len() == 1 => VariantKind::Newtype,
                Fields::Unnamed(_) => VariantKind::Tuple,
                Fields::Named(_) => VariantKind::Struct,
            },
            fields: variant
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| FieldSpec::parse(field, index, None, errors))
                .collect(),
            skip: None,
            forwarded: variant
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"))
                .cloned()
                .collect(),
            span: variant.ident.span(),
        };

        for attr in variant.attrs.iter().filter(|a| a.path().is_ident("schema")) {
            let result = attr.parse_nested_meta(|meta| {
                let Some(key) = meta.path.get_ident().map(ToString::to_string) else {
                    errors.error(meta.path.span(), "expected a plain name");
                    return Ok(());
                };
                match key.as_str() {
                    "rename" => {
                        if let Some(value) = string_value(&meta, "rename", errors) {
                            this.wire_name = value;
                        }
                    }
                    "skip" => this.skip = Some(meta.path.span()),
                    "title" => this.title = string_value(&meta, "title", errors),
                    "description" => {
                        this.description = string_value(&meta, "description", errors);
                    }
                    "deprecated" => {
                        this.deprecated = Some(if meta.input.peek(syn::Token![=]) {
                            string_value(&meta, "deprecated", errors)
                        } else {
                            None
                        });
                    }
                    other => {
                        errors.unknown_key(meta.path.span(), "schema", other, VARIANT_KEYS);
                        let _ = meta.value().and_then(|v| v.parse::<Expr>());
                    }
                }
                Ok(())
            });
            if let Err(error) = result {
                errors.push(error);
            }
        }

        this
    }

    /// The `#[serde(...)]` attributes the shadow variant carries.
    fn serde_attrs(&self, rename_all: Option<RenameRule>) -> TokenStream {
        let mut entries: Vec<TokenStream> = Vec::new();
        let implied = rename_all.map_or_else(
            || self.ident.to_string(),
            |rule| rule.apply(&self.ident.to_string()),
        );
        if self.wire_name != implied {
            let name = &self.wire_name;
            entries.push(quote!(rename = #name));
        }
        if self.skip.is_some() {
            entries.push(quote!(skip));
        }
        if entries.is_empty() {
            TokenStream::new()
        } else {
            quote!(#[serde(#(#entries),*)])
        }
    }

    /// The prefix qualifying this variant's generated default functions.
    fn prefix(&self, container: &Ident) -> String {
        format!("{container}_{}", self.ident)
    }

    /// The pattern that binds every field of this variant, and the bindings.
    fn pattern(&self) -> (TokenStream, Vec<TokenStream>) {
        let ident = &self.ident;
        match self.kind {
            VariantKind::Unit => (quote!(Self::#ident), Vec::new()),
            VariantKind::Newtype | VariantKind::Tuple => {
                let bindings: Vec<Ident> = (0..self.fields.len())
                    .map(|index| format_ident!("__f{}", index))
                    .collect();
                let refs = bindings.iter().map(|b| quote!(#b)).collect();
                (quote!(Self::#ident(#(#bindings),*)), refs)
            }
            VariantKind::Struct => {
                let names: Vec<&Ident> = self
                    .fields
                    .iter()
                    .map(|f| f.ident.as_ref().expect("named field"))
                    .collect();
                let refs = names.iter().map(|n| quote!(#n)).collect();
                (quote!(Self::#ident { #(#names),* }), refs)
            }
        }
    }
}

fn expand_enum(container: &Container, data: &DataEnum, errors: &mut Diagnostics) -> TokenStream {
    let repr = container.repr();
    let variants: Vec<VariantSpec> = data
        .variants
        .iter()
        .map(|variant| VariantSpec::parse(variant, container.rename_all, errors))
        .collect();

    if variants.is_empty() {
        errors.help(
            container.ident.span(),
            "an enum with no variants cannot be deserialised",
            "give it at least one variant, or use a unit struct",
        );
    }

    check_enum_shape(container, &repr, &variants, errors);

    let live: Vec<&VariantSpec> = variants.iter().filter(|v| v.skip.is_none()).collect();
    let all_unit = !live.is_empty() && live.iter().all(|v| v.kind == VariantKind::Unit);

    // Pointers depend on the representation: an externally tagged variant's
    // fields live under the variant name, an internally tagged one's do not.
    let mut variants = variants;
    for variant in &mut variants {
        let prefix = match &repr {
            Repr::External => format!("/{}", escape_pointer_token(&variant.wire_name)),
            Repr::Adjacent(_, content) => format!("/{}", escape_pointer_token(content)),
            Repr::Internal(_) | Repr::Untagged => String::new(),
        };
        for field in &mut variant.fields {
            if variant.kind == VariantKind::Newtype {
                field.pointer.clone_from(&prefix);
            } else {
                field.pointer = format!("{prefix}{}", field.pointer);
            }
        }
    }

    let serde = emit_enum_serde(container, &variants);
    let validate = emit_validate_impl(container, emit_enum_validations(&variants));

    let base_constraints = variants.iter().any(|variant| {
        variant.fields.iter().any(|field| {
            !field.is_skipped()
                && (!field.constraints.is_empty()
                    || field.each.as_ref().is_some_and(|(c, _)| !c.is_empty()))
        })
    });
    let field_types: Vec<&FieldSpec> = variants
        .iter()
        .filter(|v| v.skip.is_none())
        .flat_map(|v| v.fields.iter())
        .collect();
    let has_constraints = emit_has_constraints(container, &field_types, base_constraints);

    let body = if all_unit {
        emit_enum_string_schema(container, &variants)
    } else {
        emit_enum_composed_schema(container, &repr, &variants)
    };
    let schema = emit_schema_impl(container, body, has_constraints);
    let responses = emit_response_impls(container);

    quote! {
        #serde
        #validate
        #schema
        #responses
    }
}

/// The shapes serde cannot represent, reported by the macro so the error names
/// the user's variant instead of a serde internal.
fn check_enum_shape(
    container: &Container,
    repr: &Repr,
    variants: &[VariantSpec],
    errors: &mut Diagnostics,
) {
    if !container.from.is_empty() {
        errors.help(
            container.ident.span(),
            "`from` is only generated for structs",
            "write the `impl From<…>` by hand: an enum conversion has to choose a variant",
        );
    }

    if matches!(repr, Repr::Internal(_)) {
        for variant in variants {
            if variant.kind == VariantKind::Tuple {
                errors.help(
                    variant.span,
                    format!(
                        "`{}` holds several values, which an internally tagged enum cannot carry",
                        variant.ident
                    ),
                    "name the values:\n    Variant { first: A, second: B }\nor drop the `tag` to \
                     use the default representation",
                );
            }
        }
    }

    if matches!(repr, Repr::Untagged) {
        let mut seen: Vec<(String, &VariantSpec)> = Vec::new();
        for variant in variants {
            let signature = untagged_signature(variant);
            if let Some((_, first)) = seen.iter().find(|(s, _)| *s == signature) {
                let shape = if variant.kind == VariantKind::Unit {
                    "both are `null`"
                } else {
                    "they have the same shape"
                };
                errors.help(
                    variant.span,
                    format!(
                        "`{}` and `{}` are indistinguishable in an untagged enum: {shape}",
                        first.ident, variant.ident
                    ),
                    "give them different shapes, or use `#[schema(tag = \"kind\")]` so the wire \
                     format says which variant it is",
                );
            } else {
                seen.push((signature, variant));
            }
        }
    }
}

/// A variant's wire shape, as far as an untagged match can tell them apart.
fn untagged_signature(variant: &VariantSpec) -> String {
    match variant.kind {
        VariantKind::Unit => "null".to_owned(),
        VariantKind::Newtype => format!("newtype:{}", variant.fields[0].ty.to_token_stream()),
        VariantKind::Tuple => format!("tuple:{}", variant.fields.len()),
        VariantKind::Struct => {
            let mut names: Vec<String> = variant
                .fields
                .iter()
                .filter(|f| !f.is_skipped())
                .map(|f| format!("{}:{}", f.wire_name, u8::from(f.is_optional())))
                .collect();
            names.sort();
            format!("struct:{}", names.join(","))
        }
    }
}

/// The shadow enum and the two delegations.
fn emit_enum_serde(container: &Container, variants: &[VariantSpec]) -> TokenStream {
    let p = private();
    let ident = &container.ident;
    let remote = ident.to_string();
    let defaults: TokenStream = variants
        .iter()
        .map(|variant| emit_default_fns(&variant.prefix(ident), &variant.fields))
        .collect();
    if container.no_serde {
        return defaults;
    }
    let container_attrs = container.serde_attrs();
    let generics = &container.generics;
    let where_clause = container.generics.where_clause.as_ref();

    let shadow_variants: Vec<TokenStream> = variants
        .iter()
        .map(|variant| {
            let attrs = variant.serde_attrs(container.rename_all);
            let prefix = variant.prefix(ident);
            let forwarded = &variant.forwarded;
            let name = &variant.ident;
            let body = match variant.kind {
                VariantKind::Unit => TokenStream::new(),
                VariantKind::Newtype | VariantKind::Tuple => {
                    let entries: Vec<TokenStream> = variant
                        .fields
                        .iter()
                        .map(|field| {
                            let inner = field.serde_attrs(None, &prefix);
                            let ty = &field.ty;
                            quote!(#inner #ty)
                        })
                        .collect();
                    quote!((#(#entries),*))
                }
                VariantKind::Struct => {
                    let entries: Vec<TokenStream> = variant
                        .fields
                        .iter()
                        .map(|field| {
                            let inner = field.serde_attrs(None, &prefix);
                            let forwarded = forwarded_attrs(field);
                            let name = field.ident.as_ref().expect("named field");
                            let ty = &field.ty;
                            quote!(#(#forwarded)* #inner #name: #ty)
                        })
                        .collect();
                    quote!({ #(#entries),* })
                }
            };
            quote!(#(#forwarded)* #attrs #name #body)
        })
        .collect();

    let (serialize, deserialize) = serde_delegations(container, TokenStream::new());

    quote! {
        #defaults

        #[doc(hidden)]
        // As in the struct arm: the shadow mirrors the user's variant and field
        // names and is never named outside this block.
        #[allow(non_camel_case_types, non_snake_case, dead_code, clippy::all, clippy::pedantic)]
        const _: () = {
            use #p::serde as _serde;

            #[derive(_serde::Serialize, _serde::Deserialize)]
            #[serde(crate = "_serde")]
            #[serde(remote = #remote)]
            #[serde(#container_attrs)]
            enum __MosoSerde #generics #where_clause {
                #(#shadow_variants),*
            }

            #serialize
            #deserialize
        };
    }
}

/// One match arm per variant, validating whatever it carries.
fn emit_enum_validations(variants: &[VariantSpec]) -> TokenStream {
    let mut statics = 0usize;
    let mut arms: Vec<TokenStream> = Vec::new();

    for variant in variants {
        let (pattern, bindings) = variant.pattern();
        let mut body = TokenStream::new();
        for (field, binding) in variant.fields.iter().zip(&bindings) {
            if field.is_skipped() {
                continue;
            }
            let block = emit_one_field_validation(field, binding, &mut statics);
            if !block.is_empty() {
                body.extend(quote!({ #block }));
            }
        }
        let forwarded = &variant.forwarded;
        arms.push(quote!(#(#forwarded)* #pattern => { #body }));
    }

    if arms.is_empty() {
        return TokenStream::new();
    }
    quote! {
        match self {
            #(#arms)*
        }
    }
}

/// An enum whose variants all carry nothing: a string with an `enum` keyword,
/// which is what a client generator turns into a real union type.
fn emit_enum_string_schema(container: &Container, variants: &[VariantSpec]) -> TokenStream {
    let p = private();
    let names: Vec<&String> = variants
        .iter()
        .filter(|v| v.skip.is_none())
        .map(|v| &v.wire_name)
        .collect();
    let node = quote!(__node);
    let annotations = emit_container_annotations(container, &node);
    quote! {
        let mut __node = #p::StringBuilder::new()
            .enumeration(::std::vec![
                #(#p::serde_json::Value::from(#names)),*
            ])
            .build();
        #annotations
        __node
    }
}

/// The `oneOf` construction for an enum that carries data, in whichever
/// representation the container asked for.
fn emit_enum_composed_schema(
    container: &Container,
    repr: &Repr,
    variants: &[VariantSpec],
) -> TokenStream {
    let p = private();
    let node = quote!(__node);
    let annotations = emit_container_annotations(container, &node);
    let mut blocks: Vec<TokenStream> = Vec::new();

    for variant in variants {
        if variant.skip.is_some() {
            continue;
        }
        let prefix = variant.prefix(&container.ident);
        let prefix = prefix.as_str();
        let payload = emit_variant_payload(variant, prefix);
        let wire = &variant.wire_name;
        let description = variant.description.as_ref().map(|text| {
            quote!(__variant.description = ::core::option::Option::Some(
                ::std::borrow::Cow::Borrowed(#text)
            );)
        });
        let build = match repr {
            Repr::External => match variant.kind {
                VariantKind::Unit => quote! {
                    let mut __variant = #p::SchemaNode::constant(
                        #p::serde_json::Value::from(#wire)
                    );
                },
                _ => quote! {
                    let __payload = { #payload };
                    let mut __variant = #p::ObjectBuilder::new()
                        .property(#wire, __payload, true)
                        .additional_properties(false)
                        .build();
                },
            },
            Repr::Internal(tag) => match variant.kind {
                VariantKind::Unit => quote! {
                    let mut __variant = #p::ObjectBuilder::new()
                        .property(
                            #tag,
                            #p::SchemaNode::constant(#p::serde_json::Value::from(#wire)),
                            true,
                        )
                        .build();
                },
                VariantKind::Struct => {
                    let properties = emit_variant_properties(variant, prefix);
                    quote! {
                        let mut __object = #p::ObjectBuilder::new().property(
                            #tag,
                            #p::SchemaNode::constant(#p::serde_json::Value::from(#wire)),
                            true,
                        );
                        #properties
                        let mut __variant = __object.build();
                    }
                }
                _ => quote! {
                    let __payload = { #payload };
                    let mut __variant = #p::ObjectBuilder::new()
                        .property(
                            #tag,
                            #p::SchemaNode::constant(#p::serde_json::Value::from(#wire)),
                            true,
                        )
                        .all_of(__payload)
                        .build();
                },
            },
            Repr::Adjacent(tag, content) => match variant.kind {
                VariantKind::Unit => quote! {
                    let mut __variant = #p::ObjectBuilder::new()
                        .property(
                            #tag,
                            #p::SchemaNode::constant(#p::serde_json::Value::from(#wire)),
                            true,
                        )
                        .build();
                },
                _ => quote! {
                    let __payload = { #payload };
                    let mut __variant = #p::ObjectBuilder::new()
                        .property(
                            #tag,
                            #p::SchemaNode::constant(#p::serde_json::Value::from(#wire)),
                            true,
                        )
                        .property(#content, __payload, true)
                        .build();
                },
            },
            Repr::Untagged => match variant.kind {
                VariantKind::Unit => quote!(let mut __variant = #p::SchemaNode::null();),
                _ => quote!(let mut __variant = { #payload };),
            },
        };

        // An internally tagged union is only usable by a client generator when
        // every arm is a `$ref` the discriminator can map onto, so each variant
        // becomes a named component.
        let register = if matches!(repr, Repr::Internal(_)) {
            let suffix = variant.ident.to_string();
            quote! {
                let __name = ::std::format!(
                    "{}_{}",
                    <Self as #p::Schema>::schema_name(),
                    #suffix
                );
                __mapping.push((#wire.to_owned(), __generator.ref_for(&__name)));
                let __reference = __generator.insert(__name, __variant);
                __variants.push(#p::SchemaNode::from(__reference));
            }
        } else {
            quote!(__variants.push(__variant);)
        };

        let forwarded = &variant.forwarded;
        blocks.push(quote! {
            #(#forwarded)*
            {
                #build
                #description
                #register
            }
        });
    }

    let discriminator = match repr {
        Repr::Internal(tag) => quote! {
            let mut __discriminator = #p::Discriminator::new(#tag);
            for (__tag, __reference) in __mapping {
                __discriminator = __discriminator.with_mapping(__tag, __reference);
            }
            __node.discriminator = ::core::option::Option::Some(__discriminator);
        },
        _ => TokenStream::new(),
    };

    quote! {
        let mut __variants: ::std::vec::Vec<#p::SchemaNode> = ::std::vec::Vec::new();
        let mut __mapping: ::std::vec::Vec<(::std::string::String, ::std::string::String)> =
            ::std::vec::Vec::new();
        #(#blocks)*
        let mut __node = #p::SchemaNode::one_of(__variants);
        #discriminator
        #annotations
        __node
    }
}

/// The schema of whatever a variant carries, ignoring the tagging.
fn emit_variant_payload(variant: &VariantSpec, prefix: &str) -> TokenStream {
    let p = private();
    match variant.kind {
        VariantKind::Unit => quote!(#p::SchemaNode::null()),
        VariantKind::Newtype => {
            let body = emit_field_node(&variant.fields[0], prefix);
            quote!(#body __field)
        }
        VariantKind::Tuple => {
            let items: Vec<TokenStream> = variant
                .fields
                .iter()
                .map(|field| {
                    let body = emit_field_node(field, prefix);
                    quote!(.prefix_item({ #body __field }))
                })
                .collect();
            let count = Literal::u64_suffixed(variant.fields.len() as u64);
            quote! {
                #p::ArrayBuilder::new()
                    #(#items)*
                    .min_items(#count)
                    .max_items(#count)
                    .build()
            }
        }
        VariantKind::Struct => {
            let properties = emit_variant_properties(variant, prefix);
            quote! {
                let mut __object = #p::ObjectBuilder::new();
                #properties
                __object.build()
            }
        }
    }
}

/// The properties of a struct variant, added to an `__object` in scope.
fn emit_variant_properties(variant: &VariantSpec, prefix: &str) -> TokenStream {
    let mut out = TokenStream::new();
    for field in &variant.fields {
        if field.is_skipped() {
            continue;
        }
        let body = emit_field_node(field, prefix);
        let name = &field.wire_name;
        let required = !field.is_optional();
        let forwarded = forwarded_attrs(field);
        out.extend(quote! {
            #(#forwarded)*
            {
                #body
                __object = __object.property(#name, __field, #required);
            }
        });
    }
    out
}

// ---------------------------------------------------------------------------
// #[derive(Constrained)]
// ---------------------------------------------------------------------------

/// Which family of generated API a constrained newtype gets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    /// `inner = String`: the full string newtype treatment.
    Text,
    /// A numeric primitive.
    Number(Shape),
    /// Anything else: construction, conversion and the schema, no `Deref`.
    Opaque,
}

/// The parsed `#[constrained(...)]` of a newtype.
struct ConstrainedSpec {
    inner: Type,
    name: String,
    family: Family,
    title: Option<String>,
    description: Option<String>,
    secret: Option<Span>,
    checks: Vec<Path>,
    constraints: Constraints,
}

fn expand_constrained(input: &DeriveInput, errors: &mut Diagnostics) -> TokenStream {
    let ident = &input.ident;

    let Data::Struct(data) = &input.data else {
        errors.help(
            ident.span(),
            "`Constrained` describes a newtype",
            "wrap the value you are constraining:\n    #[derive(Constrained)]\n    \
             #[constrained(inner = String, pattern = r\"^ORD-\\d{8}$\")]\n    pub struct \
             OrderNumber(String);",
        );
        return TokenStream::new();
    };
    let Fields::Unnamed(unnamed) = &data.fields else {
        errors.help(
            ident.span(),
            "`Constrained` needs a newtype with exactly one field",
            "write it as `pub struct OrderNumber(String);`",
        );
        return TokenStream::new();
    };
    if unnamed.unnamed.len() != 1 {
        errors.help(
            unnamed.span(),
            "`Constrained` needs a newtype with exactly one field",
            "write it as `pub struct OrderNumber(String);`",
        );
        return TokenStream::new();
    }
    if !input.generics.params.is_empty() {
        errors.help(
            input.generics.span(),
            "a constrained newtype cannot be generic",
            "its invariant has to be checkable for one concrete inner type",
        );
        return TokenStream::new();
    }

    let field_ty = unnamed.unnamed[0].ty.clone();
    let mut spec = ConstrainedSpec {
        inner: field_ty.clone(),
        name: ident.to_string(),
        family: Family::Opaque,
        title: None,
        description: doc_text(&input.attrs),
        secret: None,
        checks: Vec::new(),
        constraints: Constraints::default(),
    };

    for attr in input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("constrained"))
    {
        let result = attr.parse_nested_meta(|meta| {
            let Some(key) = meta.path.get_ident().map(ToString::to_string) else {
                errors.error(meta.path.span(), "expected a plain name");
                return Ok(());
            };
            if spec.constraints.parse_key(&key, &meta, errors) {
                return Ok(());
            }
            match key.as_str() {
                "inner" => {
                    if let Some(expr) = expr_value(&meta, "inner", errors) {
                        match expr_as_type(&expr, "inner") {
                            Ok(ty) => spec.inner = ty,
                            Err(error) => errors.push(error),
                        }
                    }
                }
                "name" => {
                    if let Some(value) = string_value(&meta, "name", errors) {
                        spec.name = value;
                    }
                }
                "title" => spec.title = string_value(&meta, "title", errors),
                "description" => spec.description = string_value(&meta, "description", errors),
                "secret" => spec.secret = Some(meta.path.span()),
                "check" => {
                    if let Some(expr) = expr_value(&meta, "check", errors) {
                        match expr_as_path(&expr, "check") {
                            Ok(path) => spec.checks.push(path),
                            Err(error) => errors.push(error),
                        }
                    }
                }
                other => {
                    errors.unknown_key(meta.path.span(), "constrained", other, CONSTRAINED_KEYS);
                    let _ = meta.value().and_then(|v| v.parse::<Expr>());
                }
            }
            Ok(())
        });
        if let Err(error) = result {
            errors.push(error);
        }
    }

    if spec.inner.to_token_stream().to_string() != field_ty.to_token_stream().to_string() {
        errors.help(
            spec.inner.span(),
            "`inner` names a different type from the one the newtype holds",
            format!(
                "make them the same: `inner = {}`",
                field_ty.to_token_stream()
            ),
        );
        spec.inner = field_ty;
    }

    spec.family = match Shape::of(&spec.inner) {
        Shape::Text
            if crate::util::attrs::type_ident(&spec.inner).is_some_and(|i| i == "String") =>
        {
            Family::Text
        }
        shape if shape.is_numeric() => Family::Number(shape),
        _ => Family::Opaque,
    };
    spec.constraints.validate(errors);

    emit_constrained(ident, &spec, errors)
}

/// A `ConstraintError` expression, message and parameters decided at macro
/// time so the constructor allocates nothing on the happy path.
fn constraint_error(
    code: TokenStream,
    message: &str,
    params: &[(&str, TokenStream)],
) -> TokenStream {
    let p = private();
    let params = params
        .iter()
        .map(|(key, value)| quote!(.with_param(#key, #value)));
    quote!(#p::ConstraintError::new(#p::ErrorCode::#code, #message) #(#params)*)
}

/// The early returns that enforce a constrained type's invariant.
fn emit_constrained_guards(spec: &ConstrainedSpec, statics: &mut usize) -> TokenStream {
    let p = private();
    let c = &spec.constraints;
    let mut out = TokenStream::new();
    let text = quote!(__value.as_str());

    let len = c.len.map(|(range, _)| range).or_else(|| {
        c.non_empty.is_some().then_some(LenRange {
            min: Some(1),
            max: None,
        })
    });
    if let Some(range) = len {
        let unit = if spec.family == Family::Text {
            "characters"
        } else {
            "items"
        };
        let message = match (range.min, range.max) {
            (Some(min), Some(max)) if min == max => format!("must be exactly {min} {unit}"),
            (Some(min), Some(max)) => format!("must be between {min} and {max} {unit}"),
            (Some(min), None) => format!("must be at least {min} {unit}"),
            (None, Some(max)) => format!("must be at most {max} {unit}"),
            (None, None) => String::new(),
        };
        let mut params: Vec<(&str, TokenStream)> = Vec::new();
        if let Some(min) = range.min {
            let lit = Literal::u64_suffixed(min);
            params.push(("min", quote!(#lit)));
        }
        if let Some(max) = range.max {
            let lit = Literal::u64_suffixed(max);
            params.push(("max", quote!(#lit)));
        }
        params.push(("unit", quote!(#unit)));
        let error = constraint_error(quote!(Len), &message, &params);
        let min_test = range.min.map(|min| {
            let lit = Literal::usize_suffixed(usize::try_from(min).unwrap_or(usize::MAX));
            quote!(__length < #lit)
        });
        let max_test = range.max.map(|max| {
            let lit = Literal::usize_suffixed(usize::try_from(max).unwrap_or(usize::MAX));
            quote!(__length > #lit)
        });
        let test = match (min_test, max_test) {
            (Some(a), Some(b)) => quote!(#a || #b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => quote!(false),
        };
        out.extend(quote! {
            let __length = #text.chars().count();
            if #test {
                return ::core::result::Result::Err(#error);
            }
        });
    }

    if let Some(pattern) = &c.pattern {
        let source = &pattern.source;
        let name = format_ident!("__MOSO_RE_{}", *statics);
        *statics += 1;
        let message = format!("must match {source}");
        let error = constraint_error(quote!(Pattern), &message, &[("pattern", quote!(#source))]);
        out.extend(quote_spanned! {pattern.span=>
            static #name: ::std::sync::OnceLock<#p::regex::Regex> = ::std::sync::OnceLock::new();
            let __regex = #name.get_or_init(|| match #p::regex::Regex::new(#source) {
                ::core::result::Result::Ok(__re) => __re,
                ::core::result::Result::Err(__error) => {
                    ::core::panic!("moso: invalid pattern: {}", __error)
                }
            });
            if !__regex.is_match(#text) {
                return ::core::result::Result::Err(#error);
            }
        });
    }

    if let Some((format, span)) = &c.format {
        let message = format!("must be a valid {format}");
        let error = constraint_error(quote!(Format), &message, &[("format", quote!(#format))]);
        out.extend(quote_spanned! {*span=>
            if #p::is_valid_format(#format, #text) == ::core::option::Option::Some(false) {
                return ::core::result::Result::Err(#error);
            }
        });
    }

    for (needle, kind, method) in [
        (&c.contains, "contains", quote!(contains)),
        (&c.starts_with, "starts_with", quote!(starts_with)),
        (&c.ends_with, "ends_with", quote!(starts_with)),
    ] {
        let Some((literal, span)) = needle else {
            continue;
        };
        let method = if kind == "ends_with" {
            quote!(ends_with)
        } else {
            method
        };
        let message = match kind {
            "contains" => format!("must contain {literal:?}"),
            "starts_with" => format!("must start with {literal:?}"),
            _ => format!("must end with {literal:?}"),
        };
        let error = constraint_error(quote!(Pattern), &message, &[(kind, quote!(#literal))]);
        out.extend(quote_spanned! {*span=>
            if !#text.#method(#literal) {
                return ::core::result::Result::Err(#error);
            }
        });
    }

    if let Family::Number(shape) = spec.family
        && let Some((min, max, exclusive_min, exclusive_max)) = effective_range(c)
    {
        let message = match (min, max) {
            (Some(min), Some(max)) => format!(
                "must be between {} and {}",
                format_num(min),
                format_num(max)
            ),
            (Some(min), None) if exclusive_min => {
                format!("must be greater than {}", format_num(min))
            }
            (Some(min), None) => format!("must be at least {}", format_num(min)),
            (None, Some(max)) if exclusive_max => format!("must be less than {}", format_num(max)),
            (None, Some(max)) => format!("must be at most {}", format_num(max)),
            (None, None) => String::new(),
        };
        let mut params: Vec<(&str, TokenStream)> = Vec::new();
        if let Some(min) = min {
            let lit = num_literal(min, shape);
            params.push(("min", quote!(#lit)));
        }
        if let Some(max) = max {
            let lit = num_literal(max, shape);
            params.push(("max", quote!(#lit)));
        }
        let error = constraint_error(quote!(Range), &message, &params);
        let cast = number_cast(shape);
        let min_test = min.map(|min| {
            let lit = num_literal(min, shape);
            if exclusive_min {
                quote!(__number <= #lit)
            } else {
                quote!(__number < #lit)
            }
        });
        let max_test = max.map(|max| {
            let lit = num_literal(max, shape);
            if exclusive_max {
                quote!(__number >= #lit)
            } else {
                quote!(__number > #lit)
            }
        });
        let test = match (min_test, max_test) {
            (Some(a), Some(b)) => quote!(#a || #b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => quote!(false),
        };
        out.extend(quote! {
            let __number = __value as #cast;
            if #test {
                return ::core::result::Result::Err(#error);
            }
        });
    }

    if let Family::Number(shape) = spec.family
        && let Some((divisor, span)) = c.multiple_of
    {
        let message = format!("must be a multiple of {}", format_num(divisor));
        let literal = num_literal(divisor, shape);
        let error = constraint_error(
            quote!(MultipleOf),
            &message,
            &[("multiple_of", quote!(#literal))],
        );
        let cast = number_cast(shape);
        let test = if shape == Shape::Float {
            quote!(__remainder.abs() > 1e-9f64)
        } else {
            quote!(__remainder != (0 as #cast))
        };
        out.extend(quote_spanned! {span=>
            let __remainder = if #literal == (0 as #cast) {
                0 as #cast
            } else {
                (__value as #cast) % #literal
            };
            if #test {
                return ::core::result::Result::Err(#error);
            }
        });
    }

    let checks: Vec<TokenStream> = spec
        .checks
        .iter()
        .map(|path| {
            quote_spanned! {path.span()=>
                #path(&__value)?;
            }
        })
        .collect();
    out.extend(quote!(#(#checks)*));

    out
}

/// A bound as it reads in a message: `13`, not `13i64`.
fn format_num(num: Num) -> String {
    match num {
        Num::Int(value) => value.to_string(),
        Num::Float(value) => value.to_string(),
    }
}

/// The literal of a bound, in the width the comparison uses.
fn num_literal(num: Num, shape: Shape) -> Literal {
    match shape {
        Shape::Float => Literal::f64_suffixed(num.as_f64()),
        Shape::UnsignedInt => Literal::u64_suffixed(
            u64::try_from(num.as_int().unwrap_or(0).max(0)).unwrap_or(u64::MAX),
        ),
        _ => Literal::i64_suffixed(
            num.as_int()
                .unwrap_or(0)
                .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        ),
    }
}

/// The type a numeric comparison is performed in.
fn number_cast(shape: Shape) -> TokenStream {
    match shape {
        Shape::Float => quote!(f64),
        Shape::UnsignedInt => quote!(u64),
        _ => quote!(i64),
    }
}

/// Every impl a constrained newtype gets.
fn emit_constrained(
    ident: &Ident,
    spec: &ConstrainedSpec,
    errors: &mut Diagnostics,
) -> TokenStream {
    let p = private();
    let inner = &spec.inner;
    let name = &spec.name;
    let mut statics = 0usize;
    let guards = emit_constrained_guards(spec, &mut statics);
    let allows = generated_allows();

    if spec.constraints.unique.is_some() || spec.constraints.enum_values.is_some() {
        errors.help(
            ident.span(),
            "`unique` and `enum_values` are field constraints",
            "a constrained newtype describes one value — use `pattern` or `range` instead",
        );
    }
    if spec.family == Family::Opaque && !spec.constraints.is_empty() {
        errors.help(
            inner.span(),
            format!(
                "`{}` is neither a `String` nor a number, so the built-in constraints cannot be \
                 checked on it",
                inner.to_token_stream()
            ),
            "wrap a `String` or a numeric primitive, or use `check = my_function` to write the \
             invariant yourself",
        );
    }

    let normalise = {
        let c = &spec.constraints;
        let mut out = TokenStream::new();
        if c.trim {
            out.extend(quote! {
                let __trimmed = __value.trim();
                if __trimmed.len() != __value.len() {
                    __value = ::std::borrow::ToOwned::to_owned(__trimmed);
                }
            });
        }
        if c.lowercase {
            out.extend(quote!(__value = __value.to_lowercase();));
        }
        if c.uppercase {
            out.extend(quote!(__value = __value.to_uppercase();));
        }
        out
    };

    let doc_new = format!(
        "Construct a `{ident}`, checking its invariant.\n\n# Errors\nReturns the constraint that \
         the value violates."
    );
    let doc_unchecked = format!(
        "Construct a `{ident}` **without** checking its invariant.\n\nFor values that are already \
         known to be valid — a round-trip from the database, a test fixture. Every other \
         construction path goes through [`{ident}::new`]."
    );
    let doc_inner = format!("The wrapped value, consuming the `{ident}`.");

    let construction = match spec.family {
        Family::Text => quote! {
            // Generated constructors are not the user's to lint.
            #[allow(clippy::all, clippy::pedantic)]
            impl #ident {
                #[doc = #doc_new]
                pub fn new(
                    value: impl ::core::convert::Into<::std::string::String>,
                ) -> ::core::result::Result<Self, #p::ConstraintError> {
                    let mut __value: ::std::string::String = ::core::convert::Into::into(value);
                    #normalise
                    #guards
                    ::core::result::Result::Ok(Self(__value))
                }

                #[doc = #doc_unchecked]
                pub fn new_unchecked(
                    value: impl ::core::convert::Into<::std::string::String>,
                ) -> Self {
                    Self(::core::convert::Into::into(value))
                }

                #[doc = "The value as a string slice."]
                pub fn as_str(&self) -> &str {
                    &self.0
                }

                #[doc = #doc_inner]
                pub fn into_string(self) -> ::std::string::String {
                    self.0
                }

                #[doc = #doc_inner]
                pub fn into_inner(self) -> ::std::string::String {
                    self.0
                }
            }
        },
        Family::Number(_) | Family::Opaque => quote! {
            // As above.
            #[allow(clippy::all, clippy::pedantic)]
            impl #ident {
                #[doc = #doc_new]
                pub fn new(value: #inner) -> ::core::result::Result<Self, #p::ConstraintError> {
                    let __value = value;
                    #guards
                    ::core::result::Result::Ok(Self(__value))
                }

                #[doc = #doc_unchecked]
                pub fn new_unchecked(value: #inner) -> Self {
                    Self(value)
                }

                #[doc = #doc_inner]
                pub fn into_inner(self) -> #inner {
                    self.0
                }
            }
        },
    };

    let conversions = match spec.family {
        Family::Text => {
            let secret = spec.secret.is_some();
            let display = (!secret).then(|| {
                quote! {
                    #allows
                    impl ::core::fmt::Display for #ident {
                        fn fmt(&self, __f: &mut ::core::fmt::Formatter<'_>)
                            -> ::core::fmt::Result
                        {
                            __f.write_str(self.as_str())
                        }
                    }

                    #allows
                    impl ::core::convert::AsRef<str> for #ident {
                        fn as_ref(&self) -> &str {
                            self.as_str()
                        }
                    }

                    #allows
                    impl ::core::ops::Deref for #ident {
                        type Target = str;

                        fn deref(&self) -> &str {
                            self.as_str()
                        }
                    }

                    #allows
                    impl ::core::borrow::Borrow<str> for #ident {
                        fn borrow(&self) -> &str {
                            self.as_str()
                        }
                    }
                }
            });
            quote! {
                #display

                #allows
                impl ::core::str::FromStr for #ident {
                    type Err = #p::ConstraintError;

                    fn from_str(__s: &str) -> ::core::result::Result<Self, Self::Err> {
                        Self::new(__s)
                    }
                }

                #allows
                impl ::core::convert::TryFrom<::std::string::String> for #ident {
                    type Error = #p::ConstraintError;

                    fn try_from(__s: ::std::string::String)
                        -> ::core::result::Result<Self, Self::Error>
                    {
                        Self::new(__s)
                    }
                }

                #allows
                impl<'__a> ::core::convert::TryFrom<&'__a str> for #ident {
                    type Error = #p::ConstraintError;

                    fn try_from(__s: &'__a str) -> ::core::result::Result<Self, Self::Error> {
                        Self::new(__s)
                    }
                }

                #allows
                impl ::core::convert::From<#ident> for ::std::string::String {
                    fn from(__v: #ident) -> ::std::string::String {
                        __v.into_string()
                    }
                }
            }
        }
        Family::Number(_) | Family::Opaque => quote! {
            #allows
            impl ::core::convert::TryFrom<#inner> for #ident {
                type Error = #p::ConstraintError;

                fn try_from(__v: #inner) -> ::core::result::Result<Self, Self::Error> {
                    Self::new(__v)
                }
            }

            #allows
            impl ::core::convert::From<#ident> for #inner {
                fn from(__v: #ident) -> #inner {
                    __v.into_inner()
                }
            }
        },
    };

    let debug = spec.secret.map(|_| {
        let label = ident.to_string();
        quote! {
            #allows
            impl ::core::fmt::Debug for #ident {
                fn fmt(&self, __f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    __f.debug_tuple(#label).field(&#REDACTED).finish()
                }
            }
        }
    });

    let node = quote!(__node);
    let constraints = emit_schema_constraints(&spec.constraints, &node);
    let title = spec.title.as_ref().map(|text| {
        quote!(__node.title = ::core::option::Option::Some(::std::borrow::Cow::Borrowed(#text));)
    });
    let description = spec.description.as_ref().map(|text| {
        quote!(
            __node.description =
                ::core::option::Option::Some(::std::borrow::Cow::Borrowed(#text));
        )
    });
    let write_only = spec.secret.map(|_| quote!(__node.write_only = true;));

    quote! {
        #construction
        #conversions
        #debug

        #allows
        impl #p::serde::Serialize for #ident {
            fn serialize<__S>(&self, __serializer: __S)
                -> ::core::result::Result<__S::Ok, __S::Error>
            where
                __S: #p::serde::Serializer,
            {
                #p::serde::Serialize::serialize(&self.0, __serializer)
            }
        }

        #allows
        impl<'de> #p::serde::Deserialize<'de> for #ident {
            fn deserialize<__D>(__deserializer: __D)
                -> ::core::result::Result<Self, __D::Error>
            where
                __D: #p::serde::Deserializer<'de>,
            {
                let __raw = <#inner as #p::serde::Deserialize<'de>>::deserialize(__deserializer)?;
                match Self::new(__raw) {
                    ::core::result::Result::Ok(__value) => ::core::result::Result::Ok(__value),
                    ::core::result::Result::Err(__error) => {
                        ::core::result::Result::Err(__error.into_serde_error())
                    }
                }
            }
        }

        #allows
        impl #p::Validate for #ident {
            fn validate(
                &self,
                _ctx: &mut #p::ValidationCtx,
            ) -> ::core::result::Result<(), #p::ValidationErrors> {
                // The invariant was established on construction; a value of
                // this type cannot be invalid.
                ::core::result::Result::Ok(())
            }
        }

        #allows
        impl #p::Schema for #ident {
            fn schema_name() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(#name)
            }

            fn json_schema(__generator: &mut #p::SchemaGenerator) -> #p::SchemaNode {
                let mut __node = __generator.subschema_for::<#inner>();
                #constraints
                #title
                #description
                #write_only
                __node
            }

            fn schema_ref() -> #p::SchemaRef {
                // Constrained newtypes are written out in place rather than
                // registered: `components/schemas` holds the application's
                // models, not a one-line alias for every wrapper.
                #p::inline_schema_ref::<Self>()
            }

            const HAS_CONSTRAINTS: bool = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    /// Parse one field's `#[schema(...)]`, returning its spec and any errors.
    fn field(input: syn::Field) -> (FieldSpec, Diagnostics) {
        let mut errors = Diagnostics::new();
        let spec = FieldSpec::parse(&input, 0, None, &mut errors);
        (spec, errors)
    }

    /// The rendered text of the first error, or the empty string.
    fn message(errors: Diagnostics) -> String {
        errors
            .into_error()
            .map(|e| e.to_string())
            .unwrap_or_default()
    }

    /// Expand a derive and prove the result is syntactically valid Rust.
    fn expand(input: DeriveInput) -> String {
        let tokens = derive_schema(input);
        syn::parse2::<syn::File>(tokens.clone()).expect("the expansion must parse as a Rust file");
        tokens.to_string()
    }

    fn expand_constrained_input(input: DeriveInput) -> String {
        let tokens = derive_constrained(input);
        syn::parse2::<syn::File>(tokens.clone()).expect("the expansion must parse as a Rust file");
        tokens.to_string()
    }

    // ── the response impls, and who emits them ──────────────────────────

    #[test]
    fn a_plain_schema_emits_the_response_impls() {
        let out = expand(parse_quote! {
            struct UserOut { id: u64 }
        });
        assert!(out.contains("IntoResponse"), "{out}");
        assert!(out.contains("Describe"), "{out}");
    }

    #[test]
    fn a_sibling_responder_attribute_suppresses_them() {
        // `#[derive(Schema, Responder)]` is the documented way to return a body
        // with a status other than 200. Both derives generate `IntoResponse` +
        // `Describe`, so emitting ours here is a coherence error the user
        // cannot act on.
        let out = expand(parse_quote! {
            #[responder(status = 201)]
            struct UserCreated { id: u64 }
        });
        assert!(
            !out.contains("IntoResponse"),
            "`Responder` owns the response impls when it is present:\n{out}"
        );
        assert!(
            !out.contains("impl :: moso :: __private :: Describe"),
            "{out}"
        );
        // The schema itself is still generated — only the response pair moves.
        assert!(out.contains("json_schema"), "{out}");
    }

    #[test]
    fn explicit_no_response_still_suppresses_them() {
        let out = expand(parse_quote! {
            #[schema(no_response)]
            struct UserOut { id: u64 }
        });
        assert!(!out.contains("IntoResponse"), "{out}");
    }

    // ── ranges ──────────────────────────────────────────────────────────

    #[test]
    fn inclusive_length_ranges_keep_both_bounds() {
        let (spec, errors) = field(parse_quote! {
            #[schema(len = 3..=32)]
            username: String
        });
        assert!(errors.is_empty());
        assert_eq!(
            spec.constraints.len.map(|(range, _)| range),
            Some(LenRange {
                min: Some(3),
                max: Some(32)
            })
        );
    }

    #[test]
    fn open_length_ranges_leave_one_side_unset() {
        let (lower, _) = field(parse_quote! {
            #[schema(len = 12..)]
            password: String
        });
        assert_eq!(
            lower.constraints.len.map(|(r, _)| r),
            Some(LenRange {
                min: Some(12),
                max: None
            })
        );

        let (upper, _) = field(parse_quote! {
            #[schema(len = ..=10)]
            tags: Vec<String>
        });
        assert_eq!(
            upper.constraints.len.map(|(r, _)| r),
            Some(LenRange {
                min: None,
                max: Some(10)
            })
        );
    }

    #[test]
    fn a_half_open_length_becomes_its_inclusive_equivalent() {
        // `1..10` admits nine values; `maxLength` is inclusive, so it is 9.
        let (spec, errors) = field(parse_quote! {
            #[schema(len = 1..10)]
            name: String
        });
        assert!(errors.is_empty());
        assert_eq!(spec.constraints.len.unwrap().0.max, Some(9));
    }

    #[test]
    fn a_bare_length_means_exactly_that_many() {
        let (spec, _) = field(parse_quote! {
            #[schema(len = 4)]
            code: String
        });
        assert_eq!(
            spec.constraints.len.map(|(r, _)| r),
            Some(LenRange {
                min: Some(4),
                max: Some(4)
            })
        );
    }

    #[test]
    fn a_float_range_keeps_its_exclusive_upper_bound() {
        let (spec, errors) = field(parse_quote! {
            #[schema(range = 0.0..1.0)]
            ratio: f64
        });
        assert!(errors.is_empty(), "{}", message(errors));
        let range = spec.constraints.range.expect("a range");
        assert_eq!(range.min, Some(Num::Float(0.0)));
        assert_eq!(range.max, Some(Num::Float(1.0)));
        assert!(range.exclusive_max);
    }

    #[test]
    fn negative_bounds_parse() {
        let (spec, errors) = field(parse_quote! {
            #[schema(range = -40..=85)]
            celsius: i8
        });
        assert!(errors.is_empty(), "{}", message(errors));
        assert_eq!(spec.constraints.range.unwrap().min, Some(Num::Int(-40)));
    }

    #[test]
    fn a_range_with_no_bounds_is_rejected_with_a_fix() {
        let (_, errors) = field(parse_quote! {
            #[schema(range = ..)]
            n: u8
        });
        let text = message(errors);
        assert!(text.contains("needs at least one bound"), "{text}");
        assert!(text.contains("help: write it as"), "{text}");
    }

    #[test]
    fn a_non_literal_bound_names_the_rule() {
        let (_, errors) = field(parse_quote! {
            #[schema(range = MIN..=MAX)]
            n: u8
        });
        let text = message(errors);
        assert!(text.contains("must be a literal number"), "{text}");
    }

    #[test]
    fn an_empty_range_is_rejected() {
        let (_, errors) = field(parse_quote! {
            #[schema(len = 32..=3)]
            name: String
        });
        assert!(message(errors).contains("empty"));
    }

    #[test]
    fn positive_folds_into_an_exclusive_lower_bound() {
        let (spec, _) = field(parse_quote! {
            #[schema(positive)]
            amount: i64
        });
        let (min, max, exclusive_min, exclusive_max) =
            effective_range(&spec.constraints).expect("a range");
        assert_eq!(min, Some(Num::Int(0)));
        assert_eq!(max, None);
        assert!(exclusive_min);
        assert!(!exclusive_max);
    }

    #[test]
    fn non_negative_folds_into_an_inclusive_zero() {
        let (spec, _) = field(parse_quote! {
            #[schema(non_negative)]
            amount: i64
        });
        let (min, _, exclusive_min, _) = effective_range(&spec.constraints).expect("a range");
        assert_eq!(min, Some(Num::Int(0)));
        assert!(!exclusive_min);
    }

    #[test]
    fn positive_and_non_negative_together_are_a_contradiction() {
        let (_, errors) = field(parse_quote! {
            #[schema(positive, non_negative)]
            amount: i64
        });
        assert!(message(errors).contains("different things"));
    }

    // ── patterns and formats ────────────────────────────────────────────

    #[test]
    fn a_valid_pattern_is_kept_verbatim() {
        let (spec, errors) = field(parse_quote! {
            #[schema(pattern = r"^[a-z0-9_]+$")]
            username: String
        });
        assert!(errors.is_empty());
        assert_eq!(spec.constraints.pattern.unwrap().source, "^[a-z0-9_]+$");
    }

    #[test]
    fn an_invalid_pattern_is_a_compile_error() {
        let (spec, errors) = field(parse_quote! {
            #[schema(pattern = r"^[a-z")]
            username: String
        });
        let text = message(errors);
        assert!(text.contains("does not compile"), "{text}");
        assert!(text.contains("unclosed character class"), "{text}");
        assert!(
            spec.constraints.pattern.is_none(),
            "no half-parsed pattern survives"
        );
    }

    #[test]
    fn a_near_miss_format_is_corrected() {
        let (_, errors) = field(parse_quote! {
            #[schema(format = "emial")]
            email: String
        });
        assert!(message(errors).contains("did you mean `email`?"));
    }

    #[test]
    fn an_unrelated_format_is_accepted_as_an_annotation() {
        let (spec, errors) = field(parse_quote! {
            #[schema(format = "order-number")]
            order: String
        });
        assert!(errors.is_empty());
        assert_eq!(spec.constraints.format.unwrap().0, "order-number");
    }

    // ── the vocabulary ──────────────────────────────────────────────────

    #[test]
    fn an_unknown_key_suggests_the_closest_one() {
        let (_, errors) = field(parse_quote! {
            #[schema(lenght = 3..=32)]
            username: String
        });
        let text = message(errors);
        assert!(
            text.contains("unknown `schema` attribute `lenght`"),
            "{text}"
        );
        assert!(text.contains("did you mean `len`?"), "{text}");
    }

    #[test]
    fn a_field_key_inside_each_says_where_it_belongs() {
        let (_, errors) = field(parse_quote! {
            #[schema(each(unique))]
            tags: Vec<String>
        });
        let text = message(errors);
        assert!(
            text.contains("applies to the field, not to each element"),
            "{text}"
        );
    }

    #[test]
    fn every_documented_field_key_parses() {
        // One field per row of the vocabulary tables in `01-http/13`.
        let cases: Vec<syn::Field> = vec![
            parse_quote!(#[schema(len = 1..=2)] a: String),
            parse_quote!(#[schema(pattern = "x")] a: String),
            parse_quote!(#[schema(format = "uuid")] a: String),
            parse_quote!(#[schema(trim)] a: String),
            parse_quote!(#[schema(lowercase)] a: String),
            parse_quote!(#[schema(uppercase)] a: String),
            parse_quote!(#[schema(non_empty)] a: String),
            parse_quote!(#[schema(contains = "x")] a: String),
            parse_quote!(#[schema(starts_with = "x")] a: String),
            parse_quote!(#[schema(ends_with = "x")] a: String),
            parse_quote!(#[schema(range = 1..=2)] a: u8),
            parse_quote!(#[schema(multiple_of = 5)] a: u8),
            parse_quote!(#[schema(positive)] a: i8),
            parse_quote!(#[schema(non_negative)] a: i8),
            parse_quote!(#[schema(unique)] a: Vec<u8>),
            parse_quote!(#[schema(each(len = 1..=2))] a: Vec<String>),
            parse_quote!(#[schema(nested)] a: Inner),
            parse_quote!(#[schema(default = 1)] a: u8),
            parse_quote!(#[schema(rename = "b")] a: u8),
            parse_quote!(#[schema(skip)] a: u8),
            parse_quote!(#[schema(read_only)] a: u8),
            parse_quote!(#[schema(write_only)] a: u8),
            parse_quote!(#[schema(secret)] a: Password),
            parse_quote!(#[schema(deprecated = "gone")] a: u8),
            parse_quote!(#[schema(example = 1)] a: u8),
            parse_quote!(#[schema(flatten)] a: Inner),
            parse_quote!(#[schema(title = "T")] a: u8),
            parse_quote!(#[schema(description = "D")] a: u8),
            parse_quote!(#[schema(enum_values = ["a", "b"])] a: String),
            parse_quote!(#[schema(delimiter = ",")] a: Vec<String>),
            parse_quote!(#[schema(flatten_bracket)] a: Inner),
        ];
        for case in cases {
            let rendered = case.to_token_stream().to_string();
            let (_, errors) = field(case);
            assert!(errors.is_empty(), "{rendered}: {}", message(errors));
        }
    }

    #[test]
    fn secret_implies_write_only() {
        let (spec, _) = field(parse_quote! {
            #[schema(secret)]
            password: Password
        });
        assert!(spec.write_only);
        assert!(spec.secret.is_some());
    }

    /// Secrecy is a property of the type, not of the attribute: a `String`
    /// marked secret still prints itself everywhere except this struct's
    /// derived `Debug`.
    #[test]
    fn secret_on_a_leaky_type_is_rejected_with_a_fix() {
        let (_, errors) = field(parse_quote! {
            #[schema(secret)]
            password: String
        });
        let rendered = message(errors);
        assert!(rendered.contains("needs a secret type"), "{rendered}");
        assert!(rendered.contains("pub password: Password,"), "{rendered}");
    }

    #[test]
    fn secret_on_a_secret_type_is_accepted_through_option_and_box() {
        let cases: Vec<syn::Field> = vec![
            parse_quote!(#[schema(secret)] a: Password),
            parse_quote!(#[schema(secret)] a: SecretString),
            parse_quote!(#[schema(secret)] a: Option<Password>),
            parse_quote!(#[schema(secret)] a: Box<SecretString>),
            // A user newtype cannot be proven leaky, so it is left alone.
            parse_quote!(#[schema(secret)] a: ApiKey),
        ];
        for case in cases {
            let rendered = case.to_token_stream().to_string();
            let (_, errors) = field(case);
            assert!(errors.is_empty(), "{rendered}: {}", message(errors));
        }
    }

    #[test]
    fn a_secret_byte_string_is_pointed_at_secret_bytes() {
        let (_, errors) = field(parse_quote! {
            #[schema(secret)]
            token: Vec<u8>
        });
        assert!(message(errors).contains("pub token: SecretBytes,"));
    }

    #[test]
    fn rename_reaches_the_pointer() {
        let mut errors = Diagnostics::new();
        let input: syn::Field = parse_quote!(created_at: String);
        let spec = FieldSpec::parse(&input, 0, Some(RenameRule::Camel), &mut errors);
        assert_eq!(spec.wire_name, "createdAt");
        assert_eq!(spec.pointer, "/createdAt");
    }

    #[test]
    fn a_slash_in_a_name_is_escaped_in_the_pointer() {
        let (spec, _) = field(parse_quote! {
            #[schema(rename = "a/b")]
            a: String
        });
        assert_eq!(spec.pointer, "/a~1b");
    }

    #[test]
    fn optionality_comes_from_the_type_or_a_default() {
        let (plain, _) = field(parse_quote!(a: String));
        assert!(!plain.is_optional());
        let (option, _) = field(parse_quote!(a: Option<String>));
        assert!(option.is_optional());
        let (defaulted, _) = field(parse_quote!(#[schema(default = 1)] a: u8));
        assert!(defaulted.is_optional());
    }

    // ── shapes ──────────────────────────────────────────────────────────

    #[test]
    fn shapes_are_classified_through_option() {
        assert_eq!(Shape::of(&parse_quote!(String)), Shape::Text);
        assert_eq!(Shape::of(&parse_quote!(Option<u8>)), Shape::UnsignedInt);
        assert_eq!(Shape::of(&parse_quote!(Option<Box<i64>>)), Shape::SignedInt);
        assert_eq!(Shape::of(&parse_quote!(f32)), Shape::Float);
        assert_eq!(Shape::of(&parse_quote!(Vec<String>)), Shape::Sequence);
        assert_eq!(Shape::of(&parse_quote!(BTreeMap<String, u8>)), Shape::Map);
        assert_eq!(Shape::of(&parse_quote!([u8; 4])), Shape::Sequence);
        assert_eq!(Shape::of(&parse_quote!(bool)), Shape::Bool);
        // Anything unrecognised is treated as text, which is what a constrained
        // string newtype almost always is.
        assert_eq!(Shape::of(&parse_quote!(Email)), Shape::Text);
    }

    #[test]
    fn a_rejected_constraint_is_removed_so_nothing_cascades() {
        let (spec, errors) = field(parse_quote! {
            #[schema(each(len = 1..=2))]
            count: u32
        });
        assert!(!errors.is_empty());
        assert!(
            spec.each.is_none(),
            "the `each` rules must not reach codegen"
        );
    }

    #[test]
    fn a_text_rule_on_a_number_is_rejected_and_removed() {
        let (spec, errors) = field(parse_quote! {
            #[schema(pattern = "x")]
            count: u32
        });
        assert!(message(errors).contains("needs a string"));
        assert!(spec.constraints.pattern.is_none());
    }

    #[test]
    fn password_is_read_through_expose() {
        let value = quote!(__value);
        let text = text_accessor(&parse_quote!(Password), &value).to_string();
        assert!(text.contains("expose"), "{text}");
        let text = text_accessor(&parse_quote!(String), &value).to_string();
        assert!(text.contains("AsRef"), "{text}");
    }

    // ── defaults ────────────────────────────────────────────────────────

    #[test]
    fn a_quoted_path_is_code_and_a_quoted_word_is_a_string() {
        let code = unquote_expr(parse_quote!("Locale::EN"));
        assert!(matches!(code, Expr::Path(_)), "a `::` path is code");
        let call = unquote_expr(parse_quote!("Vec::new()"));
        assert!(matches!(call, Expr::Call(_)), "a call is code");
        let text = unquote_expr(parse_quote!("hello"));
        assert!(
            matches!(
                text,
                Expr::Lit(ExprLit {
                    lit: Lit::Str(_),
                    ..
                })
            ),
            "a plain word stays a string"
        );
    }

    // ── expansions ──────────────────────────────────────────────────────

    #[test]
    fn the_reference_struct_expands_to_the_documented_shape() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            pub struct CreateUser {
                /// Public handle.
                #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
                pub username: String,
                pub email: Email,
                #[schema(range = 13..=130)]
                pub age: Option<u8>,
            }
        });
        assert!(out.contains("impl :: moso :: __private :: Validate for CreateUser"));
        assert!(out.contains("impl :: moso :: __private :: Schema for CreateUser"));
        assert!(out.contains("check_len_str"));
        assert!(out.contains("check_pattern"));
        // An unsigned field is checked in `u64`: `62-macro-reference.md`
        // sketches `check_range_i64`, but casting a `u64` through `i64` would
        // wrap at 2^63 and turn a large valid value into a range failure.
        assert!(out.contains("check_range_u64"));
        assert!(out.contains("apply_len"));
        assert!(out.contains("\"/username\""));
        assert!(out.contains("\"/age\""));
        assert!(out.contains("OnceLock"));
        assert!(out.contains("remote = \"CreateUser\""));
        assert!(out.contains("HAS_CONSTRAINTS : bool = true"));
        assert!(!out.contains("compile_error"), "{out}");
    }

    #[test]
    fn one_attribute_produces_both_halves() {
        // The property the whole design rests on: `len = 3..=32` must appear as
        // a runtime check *and* as a document keyword, from one source.
        let out = expand(parse_quote! {
            #[derive(Schema)]
            struct S {
                #[schema(len = 3..=32)]
                a: String,
            }
        });
        assert!(out.contains("check_len_str"), "the runtime half");
        assert!(
            out.contains("apply_len (:: core :: option :: Option :: Some (3u64)"),
            "the documented half"
        );
    }

    #[test]
    fn a_constraint_free_model_documents_no_422() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            struct S {
                a: String,
                b: u32,
            }
        });
        assert!(
            out.contains("HAS_CONSTRAINTS : bool = false || < String"),
            "the constant is computed from the fields, not hard-coded: {out}"
        );
    }

    #[test]
    fn a_self_referential_field_is_left_out_of_has_constraints() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            struct Category {
                name: String,
                children: Vec<Category>,
            }
        });
        assert!(
            !out.contains(
                "< Vec < Category > as :: moso :: __private :: Schema > :: HAS_CONSTRAINTS"
            ),
            "that would be a const evaluation cycle: {out}"
        );
    }

    #[test]
    fn a_secret_field_gets_a_redacting_debug() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            struct S {
                #[schema(secret)]
                password: Password,
            }
        });
        assert!(out.contains("impl :: core :: fmt :: Debug for S"));
        assert!(out.contains("[redacted]"));
        assert!(out.contains("skip_serializing"));
        assert!(out.contains("write_only = true"));
    }

    #[test]
    fn a_model_without_secrets_keeps_its_own_debug() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            struct S {
                a: String,
            }
        });
        assert!(!out.contains("impl :: core :: fmt :: Debug for S"));
    }

    #[test]
    fn no_serde_leaves_the_serde_impls_to_the_user() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            #[schema(no_serde)]
            struct S {
                a: String,
            }
        });
        assert!(!out.contains("remote ="));
        assert!(out.contains("impl :: moso :: __private :: Schema for S"));
    }

    #[test]
    fn from_generates_one_conversion_per_source() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            #[schema(from = User)]
            struct UserOut {
                id: u32,
                name: String,
            }
        });
        assert!(out.contains("impl :: core :: convert :: From < User > for UserOut"));
        assert!(out.contains("id : :: core :: convert :: Into :: into (__source . id)"));
    }

    #[test]
    fn the_four_enum_representations_expand() {
        let external = expand(parse_quote! {
            #[derive(Schema)]
            enum E { A(u8), B { c: u8 } }
        });
        assert!(external.contains("one_of"));

        let internal = expand(parse_quote! {
            #[derive(Schema)]
            #[schema(tag = "kind")]
            enum E { A { c: u8 }, B { c: u8 } }
        });
        assert!(internal.contains("Discriminator :: new (\"kind\")"));
        assert!(internal.contains("tag = \"kind\""));

        let adjacent = expand(parse_quote! {
            #[derive(Schema)]
            #[schema(tag = "kind", content = "data")]
            enum E { A(u8) }
        });
        assert!(adjacent.contains("content = \"data\""));

        let untagged = expand(parse_quote! {
            #[derive(Schema)]
            #[schema(untagged)]
            enum E { A(u8), B(String) }
        });
        assert!(untagged.contains("untagged"));
        assert!(!untagged.contains("Discriminator"));
    }

    #[test]
    fn a_unit_only_enum_is_a_string_enum() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            #[schema(rename_all = "snake_case")]
            enum Status { Draft, Published }
        });
        assert!(out.contains("StringBuilder"));
        assert!(out.contains("\"draft\""));
        assert!(out.contains("\"published\""));
    }

    #[test]
    fn an_internally_tagged_tuple_variant_is_rejected() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            #[schema(tag = "kind")]
            enum E { A(u8, u8) }
        });
        assert!(out.contains("compile_error"));
        assert!(out.contains("internally tagged enum cannot carry"));
    }

    #[test]
    fn a_borrowing_model_is_rejected_with_the_reason() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            struct S<'a> { a: &'a str }
        });
        assert!(out.contains("cannot borrow"));
        assert!(out.contains("'static"));
    }

    #[test]
    fn a_union_is_rejected_with_a_fix() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            union U { a: u32 }
        });
        assert!(out.contains("cannot be derived for a union"));
    }

    #[test]
    fn deny_unknown_and_flatten_cannot_both_be_used() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            #[schema(deny_unknown)]
            struct S {
                #[schema(flatten)]
                inner: Inner,
            }
        });
        assert!(out.contains("cannot both be used"));
    }

    #[test]
    fn generics_bound_by_schema_and_named_by_the_mangler() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            struct Page<T> { items: Vec<T> }
        });
        assert!(out.contains("impl < T : :: moso :: __private :: Schema >"));
        assert!(out.contains("generic_schema_name (\"Page\""));
    }

    #[test]
    fn each_reaches_both_the_loop_and_the_items_schema() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            struct S {
                #[schema(each(len = 1..=24))]
                tags: Vec<String>,
            }
        });
        assert!(out.contains("__element_pointer"));
        assert!(out.contains("\"/tags/{}\""));
        assert!(out.contains("items . as_deref_mut"));
    }

    #[test]
    fn a_newtype_reports_at_the_root_pointer() {
        let out = expand(parse_quote! {
            #[derive(Schema)]
            struct Wrapper(#[schema(len = 1..=8)] String);
        });
        assert!(out.contains("check_len_str"));
        assert!(
            out.contains("\"\""),
            "the whole document is the value: {out}"
        );
    }

    // ── #[derive(Constrained)] ──────────────────────────────────────────

    #[test]
    fn the_reference_constrained_newtype_expands() {
        let out = expand_constrained_input(parse_quote! {
            #[derive(Constrained)]
            #[constrained(inner = String, pattern = r"^ORD-\d{8}$", format = "order-number")]
            pub struct OrderNumber(String);
        });
        assert!(out.contains("pub fn new"));
        assert!(out.contains("new_unchecked"));
        assert!(out.contains("ConstraintError"));
        assert!(out.contains("impl :: core :: str :: FromStr for OrderNumber"));
        assert!(out.contains("impl :: core :: ops :: Deref for OrderNumber"));
        assert!(out.contains("inline_schema_ref"));
        assert!(out.contains("into_serde_error"));
        assert!(!out.contains("compile_error"), "{out}");
    }

    #[test]
    fn a_constrained_number_gets_a_range_guard_and_no_deref() {
        let out = expand_constrained_input(parse_quote! {
            #[derive(Constrained)]
            #[constrained(inner = u16, range = 1..=65000)]
            pub struct Port(u16);
        });
        assert!(out.contains("ErrorCode :: Range"));
        assert!(!out.contains("Deref"));
        assert!(out.contains("impl :: core :: convert :: TryFrom < u16 > for Port"));
    }

    #[test]
    fn a_constrained_secret_is_redacted_and_write_only() {
        let out = expand_constrained_input(parse_quote! {
            #[derive(Constrained)]
            #[constrained(inner = String, secret, len = 12..)]
            pub struct Token(String);
        });
        assert!(out.contains("[redacted]"));
        assert!(out.contains("write_only = true"));
        assert!(!out.contains("impl :: core :: fmt :: Display for Token"));
    }

    #[test]
    fn constrained_needs_a_newtype() {
        let out = expand_constrained_input(parse_quote! {
            #[derive(Constrained)]
            #[constrained(inner = String)]
            pub struct NotANewtype { value: String }
        });
        assert!(out.contains("needs a newtype"));
    }

    #[test]
    fn an_inner_that_disagrees_with_the_field_is_reported() {
        let out = expand_constrained_input(parse_quote! {
            #[derive(Constrained)]
            #[constrained(inner = u32)]
            pub struct Odd(String);
        });
        assert!(out.contains("names a different type"));
    }

    #[test]
    fn every_expansion_is_valid_rust() {
        // `expand` parses its output as a `syn::File`; this exercises the
        // shapes that are easiest to get syntactically wrong.
        let shapes: Vec<DeriveInput> = vec![
            parse_quote!(
                #[derive(Schema)]
                struct Unit;
            ),
            parse_quote!(
                #[derive(Schema)]
                struct Tuple(u8, String);
            ),
            parse_quote!(
                #[derive(Schema)]
                struct Newtype(u8);
            ),
            parse_quote!(
                #[derive(Schema)]
                struct Generic<T, const N: usize> {
                    a: [T; N],
                }
            ),
            parse_quote!(
                #[derive(Schema)]
                enum Empty {}
            ),
            parse_quote! {
                #[derive(Schema)]
                #[schema(rename_all = "kebab-case", deny_unknown, title = "T", description = "D")]
                struct Everything {
                    #[schema(len = 1..=2, trim, lowercase)]
                    a: String,
                    #[schema(each(pattern = "x"), unique)]
                    b: Vec<String>,
                    #[schema(range = 1..=2, multiple_of = 2)]
                    c: u8,
                    #[schema(nested)]
                    d: Inner,
                    #[schema(skip)]
                    e: u8,
                    #[schema(flatten, nested)]
                    f: Inner,
                }
            },
        ];
        for shape in shapes {
            let _ = expand(shape);
        }
    }
}
