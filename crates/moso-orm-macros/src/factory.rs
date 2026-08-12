//! `#[derive(Factory)]`: a typed builder for test fixtures and seed data.
//!
//! ```text
//! #[derive(Entity, Factory)]
//! #[factory(email = "format!(\"user{n}@example.com\")", name = "String::from(\"Ada\")")]
//! pub struct User { … }
//!
//! let admin  = User::factory().is_admin(true).create(&db).await?;
//! let twenty = User::factory().count(20).create_many(&db).await?;
//! let draft  = User::factory().build();          // unsaved
//! ```
//!
//! # What it builds from
//!
//! The same `#[entity(..)]` model `#[derive(Entity)]` reads, so the factory's
//! setters are exactly the `New…` struct's fields — no second list to keep in
//! step. A field with no `#[factory(field = "…")]` default falls back to
//! `Default::default()`, so give one for any type that is not `Default`.
//!
//! Inside a default expression, `n` is the row's index within a `count(..)`
//! run. That is what makes twenty rows twenty *different* rows without a
//! `sequence(..)` closure.
//!
//! # `#[factory(..)]` is a container attribute, and only a container attribute
//!
//! Every other attribute in this crate has a field form, so the natural thing
//! to write is `#[factory(default = "…")]` above the field it belongs to. That
//! spelling is refused, by [`misplaced_field_settings`], with the container
//! line to write instead.
//!
//! It has to be refused rather than ignored. `factory` is a declared helper
//! attribute of the derive, so rustc accepts it wherever it appears and strips
//! it before anything else runs: a field-level default would vanish without a
//! word and the field would quietly fall back to `Default::default()`. A
//! default that is silently *not* the one you wrote is the worst shape a
//! mistake can take, because nothing ever looks wrong enough to investigate.
//!
//! Refusing also keeps one home for the fact. Two spellings of one default
//! would be two things to document, two orderings to define when both are
//! given, and a second place for the next reader to look.
//!
//! The refusal is a single error and nothing else: the factory is still built,
//! still using the expression that was written in the wrong place, so neither a
//! `User::factory()` elsewhere nor a field type that has no `Default` produces
//! a second error for the same mistake.
//!
//! # There is no faker
//!
//! `43-testing.md` writes the defaults as `faker::internet::Email`. Moso ships
//! no faker crate and this derive invents none: the string is an ordinary Rust
//! expression, so `faker::internet::Email` works if the application depends on
//! a crate that provides it, and `format!("user{n}@example.com")` works with no
//! dependency at all and is reproducible by construction.

use core::slice;

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned as _;
use syn::{Attribute, Data, DeriveInput, Expr, Ident, Type};

use crate::entity::{ColumnModel, EntityModel};
use crate::shared::{
    err, err_with_note, factory_struct_name, private_path, settings_of, unknown_setting,
};

/// One factory, as the attributes describe it.
#[derive(Clone, Debug)]
pub struct FactoryModel {
    /// The entity the factory builds.
    pub entity: EntityModel,
    /// The generated factory struct's name.
    pub struct_name: String,
    /// Per-field default expressions, in the entity's field order.
    pub defaults: Vec<(String, Expr)>,
}

impl FactoryModel {
    /// Reads the model out of a `#[derive(Factory)]` input.
    ///
    /// # Errors
    ///
    /// Everything [`EntityModel::parse`] reports, plus a `#[factory(..)]`
    /// setting that names no member and a default that is not a Rust
    /// expression. A misplaced field-level attribute is [`expand`]'s to report,
    /// because it does not stop the factory from being built.
    pub fn parse(input: &DeriveInput) -> syn::Result<Self> {
        let entity = EntityModel::parse(input)?;
        let struct_name = factory_struct_name(&entity.type_name);
        let mut defaults = Vec::new();

        // The embedded fields belong here as well as the plain columns:
        // `generate` builds a setter for each one and looks its default up by
        // the same name, so leaving them out of `known` would reject a default
        // the expansion is already asking for — and an embedded value object is
        // exactly the kind of type that has no `Default` to fall back on.
        let known: Vec<&str> = entity
            .insertable()
            .iter()
            .map(|column| Self::member_of(column))
            .chain(entity.embeds.iter().map(|embed| embed.field.as_str()))
            .collect();

        for setting in settings_of(&input.attrs, "factory")? {
            let name = setting.name();
            if !known.contains(&name.as_str()) {
                return Err(unknown_setting(&name, &known, setting.span(), "factory"));
            }
            let literal = setting.value()?.string()?;
            let parsed: Expr = syn::parse_str(&literal).map_err(|error| {
                err(
                    setting.span(),
                    &format!("`{literal}` is not a Rust expression: {error}"),
                    "the default is evaluated once per row, so it is ordinary Rust — \
                     `format!(\"user{n}@example.com\")`, `PasswordHash::test()`, `42`",
                )
            })?;
            defaults.push((name, parsed));
        }
        Ok(Self {
            entity,
            struct_name,
            defaults,
        })
    }

