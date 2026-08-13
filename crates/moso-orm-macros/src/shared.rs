//! Helpers every expander needs: the private path, name derivation, the
//! attribute grammar, and the one-error-per-mistake convention.

use heck::{ToSnakeCase as _, ToUpperCamelCase as _};
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned as _;
use syn::{Attribute, Expr, GenericArgument, Lit, PathArguments, Token, Type};

/// The only path generated code may name (decision D6).
///
/// Everything a generated body needs is re-exported there, so this crate never
/// names `moso_orm`, `moso_core` or `moso_sql` — which is what lets those move
/// without touching a macro.
///
/// ```text
/// let path = private_path();
/// assert_eq!(path.to_string().replace(' ', ""), "::moso::__private");
/// ```
pub fn private_path() -> TokenStream {
    quote!(::moso::__private)
}

/// The default table name for a type: `snake_case`, pluralised.
///
/// The pluralisation is deliberately naïve — `s`, or `es` after a sibilant, or
/// `ies` for a consonant followed by `y`. English is not regular and a macro
/// that guesses `people` from `Person` would be wrong more often than useful;
/// `#[entity(table = "people")]` is one line and says what it means.
///
/// ```text
/// assert_eq!(default_table_name("User"), "users");
/// assert_eq!(default_table_name("PostTag"), "post_tags");
/// assert_eq!(default_table_name("Address"), "addresses");
/// assert_eq!(default_table_name("Category"), "categories");
/// assert_eq!(default_table_name("Status"), "statuses");
/// ```
#[must_use]
pub fn default_table_name(type_name: &str) -> String {
    let snake = type_name.to_snake_case();
    pluralise(&snake)
}

/// Pluralises one `snake_case` word.
///
/// ```text
/// assert_eq!(pluralise("post"), "posts");
/// assert_eq!(pluralise("box"), "boxes");
/// ```
#[must_use]
pub fn pluralise(word: &str) -> String {
    let ends_with_sibilant = word.ends_with('s')
        || word.ends_with('x')
        || word.ends_with('z')
        || word.ends_with("ch")
        || word.ends_with("sh");

    if ends_with_sibilant {
        return format!("{word}es");
    }

    if let Some(stem) = word.strip_suffix('y') {
        let preceded_by_consonant = stem
            .chars()
            .next_back()
            .is_some_and(|character| !matches!(character, 'a' | 'e' | 'i' | 'o' | 'u'));
        if preceded_by_consonant {
            return format!("{stem}ies");
        }
    }

    format!("{word}s")
}

/// The default column name for a field: the field name, unchanged.
///
/// Fields are already `snake_case` in idiomatic Rust, and silently rewriting
/// one would make `#[entity(column = "…")]` look optional when it is not.
///
/// ```text
/// assert_eq!(default_column_name("created_at"), "created_at");
/// assert_eq!(default_column_name("isAdmin"), "is_admin");
/// ```
#[must_use]
pub fn default_column_name(field_name: &str) -> String {
    field_name.to_snake_case()
}

/// The name of the generated insert struct for an entity.
///
/// ```text
/// assert_eq!(new_struct_name("User"), "NewUser");
/// ```
#[must_use]
pub fn new_struct_name(type_name: &str) -> String {
    format!("New{}", type_name.to_upper_camel_case())
}

/// The name of the generated factory struct for an entity.
///
/// ```text
/// assert_eq!(factory_struct_name("User"), "UserFactory");
/// ```
#[must_use]
pub fn factory_struct_name(type_name: &str) -> String {
    format!("{}Factory", type_name.to_upper_camel_case())
}

/// The name of the generated column constant for a field.
///
/// ```text
/// assert_eq!(column_const_name("created_at"), "CREATED_AT");
/// assert_eq!(column_const_name("email"), "EMAIL");
/// ```
#[must_use]
pub fn column_const_name(field_name: &str) -> String {
    field_name.to_snake_case().to_uppercase()
}

