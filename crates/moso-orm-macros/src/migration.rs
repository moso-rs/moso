//! `#[migration]`: the metadata a hand-written Rust migration carries.
//!
//! ```text
//! // migrations/20260730T090000_backfill_slugs.rs
//! #[migration]
//! pub struct BackfillSlugs;
//!
//! impl Migration for BackfillSlugs { /* up, down, REVERSIBLE, TRANSACTIONAL */ }
//! ```
//!
//! # What it adds, and what it deliberately does not
//!
//! It adds the four facts the runner needs and the `impl Migration` block does
//! not carry: the **version** that orders the migration, the **name** that
//! `moso db status` prints, the **description** taken from the doc comment,
//! and the **source location**. All four are inherent `const`s, so they compose
//! with whatever `impl Migration` the author writes and cannot collide with it.
//!
//! It does **not** register the migration anywhere. ADR-0004 rules out
//! link-time registries, so the list of migrations is written down — by the
//! generator, in `migrations/mod.rs` — and this attribute's job is to make each
//! entry describe itself.
//!
//! # Where the version comes from
//!
//! `#[migration(version = "20260730T090000")]` if it is written, otherwise the
//! leading timestamp of the file's own name, which is where the convention puts
//! it. A file that has neither is an error naming both fixes, because a
//! migration whose order is a guess is worse than one that does not compile.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned as _;
use syn::{Item, parse2};

use crate::shared::{doc_comment, err, settings_of, unknown_setting};

/// Every attribute `#[migration(..)]` accepts.
pub const ATTRIBUTES: &[&str] = &["version", "name", "description"];

/// One migration, as the attribute describes it.
#[derive(Clone, Debug)]
pub struct MigrationModel {
    /// The Rust type's identifier.
    pub ident: syn::Ident,
    /// The version the runner orders migrations by.
    pub version: String,
    /// The migration's name, as `moso db status` prints it.
    pub name: String,
    /// The description, defaulted from the doc comment.
    pub description: String,
}