    /// The `New…` field name a column fills.
    fn member_of(column: &ColumnModel) -> &str {
        if column.field.is_empty() {
            &column.column
        } else {
            &column.field
        }
    }

    /// The default expression for one member, or `Default::default()`.
    fn default_for(&self, member: &str) -> TokenStream {
        self.defaults
            .iter()
            .find(|(name, _)| name == member)
            .map_or_else(
                || quote!(::core::default::Default::default()),
                |(_, expression)| quote!(#expression),
            )
    }

    /// The whole expansion.
    #[must_use]
    pub fn generate(&self) -> TokenStream {
        let private = private_path();
        let entity = &self.entity.ident;
        let entity_name = &self.entity.type_name;
        let vis = &self.entity.vis;
        let factory = format_ident!("{}", self.struct_name);
        let new_ident = format_ident!("{}", self.entity.new_struct);
        let insertable = self.entity.insertable();

        let members: Vec<(syn::Ident, Type, TokenStream)> = insertable
            .iter()
            .map(|column| {
                let member = Self::member_of(column);
                (
                    format_ident!("{member}"),
                    self.entity.factory_field_type(column),
                    self.default_for(member),
                )
            })
            .chain(self.entity.embeds.iter().map(|embed| {
                (
                    format_ident!("{}", embed.field),
                    embed.ty.clone(),
                    self.default_for(&embed.field),
                )
            }))
            .collect();

        let fields = members
            .iter()
            .map(|(member, ty, _)| quote!(#member: ::core::option::Option<#ty>,));
        let nones = members
            .iter()
            .map(|(member, _, _)| quote!(#member: ::core::option::Option::None,));
        let setters = members.iter().map(|(member, ty, _)| {
            let doc = format!("Sets `{member}` on every row this factory builds.");
            quote! {
                #[doc = #doc]
                #[must_use]
                #vis fn #member(mut self, value: impl ::core::convert::Into<#ty>) -> Self {
                    self.#member = ::core::option::Option::Some(
                        ::core::convert::Into::into(value),
                    );
                    self
                }
            }
        });
        let assignments = members.iter().map(|(member, _, default)| {
            quote! {
                #member: match &self.#member {
                    ::core::option::Option::Some(__value) => ::core::clone::Clone::clone(__value),
                    ::core::option::Option::None => #default,
                },
            }
        });

        let factory_doc = format!(
            "Builds `{entity_name}` rows for tests and seeds.\n\n\
             Every setter fixes a value for every row; `count(..)` decides how many rows there \
             are, and `sequence(..)` varies them. Nothing touches the database until `create` or \
             `create_many`."
        );
        let entry_doc = format!("A factory for `{entity_name}` rows.");
        let build_doc = format!("One unsaved `{}`.", self.entity.new_struct);
        let build_many_doc = format!("`count` unsaved `{}` rows.", self.entity.new_struct);
        let create_doc = format!(
            "Inserts one row and returns the `{entity_name}` the database wrote.\n\n\
             # Errors\n\n\
             Anything `moso::db::Error` reports — a constraint violation, a connection failure."
        );
        let create_many_doc = "Inserts `count` rows in **one** statement and returns them.\n\n\
             # Errors\n\n\
             Anything `moso::db::Error` reports.";

        quote! {
            #[doc = #factory_doc]
            #vis struct #factory {
                #(#fields)*
                count: usize,
                sequence: ::std::vec::Vec<
                    ::std::boxed::Box<
                        dyn ::core::ops::Fn(usize, #new_ident) -> #new_ident
                            + ::core::marker::Send
                            + ::core::marker::Sync,
                    >,
                >,
            }

            #[automatically_derived]
            impl #factory {
                /// A factory with every field left to its default.
                #[must_use]
                #vis fn new() -> Self {
                    Self {
                        #(#nones)*
                        count: 1,
                        sequence: ::std::vec::Vec::new(),
                    }
                }

                #(#setters)*

                /// How many rows `build_many` and `create_many` produce.
                #[must_use]
                #vis const fn count(mut self, rows: usize) -> Self {
                    self.count = rows;
                    self
                }

                /// Varies each row by its index.
                ///
                /// Applied after the fixed values, in the order the closures
                /// were added.
                #[must_use]
                #vis fn sequence(
                    mut self,
                    step: impl ::core::ops::Fn(usize, #new_ident) -> #new_ident
                        + ::core::marker::Send
                        + ::core::marker::Sync
                        + 'static,
                ) -> Self {
                    self.sequence.push(::std::boxed::Box::new(step));
                    self
                }

                #[doc = #build_doc]
                #[must_use]
                #vis fn build(&self) -> #new_ident {
                    self.build_at(0)
                }

                #[doc = #build_many_doc]
                #[must_use]
                #vis fn build_many(&self) -> ::std::vec::Vec<#new_ident> {
                    (0..self.count).map(|__index| self.build_at(__index)).collect()
                }

