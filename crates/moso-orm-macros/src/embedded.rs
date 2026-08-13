//! `#[derive(Embedded)]`: a value object whose fields become columns of its
//! owner.
//!
//! `Address { line1, city, postcode }` with `#[embedded(prefix = "address_")]`
//! becomes `address_line1`, `address_city` and `address_postcode` on the owning
//! table — one prefix, no join, and no second row to fetch.
//!
//! # What comes out, and why it is inherent rather than a trait
//!
//! ```text
//! impl Address {
//!     pub const MOSO_COLUMNS: &'static [ColumnDef];
//!     pub const MOSO_COLUMN_NAMES: &'static [&'static str];
//!     pub fn moso_into_values(self) -> Vec<Expr>;
//!     pub fn moso_from_row(row: &Row, offset: usize) -> Result<Self, DecodeError>;
//!     pub fn moso_descriptors() -> Vec<ColumnDescriptor>;
//! }
//! ```
//!
//! The owner splices `Address::MOSO_COLUMNS` into its own `Entity::COLUMNS`,
//! which is a `const`. A trait's associated const would work equally well for
//! the splice, but the *prefix* has to be baked into the names at expansion
//! time — string concatenation is not available in a `const fn` on stable — so
//! the names are literals here and the owner never rewrites them. Inherent
//! items keep that fact visible in one place.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::spanned::Spanned as _;
use syn::{Data, DeriveInput, Fields, Type, Visibility};

use crate::shared::{
    Setting, default_column_name, doc_comment, err, option_inner, private_path, settings_of,
    unknown_setting, validate_sql_ident,
};

/// Every container attribute `#[embedded(..)]` accepts.
pub const CONTAINER_ATTRIBUTES: &[&str] = &["prefix"];

/// Every field attribute `#[embedded(..)]` accepts.
pub const FIELD_ATTRIBUTES: &[&str] = &["column", "len", "precision", "json", "default", "comment"];

/// One embedded struct, as the attributes describe it.
#[derive(Clone, Debug)]
pub struct EmbeddedModel {
    /// The Rust type's name.
    pub type_name: String,
    /// The Rust type's identifier.
    pub ident: syn::Ident,
    /// The type's visibility, which the generated items inherit.
    pub vis: Visibility,
    /// Prepended to every generated column name.
    pub prefix: Option<String>,
    /// The fields, in declaration order.
    pub fields: Vec<EmbeddedField>,
}

impl EmbeddedModel {
    /// Reads the model out of a `#[derive(Embedded)]` input.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] for anything that is not a named-field struct, and for an
    /// unknown attribute.
    pub fn parse(input: &DeriveInput) -> syn::Result<Self> {
        let Data::Struct(data) = &input.data else {
            return Err(syn::Error::new(
                input.span(),
                "`#[derive(Embedded)]` flattens a struct into its owner's columns\n  \
                 help: put it on a `struct` with named fields",
            ));
        };
        let Fields::Named(fields) = &data.fields else {
            return Err(syn::Error::new(
                data.fields.span(),
                "`#[derive(Embedded)]` needs named fields, because each one becomes a column\n  \
                 help: give every field a name",
            ));
        };
        if !input.generics.params.is_empty() {
            return Err(err(
                input.generics.span(),
                "a generic value object is not a fixed set of columns",
                "the owning table's columns are decided at expansion time, so they cannot depend \
                 on a type parameter",
            ));
        }

        let mut model = Self {
            type_name: input.ident.to_string(),
            ident: input.ident.clone(),
            vis: input.vis.clone(),
            prefix: None,
            fields: Vec::new(),
        };

        for setting in settings_of(&input.attrs, "embedded")? {
            match setting.name().as_str() {
                "prefix" => model.prefix = Some(setting.value()?.string()?),
                unknown => {
                    return Err(unknown_setting(
                        unknown,
                        CONTAINER_ATTRIBUTES,
                        setting.span(),
                        "embedded",
                    ));
                }
            }
        }

        for field in &fields.named {
            let Some(name) = field.ident.as_ref() else {
                continue;
            };
            let parsed = EmbeddedField::parse(&name.to_string(), field, model.prefix.as_deref())?;
            model.fields.push(parsed);
        }
        Ok(model)
    }

