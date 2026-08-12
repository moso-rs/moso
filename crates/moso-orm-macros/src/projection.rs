//! `#[derive(Projection)]`: a struct a partial select decodes into.
//!
//! The expansion is the one `moso_orm::projection`'s module documentation
//! specifies. Two halves matter:
//!
//! * **`impl ProjectionScope<E>` once per named entity.** Every column the
//!   derive emits goes through `checked_column…`, whose `P: ProjectionScope<E>`
//!   bound is satisfied only by those entities. Referencing a column of an
//!   entity the projection did not join is therefore a compile error *at the
//!   field*, with no runtime lookup and nothing to remember to call.
//! * **A positional `from_row`.** Field *i* reads column *i*, because
//!   `select_items` built the list in the same order.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned as _;
use syn::{Data, DeriveInput, Fields, Type};

use crate::shared::{
    Setting, SettingValue, column_const_name, err, private_path, settings_of, unknown_setting,
};

/// Every container attribute `#[projection(..)]` accepts.
pub const CONTAINER_ATTRIBUTES: &[&str] = &["entity", "join"];

/// Every field attribute `#[projection(..)]` accepts.
pub const FIELD_ATTRIBUTES: &[&str] = &["column", "expr", "agg", "alias", "skip"];

/// Every aggregate `agg = "…"` accepts, for the "did you mean" path.
const AGGREGATES: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "array_agg",
    "string_agg",
    "json_agg",
    "jsonb_agg",
    "json_object_agg",
    "jsonb_object_agg",
    "bool_and",
    "bool_or",
    "stddev",
    "variance",
];

/// One projection, as the attributes describe it.
#[derive(Clone, Debug)]
pub struct ProjectionModel {
    /// The Rust type's name.
    pub type_name: String,
    /// The Rust type's identifier.
    pub ident: syn::Ident,
    /// The entity the columns resolve against, when one was named.
    pub entity: Option<Type>,
    /// Other entities whose columns may be referenced.
    pub joined: Vec<Type>,
    /// The fields, in decode order.
    pub fields: Vec<ProjectionField>,
}

impl ProjectionModel {
    /// Reads the model out of a `#[derive(Projection)]` input.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] for anything that is not a named-field struct, for an
    /// unknown attribute, and for a field that names neither a column nor an
    /// expression when the container named no entity to resolve against.
    pub fn parse(input: &DeriveInput) -> syn::Result<Self> {
        let Data::Struct(data) = &input.data else {
            return Err(syn::Error::new(
                input.span(),
                "`#[derive(Projection)]` describes the shape of a row\n  \
                 help: put it on a `struct` with named fields",
            ));
        };
        let Fields::Named(fields) = &data.fields else {
            return Err(syn::Error::new(
                data.fields.span(),
                "`#[derive(Projection)]` needs named fields, one per selected column\n  \
                 help: give every field a name",
            ));
        };
        if !input.generics.params.is_empty() {
            return Err(err(
                input.generics.span(),
                "a generic struct is not one `SELECT` list",
                "a projection's columns are fixed, so they cannot depend on a type parameter — \
                 write one projection per query shape",
            ));
        }

        let mut model = Self {
            type_name: input.ident.to_string(),
            ident: input.ident.clone(),
            entity: None,
            joined: Vec::new(),
            fields: Vec::new(),
        };

        for setting in settings_of(&input.attrs, "projection")? {
            match setting.name().as_str() {
                "entity" => model.entity = Some(setting.value()?.ty()?),
                "join" => match &setting {
                    Setting::Call(_, items) => {
                        for item in items {
                            model.joined.push(item.as_type()?);
                        }
                    }
                    other => model.joined.push(other.value()?.ty()?),
                },
                unknown => {
                    return Err(unknown_setting(
                        unknown,
                        CONTAINER_ATTRIBUTES,
                        setting.span(),
                        "projection",
                    ));
                }
            }
        }

        for field in &fields.named {
            let Some(name) = field.ident.as_ref() else {
                continue;
            };
            model
                .fields
                .push(ProjectionField::parse(&name.to_string(), field, &model)?);
        }
        Ok(model)
    }

    /// How many columns the projection reads.
    #[must_use]
    pub fn width(&self) -> usize {
        self.fields.iter().filter(|field| !field.skip).count()
    }
}