/// The default index name for a column, matching what PostgreSQL would pick.
///
/// ```text
/// assert_eq!(default_index_name("users", &["email"]), "users_email_idx");
/// assert_eq!(default_index_name("posts", &["a", "b"]), "posts_a_b_idx");
/// ```
#[must_use]
pub fn default_index_name(table: &str, columns: &[&str]) -> String {
    format!("{table}_{}_idx", columns.join("_"))
}

/// The default foreign-key constraint name, matching what PostgreSQL would
/// pick for `ALTER TABLE … ADD FOREIGN KEY`.
///
/// ```text
/// assert_eq!(default_foreign_key_name("posts", "author_id"), "posts_author_id_fkey");
/// ```
#[must_use]
pub fn default_foreign_key_name(table: &str, column: &str) -> String {
    format!("{table}_{column}_fkey")
}

/// A message with exactly one `help:` line, the shape every hand-written
/// diagnostic in this crate takes.
///
/// ```text
/// assert_eq!(with_help("broken", "fix it"), "broken\n  help: fix it");
/// ```
#[must_use]
pub fn with_help(message: &str, help: &str) -> String {
    format!("{message}\n  help: {help}")
}

/// One `syn::Error` with a `help:` line, for the fallible parsing paths.
///
/// ```text
/// let error = err(proc_macro2::Span::call_site(), "broken", "fix it");
/// assert!(error.to_string().contains("help:"));
/// ```
#[must_use]
pub fn err(span: Span, message: &str, help: &str) -> syn::Error {
    syn::Error::new(span, with_help(message, help))
}

/// One `syn::Error` in the full house shape: what is wrong, one sentence saying
/// why the rule exists, then the fix.
///
/// Most of this crate's diagnostics stop at [`err`], because a misspelt setting
/// needs no justification — the suggestion *is* the explanation. A rule a
/// reader could reasonably think arbitrary earns the middle line, and
/// `docs/04-devex/41-diagnostics.md` asks for it in that order.
///
/// ```text
/// let error = err_with_note(span, "broken", "because", "fix it");
/// assert_eq!(error.to_string(), "broken\n  note: because\n  help: fix it");
/// ```
#[must_use]
pub fn err_with_note(span: Span, message: &str, note: &str, help: &str) -> syn::Error {
    syn::Error::new(span, with_help(&format!("{message}\n  note: {note}"), help))
}

/// The optimal-string-alignment distance between two attribute names.
///
/// Levenshtein with one addition: a transposition costs **one** edit, not two.
/// That matters because `tabel` for `table` is the single most common
/// attribute typo, and plain Levenshtein scores it 2 — far enough to suppress
/// the suggestion exactly when it would be most useful.
///
/// ```text
/// assert_eq!(edit_distance("table", "tabel"), 1);
/// assert_eq!(edit_distance("pk", "pk"), 0);
/// assert!(edit_distance("unique", "banana") > 3);
/// ```
#[must_use]
pub fn edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut distance = vec![vec![0_usize; right.len() + 1]; left.len() + 1];

    for (i, row) in distance.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in distance[0].iter_mut().enumerate() {
        *cell = j;
    }

    for i in 1..=left.len() {
        for j in 1..=right.len() {
            let substitution = usize::from(left[i - 1] != right[j - 1]);
            let mut best = (distance[i - 1][j] + 1)
                .min(distance[i][j - 1] + 1)
                .min(distance[i - 1][j - 1] + substitution);
            if i > 1 && j > 1 && left[i - 1] == right[j - 2] && left[i - 2] == right[j - 1] {
                best = best.min(distance[i - 2][j - 2] + 1);
            }
            distance[i][j] = best;
        }
    }
    distance[left.len()][right.len()]
}

/// The closest known attribute to `unknown`, when one is close enough to
/// suggest.
///
/// ```text
/// assert_eq!(did_you_mean("uniqe", &["unique", "index"]), Some("unique"));
/// assert_eq!(did_you_mean("banana", &["unique", "index"]), None);
/// ```
#[must_use]
pub fn did_you_mean<'a>(unknown: &str, known: &[&'a str]) -> Option<&'a str> {
    let threshold = (unknown.len() / 3).max(1);
    known
        .iter()
        .map(|candidate| (edit_distance(unknown, candidate), *candidate))
        .filter(|(distance, _)| *distance <= threshold)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// The "that is not a setting" error, with a suggestion when one is close and