impl MigrationModel {
    /// Reads the model out of an annotated item.
    ///
    /// `file` is the source file's name, when the caller could learn it; the
    /// version falls back to its leading timestamp.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] for anything that is not a struct, for an unknown
    /// setting, and for a migration with no version and no timestamped file
    /// name.
    pub fn parse(attributes: TokenStream, item: &Item, file: Option<&str>) -> syn::Result<Self> {
        let (ident, attrs) = match item {
            Item::Struct(item) => (item.ident.clone(), item.attrs.clone()),
            Item::Enum(item) => (item.ident.clone(), item.attrs.clone()),
            other => {
                return Err(err(
                    other.span(),
                    "`#[migration]` describes one migration, which is one type",
                    "put it on the `struct` that implements `Migration`",
                ));
            }
        };

        use heck::ToSnakeCase as _;
        let mut model = Self {
            name: ident.to_string().to_snake_case(),
            description: doc_comment(&attrs).unwrap_or_default(),
            ident,
            version: String::new(),
        };

        let holder: syn::Attribute = syn::parse_quote!(#[migration(#attributes)]);
        for setting in settings_of(&[holder], "migration")? {
            match setting.name().as_str() {
                "version" => model.version = setting.value()?.string()?,
                "name" => model.name = setting.value()?.string()?,
                "description" => model.description = setting.value()?.string()?,
                unknown => {
                    return Err(unknown_setting(
                        unknown,
                        ATTRIBUTES,
                        setting.span(),
                        "migration",
                    ));
                }
            }
        }

        if model.version.is_empty() {
            model.version = file.and_then(version_from_file_name).ok_or_else(|| {
                err(
                    model.ident.span(),
                    &format!("`{}` does not say when it runs", model.ident),
                    "name the file `migrations/20260730T090000_backfill_slugs.rs`, or write \
                         `#[migration(version = \"20260730T090000\")]` — the runner applies \
                         migrations in version order, so a missing one is an ordering bug waiting \
                         to happen",
                )
            })?;
        }
        Ok(model)
    }

    /// The generated `const` block, alongside the item itself.
    #[must_use]
    pub fn generate(&self, item: &Item) -> TokenStream {
        let ident = &self.ident;
        let version = &self.version;
        let name = &self.name;
        let description = &self.description;
        let marker = format_ident!("__MOSO_MIGRATION_{}", name.to_uppercase());

        let version_doc = format!(
            "When `{ident}` runs, relative to its siblings.\n\n\
             The runner applies migrations in ascending version order and records each one it \
             applied under this string."
        );
        let name_doc = format!("`{ident}`'s name, as `moso db status` prints it.");
        let description_doc = format!("What `{ident}` does, taken from its documentation comment.");
        let source_doc = format!("Where `{ident}` is declared, for a message that has to say so.");
        let marker_doc = format!(
            "The three facts about `{ident}` a generated `migrations/mod.rs` collects.\n\n\
             Not public API: its shape follows the runner's needs."
        );

        quote! {
            #item

            #[automatically_derived]
            impl #ident {
                #[doc = #version_doc]
                pub const VERSION: &'static str = #version;

                #[doc = #name_doc]
                pub const NAME: &'static str = #name;

                #[doc = #description_doc]
                pub const DESCRIPTION: &'static str = #description;

                #[doc = #source_doc]
                pub const SOURCE: (&'static str, u32) = (::core::file!(), ::core::line!());
            }

            #[doc = #marker_doc]
            #[doc(hidden)]
            #[allow(non_upper_case_globals)]
            pub const #marker: (&str, &str, &str) = (#version, #name, #description);
        }
    }
}

/// The leading timestamp of `20260730T090000_backfill_slugs.rs`.
///
/// Deliberately strict: everything before the first underscore, and only when
/// it is at least eight characters of digits and letters. A loose rule would
/// happily read `backfill` as a version and order the migration by it.
fn version_from_file_name(file: &str) -> Option<String> {
    let stem = std::path::Path::new(file).file_stem()?.to_str()?;
    let head = stem.split('_').next()?;
    let long_enough = head.len() >= 8;
    let starts_with_digits = head
        .chars()
        .take(8)
        .all(|character| character.is_ascii_digit());
    let plausible = head
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-');
    (long_enough && starts_with_digits && plausible).then(|| head.to_owned())
}

/// Expands `#[migration]`.
pub fn expand(attributes: TokenStream, item: TokenStream, file: Option<&str>) -> TokenStream {
    let parsed: Item = match parse2(item.clone()) {
        Ok(parsed) => parsed,
        Err(error) => return error.to_compile_error(),
    };
    match MigrationModel::parse(attributes, &parsed, file) {
        Ok(model) => model.generate(&parsed),
        Err(error) => {
            // The item is emitted alongside the error so that the rest of the
            // file still resolves and the user reads one message, not twenty.
            let error = error.to_compile_error();
            quote!(#item #error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(attributes: &str, item: &str, file: Option<&str>) -> syn::Result<MigrationModel> {
        let attributes: TokenStream = attributes.parse().expect("the attributes lex");
        let parsed: Item = syn::parse_str(item).expect("the item parses");
        MigrationModel::parse(attributes, &parsed, file)
    }

    fn expand_str(attributes: &str, item: &str, file: Option<&str>) -> String {
        let attributes: TokenStream = attributes.parse().expect("the attributes lex");
        let item: TokenStream = item.parse().expect("the item lexes");
        let tokens = expand(attributes, item, file);
        crate::shared::parses_as_rust(&tokens)
            .unwrap_or_else(|error| panic!("the expansion is not valid Rust: {error}"));
        tokens.to_string()
    }

    #[test]
    fn the_name_defaults_to_the_type_in_snake_case() {
        let model = parse(
            "version = \"20260730T090000\"",
            "pub struct BackfillSlugs;",
            None,
        )
        .expect("a migration");
        assert_eq!(model.name, "backfill_slugs");
        assert_eq!(model.version, "20260730T090000");
    }

    #[test]
    fn the_version_falls_back_to_the_file_name() {
        let model = parse(
            "",
            "pub struct BackfillSlugs;",
            Some("migrations/20260730T090000_backfill_slugs.rs"),
        )
        .expect("a timestamped file");
        assert_eq!(model.version, "20260730T090000");
    }

    #[test]
    fn a_file_name_that_is_not_a_timestamp_is_not_guessed_at() {
        let error = parse("", "pub struct BackfillSlugs;", Some("src/backfill.rs"))
            .expect_err("no version anywhere");
        let text = error.to_string();
        assert!(text.contains("does not say when it runs"), "{text}");
        assert!(text.contains("version = "), "{text}");
    }

    #[test]
    fn the_doc_comment_becomes_the_description() {
        let model = parse(
            "version = \"1\"",
            "/// Fills in the slugs the old code left null.\npub struct BackfillSlugs;",
            None,
        )
        .expect("a documented migration");
        assert_eq!(
            model.description,
            "Fills in the slugs the old code left null."
        );
    }

    #[test]
    fn the_expansion_keeps_the_item_and_adds_the_four_constants() {
        let out = expand_str(
            "version = \"20260730T090000\"",
            "pub struct BackfillSlugs;",
            None,
        );
        assert!(out.contains("pub struct BackfillSlugs ;"), "{out}");
        assert!(
            out.contains("const VERSION : & 'static str = \"20260730T090000\""),
            "{out}"
        );
        assert!(
            out.contains("const NAME : & 'static str = \"backfill_slugs\""),
            "{out}"
        );
        assert!(out.contains("const DESCRIPTION"), "{out}");
        assert!(out.contains("const SOURCE"), "{out}");
        assert!(out.contains("file ! ()"), "{out}");
        assert!(!out.contains("compile_error"), "{out}");
    }

    #[test]
    fn an_unknown_setting_names_the_real_ones() {
        let error = parse("verison = \"1\"", "struct M;", None).expect_err("a typo");
        assert!(error.to_string().contains("did you mean `version`?"));
    }

    #[test]
    fn a_function_is_refused_because_a_migration_is_a_type() {
        let error = parse("version = \"1\"", "fn up() {}", None).expect_err("not a type");
        assert!(error.to_string().contains("one type"));
    }

    #[test]
    fn an_error_still_emits_the_item_so_the_file_resolves() {
        let out = expand_str("", "pub struct BackfillSlugs;", None);
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("pub struct BackfillSlugs ;"), "{out}");
    }

    #[test]
    fn a_file_stem_is_read_from_a_path_of_any_shape() {
        assert_eq!(
            version_from_file_name("/a/b/20260730T090000_x.rs").as_deref(),
            Some("20260730T090000")
        );
        assert_eq!(version_from_file_name("x.rs"), None);
        assert_eq!(
            version_from_file_name("0001_x.rs"),
            None,
            "too short to order by"
        );
    }
}