    /// The whole expansion.
    #[must_use]
    pub fn generate(&self) -> TokenStream {
        let private = private_path();
        let ident = &self.ident;
        let vis = &self.vis;
        let name = &self.type_name;

        let defs = self.fields.iter().map(|field| {
            let column = &field.column;
            let ty = field.sql_type();
            let nullable = field.nullable.then(|| quote!(.nullable()));
            let default = field.default.is_some().then(|| quote!(.with_default()));
            quote! {
                #private::ColumnDef::new(#column, <#ty as #private::SqlType>::KIND)
                    #nullable
                    #default
            }
        });
        let names = self.fields.iter().map(|field| &field.column);

        let values = self.fields.iter().map(|field| {
            let member = format_ident!("{}", field.field);
            let ty = field.sql_type();
            let taken = if field.json {
                if field.nullable {
                    quote!(self.#member.map(#private::SqlJson::new))
                } else {
                    quote!(#private::SqlJson::new(self.#member))
                }
            } else {
                quote!(self.#member)
            };
            quote! {
                __values.push(#private::Expr::bound(
                    <#ty as #private::SqlType>::into_value(#taken),
                ));
            }
        });

        let reads = self.fields.iter().enumerate().map(|(index, field)| {
            let member = format_ident!("{}", field.field);
            let field_name = &field.field;
            let ty = field.sql_type();
            let read = quote! {
                <#ty as #private::SqlType>::decode(__row, __offset + #index)
                    .map_err(|__error| __error.in_entity(#name).in_field(#field_name))?
            };
            let value = if field.json {
                if field.nullable {
                    quote!(#read.map(#private::SqlJson::into_inner))
                } else {
                    quote!(#read.into_inner())
                }
            } else {
                read
            };
            quote!(#member: #value)
        });

        let descriptors = self.fields.iter().map(|field| {
            let column = &field.column;
            let sql_ty = field.sql_type();
            let data_type = if let Some(length) = field.max_length {
                quote!(#private::DataType::VarChar(::core::option::Option::Some(#length)))
            } else if let Some((precision, scale)) = field.precision {
                quote! {
                    #private::DataType::Numeric {
                        precision: ::core::option::Option::Some(#precision),
                        scale: ::core::option::Option::Some(#scale),
                    }
                }
            } else {
                quote!(<#sql_ty as #private::SqlType>::data_type())
            };
            let field_name = &field.field;
            let nullable = field.nullable.then(|| quote!(.nullable()));
            let default = field
                .default
                .as_ref()
                .map(|sql| quote!(.default(#private::ColumnDefault::sql(#sql))));
            let max_length = field.max_length.map(|length| quote!(.max_length(#length)));
            let numeric = field
                .precision
                .map(|(precision, scale)| quote!(.numeric(#precision, #scale)));
            let comment = field.comment.as_ref().map(|text| quote!(.comment(#text)));
            quote! {
                #private::ColumnDescriptor::builder(
                    #private::Ident::from_static(#column),
                    #data_type,
                )
                .field(#field_name)
                #nullable
                #default
                #max_length
                #numeric
                #comment
                .build()
            }
        });

        let columns_doc = format!(
            "Every column `{name}` contributes to its owner, in decode order.\n\n\
             Spliced into the owning entity's `Entity::COLUMNS` at compile time."
        );
        let names_doc = "The same columns' names, for an `INSERT` list.";
        let values_doc = "The values, in `MOSO_COLUMNS` order.";
        let from_row_doc = format!(
            "Decodes the `{name}` block that starts at `offset`.\n\n\
             # Errors\n\n\
             `DecodeError` naming this type, the field and both types."
        );
        let descriptors_doc = "The rich descriptors, for the migration differ and the admin.";

        quote! {
            #[automatically_derived]
            impl #ident {
                #[doc = #columns_doc]
                #[doc(hidden)]
                #vis const MOSO_COLUMNS: &'static [#private::ColumnDef] = &[#(#defs),*];

                #[doc = #names_doc]
                #[doc(hidden)]
                #vis const MOSO_COLUMN_NAMES: &'static [&'static str] = &[#(#names),*];

                #[doc = #values_doc]
                #[doc(hidden)]
                #vis fn moso_into_values(self) -> ::std::vec::Vec<#private::Expr> {
                    let mut __values =
                        ::std::vec::Vec::with_capacity(Self::MOSO_COLUMNS.len());
                    #(#values)*
                    __values
                }

                #[doc = #from_row_doc]
                #[doc(hidden)]
                #vis fn moso_from_row(
                    __row: &#private::Row,
                    __offset: usize,
                ) -> ::core::result::Result<Self, #private::DecodeError> {
                    ::core::result::Result::Ok(Self { #(#reads),* })
                }

                #[doc = #descriptors_doc]
                #[doc(hidden)]
                #vis fn moso_descriptors() -> ::std::vec::Vec<#private::ColumnDescriptor> {
                    ::std::vec![#(#descriptors),*]
                }
            }
        }
    }
}

/// One field of an embedded value object.
#[derive(Clone, Debug)]
pub struct EmbeddedField {
    /// The Rust field name.
    pub field: String,
    /// The column it becomes, prefix included.
    pub column: String,
    /// The field's declared Rust type.
    pub ty: Type,
    /// Whether the declared type is syntactically an `Option<..>`.
    pub nullable: bool,
    /// Whether it is stored as `jsonb`.
    pub json: bool,
    /// The database default, as SQL.
    pub default: Option<String>,
    /// The `varchar` length.
    pub max_length: Option<u32>,
    /// The `numeric(p, s)`.
    pub precision: Option<(u8, u8)>,
    /// The column comment, defaulted from the field's doc comment.
    pub comment: Option<String>,
    /// Where it was declared.
    pub span: Span,
}

impl EmbeddedField {
    /// Reads one field.
    fn parse(field_name: &str, field: &syn::Field, prefix: Option<&str>) -> syn::Result<Self> {
        let mut parsed = Self {
            column: format!(
                "{}{}",
                prefix.unwrap_or_default(),
                default_column_name(field_name)
            ),
            field: field_name.to_owned(),
            nullable: option_inner(&field.ty).is_some(),
            ty: field.ty.clone(),
            json: false,
            default: None,
            max_length: None,
            precision: None,
            comment: doc_comment(&field.attrs),
            span: field.ty.span(),
        };

        for setting in settings_of(&field.attrs, "embedded")? {
            match setting.name().as_str() {
                "column" => {
                    parsed.column = format!(
                        "{}{}",
                        prefix.unwrap_or_default(),
                        setting.value()?.string()?
                    );
                }
                "json" => parsed.json = true,
                "default" => parsed.default = Some(setting.value()?.string()?),
                "comment" => parsed.comment = Some(setting.value()?.string()?),
                "len" => parsed.max_length = Some(setting.value()?.integer()?),
                "precision" => {
                    let items = setting.items()?;
                    if items.len() != 2 {
                        return Err(err(
                            setting.span(),
                            "`precision(..)` takes the total digits and the digits after the point",
                            "write `precision(10, 2)`",
                        ));
                    }
                    let read = |item: &Setting| -> syn::Result<u8> {
                        match item {
                            Setting::Positional(value) => value.integer(),
                            other => Err(err(
                                other.span(),
                                "`precision(..)` takes two whole numbers",
                                "write `precision(10, 2)`",
                            )),
                        }
                    };
                    parsed.precision = Some((read(&items[0])?, read(&items[1])?));
                }
                unknown => {
                    return Err(unknown_setting(
                        unknown,
                        FIELD_ATTRIBUTES,
                        setting.span(),
                        "embedded",
                    ));
                }
            }
        }

        validate_sql_ident(&parsed.column, parsed.span, "column")?;
        Ok(parsed)
    }

    /// The type the field binds and decodes as.
    #[must_use]
    pub fn sql_type(&self) -> Type {
        if !self.json {
            return self.ty.clone();
        }
        let private = private_path();
        match option_inner(&self.ty) {
            Some(inner) => syn::parse_quote!(::core::option::Option<#private::SqlJson<#inner>>),
            None => {
                let ty = &self.ty;
                syn::parse_quote!(#private::SqlJson<#ty>)
            }
        }
    }
}

/// Expands `#[derive(Embedded)]`.
pub fn expand(input: TokenStream) -> TokenStream {
    let input: DeriveInput = match syn::parse2(input) {
        Ok(parsed) => parsed,
        Err(error) => return error.to_compile_error(),
    };
    match EmbeddedModel::parse(&input) {
        Ok(model) => model.generate(),
        Err(error) => error.to_compile_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> syn::Result<EmbeddedModel> {
        let input: DeriveInput = syn::parse_str(source).expect("the test source parses");
        EmbeddedModel::parse(&input)
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
    fn the_prefix_is_applied_to_every_field() {
        let model = parse(
            "#[embedded(prefix = \"address_\")] struct Address { line1: String, city: String }",
        )
        .expect("an embedded struct");
        assert_eq!(model.fields[0].column, "address_line1");
        assert_eq!(model.fields[1].column, "address_city");
        assert_eq!(model.fields.len(), 2);
    }

    #[test]
    fn without_a_prefix_the_column_is_the_field() {
        let model = parse("struct Address { line1: String }").expect("no prefix");
        assert_eq!(model.fields[0].column, "line1");
        assert_eq!(model.prefix, None);
    }

    #[test]
    fn a_tuple_struct_is_refused() {
        let error = parse("struct Address(String);").expect_err("no column names");
        assert!(error.to_string().contains("named fields"));
    }

    #[test]
    fn the_expansion_carries_the_four_pieces_the_owner_splices() {
        let out = expand_str(
            "#[embedded(prefix = \"address_\")] struct Address { line1: String, city: String }",
        );
        assert!(out.contains("const MOSO_COLUMNS"), "{out}");
        assert!(out.contains("const MOSO_COLUMN_NAMES"), "{out}");
        assert!(out.contains("fn moso_into_values"), "{out}");
        assert!(out.contains("fn moso_from_row"), "{out}");
        assert!(out.contains("fn moso_descriptors"), "{out}");
        assert!(out.contains("\"address_line1\""), "{out}");
        assert!(out.contains("\"address_city\""), "{out}");
    }

    #[test]
    fn decoding_is_positional_from_the_owners_offset() {
        let out = expand_str("struct Address { line1: String, city: String }");
        assert!(out.contains("decode (__row , __offset + 0usize)"), "{out}");
        assert!(out.contains("decode (__row , __offset + 1usize)"), "{out}");
    }

    #[test]
    fn a_nullable_field_becomes_a_nullable_column() {
        let out = expand_str("struct Address { line2: Option<String> }");
        assert!(out.contains(". nullable ()"), "{out}");
    }

    #[test]
    fn a_length_becomes_a_varchar_in_the_descriptor() {
        let out = expand_str("struct Address { #[embedded(len = 64)] city: String }");
        assert!(out.contains("VarChar"), "{out}");
        assert!(out.contains("max_length (64u32)"), "{out}");
    }

    #[test]
    fn an_unknown_setting_gets_one_error_with_a_suggestion() {
        let error = parse("#[embedded(prefx = \"a_\")] struct Address { city: String }")
            .expect_err("`prefx` is not a setting");
        assert!(error.to_string().contains("did you mean `prefix`?"));
    }

    #[test]
    fn the_expansion_names_only_the_private_path() {
        let out = expand_str("struct Address { city: String }");
        assert!(!out.contains("moso_orm"), "{out}");
        assert!(!out.contains("moso_sql"), "{out}");
        assert!(!out.contains("compile_error"), "{out}");
    }
}