/// the full list when none is.
///
/// ```text
/// let error = unknown_setting("tabel", &["table"], proc_macro2::Span::call_site(), "entity");
/// assert!(error.to_string().contains("did you mean `table`?"));
/// ```
#[must_use]
pub fn unknown_setting(unknown: &str, known: &[&str], span: Span, macro_name: &str) -> syn::Error {
    let help = did_you_mean(unknown, known).map_or_else(
        || format!("the settings are: {}", known.join(", ")),
        |candidate| format!("did you mean `{candidate}`?"),
    );
    err(
        span,
        &format!("`{unknown}` is not an `#[{macro_name}(..)]` setting"),
        &help,
    )
}

// ---------------------------------------------------------------------------
// Identifier validation
// ---------------------------------------------------------------------------

/// The longest identifier PostgreSQL keeps, in bytes. Longer names are silently
/// truncated by the server, which turns two columns into one.
pub const MAX_IDENT_LEN: usize = 63;

/// Rejects a name `moso_sql::Ident::from_static` would panic on, at macro
/// expansion time and against the user's own span.
///
/// The generated code is full of `Ident::from_static("…")`, whose failure mode
/// is a `const` evaluation panic pointing at generated tokens. Checking here
/// turns that into one sentence naming the attribute the user wrote.
///
/// ```text
/// assert!(validate_sql_ident("email", span, "column").is_ok());
/// assert!(validate_sql_ident("", span, "column").is_err());
/// ```
pub fn validate_sql_ident(raw: &str, span: Span, what: &str) -> syn::Result<()> {
    if raw.is_empty() {
        return Err(err(
            span,
            &format!("an empty {what} name is not a SQL identifier"),
            &format!("give the {what} a name, as in `column = \"email\"`"),
        ));
    }
    if raw.len() > MAX_IDENT_LEN {
        return Err(err(
            span,
            &format!(
                "the {what} name `{raw}` is {} bytes, and PostgreSQL keeps {MAX_IDENT_LEN}",
                raw.len()
            ),
            "shorten it — the server truncates silently, which can merge two names into one",
        ));
    }
    if let Some(bad) = raw
        .bytes()
        .find(|byte| byte.is_ascii_control() || matches!(byte, b'"' | b'`' | b'\\'))
    {
        return Err(err(
            span,
            &format!(
                "the {what} name `{raw}` contains the byte {bad:#04x}, which cannot be quoted"
            ),
            "use letters, digits and underscores",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Type inspection
// ---------------------------------------------------------------------------

/// The `T` of an `Option<T>`, when the written type is syntactically one.
///
/// Syntactic, deliberately: a macro cannot resolve a type alias, and a column
/// whose nullability depended on one would be nullable in the migration and
/// not in the decoder. `Option<T>` written out is the contract.
///
/// ```text
/// let ty: syn::Type = syn::parse_quote!(Option<String>);
/// assert!(option_inner(&ty).is_some());
/// ```
#[must_use]
pub fn option_inner(ty: &Type) -> Option<&Type> {
    generic_argument_of(ty, "Option")
}

/// The `T` of a `Related<T>`, which is what marks a field a relation.
///
/// ```text
/// let ty: syn::Type = syn::parse_quote!(Related<Vec<Post>>);
/// assert!(related_inner(&ty).is_some());
/// ```
#[must_use]
pub fn related_inner(ty: &Type) -> Option<&Type> {
    generic_argument_of(ty, "Related")
}

/// The `T` of a `Vec<T>`.
///
/// ```text
/// let ty: syn::Type = syn::parse_quote!(Vec<Post>);
/// assert!(vec_inner(&ty).is_some());
/// ```
#[must_use]
pub fn vec_inner(ty: &Type) -> Option<&Type> {
    generic_argument_of(ty, "Vec")
}

/// The single generic argument of `Wrapper<T>`, when the last path segment is
/// `wrapper`.
fn generic_argument_of<'t>(ty: &'t Type, wrapper: &str) -> Option<&'t Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != wrapper {
        return None;
    }
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    arguments.args.iter().find_map(|argument| match argument {
        GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })
}

/// The last identifier of a path type — `User` from `crate::models::User`.
///
/// ```text
/// let ty: syn::Type = syn::parse_quote!(crate::models::User);
/// assert_eq!(type_name_of(&ty).unwrap(), "User");
/// ```
#[must_use]
pub fn type_name_of(ty: &Type) -> Option<String> {
    let Type::Path(path) = ty else {
        return None;
    };
    path.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

// ---------------------------------------------------------------------------
// Documentation
// ---------------------------------------------------------------------------

/// The `///` comment on an item, joined into one paragraph.
///
/// Used as the column's `COMMENT ON`, so that the sentence a developer already
/// wrote for the reader of the struct is the sentence a DBA reads in `psql`.
///
/// ```text
/// assert_eq!(doc_comment(&[]), None);
/// ```
#[must_use]
pub fn doc_comment(attrs: &[Attribute]) -> Option<String> {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let syn::Meta::NameValue(pair) = &attr.meta else {
            continue;
        };
        let Expr::Lit(syn::ExprLit {
            lit: Lit::Str(text),
            ..
        }) = &pair.value
        else {
            continue;
        };
        lines.push(text.value().trim().to_owned());
    }
    let joined = lines.join(" ").trim().to_owned();
    (!joined.is_empty()).then_some(joined)
}

// ---------------------------------------------------------------------------
// The attribute grammar
// ---------------------------------------------------------------------------

/// The right-hand side of a `key = …`, or one positional item of a `key(…)`.
///
/// Two shapes and no more: a literal (`"users"`, `255`) or a type path
/// (`User`, `crate::models::Post`). `syn::Meta` cannot express the second,
/// which is why this crate parses its own attributes.
#[derive(Clone, Debug)]
pub enum SettingValue {
    /// A literal: `"users"`, `255`, `true`.
    Lit(Lit),
    /// A type path: `User`, `crate::models::Post`.
    Type(Box<Type>),
}

impl SettingValue {
    /// The string behind a `key = "value"`.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] when the value is not a string literal.
    pub fn string(&self) -> syn::Result<String> {
        match self {
            Self::Lit(Lit::Str(text)) => Ok(text.value()),
            other => Err(err(
                other.span(),
                "this setting takes a string",
                "quote it, as in `table = \"users\"`",
            )),
        }
    }

    /// The integer behind a `key = 255`.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] when the value is not an integer literal, or does not fit.
    pub fn integer<T>(&self) -> syn::Result<T>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        match self {
            Self::Lit(Lit::Int(number)) => number.base10_parse::<T>(),
            other => Err(err(
                other.span(),
                "this setting takes a whole number",
                "write it without quotes, as in `len = 255`",
            )),
        }
    }

    /// The type behind a `key = User`.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] when the value is a literal rather than a path.
    pub fn ty(&self) -> syn::Result<Type> {
        match self {
            Self::Type(ty) => Ok((**ty).clone()),
            other => Err(err(
                other.span(),
                "this setting takes a type",
                "name the entity without quotes, as in `has_many = Post`",
            )),
        }
    }

    /// Where the value was written.
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            Self::Lit(lit) => lit.span(),
            Self::Type(ty) => ty.span(),
        }
    }
}