/// Where one projected field's value comes from.
#[derive(Clone, Debug)]
pub enum Source {
    /// A column constant, such as `User::ID`.
    Column(Type),
    /// An aggregate over a column constant.
    Aggregate(Type, String),
    /// A raw SQL expression.
    Expr(String),
    /// Not selected at all; filled with `Default::default()`.
    Skipped,
}

/// One projected field.
#[derive(Clone, Debug)]
pub struct ProjectionField {
    /// The Rust field name.
    pub field: String,
    /// The Rust field's type.
    pub ty: Type,
    /// Where the value comes from.
    pub source: Source,
    /// The output name in the `SELECT` list.
    pub alias: String,
    /// Whether the field is filled without reading a column.
    pub skip: bool,
    /// Where it was declared.
    pub span: Span,
}

impl ProjectionField {
    /// Reads one field.
    fn parse(field_name: &str, field: &syn::Field, model: &ProjectionModel) -> syn::Result<Self> {
        let settings = settings_of(&field.attrs, "projection")?;
        let mut column: Option<Type> = None;
        let mut aggregate: Option<String> = None;
        let mut expression: Option<String> = None;
        let mut alias = field_name.to_owned();
        let mut skip = false;

        for setting in &settings {
            match setting.name().as_str() {
                "column" => column = Some(column_path(setting.value()?)?),
                "expr" => expression = Some(setting.value()?.string()?),
                "alias" => alias = setting.value()?.string()?,
                "skip" => skip = true,
                "agg" => {
                    let name = setting.value()?.string()?;
                    if !AGGREGATES.contains(&name.as_str()) {
                        return Err(unknown_setting(
                            &name,
                            AGGREGATES,
                            setting.span(),
                            "projection",
                        ));
                    }
                    aggregate = Some(name);
                }
                unknown => {
                    return Err(unknown_setting(
                        unknown,
                        FIELD_ATTRIBUTES,
                        setting.span(),
                        "projection",
                    ));
                }
            }
        }

        if expression.is_some() && column.is_some() {
            return Err(err(
                field.ty.span(),
                &format!("`{field_name}` reads both a column and an expression"),
                "keep one — `column = User::EMAIL` or `expr = \"count(posts.id)\"`",
            ));
        }

        let source = if skip {
            Source::Skipped
        } else if let Some(fragment) = expression {
            Source::Expr(fragment)
        } else {
            let column = match column {
                Some(column) => column,
                None => {
                    let Some(entity) = &model.entity else {
                        return Err(err(
                            field.ty.span(),
                            &format!("`{field_name}` does not say which column it reads"),
                            "name the entity once — `#[projection(entity = User)]` — or the \
                             column here: `#[projection(column = User::EMAIL)]`",
                        ));
                    };
                    let constant = format_ident!("{}", column_const_name(field_name));
                    syn::parse_quote!(#entity::#constant)
                }
            };
            match aggregate {
                Some(function) => Source::Aggregate(column, function),
                None => Source::Column(column),
            }
        };

        Ok(Self {
            field: field_name.to_owned(),
            ty: field.ty.clone(),
            source,
            alias,
            skip,
            span: field.ty.span(),
        })
    }
}

/// A `column = …` value: a path, written bare or quoted.
///
/// Both spellings exist in the design documents, and refusing either would be
/// pedantry — `column = Post::CREATED_AT` reads better and `column =
/// "Post::CREATED_AT"` is what someone who has written `serde` reaches for.
fn column_path(value: &SettingValue) -> syn::Result<Type> {
    match value {
        SettingValue::Type(ty) => Ok((**ty).clone()),
        SettingValue::Lit(syn::Lit::Str(text)) => text.parse::<Type>().map_err(|_| {
            err(
                text.span(),
                &format!("`{}` is not a column constant", text.value()),
                "write the constant, as in `column = Post::CREATED_AT`",
            )
        }),
        other => Err(err(
            other.span(),
            "`column = …` takes a column constant",
            "write `column = Post::CREATED_AT`",
        )),
    }
}

/// The `AggregateFunc` variant a spelling names.
fn aggregate_tokens(name: &str) -> TokenStream {
    let private = private_path();
    let variant = match name {
        "count" => quote!(Count),
        "sum" => quote!(Sum),
        "avg" => quote!(Avg),
        "min" => quote!(Min),
        "max" => quote!(Max),
        "array_agg" => quote!(ArrayAgg),
        "string_agg" => quote!(StringAgg),
        "json_agg" => quote!(JsonAgg),
        "jsonb_agg" => quote!(JsonbAgg),
        "json_object_agg" => quote!(JsonObjectAgg),
        "jsonb_object_agg" => quote!(JsonbObjectAgg),
        "bool_and" => quote!(BoolAnd),
        "bool_or" => quote!(BoolOr),
        "stddev" => quote!(StdDev),
        _ => quote!(Variance),
    };
    quote!(#private::AggregateFunc::#variant)
}

impl ProjectionModel {
    /// The whole expansion.
    #[must_use]
    pub fn generate(&self) -> TokenStream {
        let private = private_path();
        let ident = &self.ident;
        let name = &self.type_name;
        let width = self.width();

        let scopes = self.entity.iter().chain(self.joined.iter()).map(|entity| {
            quote! {
                #[automatically_derived]
                impl #private::ProjectionScope<#entity> for #ident {}
            }
        });

        let items = self.fields.iter().filter_map(|field| {
            let alias = &field.alias;
            match &field.source {
                Source::Skipped => None,
                Source::Column(column) => Some(quote! {
                    #private::checked_column_as::<Self, _, _>(#column, #alias)
                }),
                Source::Aggregate(column, function) => {
                    let function = aggregate_tokens(function);
                    Some(quote! {
                        #private::checked_aggregate::<Self, _, _>(#column, #function, #alias)
                    })
                }
                Source::Expr(fragment) => Some(quote! {
                    #private::raw_expr_as(#fragment, #alias)
                }),
            }
        });

        let mut index = 0_usize;
        let assignments: Vec<TokenStream> = self
            .fields
            .iter()
            .map(|field| {
                let member = format_ident!("{}", field.field);
                let field_name = &field.field;
                if field.skip {
                    return quote!(#member: ::core::default::Default::default());
                }
                let ty = &field.ty;
                let at = index;
                index += 1;
                // The field's own span, so "this type is not a column type"
                // points at the field rather than at generated tokens.
                quote_spanned! { field.span =>
                    #member: <#ty as #private::SqlType>::decode(__row, #at)
                        .map_err(|__error| __error.in_entity(#name).in_field(#field_name))?
                }
            })
            .collect();

        quote! {
            #(#scopes)*

            #[automatically_derived]
            impl #private::Projection for #ident {
                const COLUMNS: usize = #width;

                fn select_items() -> ::std::vec::Vec<#private::SelectItem> {
                    ::std::vec![#(#items),*]
                }

                fn from_row(
                    __row: &#private::Row,
                ) -> ::core::result::Result<Self, #private::DecodeError> {
                    ::core::result::Result::Ok(Self { #(#assignments),* })
                }
            }
        }
    }
}

/// Expands `#[derive(Projection)]`.
pub fn expand(input: TokenStream) -> TokenStream {
    let input: DeriveInput = match syn::parse2(input) {
        Ok(parsed) => parsed,
        Err(error) => return error.to_compile_error(),
    };
    match ProjectionModel::parse(&input) {
        Ok(model) => model.generate(),
        Err(error) => error.to_compile_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> syn::Result<ProjectionModel> {
        let input: DeriveInput = syn::parse_str(source).expect("the test source parses");
        ProjectionModel::parse(&input)
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
    fn the_fields_are_the_projection_in_decode_order() {
        let model =
            parse("#[projection(entity = User)] struct Summary { id: i64, post_count: i64 }")
                .expect("a projection");
        assert_eq!(model.width(), 2);
        assert_eq!(model.fields[0].field, "id");
        assert_eq!(model.fields[1].field, "post_count");
        assert_eq!(
            model
                .entity
                .as_ref()
                .and_then(crate::shared::type_name_of)
                .as_deref(),
            Some("User"),
        );
    }

    #[test]
    fn an_enum_is_refused_with_a_fix() {
        let error = parse("enum Summary { A }").expect_err("an enum is not a row");
        assert!(error.to_string().contains("help:"));
    }

    #[test]
    fn the_attribute_vocabulary_is_frozen() {
        assert!(CONTAINER_ATTRIBUTES.contains(&"entity"));
        assert!(FIELD_ATTRIBUTES.contains(&"expr"));
        assert!(FIELD_ATTRIBUTES.contains(&"agg"));
    }

    #[test]
    fn the_scope_impl_is_written_once_per_named_entity() {
        let out = expand_str(
            "#[projection(entity = User, join = Post)]
             struct Summary { id: i64 }",
        );
        assert!(
            out.contains("impl :: moso :: __private :: ProjectionScope < User > for Summary"),
            "{out}"
        );
        assert!(
            out.contains("impl :: moso :: __private :: ProjectionScope < Post > for Summary"),
            "{out}"
        );
    }

    #[test]
    fn a_bare_field_reads_the_entity_constant_of_the_same_name() {
        let out = expand_str("#[projection(entity = User)] struct Summary { email: String }");
        assert!(
            out.contains("checked_column_as :: < Self , _ , _ > (User :: EMAIL , \"email\")"),
            "{out}"
        );
    }

    #[test]
    fn an_expression_becomes_an_aliased_raw_fragment() {
        let out = expand_str(
            "#[projection(entity = User)]
             struct Summary {
                 id: i64,
                 #[projection(expr = \"count(posts.id)\")] post_count: i64,
             }",
        );
        assert!(
            out.contains("raw_expr_as (\"count(posts.id)\" , \"post_count\")"),
            "{out}"
        );
    }

    #[test]
    fn an_aggregate_names_the_function_and_the_alias() {
        let out = expand_str(
            "#[projection(entity = User, join = Post)]
             struct Summary {
                 id: i64,
                 #[projection(column = Post::CREATED_AT, agg = \"max\")] last: i64,
             }",
        );
        assert!(
            out.contains(
                "checked_aggregate :: < Self , _ , _ > (Post :: CREATED_AT , \
                 :: moso :: __private :: AggregateFunc :: Max , \"last\")"
            ),
            "{out}"
        );
    }

    #[test]
    fn a_quoted_column_constant_is_accepted_too() {
        let out = expand_str(
            "#[projection(entity = User, join = Post)]
             struct Summary { #[projection(column = \"Post::TITLE\")] title: String }",
        );
        assert!(out.contains("Post :: TITLE"), "{out}");
    }

    #[test]
    fn from_row_decodes_positionally_and_names_the_field() {
        let out =
            expand_str("#[projection(entity = User)] struct Summary { id: i64, email: String }");
        assert!(out.contains("decode (__row , 0usize)"), "{out}");
        assert!(out.contains("decode (__row , 1usize)"), "{out}");
        assert!(
            out.contains("in_entity (\"Summary\") . in_field (\"email\")"),
            "{out}"
        );
        assert!(out.contains("const COLUMNS : usize = 2usize"), "{out}");
    }

    #[test]
    fn a_skipped_field_is_defaulted_and_shifts_no_index() {
        let out = expand_str(
            "#[projection(entity = User)]
             struct Summary {
                 id: i64,
                 #[projection(skip)] note: String,
                 email: String,
             }",
        );
        assert!(
            out.contains("note : :: core :: default :: Default :: default ()"),
            "{out}"
        );
        assert!(out.contains("decode (__row , 1usize)"), "{out}");
        assert!(out.contains("const COLUMNS : usize = 2usize"), "{out}");
    }

    #[test]
    fn a_field_with_no_entity_and_no_column_names_both_fixes() {
        let error = parse("struct Summary { id: i64 }").expect_err("nothing to resolve against");
        let text = error.to_string();
        assert!(text.contains("does not say which column"), "{text}");
        assert!(text.contains("entity = User"), "{text}");
    }

    #[test]
    fn an_unknown_aggregate_suggests_the_closest_real_one() {
        let error = parse(
            "#[projection(entity = User)]
             struct Summary { #[projection(agg = \"mox\")] a: i64 }",
        )
        .expect_err("`mox` is not an aggregate");
        assert!(error.to_string().contains("did you mean `max`?"));
    }

    #[test]
    fn a_field_that_reads_two_things_is_refused() {
        let error = parse(
            "#[projection(entity = User)]
             struct Summary { #[projection(column = User::ID, expr = \"1\")] a: i64 }",
        )
        .expect_err("both");
        assert!(
            error
                .to_string()
                .contains("both a column and an expression")
        );
    }

    #[test]
    fn the_expansion_names_only_the_private_path() {
        let out = expand_str("#[projection(entity = User)] struct Summary { id: i64 }");
        assert!(!out.contains("moso_orm"), "{out}");
        assert!(!out.contains("moso_sql"), "{out}");
        assert!(!out.contains("compile_error"), "{out}");
    }
}
