//! `#[derive(DbEnum)]`: an enum that is one column.
//!
//! Implements **both** `DbEnum` and `SqlType`, because one without the other is
//! never useful: `DbEnum` says how a variant is spelled in the database and
//! `SqlType` is what makes the enum usable as a column type at all.
//!
//! # The three storage strategies
//!
//! | `as = …` | Column type | Adding a variant |
//! | --- | --- | --- |
//! | `"text"` (default) | `text` | no migration |
//! | `"int"` | `integer` | no migration, and unreadable in `psql` |
//! | `"pg_enum"` | `CREATE TYPE … AS ENUM` | `ALTER TYPE … ADD VALUE` |
//!
//! Reading a value the enum does not know is a **decode error naming the value
//! and listing the variants**, never a silent fallback — a row whose status is
//! `"refunded"` and whose code has three variants is a bug, and a default would
//! hide it.

use heck::{
    ToKebabCase as _, ToLowerCamelCase as _, ToShoutySnakeCase as _, ToSnakeCase as _,
    ToUpperCamelCase as _,
};
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::spanned::Spanned as _;
use syn::{Data, DeriveInput, Expr, Fields};

use crate::shared::{err, private_path, settings_of, unknown_setting, validate_sql_ident};

/// Every container attribute `#[db_enum(..)]` accepts.
pub const CONTAINER_ATTRIBUTES: &[&str] = &["as", "type_name", "rename_all"];

/// Every variant attribute `#[db_enum(..)]` accepts.
pub const VARIANT_ATTRIBUTES: &[&str] = &["rename"];

/// Every spelling `rename_all = "…"` accepts.
const RENAME_RULES: &[&str] = &[
    "snake_case",
    "camelCase",
    "PascalCase",
    "SCREAMING_SNAKE_CASE",
    "kebab-case",
    "lowercase",
    "UPPERCASE",
    "verbatim",
];

/// How the column stores a variant, mirroring `moso_orm::EnumStorage`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StorageModel {
    /// One `text` column holding the variant's name. The default.
    #[default]
    Text,
    /// One `integer` column holding the discriminant.
    Int,
    /// A PostgreSQL `CREATE TYPE … AS ENUM`.
    PgEnum,
}

impl StorageModel {
    /// Reads `as = "…"`.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] naming the three accepted spellings.
    pub fn parse(value: &str, span: Span) -> syn::Result<Self> {
        match value {
            "text" => Ok(Self::Text),
            "int" => Ok(Self::Int),
            "pg_enum" => Ok(Self::PgEnum),
            other => Err(err(
                span,
                &format!("`{other}` is not an enum storage strategy"),
                "one of `\"text\"` (the default), `\"int\"` or `\"pg_enum\"`",
            )),
        }
    }