impl Parse for SettingValue {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.peek(Lit) {
            return Ok(Self::Lit(input.parse()?));
        }
        Ok(Self::Type(Box::new(input.parse()?)))
    }
}

/// One item inside `#[entity(..)]`, `#[projection(..)]`, `#[embedded(..)]`,
/// `#[db_enum(..)]`, `#[factory(..)]` or `#[migration(..)]`.
///
/// The grammar is uniform across all six so that a user who has learnt one
/// attribute has learnt the rest.
#[derive(Clone, Debug)]
pub enum Setting {
    /// A bare flag: `pk`, `timestamps`, `unique`.
    Word(syn::Ident),
    /// A named value: `table = "users"`, `has_many = Post`, `len = 255`.
    Assign(syn::Ident, SettingValue),
    /// A nested list: `index(columns("a"), unique)`, `precision(10, 2)`.
    Call(syn::Ident, Vec<Setting>),
    /// A positional item inside a list: the `"a"` of `columns("a")`.
    Positional(SettingValue),
    /// `where = "deleted_at is null"`. `where` is a Rust keyword and therefore
    /// cannot be an [`syn::Ident`], which is the whole reason this variant
    /// exists rather than being an [`Setting::Assign`].
    Where(SettingValue),
    /// `as = "text"`. `as` is a keyword, exactly like `where`.
    As(SettingValue),
}