                /// One row, as it would be at index `n` of a `count(..)` run.
                #[must_use]
                #vis fn build_at(&self, __index: usize) -> #new_ident {
                    #[allow(unused_variables)]
                    let n: usize = __index;
                    let mut __row = #new_ident { #(#assignments)* };
                    for __step in &self.sequence {
                        __row = __step(__index, __row);
                    }
                    __row
                }

                #[doc = #create_doc]
                #vis async fn create(
                    &self,
                    executor: impl #private::Executor<'_>,
                ) -> #private::OrmResult<#entity> {
                    #entity::insert(self.build())
                        .returning_entity()
                        .fetch_one(executor)
                        .await
                }

                #[doc = #create_many_doc]
                #vis async fn create_many(
                    &self,
                    executor: impl #private::Executor<'_>,
                ) -> #private::OrmResult<::std::vec::Vec<#entity>> {
                    #entity::insert_many(self.build_many())
                        .returning_entity()
                        .fetch_all(executor)
                        .await
                }
            }

            #[automatically_derived]
            impl ::core::default::Default for #factory {
                fn default() -> Self {
                    Self::new()
                }
            }

            #[automatically_derived]
            impl #entity {
                #[doc = #entry_doc]
                #[must_use]
                #vis fn factory() -> #factory {
                    #factory::new()
                }
            }
        }
    }
}