    /// The `EnumStorage` variant the generated code names.
    fn tokens(self) -> TokenStream {
        let private = private_path();
        match self {
            Self::Text => quote!(#private::EnumStorage::Text),
            Self::Int => quote!(#private::EnumStorage::Int),
            Self::PgEnum => quote!(#private::EnumStorage::PgEnum),
        }
    }
}

/// One enum column, as the attributes describe it.
#[derive(Clone, Debug)]
pub struct DbEnumModel {
    /// The Rust type's name.
    pub type_name: String,
    /// The Rust type's identifier.
    pub ident: syn::Ident,
    /// How the values are stored.
    pub storage: StorageModel,
    /// The PostgreSQL type name, for `pg_enum`.
    pub sql_type_name: String,
    /// The variants, with their stored spellings and discriminants.
    pub variants: Vec<VariantModel>,
}

/// One variant of a database enum.
#[derive(Clone, Debug)]
pub struct VariantModel {
    /// The Rust variant's identifier.
    pub ident: syn::Ident,
    /// The Rust variant's name.
    pub name: String,
    /// How it is spelled in a `text` or `pg_enum` column.
    pub stored: String,
    /// What it is stored as in an `integer` column.
    pub discriminant: Expr,
}

impl DbEnumModel {
    /// Reads the model out of a `#[derive(DbEnum)]` input.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] for anything that is not a fieldless enum, for an unknown
    /// attribute, and for two variants that would be stored identically.
    pub fn parse(input: &DeriveInput) -> syn::Result<Self> {
        let Data::Enum(data) = &input.data else {
            return Err(syn::Error::new(
                input.span(),
                "`#[derive(DbEnum)]` describes a column with a fixed set of values\n  \
                 help: put it on an `enum`; for a struct, store it as JSON with `Json<T>`",
            ));
        };
        if !input.generics.params.is_empty() {
            return Err(err(
                input.generics.span(),
                "a generic enum is not one column",
                "a column's set of values is fixed, so it cannot depend on a type parameter",
            ));
        }

        let type_name = input.ident.to_string();
        let mut model = Self {
            sql_type_name: type_name.to_snake_case(),
            ident: input.ident.clone(),
            type_name,
            storage: StorageModel::default(),
            variants: Vec::new(),
        };

        let mut rename_all = String::from("snake_case");
        for setting in settings_of(&input.attrs, "db_enum")? {
            match setting.name().as_str() {
                "as" => {
                    model.storage =
                        StorageModel::parse(&setting.value()?.string()?, setting.span())?;
                }
                "type_name" => model.sql_type_name = setting.value()?.string()?,
                "rename_all" => {
                    rename_all = setting.value()?.string()?;
                    if !RENAME_RULES.contains(&rename_all.as_str()) {
                        return Err(unknown_setting(
                            &rename_all,
                            RENAME_RULES,
                            setting.span(),
                            "db_enum",
                        ));
                    }
                }
                unknown => {
                    return Err(unknown_setting(
                        unknown,
                        CONTAINER_ATTRIBUTES,
                        setting.span(),
                        "db_enum",
                    ));
                }
            }
        }
        validate_sql_ident(&model.sql_type_name, input.ident.span(), "type")?;

        if data.variants.is_empty() {
            return Err(err(
                input.ident.span(),
                &format!(
                    "`{}` has no variants, so no value can be stored",
                    model.type_name
                ),
                "give the enum at least one variant, or drop the column",
            ));
        }

        for (ordinal, variant) in data.variants.iter().enumerate() {
            if !matches!(variant.fields, Fields::Unit) {
                return Err(err(
                    variant.span(),
                    &format!(
                        "`{}` carries data, and a column holds one value",
                        variant.ident
                    ),
                    "remove the payload, or store the enum as JSON with `Json<T>`",
                ));
            }
            let name = variant.ident.to_string();
            let mut stored = rename(&name, &rename_all);
            for setting in settings_of(&variant.attrs, "db_enum")? {
                match setting.name().as_str() {
                    "rename" => stored = setting.value()?.string()?,
                    unknown => {
                        return Err(unknown_setting(
                            unknown,
                            VARIANT_ATTRIBUTES,
                            setting.span(),
                            "db_enum",
                        ));
                    }
                }
            }
            let discriminant = variant.discriminant.as_ref().map_or_else(
                || {
                    let ordinal = i32::try_from(ordinal).unwrap_or(i32::MAX);
                    syn::parse_quote!(#ordinal)
                },
                |(_, expr)| expr.clone(),
            );
            model.variants.push(VariantModel {
                ident: variant.ident.clone(),
                name,
                stored,
                discriminant,
            });
        }

        let mut seen = std::collections::HashSet::new();
        for variant in &model.variants {
            if !seen.insert(variant.stored.clone()) {
                return Err(err(
                    variant.ident.span(),
                    &format!(
                        "`{}` and an earlier variant are both stored as `{}`",
                        variant.name, variant.stored
                    ),
                    "two variants that read back as one make the column ambiguous — rename one \
                     with `#[db_enum(rename = \"…\")]`",
                ));
            }
        }
        Ok(model)
    }

    /// The whole expansion: `DbEnum` and `SqlType`.
    #[must_use]
    pub fn generate(&self) -> TokenStream {
        let private = private_path();
        let ident = &self.ident;
        let name = &self.type_name;
        let sql_type_name = &self.sql_type_name;
        let storage = self.storage.tokens();

        let stored: Vec<&String> = self
            .variants
            .iter()
            .map(|variant| &variant.stored)
            .collect();
        let as_str = self.variants.iter().map(|variant| {
            let variant_ident = &variant.ident;
            let stored = &variant.stored;
            quote!(Self::#variant_ident => #stored,)
        });
        let from_str = self.variants.iter().map(|variant| {
            let variant_ident = &variant.ident;
            let stored = &variant.stored;
            quote!(#stored => ::core::option::Option::Some(Self::#variant_ident),)
        });
        let as_int = self.variants.iter().map(|variant| {
            let variant_ident = &variant.ident;
            let discriminant = &variant.discriminant;
            quote!(Self::#variant_ident => #discriminant,)
        });
        let from_int = self.variants.iter().map(|variant| {
            let variant_ident = &variant.ident;
            let discriminant = &variant.discriminant;
            quote!(#discriminant => ::core::option::Option::Some(Self::#variant_ident),)
        });

        let known = stored
            .iter()
            .map(|value| format!("`{value}`"))
            .collect::<Vec<_>>()
            .join(", ");

        let (kind, data_type, to_value, decode) = match self.storage {
            StorageModel::Int => (
                quote!(#private::ValueKind::I32),
                quote!(#private::DataType::Integer),
                quote!(#private::Value::I32(<Self as #private::DbEnum>::as_db_int(self))),
                quote! {
                    let __stored = __row.get_i32(__index)?;
                    <Self as #private::DbEnum>::from_db_int(__stored).ok_or_else(|| {
                        #private::DecodeError::malformed(
                            __index,
                            #name,
                            ::std::format!(
                                "`{__stored}` is not a variant of `{}`; the variants are {}",
                                #name,
                                #known,
                            ),
                        )
                    })
                },
            ),
            StorageModel::Text => (
                quote!(#private::ValueKind::Text),
                quote!(#private::DataType::Text),
                quote!(#private::Value::text(<Self as #private::DbEnum>::as_db_str(self))),
                text_decode(name, &known),
            ),
            StorageModel::PgEnum => (
                quote!(#private::ValueKind::Text),
                quote! {
                    #private::DataType::Enum(#private::TypeRef::from_static(#sql_type_name))
                },
                quote!(#private::Value::text(<Self as #private::DbEnum>::as_db_str(self))),
                text_decode(name, &known),
            ),
        };

        quote! {
            #[automatically_derived]
            impl #private::DbEnum for #ident {
                const VARIANTS: &'static [&'static str] = &[#(#stored),*];
                const STORAGE: #private::EnumStorage = #storage;
                const TYPE_NAME: &'static str = #sql_type_name;

                fn as_db_str(&self) -> &'static str {
                    match self { #(#as_str)* }
                }

                fn from_db_str(__value: &str) -> ::core::option::Option<Self> {
                    match __value {
                        #(#from_str)*
                        _ => ::core::option::Option::None,
                    }
                }

                fn as_db_int(&self) -> i32 {
                    match self { #(#as_int)* }
                }

                fn from_db_int(__value: i32) -> ::core::option::Option<Self> {
                    match __value {
                        #(#from_int)*
                        _ => ::core::option::Option::None,
                    }
                }
            }

            #[automatically_derived]
            impl #private::SqlType for #ident {
                const KIND: #private::ValueKind = #kind;
                const TYPE_NAME: &'static str = #name;

                fn data_type() -> #private::DataType {
                    #data_type
                }

                fn to_value(&self) -> #private::Value {
                    #to_value
                }

                fn decode(
                    __row: &#private::Row,
                    __index: usize,
                ) -> ::core::result::Result<Self, #private::DecodeError> {
                    #decode
                }
            }
        }
    }
}

/// The `decode` body shared by the two textual strategies.
fn text_decode(name: &str, known: &str) -> TokenStream {
    let private = private_path();
    quote! {
        let __stored = __row.get_str(__index)?;
        <Self as #private::DbEnum>::from_db_str(__stored).ok_or_else(|| {
            #private::DecodeError::malformed(
                __index,
                #name,
                ::std::format!(
                    "`{__stored}` is not a variant of `{}`; the variants are {}",
                    #name,
                    #known,
                ),
            )
        })
    }
}

/// Applies a `rename_all` rule to one variant's name.
fn rename(name: &str, rule: &str) -> String {
    match rule {
        "camelCase" => name.to_lower_camel_case(),
        "PascalCase" => name.to_upper_camel_case(),
        "SCREAMING_SNAKE_CASE" => name.to_shouty_snake_case(),
        "kebab-case" => name.to_kebab_case(),
        "lowercase" => name.to_lowercase(),
        "UPPERCASE" => name.to_uppercase(),
        "verbatim" => name.to_owned(),
        _ => name.to_snake_case(),
    }
}

/// Expands `#[derive(DbEnum)]`.
pub fn expand(input: TokenStream) -> TokenStream {
    let input: DeriveInput = match syn::parse2(input) {
        Ok(parsed) => parsed,
        Err(error) => return error.to_compile_error(),
    };
    match DbEnumModel::parse(&input) {
        Ok(model) => model.generate(),
        Err(error) => error.to_compile_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> syn::Result<DbEnumModel> {
        let input: DeriveInput = syn::parse_str(source).expect("the test source parses");
        DbEnumModel::parse(&input)
    }

    fn expand_str(source: &str) -> String {
        let input: TokenStream = source.parse().expect("the test source lexes");
        let tokens = expand(input);
        if !tokens.to_string().contains("compile_error") {
            crate::shared::parses_as_rust(&tokens)
                .unwrap_or_else(|error| panic!("the expansion is not valid Rust: {error}"));
        }
        tokens.to_string()
    }

    #[test]
    fn variants_are_stored_in_snake_case_by_default() {
        let model = parse("enum Status { Pending, PaidInFull }").expect("an enum");
        assert_eq!(model.variants[0].stored, "pending");
        assert_eq!(model.variants[1].stored, "paid_in_full");
        assert_eq!(model.sql_type_name, "status");
        assert_eq!(model.storage, StorageModel::Text);
        assert_ne!(model.storage, StorageModel::PgEnum);
    }

    #[test]
    fn a_rename_rule_and_a_per_variant_rename_both_apply() {
        let model = parse(
            "#[db_enum(rename_all = \"SCREAMING_SNAKE_CASE\")]
             enum Status { PaidInFull, #[db_enum(rename = \"n/a\")] Unknown }",
        )
        .expect("renamed");
        assert_eq!(model.variants[0].stored, "PAID_IN_FULL");
        assert_eq!(model.variants[1].stored, "n/a");
    }

    #[test]
    fn a_variant_with_a_payload_is_refused_with_the_json_alternative() {
        let error = parse("enum Status { Paid(i64) }").expect_err("a payload is not a column");
        let text = error.to_string();
        assert!(text.contains("carries data"), "{text}");
        assert!(text.contains("Json<T>"), "{text}");
    }

    #[test]
    fn a_struct_is_refused() {
        let error = parse("struct Status { paid: bool }").expect_err("not an enum");
        assert!(error.to_string().contains("help:"));
    }

    #[test]
    fn two_variants_that_would_collide_are_refused() {
        let error = parse("enum Status { Paid, #[db_enum(rename = \"paid\")] Settled }")
            .expect_err("both store `paid`");
        assert!(error.to_string().contains("both stored as"));
    }

    #[test]
    fn an_empty_enum_is_refused() {
        let error = parse("enum Status {}").expect_err("nothing to store");
        assert!(error.to_string().contains("no variants"));
    }

    #[test]
    fn the_storage_strategies_are_the_three_documented_ones() {
        let span = Span::call_site();
        assert_eq!(
            StorageModel::parse("text", span).expect("text"),
            StorageModel::Text
        );
        assert_eq!(
            StorageModel::parse("int", span).expect("int"),
            StorageModel::Int
        );
        assert_eq!(
            StorageModel::parse("pg_enum", span).expect("pg_enum"),
            StorageModel::PgEnum
        );
        let error = StorageModel::parse("blob", span).expect_err("not a strategy");
        assert!(error.to_string().contains("help:"));
    }

    #[test]
    fn the_expansion_implements_both_traits() {
        let out = expand_str("enum Status { Pending, Paid }");
        assert!(
            out.contains("impl :: moso :: __private :: DbEnum for Status"),
            "{out}"
        );
        assert!(
            out.contains("impl :: moso :: __private :: SqlType for Status"),
            "{out}"
        );
        assert!(out.contains("const VARIANTS"), "{out}");
        assert!(out.contains("\"pending\""), "{out}");
        assert!(!out.contains("compile_error"), "{out}");
        assert!(!out.contains("moso_orm"), "{out}");
    }

    #[test]
    fn a_text_enum_binds_and_reads_text() {
        let out = expand_str("enum Status { Pending }");
        assert!(out.contains("ValueKind :: Text"), "{out}");
        assert!(out.contains("DataType :: Text"), "{out}");
        assert!(out.contains("get_str (__index)"), "{out}");
    }

    #[test]
    fn an_int_enum_binds_and_reads_an_integer() {
        let out = expand_str("#[db_enum(as = \"int\")] enum Status { Pending, Paid }");
        assert!(out.contains("ValueKind :: I32"), "{out}");
        assert!(out.contains("DataType :: Integer"), "{out}");
        assert!(out.contains("get_i32 (__index)"), "{out}");
    }

    #[test]
    fn a_pg_enum_names_the_created_type() {
        let out = expand_str(
            "#[db_enum(as = \"pg_enum\", type_name = \"order_status\")] enum Status { Paid }",
        );
        assert!(out.contains("DataType :: Enum"), "{out}");
        assert!(
            out.contains("TypeRef :: from_static (\"order_status\")"),
            "{out}"
        );
        assert!(out.contains("EnumStorage :: PgEnum"), "{out}");
    }

    #[test]
    fn explicit_discriminants_are_what_the_int_column_stores() {
        let out = expand_str("#[db_enum(as = \"int\")] enum Status { Pending = 10, Paid = 20 }");
        assert!(out.contains("Self :: Pending => 10 ,"), "{out}");
        assert!(
            out.contains("10 => :: core :: option :: Option :: Some (Self :: Pending)"),
            "{out}"
        );
    }

    #[test]
    fn an_unknown_value_is_a_decode_error_that_lists_the_variants() {
        let out = expand_str("enum Status { Pending, Paid }");
        assert!(out.contains("DecodeError :: malformed"), "{out}");
        assert!(out.contains("the variants are"), "{out}");
        assert!(
            !out.contains("unwrap_or_default"),
            "no silent fallback: {out}"
        );
    }
}