impl Setting {
    /// The setting's name, for the "did you mean" path.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::Word(name) | Self::Assign(name, _) | Self::Call(name, _) => name.to_string(),
            Self::Positional(_) => String::from("<value>"),
            Self::Where(_) => String::from("where"),
            Self::As(_) => String::from("as"),
        }
    }

    /// Where it was written.
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            Self::Word(name) | Self::Assign(name, _) | Self::Call(name, _) => name.span(),
            Self::Positional(value) | Self::Where(value) | Self::As(value) => value.span(),
        }
    }

    /// The value of a `key = value`, refusing the flag and list forms with a
    /// message that shows the shape the setting wants.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] when the setting is not a `key = value`.
    pub fn value(&self) -> syn::Result<&SettingValue> {
        match self {
            Self::Assign(_, value) | Self::Where(value) | Self::As(value) => Ok(value),
            other => Err(err(
                other.span(),
                &format!("`{}` takes a value", other.name()),
                &format!("write it as `{} = \"…\"`", other.name()),
            )),
        }
    }

    /// The setting read as a type, for a positional entity name.
    ///
    /// A bare `Post` inside `types(Post, Comment)` lexes as an identifier with
    /// nothing after it, which is [`Setting::Word`]; a qualified
    /// `crate::models::Post` lexes as a type. Both mean the same thing here.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] when the setting is a literal or a list.
    pub fn as_type(&self) -> syn::Result<Type> {
        match self {
            Self::Word(name) => Ok(Type::Path(syn::TypePath {
                qself: None,
                path: syn::Path::from(name.clone()),
            })),
            Self::Positional(value) => value.ty(),
            other => Err(err(
                other.span(),
                "this list takes entity names",
                "write them without quotes, as in `types(Post, Comment)`",
            )),
        }
    }

    /// The items of a `key(..)`, refusing the other two forms.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] when the setting is not a list.
    pub fn items(&self) -> syn::Result<&[Setting]> {
        match self {
            Self::Call(_, items) => Ok(items),
            other => Err(err(
                other.span(),
                &format!("`{}` takes a list", other.name()),
                &format!("write it as `{}(…)`", other.name()),
            )),
        }
    }
}

impl Parse for Setting {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.peek(Token![where]) {
            input.parse::<Token![where]>()?;
            input.parse::<Token![=]>()?;
            return Ok(Self::Where(input.parse()?));
        }
        if input.peek(Token![as]) {
            input.parse::<Token![as]>()?;
            input.parse::<Token![=]>()?;
            return Ok(Self::As(input.parse()?));
        }
        if input.peek(Lit) {
            return Ok(Self::Positional(input.parse()?));
        }
        if !input.peek(syn::Ident) {
            return Ok(Self::Positional(input.parse()?));
        }