impl EntityModel {
    /// The type a factory setter takes for one column: the `New…` field's,
    /// with the "the database will fill it in" `Option` peeled off, because a
    /// factory that is *setting* the value is not leaving it to the default.
    fn factory_field_type(&self, column: &ColumnModel) -> Type {
        if column.default.is_some() {
            let bare = column.bare_type();
            return syn::parse_quote!(::core::option::Option<#bare>);
        }
        column.ty.clone()
    }
}

// ---------------------------------------------------------------------------
// The misplaced-attribute check
// ---------------------------------------------------------------------------

/// Every field-level `#[factory(..)]`, as the error to report and the defaults
/// to go on using anyway.
///
/// Two decisions are folded together here, and both come from
/// `docs/04-devex/41-diagnostics.md`'s rule 4.
///
/// *One error.* Whatever the key, the answer is the same — the container
/// carries every default — so only the first misplaced attribute is reported,
/// and it is reported once for the placement rather than once per setting.
///
/// *No cascade.* The recovered defaults are the point of the second return
/// value. A field whose type has no `Default` is exactly the field a default
/// gets written for — an `Email`, an embedded `Address` — so dropping the
/// expression on the floor would raise a second, derived "the trait bound
/// `Email: Default` is not satisfied" against the derive's own span. The
/// expansion therefore goes on using the value the user wrote, and the build
/// fails on the placement alone.
///
/// Returns `None` when every field is clean, which is the only case that
/// expands to a factory on its own.
fn misplaced_field_settings(input: &DeriveInput) -> Option<(syn::Error, Vec<(String, Expr)>)> {
    let Data::Struct(data) = &input.data else {
        return None;
    };
    let mut first = None;
    let mut recovered = Vec::new();

    for field in &data.fields {
        let Some(attribute) = field
            .attrs
            .iter()
            .find(|attribute| attribute.path().is_ident("factory"))
        else {
            continue;
        };
        let member = field
            .ident
            .as_ref()
            .map_or_else(|| String::from("field"), ToString::to_string);
        let written = single_expression(attribute);

        if first.is_none() {
            first = Some(misplaced_default(
                attribute,
                &member,
                written.as_deref(),
                &input.ident,
            ));
        }
        if let Some(expression) = written.and_then(|text| syn::parse_str::<Expr>(&text).ok()) {
            recovered.push((member, expression));
        }
    }

    first.map(|error| (error, recovered))
}

/// The expression behind a lone `key = "…"`, when that is the whole attribute.
///
/// Anything else — two settings, a flag, a number — has no single expression to
/// recover or to quote back, and the caller falls back to showing the shape.
fn single_expression(attribute: &Attribute) -> Option<String> {
    let settings = settings_of(slice::from_ref(attribute), "factory").ok()?;
    let [only] = settings.as_slice() else {
        return None;
    };
    only.value().ok()?.string().ok()
}

/// The diagnostic for a field-level `#[factory(..)]`.
///
/// The help echoes the user's own expression back at them under the member's
/// name, so the fix is one line they can paste rather than a rule they have to
/// apply. `written` is `None` when there was no single expression to move, and
/// the shape is shown with the value left for the user to fill in.
///
/// The span is the attribute's *path* rather than the whole attribute because
/// `Span::join` is nightly-only: a joined span degrades on stable to the span
/// of the first token, which would put a single caret under the `#`. Pointing
/// at `factory` names the token that is in the wrong place.
fn misplaced_default(
    attribute: &Attribute,
    member: &str,
    written: Option<&str>,
    type_name: &Ident,
) -> syn::Error {
    let line = written.map_or_else(
        || format!("{member} = \"…\""),
        |expression| format!("{member} = {expression:?}"),
    );
    err_with_note(
        attribute.path().span(),
        &format!("a factory default belongs on `{type_name}`, not on its `{member}` field"),
        "one `#[factory(..)]` carries every default, so a fixture reads in one place",
        &format!("delete this line and write it above the struct:\n        #[factory({line})]"),
    )
}

/// Expands `#[derive(Factory)]`.
///
/// A misplaced field-level attribute is reported *beside* the factory rather
/// than instead of it. Where the attribute sits says nothing about whether the
/// factory can be built, so suppressing the expansion would turn one mistake
/// into two errors the moment a `User::factory()` twenty lines away stopped
/// resolving (`docs/04-devex/41-diagnostics.md`, style guide rule 4).
pub fn expand(input: TokenStream) -> TokenStream {
    let input: DeriveInput = match syn::parse2(input) {
        Ok(parsed) => parsed,
        Err(error) => return error.to_compile_error(),
    };
    let mut model = match FactoryModel::parse(&input) {
        Ok(model) => model,
        Err(error) => return error.to_compile_error(),
    };
    match misplaced_field_settings(&input) {
        None => model.generate(),
        Some((error, recovered)) => {
            // The container's own defaults are already in the list, and
            // `default_for` takes the first match, so a member declared in both
            // places keeps the one written where it belongs.
            model.defaults.extend(recovered);
            let complaint = error.to_compile_error();
            let factory = model.generate();
            quote!(#complaint #factory)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> syn::Result<FactoryModel> {
        let input: DeriveInput = syn::parse_str(source).expect("the test source parses");
        FactoryModel::parse(&input)
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

    fn refuse(source: &str) -> (syn::Error, Vec<(String, Expr)>) {
        let input: DeriveInput = syn::parse_str(source).expect("the test source parses");
        misplaced_field_settings(&input).expect("a field-level `#[factory(..)]`")
    }

    #[test]
    fn the_setters_are_the_insertable_columns() {
        let model = parse(
            "#[entity(timestamps)]
             struct User {
                 #[entity(pk, default = \"uuid_generate_v7()\")] id: i64,
                 email: String,
                 created_at: i64,
                 updated_at: i64,
             }",
        )
        .expect("a factory");
        assert_eq!(model.struct_name, "UserFactory");
        let names: Vec<&str> = model
            .entity
            .insertable()
            .iter()
            .map(|column| FactoryModel::member_of(column))
            .collect();
        assert_eq!(names, ["email"]);
    }

    #[test]
    fn a_default_is_an_ordinary_rust_expression() {
        let model = parse(
            "#[factory(email = \"format!(\\\"user{n}@example.com\\\")\")]
             struct User { #[entity(pk)] id: i64, email: String }",
        )
        .expect("a default");
        assert_eq!(model.defaults.len(), 1);
        assert_eq!(model.defaults[0].0, "email");
    }

    #[test]
    fn an_embedded_field_may_be_given_a_default_like_any_other() {
        let model = parse(
            "#[factory(billing = \"Address::sample()\")]
             struct User {
                 #[entity(pk)] id: i64,
                 #[entity(embedded)] billing: Address,
             }",
        )
        .expect("an embedded field is a factory member");
        assert_eq!(model.defaults[0].0, "billing");
    }

    #[test]
    fn a_default_for_a_column_that_is_not_there_names_the_ones_that_are() {
        let error =
            parse("#[factory(emial = \"1\")] struct User { #[entity(pk)] id: i64, email: String }")
                .expect_err("`emial` is not a column");
        assert!(error.to_string().contains("did you mean `email`?"));
    }

    #[test]
    fn a_default_written_on_the_field_is_refused_and_not_ignored() {
        let (error, recovered) = refuse(
            "struct Author {
                 #[entity(pk)] id: i64,
                 #[factory(default = \"format!(\\\"author{n}@example.com\\\")\")] email: String,
             }",
        );
        let text = error.to_string();
        assert!(
            text.contains("belongs on `Author`, not on its `email` field"),
            "{text}"
        );
        assert!(
            text.contains("note: one `#[factory(..)]` carries"),
            "{text}"
        );
        assert!(
            text.contains("#[factory(email = \"format!(\\\"author{n}@example.com\\\")\")]"),
            "the help rewrites the user's own expression under the member's name: {text}"
        );
        assert_eq!(
            recovered[0].0, "email",
            "the expression is kept for recovery"
        );
    }

    #[test]
    fn a_field_attribute_the_derive_cannot_rewrite_still_shows_the_shape() {
        let (error, recovered) = refuse(
            "struct Author { #[entity(pk)] id: i64, #[factory(skip, len = 3)] email: String }",
        );
        let text = error.to_string();
        assert!(text.contains("#[factory(email = \"…\")]"), "{text}");
        assert_eq!(text.matches("help:").count(), 1, "{text}");
        assert!(
            recovered.is_empty(),
            "there is no single expression to keep"
        );
    }

    #[test]
    fn two_misplaced_defaults_are_one_error_and_both_are_recovered() {
        let (error, recovered) = refuse(
            "struct Author {
                 #[entity(pk)] id: i64,
                 #[factory(default = \"1\")] email: String,
                 #[factory(default = \"2\")] name: String,
             }",
        );
        assert!(error.to_string().contains("`email` field"), "the first one");
        assert_eq!(recovered.len(), 2, "neither may cascade");
    }

    #[test]
    fn the_refusal_still_emits_the_factory_so_one_mistake_is_one_error() {
        let out = expand_str(
            "struct Author { #[entity(pk)] id: i64, #[factory(default = \"1\")] email: String }",
        );
        assert_eq!(out.matches("compile_error").count(), 1, "{out}");
        assert!(
            out.contains("struct AuthorFactory") && out.contains("fn factory () -> AuthorFactory"),
            "an `Author::factory()` elsewhere must still resolve: {out}"
        );
        assert!(
            out.contains("Option :: None => 1"),
            "the recovered expression is used, so no `Default` bound is demanded: {out}"
        );
    }

    #[test]
    fn a_default_that_is_not_an_expression_says_so() {
        let error = parse(
            "#[factory(email = \"fn broken(\")] struct User { #[entity(pk)] id: i64, email: String }",
        )
        .expect_err("not an expression");
        assert!(error.to_string().contains("is not a Rust expression"));
    }

    #[test]
    fn the_expansion_carries_the_builder_and_both_entry_points() {
        let out = expand_str("struct User { #[entity(pk)] id: i64, email: String }");
        assert!(out.contains("struct UserFactory"), "{out}");
        assert!(out.contains("fn factory () -> UserFactory"), "{out}");
        assert!(out.contains("fn email (mut self"), "{out}");
        assert!(out.contains("const fn count"), "{out}");
        assert!(out.contains("fn sequence"), "{out}");
        assert!(out.contains("fn build (& self)"), "{out}");
        assert!(out.contains("fn build_many"), "{out}");
        assert!(out.contains("async fn create "), "{out}");
        assert!(out.contains("async fn create_many"), "{out}");
        assert!(!out.contains("compile_error"), "{out}");
    }

    #[test]
    fn creating_many_is_one_statement_not_a_loop() {
        let out = expand_str("struct User { #[entity(pk)] id: i64, email: String }");
        assert!(out.contains("insert_many (self . build_many ())"), "{out}");
        assert!(!out.contains("for __row in"), "{out}");
    }

    #[test]
    fn the_row_index_is_in_scope_for_every_default() {
        let out = expand_str("struct User { #[entity(pk)] id: i64, email: String }");
        assert!(out.contains("let n : usize = __index"), "{out}");
    }

    #[test]
    fn the_expansion_names_only_the_private_path() {
        let out = expand_str("struct User { #[entity(pk)] id: i64 }");
        assert!(!out.contains("moso_orm"), "{out}");
        assert!(!out.contains("moso_sql"), "{out}");
    }
}