        let name: syn::Ident = input.parse()?;
        if input.peek(Token![=]) {
            input.parse::<Token![=]>()?;
            return Ok(Self::Assign(name, input.parse()?));
        }
        if input.peek(syn::token::Paren) {
            let inner;
            syn::parenthesized!(inner in input);
            let items = Punctuated::<Setting, Token![,]>::parse_terminated(&inner)?;
            return Ok(Self::Call(name, items.into_iter().collect()));
        }
        if input.peek(Token![::]) || input.peek(Token![<]) {
            // `has_many = Post` is an assignment; a bare `crate::Post` in a
            // positional list is a type. Re-parse from the identifier.
            let rest: Type = syn::parse2(quote!(#name))?;
            return Ok(Self::Positional(SettingValue::Type(Box::new(rest))));
        }
        Ok(Self::Word(name))
    }
}

/// Every `#[name(..)]` on an item, flattened into one list of settings.
///
/// Repeating the attribute is allowed and means the same as writing one with
/// both settings, because `#[entity(pk)] #[entity(unique)]` is what a
/// three-line diff produces and refusing it would be pedantry.
///
/// # Errors
///
/// [`syn::Error`] from the grammar above, on the user's own span.
pub fn settings_of(attrs: &[Attribute], name: &str) -> syn::Result<Vec<Setting>> {
    let mut all = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident(name) {
            continue;
        }
        let parsed = attr.parse_args_with(Punctuated::<Setting, Token![,]>::parse_terminated)?;
        all.extend(parsed);
    }
    Ok(all)
}

/// Whether a generated token stream is syntactically valid Rust.
///
/// Every expander's tests run their output through this. It does not typecheck
/// anything — that is what `crates/moso-orm`'s own suite and the facade's
/// compile tests are for — but it catches the whole class of "the `quote!`
/// forgot a comma", which otherwise surfaces as a baffling error inside a
/// user's crate.
#[cfg(test)]
pub fn parses_as_rust(tokens: &TokenStream) -> syn::Result<()> {
    syn::parse2::<syn::File>(tokens.clone()).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn parse_settings(tokens: TokenStream) -> syn::Result<Vec<Setting>> {
        let attr: Attribute = syn::parse_quote!(#[entity(#tokens)]);
        settings_of(&[attr], "entity")
    }

    #[test]
    fn table_names_are_snake_case_and_pluralised() {
        assert_eq!(default_table_name("User"), "users");
        assert_eq!(default_table_name("BlogPost"), "blog_posts");
        assert_eq!(default_table_name("Category"), "categories");
        assert_eq!(default_table_name("Box"), "boxes");
        assert_eq!(default_table_name("Dish"), "dishes");
        assert_eq!(default_table_name("Batch"), "batches");
        assert_eq!(
            default_table_name("Day"),
            "days",
            "a vowel before `y` keeps it"
        );
    }

    #[test]
    fn generated_names_follow_the_documented_pattern() {
        assert_eq!(new_struct_name("User"), "NewUser");
        assert_eq!(factory_struct_name("User"), "UserFactory");
        assert_eq!(column_const_name("created_at"), "CREATED_AT");
        assert_eq!(default_index_name("users", &["email"]), "users_email_idx");
        assert_eq!(
            default_foreign_key_name("posts", "author_id"),
            "posts_author_id_fkey"
        );
    }

    #[test]
    fn the_private_path_is_the_only_one_generated_code_names() {
        let path = private_path().to_string().replace(' ', "");
        assert_eq!(path, "::moso::__private");
        assert!(
            !path.contains("moso_orm"),
            "decision D6: never a runtime crate"
        );
        assert!(!path.contains("moso_core"));
        assert!(!path.contains("moso_sql"));
    }

    #[test]
    fn a_misspelling_gets_a_suggestion_and_nonsense_does_not() {
        let known = ["unique", "index", "readonly", "encrypted"];
        assert_eq!(did_you_mean("uniqe", &known), Some("unique"));
        assert_eq!(did_you_mean("indx", &known), Some("index"));
        assert_eq!(did_you_mean("banana", &known), None);
    }

    #[test]
    fn an_error_carries_exactly_one_help_line() {
        let error = err(Span::call_site(), "problem", "the fix");
        let text = error.to_string();
        assert_eq!(text.matches("help:").count(), 1, "{text}");
        assert!(text.contains("the fix"), "{text}");
    }

    #[test]
    fn a_noted_error_reads_statement_then_reason_then_fix() {
        let error = err_with_note(Span::call_site(), "problem", "because", "the fix");
        assert_eq!(
            error.to_string(),
            "problem\n  note: because\n  help: the fix"
        );
    }

    #[test]
    fn the_grammar_reads_flags_values_types_and_lists() {
        let settings = parse_settings(quote!(
            pk,
            table = "users",
            len = 255,
            has_many = Post,
            precision(10, 2)
        ))
        .expect("the four shapes");

        assert_eq!(settings.len(), 5);
        assert!(matches!(settings[0], Setting::Word(_)));
        assert_eq!(settings[1].value().unwrap().string().unwrap(), "users");
        assert_eq!(settings[2].value().unwrap().integer::<u32>().unwrap(), 255);
        assert_eq!(
            type_name_of(&settings[3].value().unwrap().ty().unwrap()).unwrap(),
            "Post"
        );
        assert_eq!(settings[4].items().unwrap().len(), 2);
    }

    #[test]
    fn where_and_as_are_keywords_and_are_still_settings() {
        let settings = parse_settings(quote!(index(where = "deleted_at is null"))).expect("where");
        let inner = settings[0].items().expect("a list");
        assert_eq!(inner[0].name(), "where");
        assert_eq!(
            inner[0].value().unwrap().string().unwrap(),
            "deleted_at is null"
        );

        let settings = parse_settings(quote!(as = "text")).expect("as");
        assert_eq!(settings[0].name(), "as");
        assert_eq!(settings[0].value().unwrap().string().unwrap(), "text");
    }

    #[test]
    fn a_repeated_attribute_is_the_same_as_one_with_both_settings() {
        let a: Attribute = syn::parse_quote!(#[entity(pk)]);
        let b: Attribute = syn::parse_quote!(#[entity(unique)]);
        let settings = settings_of(&[a, b], "entity").expect("two attributes");
        assert_eq!(settings.len(), 2);
        assert_eq!(settings[0].name(), "pk");
        assert_eq!(settings[1].name(), "unique");
    }

    #[test]
    fn the_wrappers_the_derive_reads_are_recognised_syntactically() {
        let option: Type = syn::parse_quote!(Option<String>);
        let related: Type = syn::parse_quote!(Related<Vec<Post>>);
        let plain: Type = syn::parse_quote!(String);

        assert!(option_inner(&option).is_some());
        assert!(option_inner(&plain).is_none());
        let inner = related_inner(&related).expect("Related<..>");
        assert!(vec_inner(inner).is_some());
        assert_eq!(type_name_of(&plain).as_deref(), Some("String"));
    }

    #[test]
    fn an_identifier_a_server_would_truncate_is_refused_here() {
        let span = Span::call_site();
        assert!(validate_sql_ident("email", span, "column").is_ok());
        assert!(validate_sql_ident("", span, "column").is_err());
        assert!(validate_sql_ident(&"x".repeat(64), span, "column").is_err());
        assert!(validate_sql_ident("we\"ird", span, "column").is_err());
    }

    #[test]
    fn a_doc_comment_becomes_one_paragraph() {
        let field: syn::Field = syn::parse_quote! {
            /// Login identity.
            /// Unique across the tenant.
            pub email: String
        };
        assert_eq!(
            doc_comment(&field.attrs).as_deref(),
            Some("Login identity. Unique across the tenant.")
        );
    }

    #[test]
    fn an_unknown_setting_names_the_alternatives() {
        let error = unknown_setting("tabel", &["table", "schema"], Span::call_site(), "entity");
        assert!(error.to_string().contains("did you mean `table`?"));
        let error = unknown_setting("bananas", &["table", "schema"], Span::call_site(), "entity");
        assert!(
            error
                .to_string()
                .contains("the settings are: table, schema")
        );
    }
}
