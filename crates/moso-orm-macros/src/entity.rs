//! `#[derive(Entity)]`: the attribute vocabulary, the model it parses into, and
//! the code it generates.
//!
//! # What comes out
//!
//! Four things, in this order:
//!
//! 1. `impl Entity for User` — `TABLE`, `COLUMNS`, `NAME`, `pk`, a **positional**
//!    `from_row`, and a `OnceLock`ed `descriptor()`.
//! 2. `impl User` — one `Column` constant per column, one relation constant per
//!    relation, a named accessor per relation, and `query`/`find`/`insert`/
//!    `insert_many`/`update`/`update_all`/`delete`/`delete_all`.
//! 3. `struct NewUser` + `impl NewEntity for NewUser` — the entity's columns
//!    minus the ones the database supplies.
//! 4. A `{Entity}{Field}Ref` enum for every polymorphic relation.
//!
//! # Two rules the generator obeys
//!
//! **Every path is `::moso::__private::…`** (decision D6), so the runtime
//! layout can move without touching this crate.
//!
//! **Indices are literals wherever they can be.** `from_row` reads column *i*
//! with a literal `i`, so decoding costs no name hashing and no lookup. The one
//! exception is `#[entity(embedded)]`, whose width is only known to the
//! compiler: there the indices become `const` expressions, still resolved
//! before the program runs.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned as _;
use syn::{Data, DeriveInput, Fields, Type, Visibility};

use crate::shared::{
    Setting, column_const_name, default_column_name, default_foreign_key_name, default_index_name,
    default_table_name, doc_comment, err, new_struct_name, option_inner, pluralise, private_path,
    related_inner, settings_of, type_name_of, unknown_setting, validate_sql_ident, vec_inner,
};

/// Every container attribute `#[entity(..)]` accepts, for the "did you mean"
/// suggestion when one is misspelled.
pub const CONTAINER_ATTRIBUTES: &[&str] = &[
    "table",
    "schema",
    "soft_delete",
    "timestamps",
    "expose",
    "index",
    "check",
    "versioned",
    "tenant",
    "audit",
    "rls",
    "comment",
    "new_derives",
];

/// Every field attribute `#[entity(..)]` accepts.
pub const FIELD_ATTRIBUTES: &[&str] = &[
    "pk",
    "column",
    "unique",
    "index",
    "default",
    "len",
    "precision",
    "json",
    "jsonb",
    "enum_as",
    "created_at",
    "updated_at",
    "readonly",
    "encrypted",
    "generated",
    "comment",
    "embedded",
    "belongs_to",
    "has_many",
    "has_one",
    "many_to_many",
    "belongs_to_any",
    "fk",
    "through",
    "left",
    "right",
    "on_delete",
    "on_update",
    "self_ref",
    "count_of",
];

/// Every setting `#[entity(index(..))]` accepts, at the container and on a
/// field.
const INDEX_ATTRIBUTES: &[&str] = &[
    "name",
    "columns",
    "unique",
    "method",
    "where",
    "include",
    "nulls_not_distinct",
];

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// One entity, as the attributes describe it.
///
/// ```text
/// let model = EntityModel::parse(&input)?;
/// assert_eq!(model.table, "users");
/// ```
#[derive(Clone, Debug)]
pub struct EntityModel {
    /// The Rust type's name.
    pub type_name: String,
    /// The Rust type's identifier, for code generation.
    pub ident: syn::Ident,
    /// The entity's visibility, which the generated items inherit.
    pub vis: Visibility,
    /// The table name, defaulted from the type name.
    pub table: String,
    /// The schema, when `#[entity(schema = "…")]` named one.
    pub schema: Option<String>,
    /// The soft-delete column.
    pub soft_delete: Option<String>,
    /// The tenant discriminator column.
    pub tenant: Option<String>,
    /// The optimistic-locking column.
    pub versioned: Option<String>,
    /// Whether `#[entity(timestamps)]` was set.
    pub timestamps: bool,
    /// Whether the entity opted out of ADR-0008.
    pub expose: bool,
    /// Whether changes are audited.
    pub audit: bool,
    /// Whether a row-level-security policy is emitted.
    pub rls: bool,
    /// The table comment, defaulted from the type's doc comment.
    pub comment: Option<String>,
    /// The generated insert struct's name.
    pub new_struct: String,
    /// Extra derives to put on the generated insert struct.
    pub new_derives: Vec<syn::Path>,
    /// The scalar columns, in declaration order.
    pub columns: Vec<ColumnModel>,
    /// The relations, in declaration order.
    pub relations: Vec<RelationModel>,
    /// The embedded value objects, in declaration order.
    pub embeds: Vec<EmbedModel>,
    /// The `#[entity(count_of = "…")]` fields, which hold a relation's row
    /// count rather than a column.
    pub counts: Vec<CountModel>,
    /// The composite, partial and method-qualified indexes.
    pub indexes: Vec<IndexModel>,
    /// The table check constraints.
    pub checks: Vec<CheckModel>,
}

impl EntityModel {
    /// Reads the model out of a `#[derive(Entity)]` input.
    ///
    /// # Errors
    ///
    /// [`syn::Error`] for an input that is not a named-field struct, for an
    /// unknown attribute (with a "did you mean" when one is close), and for a
    /// struct with no `#[entity(pk)]`.
    pub fn parse(input: &DeriveInput) -> syn::Result<Self> {
        let Data::Struct(data) = &input.data else {
            return Err(syn::Error::new(
                input.span(),
                "`#[derive(Entity)]` describes a table, and a table has named columns\n  \
                 help: put it on a `struct` with named fields",
            ));
        };
        let Fields::Named(fields) = &data.fields else {
            return Err(syn::Error::new(
                data.fields.span(),
                "`#[derive(Entity)]` needs named fields, because each one is a column\n  \
                 help: give every field a name, or write the `Entity` impl by hand",
            ));
        };
        if !input.generics.params.is_empty() {
            return Err(err(
                input.generics.span(),
                "a generic struct is not one table",
                "an entity maps to exactly one table, so its columns cannot depend on a type \
                 parameter — declare one entity per table",
            ));
        }

        let type_name = input.ident.to_string();
        let mut model = Self {
            table: default_table_name(&type_name),
            new_struct: new_struct_name(&type_name),
            ident: input.ident.clone(),
            vis: input.vis.clone(),
            type_name,
            schema: None,
            soft_delete: None,
            tenant: None,
            versioned: None,
            timestamps: false,
            expose: false,
            audit: false,
            rls: false,
            comment: doc_comment(&input.attrs),
            new_derives: Vec::new(),
            columns: Vec::new(),
            relations: Vec::new(),
            embeds: Vec::new(),
            counts: Vec::new(),
            indexes: Vec::new(),
            checks: Vec::new(),
        };
        model.read_container_attributes(&input.attrs)?;
        validate_sql_ident(&model.table, input.ident.span(), "table")?;
        if let Some(schema) = &model.schema {
            validate_sql_ident(schema, input.ident.span(), "schema")?;
        }

        for field in &fields.named {
            let Some(name) = field.ident.as_ref() else {
                continue;
            };
            model.read_field(name, field)?;
        }

        model.apply_roles()?;
        model.bind_foreign_keys()?;
        model.check()?;
        Ok(model)
    }

    /// Whether every query for this entity must name a tenant.
    #[must_use]
    pub const fn is_tenant_scoped(&self) -> bool {
        self.tenant.is_some()
    }

    /// The columns the generated `New…` struct carries: everything except a
    /// key with a default, the managed timestamps, the soft-delete flag, the
    /// version counter, the generated columns and the read-only ones.
    #[must_use]
    pub fn insertable(&self) -> Vec<&ColumnModel> {
        self.columns
            .iter()
            .filter(|column| {
                !column.readonly
                    && column.generated.is_none()
                    && !column.role.is_framework_managed()
                    && !(column.primary_key && column.default.is_some())
            })
            .collect()
    }

    /// The single primary-key column, which is what `Entity::Pk` is built from.
    #[must_use]
    pub fn primary_key(&self) -> Option<&ColumnModel> {
        self.columns.iter().find(|column| column.primary_key)
    }

    /// Reads `#[entity(..)]` from the container.
    fn read_container_attributes(&mut self, attrs: &[syn::Attribute]) -> syn::Result<()> {
        for setting in settings_of(attrs, "entity")? {
            self.read_one_container_attribute(&setting)?;
        }
        Ok(())
    }

    /// Reads one `key`, `key = "value"` or `key(..)` from a container
    /// `#[entity(..)]`.
    fn read_one_container_attribute(&mut self, setting: &Setting) -> syn::Result<()> {
        let name = setting.name();
        match name.as_str() {
            "table" => self.table = setting.value()?.string()?,
            "schema" => self.schema = Some(setting.value()?.string()?),
            "soft_delete" => self.soft_delete = Some(setting.value()?.string()?),
            "tenant" => self.tenant = Some(setting.value()?.string()?),
            "versioned" => self.versioned = Some(setting.value()?.string()?),
            "comment" => self.comment = Some(setting.value()?.string()?),
            "timestamps" => self.timestamps = true,
            "expose" => self.expose = true,
            "audit" => self.audit = true,
            "rls" => self.rls = true,
            "index" => {
                let index = IndexModel::parse(setting.items()?, setting.span(), &self.table)?;
                if index.columns.is_empty() {
                    return Err(err(
                        setting.span(),
                        "a container `index(..)` has to say which columns it covers",
                        "write `index(columns(\"tenant_id\", \"email\"), unique)`",
                    ));
                }
                self.indexes.push(index);
            }
            "check" => self
                .checks
                .push(CheckModel::parse(setting.items()?, setting.span())?),
            "new_derives" => {
                for item in setting.items()? {
                    let Setting::Word(word) = item else {
                        return Err(err(
                            item.span(),
                            "`new_derives(..)` takes trait names",
                            "write `new_derives(Debug, Default, Clone)`",
                        ));
                    };
                    self.new_derives.push(syn::Path::from(word.clone()));
                }
                self.new_derives.dedup_by(|a, b| a == b);
            }
            unknown => {
                return Err(unknown_setting(
                    unknown,
                    CONTAINER_ATTRIBUTES,
                    setting.span(),
                    "entity",
                ));
            }
        }
        Ok(())
    }

    /// Reads one field: a column, a relation, or an embedded value object.
    fn read_field(&mut self, name: &syn::Ident, field: &syn::Field) -> syn::Result<()> {
        let settings = settings_of(&field.attrs, "entity")?;
        let field_name = name.to_string();

        if let Some(relation) = RelationModel::parse(&field_name, field, &settings, self)? {
            self.relations.push(relation);
            return Ok(());
        }

        if let Some(setting) = settings.iter().find(|setting| setting.name() == "count_of") {
            let relation = setting.value()?.string()?;
            self.counts.push(CountModel {
                field: field_name,
                relation,
                span: field.ty.span(),
            });
            return Ok(());
        }

        if settings.iter().any(|setting| setting.name() == "embedded") {
            self.embeds.push(EmbedModel {
                field: field_name,
                ty: field.ty.clone(),
                position: self.columns.len(),
                doc: doc_comment(&field.attrs),
                span: field.ty.span(),
            });
            return Ok(());
        }

        let column = ColumnModel::parse(&field_name, field, &settings, &self.table)?;
        self.columns.push(column);
        Ok(())
    }

    /// Applies the container settings that name a column by its SQL name.
    fn apply_roles(&mut self) -> syn::Result<()> {
        if self.timestamps {
            for (column, role) in [
                ("created_at", ColumnRoleModel::CreatedAt),
                ("updated_at", ColumnRoleModel::UpdatedAt),
            ] {
                let Some(existing) = self
                    .columns
                    .iter_mut()
                    .find(|candidate| candidate.column == column)
                else {
                    return Err(err(
                        self.ident.span(),
                        &format!("`#[entity(timestamps)]` needs a `{column}` column"),
                        &format!(
                            "add `pub {column}: chrono::DateTime<chrono::Utc>,`, or drop \
                             `timestamps`"
                        ),
                    ));
                };
                existing.role = role;
            }
        }
        for (setting, role, what) in [
            (
                self.soft_delete.clone(),
                ColumnRoleModel::SoftDelete,
                "soft_delete",
            ),
            (self.tenant.clone(), ColumnRoleModel::Tenant, "tenant"),
            (
                self.versioned.clone(),
                ColumnRoleModel::Version,
                "versioned",
            ),
        ] {
            let Some(wanted) = setting else { continue };
            let Some(column) = self
                .columns
                .iter_mut()
                .find(|candidate| candidate.column == wanted)
            else {
                return Err(err(
                    self.ident.span(),
                    &format!("`#[entity({what} = \"{wanted}\")]` names a column that is not there"),
                    &format!("add a `{wanted}` field, or point the setting at an existing column"),
                ));
            };
            column.role = role;
            if role == ColumnRoleModel::SoftDelete && !column.nullable {
                return Err(err(
                    column.span,
                    &format!("the soft-delete column `{wanted}` has to be nullable"),
                    "a live row has no deletion time, so the column is `Option<..>` — write \
                     `Option<chrono::DateTime<chrono::Utc>>`",
                ));
            }
        }
        Ok(())
    }

    /// Binds each relation to the column it batches on, and refuses a
    /// relation whose key is not a field.
    ///
    /// # Why the key has to be a field
    ///
    /// A `belongs_to` preload groups the parents by the **foreign key**, not by
    /// the primary key, so the preloader reads `post.author_id` out of a `Post`
    /// through the `ForeignKeyFn` this derive generates. A key that is not a
    /// field has nowhere to be read from, and the preloader's fallback is the
    /// parent's own primary key — which silently returns the wrong rows. One
    /// declared field is cheaper than that bug.
    fn bind_foreign_keys(&mut self) -> syn::Result<()> {
        for index in 0..self.relations.len() {
            let (kind, field, key, nullable, span, polymorphic) = {
                let relation = &self.relations[index];
                (
                    relation.kind,
                    relation.field.clone(),
                    relation.foreign_key.clone(),
                    relation.nullable,
                    relation.span,
                    relation.polymorphic.clone(),
                )
            };
            match kind {
                RelationKindModel::BelongsTo => {
                    let key = key.unwrap_or_default();
                    let column = self.require_column(&key, &field, "belongs_to", span)?;
                    column.role = ColumnRoleModel::ForeignKey;
                    let column_ty = column.ty.clone();
                    let column_nullable = column.nullable;
                    if column_nullable != nullable {
                        return Err(err(
                            span,
                            &format!(
                                "`{field}` is `Related<{}>` and `{key}` is `{}`",
                                if nullable { "Option<..>" } else { ".." },
                                if column_nullable {
                                    "Option<..>"
                                } else {
                                    "not nullable"
                                },
                            ),
                            "make them agree: a nullable key means the related row may be absent,                              so the field is `Related<Option<..>>`",
                        ));
                    }
                    self.relations[index].key_type = Some(column_ty);
                }
                RelationKindModel::Polymorphic => {
                    let Some(polymorphic) = polymorphic else {
                        continue;
                    };
                    let type_column = self.require_column(
                        &polymorphic.type_column,
                        &field,
                        "belongs_to_any",
                        span,
                    )?;
                    type_column.role = ColumnRoleModel::Data;
                    let id_column = self.require_column(
                        &polymorphic.id_column,
                        &field,
                        "belongs_to_any",
                        span,
                    )?;
                    id_column.role = ColumnRoleModel::ForeignKey;
                    let id_type = id_column.ty.clone();
                    self.relations[index].key_type = Some(id_type);
                }
                RelationKindModel::HasMany
                | RelationKindModel::HasOne
                | RelationKindModel::ManyToMany => {}
            }
        }
        Ok(())
    }

    /// The column a relation names, or the error that says to declare it.
    fn require_column(
        &mut self,
        column: &str,
        relation: &str,
        what: &str,
        span: Span,
    ) -> syn::Result<&mut ColumnModel> {
        let entity = self.type_name.clone();
        self.columns
            .iter_mut()
            .find(|candidate| candidate.column == column)
            .ok_or_else(|| {
                err(
                    span,
                    &format!(
                        "`{entity}::{relation}` is a `{what}`, and its key column `{column}` is \
                         not a field"
                    ),
                    &format!(
                        "declare it — `pub {column}: …,` — so that `.with(..)` can batch on it \
                         and a filter can use it without a join"
                    ),
                )
            })
    }

    /// The checks that only make sense once everything is parsed.
    fn check(&self) -> syn::Result<()> {
        let keys: Vec<&ColumnModel> = self
            .columns
            .iter()
            .filter(|column| column.primary_key)
            .collect();
        match keys.len() {
            0 => {
                return Err(err(
                    self.ident.span(),
                    &format!("`{}` has no primary key", self.type_name),
                    "mark one field `#[entity(pk)]` — every table Moso manages has a key, because \
                     `update`, `delete` and every relation are written in terms of one",
                ));
            }
            1 => {}
            _ => {
                let names: Vec<&str> = keys.iter().map(|key| key.column.as_str()).collect();
                return Err(err(
                    self.ident.span(),
                    &format!(
                        "`{}` marks {} fields `#[entity(pk)]` ({})",
                        self.type_name,
                        keys.len(),
                        names.join(", ")
                    ),
                    "a composite key needs `SqlType` for the tuple, which this build does not \
                     provide — give the table a single-column key, or write `impl Entity` by hand",
                ));
            }
        }

        let mut seen = std::collections::HashSet::new();
        for column in &self.columns {
            if !seen.insert(column.column.clone()) {
                return Err(err(
                    column.span,
                    &format!("`{}` is declared twice", column.column),
                    "two fields cannot write the same column — rename one, or point one at \
                     another column with `#[entity(column = \"…\")]`",
                ));
            }
        }

        let mut constants = std::collections::HashSet::new();
        for name in self
            .columns
            .iter()
            .map(|column| column.const_name())
            .chain(self.relations.iter().map(|relation| relation.const_name()))
        {
            if !constants.insert(name.clone()) {
                return Err(err(
                    self.ident.span(),
                    &format!("two members of `{}` both generate `{name}`", self.type_name),
                    "the constant's name is the field's, upper-cased — rename one of the fields",
                ));
            }
        }

        const RESERVED: &[&str] = &[
            "query",
            "find",
            "insert",
            "insert_many",
            "update",
            "update_all",
            "delete",
            "delete_all",
            "factory",
        ];
        for relation in &self.relations {
            if RESERVED.contains(&relation.field.as_str()) {
                return Err(err(
                    relation.span,
                    &format!(
                        "the relation `{}` would shadow the generated `{}::{}`",
                        relation.field, self.type_name, relation.field
                    ),
                    "rename the field — the derive generates an accessor of the same name",
                ));
            }
        }

        for count in &self.counts {
            if !self
                .relations
                .iter()
                .any(|relation| relation.field == count.relation)
            {
                return Err(err(
                    count.span,
                    &format!(
                        "`{}` counts `{}`, which is not a relation of `{}`",
                        count.field, count.relation, self.type_name
                    ),
                    "point `count_of = \"…\"` at a `Related<..>` field of this entity",
                ));
            }
        }

        for relation in &self.relations {
            if self.soft_delete.is_some()
                && relation.kind == RelationKindModel::HasMany
                && relation.on_delete.as_deref() == Some("cascade")
            {
                return Err(err(
                    relation.span,
                    &format!(
                        "`{}` is soft-deleted, so `on_delete = \"cascade\"` on `{}` can never fire",
                        self.type_name, relation.field
                    ),
                    "a soft delete is an `UPDATE`, and an `UPDATE` does not fire a foreign-key \
                     cascade — drop the cascade and reap the children from a job, or hard-delete \
                     the parent",
                ));
            }
        }
        Ok(())
    }
}

/// One scalar column, as the attributes describe it.
#[derive(Clone, Debug)]
pub struct ColumnModel {
    /// The Rust field name. Empty for a column the derive synthesised.
    pub field: String,
    /// The SQL column name.
    pub column: String,
    /// The field's declared Rust type.
    pub ty: Type,
    /// Whether it is part of the primary key.
    pub primary_key: bool,
    /// Whether it carries a `UNIQUE`.
    pub unique: bool,
    /// Whether a single-column index is generated.
    pub indexed: bool,
    /// The index's settings, when `index(..)` carried any.
    pub index: Option<IndexModel>,
    /// Whether it is excluded from every write.
    pub readonly: bool,
    /// Whether it is encrypted at rest.
    pub encrypted: bool,
    /// Whether it is stored as `jsonb`.
    pub json: bool,
    /// The enum storage strategy, when the column holds one.
    pub enum_as: Option<String>,
    /// The database default, as SQL.
    pub default: Option<String>,
    /// The `GENERATED ALWAYS AS` expression.
    pub generated: Option<String>,
    /// The `varchar` length.
    pub max_length: Option<u32>,
    /// The `numeric(p, s)`.
    pub precision: Option<(u8, u8)>,
    /// The column comment, defaulted from the field's doc comment.
    pub comment: Option<String>,
    /// What the column is for.
    pub role: ColumnRoleModel,
    /// Whether the declared type is syntactically an `Option<..>`.
    pub nullable: bool,
    /// Whether the derive invented the column rather than reading a field.
    pub synthesised: bool,
    /// Where it was declared, for diagnostics.
    pub span: Span,
}

impl ColumnModel {
    /// Reads one column field.
    fn parse(
        field_name: &str,
        field: &syn::Field,
        settings: &[Setting],
        table: &str,
    ) -> syn::Result<Self> {
        let mut column = Self {
            column: default_column_name(field_name),
            field: field_name.to_owned(),
            nullable: option_inner(&field.ty).is_some(),
            ty: field.ty.clone(),
            primary_key: false,
            unique: false,
            indexed: false,
            index: None,
            readonly: false,
            encrypted: false,
            json: false,
            enum_as: None,
            default: None,
            generated: None,
            max_length: None,
            precision: None,
            comment: doc_comment(&field.attrs),
            role: ColumnRoleModel::Data,
            synthesised: false,
            span: field.ty.span(),
        };

        for setting in settings {
            let name = setting.name();
            match name.as_str() {
                "pk" => column.primary_key = true,
                "column" => column.column = setting.value()?.string()?,
                "unique" => column.unique = true,
                "readonly" => column.readonly = true,
                "encrypted" => column.encrypted = true,
                "json" | "jsonb" => column.json = true,
                "default" => column.default = Some(setting.value()?.string()?),
                "generated" => {
                    column.generated = Some(setting.value()?.string()?);
                    column.readonly = true;
                }
                "comment" => column.comment = Some(setting.value()?.string()?),
                "len" => column.max_length = Some(setting.value()?.integer()?),
                "enum_as" => {
                    let storage = setting.value()?.string()?;
                    if !matches!(storage.as_str(), "text" | "int" | "pg_enum") {
                        return Err(err(
                            setting.span(),
                            &format!("`{storage}` is not an enum storage strategy"),
                            "one of `\"text\"` (the default), `\"int\"` or `\"pg_enum\"`",
                        ));
                    }
                    column.enum_as = Some(storage);
                }
                "created_at" => column.role = ColumnRoleModel::CreatedAt,
                "updated_at" => column.role = ColumnRoleModel::UpdatedAt,
                "precision" => {
                    let items = setting.items()?;
                    if items.len() != 2 {
                        return Err(err(
                            setting.span(),
                            "`precision(..)` takes the total digits and the digits after the point",
                            "write `precision(10, 2)` for `numeric(10, 2)`",
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
                    column.precision = Some((read(&items[0])?, read(&items[1])?));
                }
                "index" => {
                    column.indexed = true;
                    if let Setting::Call(_, items) = setting {
                        let mut index = IndexModel::parse(items, setting.span(), table)?;
                        if index.columns.is_empty() {
                            index.columns = vec![column.column.clone()];
                            index.name = index
                                .name
                                .or_else(|| Some(default_index_name(table, &[&column.column])));
                        }
                        column.index = Some(index);
                    }
                }
                "embedded" => {
                    return Err(err(
                        setting.span(),
                        "`embedded` is not a column setting",
                        "an embedded value object contributes several columns; it is read before \
                         the column settings, so `#[entity(embedded)]` must be the only one",
                    ));
                }
                unknown => {
                    return Err(unknown_setting(
                        unknown,
                        FIELD_ATTRIBUTES,
                        setting.span(),
                        "entity",
                    ));
                }
            }
        }

        validate_sql_ident(&column.column, column.span, "column")?;
        if column.primary_key && column.nullable {
            return Err(err(
                column.span,
                &format!("the primary key `{}` cannot be nullable", column.column),
                "a key identifies a row, and `NULL` identifies nothing — drop the `Option<..>`",
            ));
        }
        if column.primary_key && column.json {
            return Err(err(
                column.span,
                &format!(
                    "the primary key `{}` cannot be a JSON column",
                    column.column
                ),
                "a key is compared for equality, and two JSON documents that mean the same thing \
                 are not equal — use a scalar key",
            ));
        }
        if let Some((precision, scale)) = column.precision
            && scale > precision
        {
            return Err(err(
                column.span,
                &format!("`precision({precision}, {scale})` keeps more decimals than digits"),
                "the second number counts digits after the point and cannot exceed the first",
            ));
        }
        Ok(column)
    }

    /// The name of the generated `Column` constant.
    #[must_use]
    pub fn const_name(&self) -> String {
        column_const_name(if self.field.is_empty() {
            &self.column
        } else {
            &self.field
        })
    }

    /// The type the column binds and decodes as, which is the declared type
    /// except for a JSON column, where it is wrapped in `Json<..>`.
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

    /// The type the enum settings apply to, with any `Option<..>` peeled off.
    #[must_use]
    pub fn bare_type(&self) -> Type {
        option_inner(&self.ty)
            .cloned()
            .unwrap_or_else(|| self.ty.clone())
    }
}

/// What a column is for, mirroring `moso_orm::ColumnRole`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColumnRoleModel {
    /// An ordinary column.
    #[default]
    Data,
    /// Set on insert.
    CreatedAt,
    /// Set on insert and update.
    UpdatedAt,
    /// The soft-delete timestamp.
    SoftDelete,
    /// The tenant discriminator.
    Tenant,
    /// The optimistic-locking counter.
    Version,
    /// A `belongs_to` foreign key.
    ForeignKey,
}

impl ColumnRoleModel {
    /// Whether the framework writes this column rather than the application.
    #[must_use]
    pub const fn is_framework_managed(self) -> bool {
        matches!(
            self,
            Self::CreatedAt | Self::UpdatedAt | Self::SoftDelete | Self::Version
        )
    }

    /// The `ColumnRole` variant the generated code names.
    fn tokens(self) -> TokenStream {
        let private = private_path();
        let variant = match self {
            Self::Data => quote!(Data),
            Self::CreatedAt => quote!(CreatedAt),
            Self::UpdatedAt => quote!(UpdatedAt),
            Self::SoftDelete => quote!(SoftDelete),
            Self::Tenant => quote!(Tenant),
            Self::Version => quote!(Version),
            Self::ForeignKey => quote!(ForeignKey),
        };
        quote!(#private::ColumnRole::#variant)
    }
}

/// One relation, as the attributes describe it.
#[derive(Clone, Debug)]
pub struct RelationModel {
    /// The Rust field name.
    pub field: String,
    /// Which of the shapes.
    pub kind: RelationKindModel,
    /// The related entity's type name.
    pub target: String,
    /// The related entity's type, as written.
    pub target_ty: Type,
    /// The foreign-key column.
    pub foreign_key: Option<String>,
    /// The join table, for a many-to-many.
    pub through: Option<String>,
    /// The join table's column pointing back here.
    pub left: Option<String>,
    /// The join table's column pointing at the target.
    pub right: Option<String>,
    /// `ON DELETE`, as written.
    pub on_delete: Option<String>,
    /// `ON UPDATE`, as written.
    pub on_update: Option<String>,
    /// Whether the relation points at its own table.
    pub self_ref: bool,
    /// Whether the related row may be absent.
    pub nullable: bool,
    /// The polymorphic settings, when there are any.
    pub polymorphic: Option<PolymorphicModel>,
    /// The declared Rust type of the key column this relation batches on.
    pub key_type: Option<Type>,
    /// The `Related<..>` payload type, for the generated accessor.
    pub payload: Type,
    /// The doc comment on the field.
    pub doc: Option<String>,
    /// Where it was declared.
    pub span: Span,
}

impl RelationModel {
    /// Reads one relation field, or `None` when the field is not one.
    fn parse(
        field_name: &str,
        field: &syn::Field,
        settings: &[Setting],
        entity: &EntityModel,
    ) -> syn::Result<Option<Self>> {
        let declared = settings.iter().find(|setting| {
            matches!(
                setting.name().as_str(),
                "belongs_to" | "has_many" | "has_one" | "many_to_many" | "belongs_to_any"
            )
        });
        let Some(payload) = related_inner(&field.ty) else {
            if let Some(declared) = declared {
                return Err(err(
                    field.ty.span(),
                    &format!("`{field_name}` is a relation, so its type is `Related<..>`"),
                    &match declared.name().as_str() {
                        "has_many" | "many_to_many" => String::from("write `Related<Vec<Target>>`"),
                        "has_one" => String::from("write `Related<Option<Target>>`"),
                        _ => String::from("write `Related<Target>` or `Related<Option<Target>>`"),
                    },
                ));
            }
            return Ok(None);
        };

        let mut relation = Self {
            field: field_name.to_owned(),
            kind: RelationKindModel::HasMany,
            target: String::new(),
            target_ty: payload.clone(),
            foreign_key: None,
            through: None,
            left: None,
            right: None,
            on_delete: None,
            on_update: None,
            self_ref: false,
            nullable: false,
            polymorphic: None,
            key_type: None,
            payload: payload.clone(),
            doc: doc_comment(&field.attrs),
            span: field.ty.span(),
        };

        // The shape of `Related<..>` chooses the default kind, so the common
        // relations need no attribute at all.
        if let Some(item) = vec_inner(payload) {
            relation.kind = RelationKindModel::HasMany;
            relation.target_ty = item.clone();
        } else if let Some(inner) = option_inner(payload) {
            relation.kind = RelationKindModel::BelongsTo;
            relation.nullable = true;
            relation.target_ty = inner.clone();
        } else {
            relation.kind = RelationKindModel::BelongsTo;
            relation.target_ty = payload.clone();
        }

        for setting in settings {
            let name = setting.name();
            match name.as_str() {
                "belongs_to" => {
                    relation.kind = RelationKindModel::BelongsTo;
                    relation.target_ty = setting.value()?.ty()?;
                }
                "has_many" => {
                    relation.kind = RelationKindModel::HasMany;
                    relation.target_ty = setting.value()?.ty()?;
                }
                "has_one" => {
                    relation.kind = RelationKindModel::HasOne;
                    relation.target_ty = setting.value()?.ty()?;
                }
                "many_to_many" => {
                    relation.kind = RelationKindModel::ManyToMany;
                    relation.target_ty = setting.value()?.ty()?;
                }
                "belongs_to_any" => {
                    relation.kind = RelationKindModel::Polymorphic;
                    relation.polymorphic =
                        Some(PolymorphicModel::parse(setting.items()?, setting.span())?);
                }
                "fk" => relation.foreign_key = Some(setting.value()?.string()?),
                "through" => relation.through = Some(setting.value()?.string()?),
                "left" => relation.left = Some(setting.value()?.string()?),
                "right" => relation.right = Some(setting.value()?.string()?),
                "on_delete" => {
                    relation.on_delete =
                        Some(referential(&setting.value()?.string()?, setting.span())?)
                }
                "on_update" => {
                    relation.on_update =
                        Some(referential(&setting.value()?.string()?, setting.span())?)
                }
                "self_ref" => relation.self_ref = true,
                unknown => {
                    return Err(unknown_setting(
                        unknown,
                        FIELD_ATTRIBUTES,
                        setting.span(),
                        "entity",
                    ));
                }
            }
        }

        relation.target = type_name_of(&relation.target_ty).ok_or_else(|| {
            err(
                relation.span,
                &format!("the target of `{field_name}` is not a named type"),
                "name the entity, as in `has_many = Post`",
            )
        })?;

        relation.apply_defaults(entity)?;
        Ok(Some(relation))
    }

    /// Fills in the names the user did not write.
    fn apply_defaults(&mut self, entity: &EntityModel) -> syn::Result<()> {
        let own = snake(&entity.type_name);
        match self.kind {
            RelationKindModel::BelongsTo => {
                self.foreign_key
                    .get_or_insert_with(|| format!("{}_id", self.field));
            }
            RelationKindModel::HasMany | RelationKindModel::HasOne => {
                self.foreign_key.get_or_insert_with(|| format!("{own}_id"));
            }
            RelationKindModel::ManyToMany => {
                self.through
                    .get_or_insert_with(|| format!("{own}_{}", pluralise(&snake(&self.target))));
                self.left.get_or_insert_with(|| format!("{own}_id"));
                self.right
                    .get_or_insert_with(|| format!("{}_id", snake(&self.target)));
            }
            RelationKindModel::Polymorphic => {}
        }
        if let Some(key) = &self.foreign_key {
            validate_sql_ident(key, self.span, "foreign key")?;
        }
        for (name, what) in [
            (self.through.as_ref(), "join table"),
            (self.left.as_ref(), "join column"),
            (self.right.as_ref(), "join column"),
        ] {
            if let Some(name) = name {
                validate_sql_ident(name, self.span, what)?;
            }
        }
        Ok(())
    }

    /// The name of the generated relation constant.
    #[must_use]
    pub fn const_name(&self) -> String {
        column_const_name(&self.field)
    }

    /// The name of the enum a polymorphic relation generates.
    #[must_use]
    pub fn reference_enum_name(&self, entity: &str) -> String {
        use heck::ToUpperCamelCase as _;
        format!("{entity}{}Ref", self.field.to_upper_camel_case())
    }
}

/// Which of the relation shapes, mirroring `moso_orm::RelationKind` plus the
/// polymorphic form, which the runtime enum records as a `BelongsTo` with a
/// `PolymorphicDescriptor` hanging off it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelationKindModel {
    /// The key is on this table.
    BelongsTo,
    /// The key is on the other table, many rows.
    HasMany,
    /// The key is on the other table, one row.
    HasOne,
    /// Through a join table.
    ManyToMany,
    /// The key is on this table and points at one of several entities.
    Polymorphic,
}

impl RelationKindModel {
    /// The `RelationKind` variant the generated code names.
    fn tokens(self) -> TokenStream {
        let private = private_path();
        let variant = match self {
            Self::BelongsTo | Self::Polymorphic => quote!(BelongsTo),
            Self::HasMany => quote!(HasMany),
            Self::HasOne => quote!(HasOne),
            Self::ManyToMany => quote!(ManyToMany),
        };
        quote!(#private::RelationKind::#variant)
    }
}

/// A polymorphic relation's discriminator and targets.
#[derive(Clone, Debug)]
pub struct PolymorphicModel {
    /// The column holding the target's type name.
    pub type_column: String,
    /// The column holding the target's key.
    pub id_column: String,
    /// Every entity the relation can point at.
    pub targets: Vec<Type>,
}

impl PolymorphicModel {
    /// Reads `belongs_to_any(types(A, B), type_column = "…", id_column = "…")`.
    fn parse(items: &[Setting], span: Span) -> syn::Result<Self> {
        let mut model = Self {
            type_column: String::from("target_type"),
            id_column: String::from("target_id"),
            targets: Vec::new(),
        };
        for item in items {
            match item.name().as_str() {
                "types" => {
                    for target in item.items()? {
                        model.targets.push(target.as_type()?);
                    }
                }
                "type_column" => model.type_column = item.value()?.string()?,
                "id_column" => model.id_column = item.value()?.string()?,
                unknown => {
                    return Err(unknown_setting(
                        unknown,
                        &["types", "type_column", "id_column"],
                        item.span(),
                        "entity",
                    ));
                }
            }
        }
        if model.targets.is_empty() {
            return Err(err(
                span,
                "a polymorphic relation has to list the entities it can point at",
                "write `belongs_to_any(types(Post, Comment), type_column = \"target_type\", \
                 id_column = \"target_id\")`",
            ));
        }
        validate_sql_ident(&model.type_column, span, "column")?;
        validate_sql_ident(&model.id_column, span, "column")?;
        Ok(model)
    }
}

/// One `#[entity(count_of = "…")]` field: a relation's row count, kept on the
/// entity rather than in a column.
///
/// `.with_count(Post::COMMENTS)` writes it; `.with(Post::COMMENTS)` writes the
/// `Related` field instead. One relation constant serves both, which is why the
/// generated setter branches on `LoadedRows::is_count`.
#[derive(Clone, Debug)]
pub struct CountModel {
    /// The Rust field name.
    pub field: String,
    /// The relation whose rows it counts.
    pub relation: String,
    /// Where it was declared.
    pub span: Span,
}

/// One embedded value object, flattened into its owner's columns.
#[derive(Clone, Debug)]
pub struct EmbedModel {
    /// The Rust field name.
    pub field: String,
    /// The value object's type.
    pub ty: Type,
    /// How many of the owner's own columns precede it.
    pub position: usize,
    /// The doc comment on the field.
    pub doc: Option<String>,
    /// Where it was declared.
    pub span: Span,
}

/// One index, composite, partial or method-qualified.
#[derive(Clone, Debug)]
pub struct IndexModel {
    /// The index's name.
    pub name: Option<String>,
    /// The columns it covers, in order.
    pub columns: Vec<String>,
    /// Whether it is `UNIQUE`.
    pub unique: bool,
    /// The access method: `btree`, `gin`, `gist`, …
    pub method: Option<String>,
    /// The `WHERE` of a partial index, as SQL.
    pub predicate: Option<String>,
    /// The `INCLUDE` columns of a covering index.
    pub include: Vec<String>,
    /// Whether `NULL`s compare equal for the uniqueness check.
    pub nulls_not_distinct: bool,
}

impl IndexModel {
    /// Reads `index(name = "…", columns("a", "b"), unique, method = "gin",
    /// where = "…", include("c"), nulls_not_distinct)`.
    fn parse(items: &[Setting], span: Span, table: &str) -> syn::Result<Self> {
        let mut model = Self {
            name: None,
            columns: Vec::new(),
            unique: false,
            method: None,
            predicate: None,
            include: Vec::new(),
            nulls_not_distinct: false,
        };
        for item in items {
            match item.name().as_str() {
                "name" => model.name = Some(item.value()?.string()?),
                "unique" => model.unique = true,
                "nulls_not_distinct" => model.nulls_not_distinct = true,
                "method" => model.method = Some(item.value()?.string()?),
                "where" => model.predicate = Some(item.value()?.string()?),
                "columns" => {
                    for column in item.items()? {
                        model.columns.push(positional_string(column)?);
                    }
                }
                "include" => {
                    for column in item.items()? {
                        model.include.push(positional_string(column)?);
                    }
                }
                "<value>" => model.columns.push(positional_string(item)?),
                unknown => {
                    return Err(unknown_setting(
                        unknown,
                        INDEX_ATTRIBUTES,
                        item.span(),
                        "entity",
                    ));
                }
            }
        }
        for column in model.columns.iter().chain(model.include.iter()) {
            validate_sql_ident(column, span, "column")?;
        }
        if model.name.is_none() && !model.columns.is_empty() {
            let borrowed: Vec<&str> = model.columns.iter().map(String::as_str).collect();
            model.name = Some(default_index_name(table, &borrowed));
        }
        if let Some(name) = &model.name {
            validate_sql_ident(name, span, "index")?;
        }
        Ok(model)
    }

    /// The generated `IndexDescriptor`.
    fn tokens(&self) -> TokenStream {
        let private = private_path();
        let name = self.name.clone().unwrap_or_default();
        let columns = self
            .columns
            .iter()
            .map(|column| quote!(.column(#private::Ident::from_static(#column))));
        let unique = self.unique.then(|| quote!(.unique()));
        let nulls = self
            .nulls_not_distinct
            .then(|| quote!(.nulls_not_distinct()));
        let method = self.method.as_ref().map(|method| {
            let variant = index_method(method);
            quote!(.method(#variant))
        });
        let predicate = self
            .predicate
            .as_ref()
            .map(|sql| quote!(.predicate(#private::Expr::raw(#private::RawExpr::new(#sql)))));
        let include = (!self.include.is_empty()).then(|| {
            let names = self
                .include
                .iter()
                .map(|column| quote!(#private::Ident::from_static(#column)));
            quote!(.include([#(#names),*]))
        });
        quote! {
            #private::IndexDescriptor::builder(#name)
                #(#columns)*
                #unique
                #nulls
                #method
                #predicate
                #include
                .build()
        }
    }
}

/// One table check constraint.
#[derive(Clone, Debug)]
pub struct CheckModel {
    /// The constraint's name.
    pub name: String,
    /// The expression, as SQL.
    pub expression: String,
}

impl CheckModel {
    /// Reads `check(name = "…", expr = "…")` or `check("…", "…")`.
    fn parse(items: &[Setting], span: Span) -> syn::Result<Self> {
        let mut name = None;
        let mut expression = None;
        let mut positional = Vec::new();
        for item in items {
            match item.name().as_str() {
                "name" => name = Some(item.value()?.string()?),
                "expr" => expression = Some(item.value()?.string()?),
                "<value>" => positional.push(positional_string(item)?),
                unknown => {
                    return Err(unknown_setting(
                        unknown,
                        &["name", "expr"],
                        item.span(),
                        "entity",
                    ));
                }
            }
        }
        if positional.len() == 2 {
            name = name.or_else(|| Some(positional[0].clone()));
            expression = expression.or_else(|| Some(positional[1].clone()));
        }
        let (Some(name), Some(expression)) = (name, expression) else {
            return Err(err(
                span,
                "a `check(..)` needs a name and an expression",
                "write `check(name = \"price_positive\", expr = \"price > 0\")`",
            ));
        };
        validate_sql_ident(&name, span, "constraint")?;
        Ok(Self { name, expression })
    }
}

/// The string of a positional literal setting.
fn positional_string(setting: &Setting) -> syn::Result<String> {
    match setting {
        Setting::Positional(value) => value.string(),
        other => Err(err(
            other.span(),
            "this list takes quoted column names",
            "write `columns(\"tenant_id\", \"email\")`",
        )),
    }
}

/// `snake_case`, without pulling `heck` into every call site.
fn snake(value: &str) -> String {
    use heck::ToSnakeCase as _;
    value.to_snake_case()
}

/// Validates and normalises an `on_delete` / `on_update` spelling.
fn referential(value: &str, span: Span) -> syn::Result<String> {
    match value {
        "cascade" | "restrict" | "set_null" | "set_default" | "no_action" => Ok(value.to_owned()),
        other => Err(err(
            span,
            &format!("`{other}` is not a referential action"),
            "one of `\"cascade\"`, `\"restrict\"`, `\"set_null\"`, `\"set_default\"` or \
             `\"no_action\"`",
        )),
    }
}

/// The `ReferentialAction` variant a spelling names.
fn referential_tokens(value: &str) -> TokenStream {
    let private = private_path();
    let variant = match value {
        "cascade" => quote!(Cascade),
        "restrict" => quote!(Restrict),
        "set_null" => quote!(SetNull),
        "set_default" => quote!(SetDefault),
        _ => quote!(NoAction),
    };
    quote!(#private::ReferentialAction::#variant)
}

/// The `IndexMethod` variant a spelling names, falling back to `Custom` so an
/// extension's method — `hnsw`, `bloom` — needs no change here.
fn index_method(value: &str) -> TokenStream {
    let private = private_path();
    match value {
        "btree" => quote!(#private::IndexMethod::BTree),
        "hash" => quote!(#private::IndexMethod::Hash),
        "gin" => quote!(#private::IndexMethod::Gin),
        "gist" => quote!(#private::IndexMethod::Gist),
        "spgist" => quote!(#private::IndexMethod::SpGist),
        "brin" => quote!(#private::IndexMethod::Brin),
        other => quote!(#private::IndexMethod::Custom(#private::Ident::from_static(#other))),
    }
}

// ---------------------------------------------------------------------------
// Code generation
// ---------------------------------------------------------------------------

/// What [`EntityModel::columns_const`] worked out: the `COLUMNS` constant, the
/// decode index of every column, and where each embedded block starts.
struct ColumnLayout {
    /// `const` offsets, when an embedded value object made the indices
    /// non-literal. Empty otherwise.
    offsets: TokenStream,
    /// The `Entity::COLUMNS` initialiser.
    columns: TokenStream,
    /// The decode index of column *i*.
    indices: Vec<TokenStream>,
    /// `(embed index, the column index its block starts at)`.
    embeds: Vec<(usize, TokenStream)>,
}

impl EntityModel {
    /// The whole expansion.
    #[must_use]
    pub fn generate(&self) -> TokenStream {
        let entity_impl = self.entity_impl();
        let inherent = self.inherent_impl();
        let new_struct = self.new_struct();
        let polymorphic = self.polymorphic_enums();
        quote! {
            #entity_impl
            #inherent
            #new_struct
            #polymorphic
        }
    }

    /// `TableRef::from_static("users")`, or the schema-qualified form.
    fn table_tokens(&self) -> TokenStream {
        let private = private_path();
        let table = &self.table;
        match &self.schema {
            Some(schema) => quote! {
                #private::TableRef::qualified(
                    #private::Ident::from_static(#schema),
                    #private::Ident::from_static(#table),
                )
            },
            None => quote!(#private::TableRef::from_static(#table)),
        }
    }

    /// One `ColumnDef` literal.
    fn column_def(&self, column: &ColumnModel) -> TokenStream {
        let private = private_path();
        let name = &column.column;
        let ty = column.sql_type();
        let primary_key = column.primary_key.then(|| quote!(.primary_key()));
        let nullable = column.nullable.then(|| quote!(.nullable()));
        let unique = column.unique.then(|| quote!(.unique()));
        let readonly = column.readonly.then(|| quote!(.readonly()));
        let default = (column.default.is_some() || column.generated.is_some())
            .then(|| quote!(.with_default()));
        let role = (column.role != ColumnRoleModel::Data).then(|| {
            let role = column.role.tokens();
            quote!(.role(#role))
        });
        quote! {
            #private::ColumnDef::new(#name, <#ty as #private::SqlType>::KIND)
                #primary_key
                #nullable
                #unique
                #readonly
                #default
                #role
        }
    }

    /// The `COLUMNS` constant, and the per-field decode indices that go with
    /// it.
    ///
    /// Two shapes: literal indices and one array literal in the common case,
    /// `const`-computed offsets and a splice when there is an embedded value
    /// object. The split exists because A3 (small expansions) matters far more
    /// often than `#[entity(embedded)]` does.
    fn columns_const(&self) -> ColumnLayout {
        let private = private_path();
        if self.embeds.is_empty() {
            let defs = self.columns.iter().map(|column| self.column_def(column));
            let indices = (0..self.columns.len())
                .map(|index| quote!(#index))
                .collect();
            return ColumnLayout {
                offsets: TokenStream::new(),
                columns: quote!(&[#(#defs),*]),
                indices,
                embeds: Vec::new(),
            };
        }

        // Split the columns into runs, one before each embedded object.
        let mut boundaries: Vec<usize> = self.embeds.iter().map(|embed| embed.position).collect();
        boundaries.push(self.columns.len());

        let mut parts = Vec::new();
        let mut offsets = Vec::new();
        let mut indices = vec![quote!(0usize); self.columns.len()];
        let mut embed_offsets = Vec::new();
        let mut previous = 0_usize;
        let mut offset_terms: Vec<TokenStream> = Vec::new();

        for (group, boundary) in boundaries.iter().copied().enumerate() {
            let run: Vec<TokenStream> = self.columns[previous..boundary]
                .iter()
                .map(|column| self.column_def(column))
                .collect();
            let run_len = boundary - previous;
            let offset_name = format_ident!("__MOSO_OFFSET_{}", group);
            let terms = offset_terms.clone();
            offsets.push(quote! {
                const #offset_name: usize = 0usize #(+ #terms)*;
            });
            for (step, index) in (previous..boundary).enumerate() {
                indices[index] = quote!(#offset_name + #step);
            }
            parts.push(quote!(&[#(#run),*]));
            offset_terms.push(quote!(#run_len));

            if let Some(embed) = self.embeds.get(group) {
                let ty = &embed.ty;
                let span = embed.span;
                embed_offsets.push((group, quote!(#offset_name + #run_len)));
                // The field's own span, so a type that forgot
                // `#[derive(Embedded)]` is reported at the field.
                parts.push(quote_spanned!(span => <#ty>::MOSO_COLUMNS));
                offset_terms.push(quote_spanned!(span => <#ty>::MOSO_COLUMNS.len()));
            }
            previous = boundary;
        }

        // The offsets are emitted twice — once inside the `COLUMNS` block and
        // once at the top of `from_row` — because a `const` declared inside a
        // block expression is scoped to it, and both need the same numbers.
        let offsets = quote!(#(#offsets)*);
        let columns = quote! {
            {
                #offsets
                const __MOSO_PARTS: &[&[#private::ColumnDef]] = &[#(#parts),*];
                const __MOSO_ALL: [#private::ColumnDef; #private::total_columns(__MOSO_PARTS)] =
                    #private::concat_columns(__MOSO_PARTS);
                &__MOSO_ALL
            }
        };
        let embeds = embed_offsets
            .into_iter()
            .map(|(group, offset)| (group, quote!({ #offset })))
            .collect();
        ColumnLayout {
            offsets,
            columns,
            indices,
            embeds,
        }
    }

    /// The `impl Entity` block.
    fn entity_impl(&self) -> TokenStream {
        let private = private_path();
        let ident = &self.ident;
        let name = &self.type_name;
        let table = self.table_tokens();
        let ColumnLayout {
            offsets,
            columns,
            indices,
            embeds: embed_offsets,
        } = self.columns_const();

        let Some(key) = self.primary_key() else {
            // `check` already refused this; the guard keeps `generate` total.
            return TokenStream::new();
        };
        let key_ty = key.sql_type();
        let key_field = format_ident!("{}", key.field);

        // Decoding: one `let` per field, in column order, then one struct
        // literal. The offsets are literals unless an embed forced them to be
        // `const` expressions; either way nothing is computed at run time.
        let mut decodes = Vec::new();
        let mut assignments = Vec::new();
        for (column, index) in self.columns.iter().zip(indices.iter()) {
            if column.synthesised {
                continue;
            }
            let field = format_ident!("{}", column.field);
            let field_name = &column.field;
            let sql_ty = column.sql_type();
            let read = quote! {
                <#sql_ty as #private::SqlType>::decode(__row, #index)
                    .map_err(|__error| __error.in_entity(#name).in_field(#field_name))?
            };
            let value = if column.json {
                if column.nullable {
                    quote!(#read.map(#private::SqlJson::into_inner))
                } else {
                    quote!(#read.into_inner())
                }
            } else {
                read
            };
            decodes.push(quote!(let #field = #value;));
            assignments.push(quote!(#field));
        }
        for (group, offset) in &embed_offsets {
            let Some(embed) = self.embeds.get(*group) else {
                continue;
            };
            let field = format_ident!("{}", embed.field);
            let ty = &embed.ty;
            let field_name = &embed.field;
            decodes.push(quote! {
                let #field = <#ty>::moso_from_row(__row, #offset)
                    .map_err(|__error| __error.in_entity(#name).in_field(#field_name))?;
            });
            assignments.push(quote!(#field));
        }
        for relation in &self.relations {
            let field = format_ident!("{}", relation.field);
            assignments.push(quote!(#field: #private::Related::NotLoaded));
        }
        for count in &self.counts {
            let field = format_ident!("{}", count.field);
            assignments.push(quote!(#field: ::core::option::Option::None));
        }

        let descriptor = self.descriptor_body();

        quote! {
            #[automatically_derived]
            impl #private::Entity for #ident {
                type Pk = #key_ty;

                const TABLE: #private::TableRef = #table;
                const COLUMNS: &'static [#private::ColumnDef] = #columns;
                const NAME: &'static str = #name;

                fn pk(&self) -> Self::Pk {
                    ::core::clone::Clone::clone(&self.#key_field)
                }

                fn from_row(
                    __row: &#private::Row,
                ) -> ::core::result::Result<Self, #private::DecodeError> {
                    #offsets
                    #(#decodes)*
                    ::core::result::Result::Ok(Self { #(#assignments),* })
                }

                fn descriptor() -> &'static #private::EntityDescriptor {
                    static __MOSO_DESCRIPTOR: ::std::sync::OnceLock<#private::EntityDescriptor> =
                        ::std::sync::OnceLock::new();
                    __MOSO_DESCRIPTOR.get_or_init(|| #descriptor)
                }
            }
        }
    }

    /// The `EntityDescriptor` the migration differ and the admin read.
    fn descriptor_body(&self) -> TokenStream {
        let private = private_path();
        let name = &self.type_name;
        let table = self.table_tokens();

        let columns = self.columns.iter().map(|column| {
            let descriptor = self.column_descriptor(column);
            quote!(__builder = __builder.column(#descriptor);)
        });
        let embeds = self.embeds.iter().map(|embed| {
            let ty = &embed.ty;
            quote! {
                for __column in <#ty>::moso_descriptors() {
                    __builder = __builder.column(__column);
                }
            }
        });
        let indexes = self
            .indexes
            .iter()
            .chain(
                self.columns
                    .iter()
                    .filter_map(|column| column.index.as_ref()),
            )
            .map(|index| {
                let index = index.tokens();
                quote!(__builder = __builder.index(#index);)
            });
        let implied = self.columns.iter().filter_map(|column| {
            if !column.indexed || column.index.is_some() || column.unique || column.primary_key {
                return None;
            }
            let index_name = default_index_name(&self.table, &[&column.column]);
            let column_name = &column.column;
            Some(quote! {
                __builder = __builder.index(
                    #private::IndexDescriptor::builder(#index_name)
                        .column(#private::Ident::from_static(#column_name))
                        .build(),
                );
            })
        });
        let checks = self.checks.iter().map(|check| {
            let name = &check.name;
            let expression = &check.expression;
            quote!(__builder = __builder.check(#private::CheckDescriptor::new(#name, #expression));)
        });
        let foreign_keys = self.relations.iter().filter_map(|relation| {
            let key = relation.foreign_key.as_ref()?;
            if relation.kind != RelationKindModel::BelongsTo {
                return None;
            }
            let target = &relation.target_ty;
            let constraint = default_foreign_key_name(&self.table, key);
            let on_delete = relation.on_delete.as_ref().map(|action| {
                let action = referential_tokens(action);
                quote!(.on_delete(#action))
            });
            let on_update = relation.on_update.as_ref().map(|action| {
                let action = referential_tokens(action);
                quote!(.on_update(#action))
            });
            Some(quote! {
                {
                    let __target_key = <#target as #private::Entity>::primary_key_columns();
                    let __target_key = __target_key.first().copied().unwrap_or("id");
                    __builder = __builder.foreign_key(
                        #private::ForeignKeyDescriptor::builder(
                            #constraint,
                            <#target as #private::Entity>::TABLE,
                        )
                        .column(
                            #private::Ident::from_static(#key),
                            #private::Ident::from_static(__target_key),
                        )
                        #on_delete
                        #on_update
                        .build(),
                    );
                }
            })
        });
        let relations = self
            .relations
            .iter()
            .map(|relation| self.relation_descriptor(relation));
        let enum_types = self.columns.iter().filter_map(|column| {
            column.enum_as.as_ref()?;
            let ty = column.bare_type();
            Some(quote! {
                __builder = __builder.enum_type(#private::EnumTypeDescriptor::new(
                    <#ty as #private::DbEnum>::TYPE_NAME,
                    <#ty as #private::DbEnum>::STORAGE,
                    <#ty as #private::DbEnum>::VARIANTS.iter().copied(),
                ));
            })
        });

        let soft_delete = self
            .soft_delete
            .as_ref()
            .map(|column| quote!(__builder = __builder.soft_delete(#column);));
        let timestamps = self
            .timestamps
            .then(|| quote!(__builder = __builder.timestamps("created_at", "updated_at");));
        let tenant = self
            .tenant
            .as_ref()
            .map(|column| quote!(__builder = __builder.tenant(#column);));
        let versioned = self
            .versioned
            .as_ref()
            .map(|column| quote!(__builder = __builder.versioned(#column);));
        let audit = self.audit.then(|| quote!(__builder = __builder.audited();));
        let expose = self
            .expose
            .then(|| quote!(__builder = __builder.exposed();));
        let rls = self
            .rls
            .then(|| quote!(__builder = __builder.row_level_security();));
        let comment = self
            .comment
            .as_ref()
            .map(|text| quote!(__builder = __builder.comment(#text);));

        quote! {
            {
                let mut __builder = #private::EntityDescriptor::builder(#name, #table);
                #(#columns)*
                #(#embeds)*
                #(#indexes)*
                #(#implied)*
                #(#checks)*
                #(#foreign_keys)*
                #(#relations)*
                #(#enum_types)*
                #soft_delete
                #timestamps
                #tenant
                #versioned
                #audit
                #expose
                #rls
                #comment
                __builder.build()
            }
        }
    }

    /// One `ColumnDescriptor`.
    fn column_descriptor(&self, column: &ColumnModel) -> TokenStream {
        let private = private_path();
        let name = &column.column;
        let sql_ty = column.sql_type();
        let bare = column.bare_type();

        let data_type = if let Some(storage) = &column.enum_as {
            let storage = match storage.as_str() {
                "int" => quote!(#private::EnumStorage::Int),
                "pg_enum" => quote!(#private::EnumStorage::PgEnum),
                _ => quote!(#private::EnumStorage::Text),
            };
            quote! {
                #storage.data_type(::core::option::Option::Some(
                    #private::TypeRef::from_static(<#bare as #private::DbEnum>::TYPE_NAME),
                ))
            }
        } else if let Some(length) = column.max_length {
            quote!(#private::DataType::VarChar(::core::option::Option::Some(#length)))
        } else if let Some((precision, scale)) = column.precision {
            quote! {
                #private::DataType::Numeric {
                    precision: ::core::option::Option::Some(#precision),
                    scale: ::core::option::Option::Some(#scale),
                }
            }
        } else {
            quote!(<#sql_ty as #private::SqlType>::data_type())
        };

        let field = (!column.field.is_empty()).then(|| {
            let field = &column.field;
            quote!(.field(#field))
        });
        let nullable = column.nullable.then(|| quote!(.nullable()));
        let primary_key = column.primary_key.then(|| quote!(.primary_key()));
        let unique = column.unique.then(|| quote!(.unique()));
        let readonly = column.readonly.then(|| quote!(.readonly()));
        let encrypted = column.encrypted.then(|| quote!(.encrypted()));
        let role = (column.role != ColumnRoleModel::Data).then(|| {
            let role = column.role.tokens();
            quote!(.role(#role))
        });
        let default = column
            .default
            .as_ref()
            .map(|sql| quote!(.default(#private::ColumnDefault::sql(#sql))));
        let generated = column
            .generated
            .as_ref()
            .map(|expression| quote!(.generated(#expression)));
        let max_length = column.max_length.map(|length| quote!(.max_length(#length)));
        let numeric = column
            .precision
            .map(|(precision, scale)| quote!(.numeric(#precision, #scale)));
        let enum_type = column.enum_as.as_ref().map(|_| {
            quote! {
                .enum_type(#private::TypeRef::from_static(
                    <#bare as #private::DbEnum>::TYPE_NAME,
                ))
            }
        });
        let comment = column.comment.as_ref().map(|text| quote!(.comment(#text)));

        quote! {
            #private::ColumnDescriptor::builder(#private::Ident::from_static(#name), #data_type)
                #field
                #nullable
                #primary_key
                #unique
                #readonly
                #encrypted
                #role
                #default
                #generated
                #max_length
                #numeric
                #enum_type
                #comment
                .build()
        }
    }

    /// One `RelationDescriptor`.
    fn relation_descriptor(&self, relation: &RelationModel) -> TokenStream {
        let private = private_path();
        let name = &relation.field;
        let kind = relation.kind.tokens();
        let target = &relation.target;
        let target_ty = &relation.target_ty;

        let target_table = (relation.kind != RelationKindModel::Polymorphic)
            .then(|| quote!(.target_table(<#target_ty as #private::Entity>::TABLE)));
        let foreign_key = relation
            .foreign_key
            .as_ref()
            .map(|key| quote!(.foreign_key(#private::Ident::from_static(#key))));
        let through = relation.through.as_ref().map(|table| {
            let left = relation.left.clone().unwrap_or_default();
            let right = relation.right.clone().unwrap_or_default();
            quote! {
                .through(#private::JoinTableDescriptor::new(
                    #private::TableRef::from_static(#table),
                    #private::Ident::from_static(#left),
                    #private::Ident::from_static(#right),
                ))
            }
        });
        let on_delete = relation.on_delete.as_ref().map(|action| {
            let action = referential_tokens(action);
            quote!(.on_delete(#action))
        });
        let on_update = relation.on_update.as_ref().map(|action| {
            let action = referential_tokens(action);
            quote!(.on_update(#action))
        });
        let nullable = relation.nullable.then(|| quote!(.nullable()));
        let self_ref = relation.self_ref.then(|| quote!(.self_referential()));
        let polymorphic = relation.polymorphic.as_ref().map(|polymorphic| {
            let type_column = &polymorphic.type_column;
            let id_column = &polymorphic.id_column;
            let targets = polymorphic
                .targets
                .iter()
                .map(|target| quote!(<#target as #private::Entity>::NAME));
            quote! {
                .polymorphic(#private::PolymorphicDescriptor::new(
                    #private::Ident::from_static(#type_column),
                    #private::Ident::from_static(#id_column),
                    [#(#targets),*],
                ))
            }
        });

        quote! {
            __builder = __builder.relation(
                #private::RelationDescriptor::builder(#name, #kind, #target)
                    #target_table
                    #foreign_key
                    #through
                    #on_delete
                    #on_update
                    #nullable
                    #self_ref
                    #polymorphic
                    .build(),
            );
        }
    }

    /// The setter `#[derive(Entity)]` supplies with `.linking(..)`.
    ///
    /// One relation constant serves both `.with(..)` and `.with_count(..)`, so
    /// the body branches on [`LoadedRows::is_count`]. When the entity has no
    /// `#[entity(count_of = "…")]` field for this relation the count branch is
    /// left out entirely, and `into_rows` reports the mismatch itself with a
    /// message that names both preloads.
    fn link_fn(&self, relation: &RelationModel) -> TokenStream {
        let private = private_path();
        let ident = &self.ident;
        let field = format_ident!("{}", relation.field);
        let target = &relation.target_ty;

        let load = match relation.kind {
            RelationKindModel::HasMany | RelationKindModel::ManyToMany => quote! {
                #private::Related::Loaded(__rows.into_rows::<#target>()?)
            },
            RelationKindModel::HasOne => quote! {
                #private::Related::Loaded(__rows.into_row::<#target>()?)
            },
            RelationKindModel::BelongsTo if relation.nullable => quote! {
                #private::Related::Loaded(__rows.into_row::<#target>()?)
            },
            RelationKindModel::BelongsTo | RelationKindModel::Polymorphic => quote! {
                #private::Related::Loaded(__rows.into_required_row::<#target>()?)
            },
        };

        let counted = self
            .counts
            .iter()
            .find(|count| count.relation == relation.field)
            .map(|count| {
                let member = format_ident!("{}", count.field);
                quote! {
                    if #private::LoadedRows::is_count(&__rows) {
                        __entity.#member = ::core::option::Option::Some(__rows.into_count()?);
                        return ::core::result::Result::Ok(());
                    }
                }
            });

        quote! {
            |__entity: &mut #ident, __rows: #private::LoadedRows| {
                #counted
                __entity.#field = #load;
                ::core::result::Result::Ok(())
            }
        }
    }

    /// One relation constant, with the setter and — for a `belongs_to` — the
    /// reader that lets the preloader batch on the foreign key.
    fn relation_constant(&self, relation: &RelationModel) -> TokenStream {
        let private = private_path();
        let ident = &self.ident;
        let vis = &self.vis;
        let name = format_ident!("{}", relation.const_name());
        let field = &relation.field;
        let target = &relation.target_ty;
        let doc = relation.doc.clone().unwrap_or_else(|| {
            format!("The `{}` relation of `{}`.", relation.field, self.type_name)
        });
        let link = self.link_fn(relation);
        let self_ref = relation.self_ref.then(|| quote!(.self_referential()));

        if relation.kind == RelationKindModel::Polymorphic {
            return self.polymorphic_constant(relation);
        }

        let (kind, arguments) = match relation.kind {
            RelationKindModel::HasMany => (quote!(HasMany), None),
            RelationKindModel::HasOne => (quote!(HasOne), None),
            RelationKindModel::ManyToMany => {
                let through = relation.through.clone().unwrap_or_default();
                let left = relation.left.clone().unwrap_or_default();
                let right = relation.right.clone().unwrap_or_default();
                (
                    quote!(ManyToMany),
                    Some(quote!(#field, #through, #left, #right)),
                )
            }
            RelationKindModel::BelongsTo | RelationKindModel::Polymorphic => {
                (quote!(BelongsTo), None)
            }
        };
        let key = relation.foreign_key.clone().unwrap_or_default();
        let arguments = arguments.unwrap_or_else(|| quote!(#field, #key));

        // Only a `belongs_to` needs a reader: every other kind batches on the
        // parent's primary key, which the preloader already has.
        let keyed_by = (relation.kind == RelationKindModel::BelongsTo)
            .then(|| {
                let column = format_ident!("{}", key);
                let key_ty = relation.key_type.clone()?;
                Some(quote! {
                    .keyed_by(|__entity: &#ident| {
                        <#key_ty as #private::SqlType>::to_value(&__entity.#column)
                    })
                })
            })
            .flatten();

        // `ManyToMany` has no `self_referential`, because a join table is
        // symmetric and there is nothing for the flag to change.
        let self_ref = (relation.kind != RelationKindModel::ManyToMany)
            .then_some(self_ref)
            .flatten();

        quote! {
            #[doc = #doc]
            #vis const #name: #private::#kind<#ident, #target> =
                #private::#kind::new(#arguments)
                    #keyed_by
                    #self_ref
                    .linking(#link);
        }
    }

    /// A `belongs_to_any` constant, its variant table, and the reader that
    /// tells the loader which target each row points at.
    fn polymorphic_constant(&self, relation: &RelationModel) -> TokenStream {
        let private = private_path();
        let ident = &self.ident;
        let vis = &self.vis;
        let name = format_ident!("{}", relation.const_name());
        let key_name = format_ident!("{}_KEY", relation.const_name());
        let variants_name = format_ident!("__MOSO_{}_VARIANTS", relation.const_name());
        let field_name = &relation.field;
        let field = format_ident!("{}", relation.field);
        let reference = format_ident!("{}", relation.reference_enum_name(&self.type_name));
        let Some(polymorphic) = &relation.polymorphic else {
            return TokenStream::new();
        };
        let type_column = &polymorphic.type_column;
        let id_column = &polymorphic.id_column;
        let type_member = format_ident!("{}", type_column);
        let id_member = format_ident!("{}", id_column);
        let key_ty = relation
            .key_type
            .clone()
            .unwrap_or_else(|| syn::parse_quote!(i64));

        let variants = polymorphic.targets.iter().map(|target| {
            let variant = format_ident!("{}", type_name_of(target).unwrap_or_default());
            let discriminant = snake(&type_name_of(target).unwrap_or_default());
            quote! {
                #private::PolymorphicVariant::to::<#target>(
                    #discriminant,
                    |__entity: &mut #ident, __rows: #private::LoadedRows| {
                        __entity.#field = #private::Related::Loaded(
                            #reference::#variant(__rows.into_required_row::<#target>()?),
                        );
                        ::core::result::Result::Ok(())
                    },
                )
            }
        });

        let doc = relation.doc.clone().unwrap_or_else(|| {
            format!("The `{}` relation of `{}`.", relation.field, self.type_name)
        });
        let key_doc = format!(
            "Reads the discriminator and the key of `{}::{}` out of a row.\n\n\
             `{}::load_all` takes it, because a polymorphic relation batches per target type.",
            self.type_name,
            relation.field,
            relation.const_name(),
        );
        let variants_doc = format!(
            "Every target `{}::{}` can point at, with the setter for each.",
            self.type_name, relation.field,
        );

        quote! {
            #[doc = #variants_doc]
            #[doc(hidden)]
            const #variants_name: &'static [#private::PolymorphicVariant<#ident>] =
                &[#(#variants),*];

            #[doc = #doc]
            #vis const #name: #private::BelongsToAny<#ident> = #private::BelongsToAny::new(
                #field_name,
                #type_column,
                #id_column,
                Self::#variants_name,
            );

            #[doc = #key_doc]
            #vis const #key_name: #private::PolymorphicKeyFn<#ident> = |__entity: &#ident| {
                (
                    #private::Value::text(::core::clone::Clone::clone(&__entity.#type_member)),
                    <#key_ty as #private::SqlType>::to_value(&__entity.#id_member),
                )
            };
        }
    }

    /// The `impl User { … }` block: the constants and the query entry points.
    fn inherent_impl(&self) -> TokenStream {
        let private = private_path();
        let ident = &self.ident;
        let vis = &self.vis;
        let new_ident = format_ident!("{}", self.new_struct);
        let scope = if self.is_tenant_scoped() {
            quote!(#private::NeedsTenant)
        } else {
            quote!(())
        };

        let column_constants = self.columns.iter().map(|column| {
            let name = format_ident!("{}", column.const_name());
            let sql_ty = column.sql_type();
            let sql_name = &column.column;
            let doc = column
                .comment
                .clone()
                .unwrap_or_else(|| format!("The `{}` column of `{}`.", column.column, self.table));
            quote! {
                #[doc = #doc]
                #vis const #name: #private::Column<#ident, #sql_ty> =
                    #private::Column::new(#sql_name);
            }
        });

        let relation_constants = self
            .relations
            .iter()
            .map(|relation| self.relation_constant(relation));

        let accessors = self.relations.iter().map(|relation| {
            let method = format_ident!("{}", relation.field);
            let field = format_ident!("{}", relation.field);
            let payload = &relation.payload;
            let entity = &self.type_name;
            let field_name = &relation.field;
            let constant = format!("{}::{}", self.type_name, relation.const_name());
            let doc = format!(
                "The loaded `{}`, or the error that names how to load it.\n\n\
                 Never queries (non-negotiable N2): a relation that was not preloaded is a \
                 mistake in the query, not a reason to issue another statement.\n\n\
                 # Errors\n\n\
                 [`NotLoaded`](::moso::db::NotLoaded) when the query that produced this row did \
                 not include `.with({constant})`.",
                relation.field,
            );
            quote! {
                #[doc = #doc]
                #vis fn #method(&self) -> ::core::result::Result<&#payload, #private::NotLoaded> {
                    match &self.#field {
                        #private::Related::Loaded(__value) => ::core::result::Result::Ok(__value),
                        #private::Related::NotLoaded => ::core::result::Result::Err(
                            #private::NotLoaded::of(#entity, #field_name, #constant),
                        ),
                    }
                }
            }
        });

        let count_accessors = self.counts.iter().map(|count| {
            let method = format_ident!("{}", count.field);
            let field = format_ident!("{}", count.field);
            let entity = &self.type_name;
            let relation = &count.relation;
            let constant = format!("{}::{}", self.type_name, column_const_name(&count.relation));
            let doc = format!(
                "How many `{relation}` rows there are, from `.with_count({constant})`.\n\n\
                 No rows are fetched to answer it — the count comes from a scalar subquery.\n\n\
                 # Errors\n\n\
                 [`NotLoaded`](::moso::db::NotLoaded) when the query did not ask for the count."
            );
            quote! {
                #[doc = #doc]
                #vis fn #method(&self) -> ::core::result::Result<i64, #private::NotLoaded> {
                    self.#field.ok_or(#private::NotLoaded::of(#entity, #relation, #constant))
                }
            }
        });

        let name = &self.type_name;
        let query_doc = format!("Every `{name}` row, unfiltered.");
        let find_doc = format!("The `{name}` with this key.");
        let insert_doc = format!("An `INSERT` of one `{name}`.");
        let insert_many_doc = format!("An `INSERT` of many `{name}` rows, in one statement.");
        let update_doc = "An `UPDATE` of this row, keyed by its primary key.";
        let update_all_doc = format!(
            "An `UPDATE` of many `{name}` rows.\n\n\
             Refuses to run without a `.filter(..)` or an explicit `.all_rows()`."
        );
        let delete_doc = "A `DELETE` of this row, keyed by its primary key.";
        let delete_all_doc = format!(
            "A `DELETE` of many `{name}` rows.\n\n\
             Refuses to run without a `.filter(..)` or an explicit `.all_rows()`."
        );

        quote! {
            #[automatically_derived]
            impl #ident {
                #(#column_constants)*
                #(#relation_constants)*
                #(#accessors)*
                #(#count_accessors)*

                #[doc = #query_doc]
                #[must_use]
                #vis fn query() -> #private::Select<Self, #scope> {
                    #private::Select::new()
                }

                #[doc = #find_doc]
                #[must_use]
                #vis fn find(
                    key: <Self as #private::Entity>::Pk,
                ) -> #private::Select<Self, #scope> {
                    #private::Select::find(key)
                }

                #[doc = #insert_doc]
                #[must_use]
                #vis fn insert(row: #new_ident) -> #private::Insert<Self> {
                    #private::Insert::row(row)
                }

                #[doc = #insert_many_doc]
                #[must_use]
                #vis fn insert_many(
                    rows: impl ::core::iter::IntoIterator<Item = #new_ident>,
                ) -> #private::Insert<Self> {
                    #private::Insert::rows(rows)
                }

                #[doc = #update_doc]
                #[must_use]
                #vis fn update(&self) -> #private::Update<Self> {
                    #private::Update::by_key(#private::Entity::pk(self))
                }

                #[doc = #update_all_doc]
                #[must_use]
                #vis fn update_all() -> #private::Update<Self> {
                    #private::Update::all()
                }

                #[doc = #delete_doc]
                #[must_use]
                #vis fn delete(&self) -> #private::Delete<Self> {
                    #private::Delete::by_key(#private::Entity::pk(self))
                }

                #[doc = #delete_all_doc]
                #[must_use]
                #vis fn delete_all() -> #private::Delete<Self> {
                    #private::Delete::all()
                }
            }
        }
    }

    /// The `New…` struct and its `NewEntity` impl.
    fn new_struct(&self) -> TokenStream {
        let private = private_path();
        let vis = &self.vis;
        let new_ident = format_ident!("{}", self.new_struct);
        let insertable = self.insertable();
        let derives = &self.new_derives;
        let derive = (!derives.is_empty()).then(|| quote!(#[derive(#(#derives),*)]));

        let fields = insertable.iter().map(|column| {
            let field = format_ident!(
                "{}",
                if column.field.is_empty() {
                    column.column.clone()
                } else {
                    column.field.clone()
                }
            );
            let ty = self.new_field_type(column);
            let doc = column.comment.clone().unwrap_or_else(|| {
                if column.default.is_some() {
                    format!(
                        "`{}`. `None` leaves it to the database's default.",
                        column.column
                    )
                } else {
                    format!("`{}`.", column.column)
                }
            });
            quote! {
                #[doc = #doc]
                #vis #field: #ty,
            }
        });
        let embedded_fields = self.embeds.iter().map(|embed| {
            let field = format_ident!("{}", embed.field);
            let ty = &embed.ty;
            let doc = embed
                .doc
                .clone()
                .unwrap_or_else(|| format!("The embedded `{}`.", embed.field));
            quote! {
                #[doc = #doc]
                #vis #field: #ty,
            }
        });

        let own_names: Vec<&str> = insertable
            .iter()
            .map(|column| column.column.as_str())
            .collect();
        let columns_const = if self.embeds.is_empty() {
            quote!(&[#(#own_names),*])
        } else {
            let embeds = self.embeds.iter().map(|embed| {
                let ty = &embed.ty;
                quote!(<#ty>::MOSO_COLUMN_NAMES)
            });
            quote! {
                {
                    const __MOSO_PARTS: &[&[&'static str]] = &[&[#(#own_names),*], #(#embeds),*];
                    const __MOSO_ALL: [&'static str; #private::total_names(__MOSO_PARTS)] =
                        #private::concat_names(__MOSO_PARTS);
                    &__MOSO_ALL
                }
            }
        };

        let values = insertable.iter().map(|column| {
            let field = format_ident!(
                "{}",
                if column.field.is_empty() {
                    column.column.clone()
                } else {
                    column.field.clone()
                }
            );
            if column.default.is_some() {
                // `None` means "let the database's default apply", which is
                // `DEFAULT` in the value list — not a bound `NULL`, which would
                // defeat the default it was written to use.
                let bare = column.bare_type();
                let (bind_ty, wrapped): (Type, TokenStream) = if column.json {
                    (
                        syn::parse_quote!(#private::SqlJson<#bare>),
                        quote!(#private::SqlJson::new(__value)),
                    )
                } else {
                    (bare, quote!(__value))
                };
                return quote! {
                    __row.push(match self.#field {
                        ::core::option::Option::Some(__value) => #private::Expr::bound(
                            <#bind_ty as #private::SqlType>::into_value(#wrapped),
                        ),
                        ::core::option::Option::None => #private::Expr::Default,
                    });
                };
            }
            let sql_ty = column.sql_type();
            let taken = if column.json {
                if column.nullable {
                    quote!(self.#field.map(#private::SqlJson::new))
                } else {
                    quote!(#private::SqlJson::new(self.#field))
                }
            } else {
                quote!(self.#field)
            };
            quote! {
                __row.push(#private::Expr::bound(
                    <#sql_ty as #private::SqlType>::into_value(#taken),
                ));
            }
        });
        let embedded_values = self.embeds.iter().map(|embed| {
            let field = format_ident!("{}", embed.field);
            quote!(__row.extend(self.#field.moso_into_values());)
        });

        let entity = &self.type_name;
        let doc = format!(
            "What has to be supplied to create a `{entity}`.\n\n\
             The entity's columns **minus** the ones the database supplies: a primary key with a \
             default, `created_at`, `updated_at`, the soft-delete flag, the version counter, the \
             generated columns and the relations. A column with a database default is an \
             `Option`, and `None` means \"let the default apply\"."
        );

        quote! {
            #[doc = #doc]
            #derive
            #vis struct #new_ident {
                #(#fields)*
                #(#embedded_fields)*
            }

            #[automatically_derived]
            impl #private::NewEntity for #new_ident {
                const COLUMNS: &'static [&'static str] = #columns_const;

                fn into_row(self) -> ::std::vec::Vec<#private::Expr> {
                    let mut __row = ::std::vec::Vec::with_capacity(
                        <Self as #private::NewEntity>::COLUMNS.len(),
                    );
                    #(#values)*
                    #(#embedded_values)*
                    __row
                }
            }
        }
    }

    /// The type a `New…` field takes.
    ///
    /// The declared type, except that a column the database can fill in is an
    /// `Option` whose `None` means "let the default apply". A nullable column
    /// that *also* has a default therefore cannot be set to `NULL` through the
    /// `New…` struct; `Update::set_null` is how that is said.
    fn new_field_type(&self, column: &ColumnModel) -> Type {
        if column.default.is_some() {
            let bare = column.bare_type();
            return syn::parse_quote!(::core::option::Option<#bare>);
        }
        column.ty.clone()
    }

    /// One `{Entity}{Field}Ref` enum per polymorphic relation.
    ///
    /// The variants hold the **loaded entity**, not its key, because that is
    /// what the field is for: `comment.target()?` gives back a `&Post` or a
    /// `&Tag` with the compiler checking which. The key lives in the two
    /// declared columns.
    fn polymorphic_enums(&self) -> TokenStream {
        let private = private_path();
        let vis = &self.vis;
        let enums = self.relations.iter().filter_map(|relation| {
            let polymorphic = relation.polymorphic.as_ref()?;
            let ident = format_ident!("{}", relation.reference_enum_name(&self.type_name));
            let variants = polymorphic.targets.iter().map(|target| {
                let name = format_ident!("{}", type_name_of(target).unwrap_or_default());
                let doc = format!("The relation points at a `{name}`.");
                quote! {
                    #[doc = #doc]
                    #name(#target),
                }
            });
            let names = polymorphic.targets.iter().map(|target| {
                let name = format_ident!("{}", type_name_of(target).unwrap_or_default());
                quote!(Self::#name(_) => <#target as #private::Entity>::NAME,)
            });
            let discriminants = polymorphic.targets.iter().map(|target| {
                let name = format_ident!("{}", type_name_of(target).unwrap_or_default());
                let discriminant = snake(&type_name_of(target).unwrap_or_default());
                quote!(Self::#name(_) => #discriminant,)
            });
            let keys = polymorphic.targets.iter().map(|target| {
                let name = format_ident!("{}", type_name_of(target).unwrap_or_default());
                quote! {
                    Self::#name(__row) => <
                        <#target as #private::Entity>::Pk as #private::SqlType
                    >::into_value(#private::Entity::pk(__row)),
                }
            });
            let doc = format!(
                "What `{}::{}` points at.\n\n\
                 A polymorphic relation stores a discriminator and a key; this enum is the loaded \
                 row, with the compiler checking which entity it came from instead of a `match` \
                 on a string.",
                self.type_name, relation.field,
            );
            Some(quote! {
                #[doc = #doc]
                #[derive(::core::clone::Clone, ::core::fmt::Debug)]
                #vis enum #ident {
                    #(#variants)*
                }

                #[automatically_derived]
                impl #ident {
                    /// The target entity's Rust type name.
                    #[must_use]
                    #vis fn type_name(&self) -> &'static str {
                        match self { #(#names)* }
                    }

                    /// The value stored in the discriminator column.
                    #[must_use]
                    #vis fn discriminant(&self) -> &'static str {
                        match self { #(#discriminants)* }
                    }

                    /// The value stored in the key column.
                    #[must_use]
                    #vis fn key(&self) -> #private::Value {
                        match self { #(#keys)* }
                    }
                }
            })
        });
        quote!(#(#enums)*)
    }
}

/// Expands `#[derive(Entity)]`.
///
/// The model is parsed first, so an attribute mistake is reported against the
/// user's own span and nothing else is generated — one mistake, one error
/// (`docs/04-devex/41-diagnostics.md`, rule 4).
pub fn expand(input: TokenStream) -> TokenStream {
    let input: DeriveInput = match syn::parse2(input) {
        Ok(parsed) => parsed,
        Err(error) => return error.to_compile_error(),
    };
    match EntityModel::parse(&input) {
        Ok(model) => model.generate(),
        Err(error) => error.to_compile_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> syn::Result<EntityModel> {
        let input: DeriveInput = syn::parse_str(source).expect("the test source parses");
        EntityModel::parse(&input)
    }

    fn expand_str(source: &str) -> String {
        let input: proc_macro2::TokenStream = source.parse().expect("the test source lexes");
        let tokens = expand(input);
        if !tokens.to_string().contains("compile_error") {
            crate::shared::parses_as_rust(&tokens)
                .unwrap_or_else(|error| panic!("the expansion is not valid Rust: {error}"));
        }
        tokens.to_string()
    }

    const SIMPLE: &str = "struct User { #[entity(pk)] id: i64 }";

    #[test]
    fn the_table_name_defaults_to_the_pluralised_type_name() {
        let model = parse(SIMPLE).expect("a simple entity");
        assert_eq!(model.table, "users");
        assert_eq!(model.new_struct, "NewUser");
        assert_eq!(model.columns.len(), 1);
    }

    #[test]
    fn the_table_name_can_be_overridden() {
        let model = parse("#[entity(table = \"people\")] struct Person { #[entity(pk)] id: i64 }")
            .expect("an override");
        assert_eq!(model.table, "people");
    }

    #[test]
    fn the_container_settings_are_recorded() {
        let model = parse(
            "#[entity(schema = \"billing\", soft_delete = \"deleted_at\", timestamps, \
             tenant = \"tenant_id\", versioned = \"version\", expose, audit, rls)]
             struct Invoice {
                 #[entity(pk)] id: i64,
                 deleted_at: Option<i64>,
                 tenant_id: i64,
                 version: i64,
                 created_at: i64,
                 updated_at: i64,
             }",
        )
        .expect("every setting");

        assert_eq!(model.schema.as_deref(), Some("billing"));
        assert_eq!(model.soft_delete.as_deref(), Some("deleted_at"));
        assert_eq!(model.tenant.as_deref(), Some("tenant_id"));
        assert_eq!(model.versioned.as_deref(), Some("version"));
        assert!(model.timestamps && model.expose && model.audit && model.rls);
        assert!(model.is_tenant_scoped());
    }

    #[test]
    fn the_soft_delete_and_tenant_columns_get_their_roles() {
        let model = parse(
            "#[entity(soft_delete = \"deleted_at\", tenant = \"tenant_id\")]
             struct Invoice {
                 #[entity(pk)] id: i64,
                 deleted_at: Option<i64>,
                 tenant_id: i64,
             }",
        )
        .expect("roles");

        let role_of = |name: &str| {
            model
                .columns
                .iter()
                .find(|column| column.column == name)
                .map(|column| column.role)
        };
        assert_eq!(role_of("deleted_at"), Some(ColumnRoleModel::SoftDelete));
        assert_eq!(role_of("tenant_id"), Some(ColumnRoleModel::Tenant));
        assert_eq!(role_of("id"), Some(ColumnRoleModel::Data));
    }

    #[test]
    fn a_related_field_is_a_relation_not_a_column() {
        let model = parse("struct Post { #[entity(pk)] id: i64, comments: Related<Vec<Comment>> }")
            .expect("a relation");
        assert_eq!(model.columns.len(), 1, "`comments` is not a column");
        assert_eq!(model.relations.len(), 1);
        assert_eq!(model.relations[0].field, "comments");
        assert_eq!(model.relations[0].kind, RelationKindModel::HasMany);
        assert_eq!(model.relations[0].target, "Comment");
        assert_eq!(model.relations[0].foreign_key.as_deref(), Some("post_id"));
    }

    #[test]
    fn a_belongs_to_binds_to_its_declared_foreign_key_column() {
        let model = parse(
            "struct Post {
                 #[entity(pk)] id: i64,
                 author_id: i64,
                 #[entity(belongs_to = User, fk = \"author_id\")] author: Related<User>,
             }",
        )
        .expect("a declared key");
        let keys: Vec<&ColumnModel> = model
            .columns
            .iter()
            .filter(|column| column.column == "author_id")
            .collect();
        assert_eq!(keys.len(), 1, "no duplicate column");
        assert_eq!(keys[0].role, ColumnRoleModel::ForeignKey);
        assert_eq!(keys[0].const_name(), "AUTHOR_ID");
        assert!(model.relations[0].key_type.is_some());
    }

    #[test]
    fn a_belongs_to_whose_key_is_not_a_field_is_refused() {
        // The preloader groups the parents by the foreign key it reads out of
        // each row. With no field to read, its fallback is the parent's own
        // primary key — which returns the wrong rows, silently.
        let error = parse(
            "struct Post {
                 #[entity(pk)] id: i64,
                 #[entity(belongs_to = User, fk = \"author_id\")] author: Related<User>,
             }",
        )
        .expect_err("no `author_id` field");
        let text = error.to_string();
        assert!(
            text.contains("its key column `author_id` is not a field"),
            "{text}"
        );
        assert!(text.contains("declare it"), "{text}");
    }

    #[test]
    fn a_nullable_relation_needs_a_nullable_key() {
        let error = parse(
            "struct Post {
                 #[entity(pk)] id: i64,
                 author_id: i64,
                 #[entity(belongs_to = User, fk = \"author_id\")] author: Related<Option<User>>,
             }",
        )
        .expect_err("the key is not nullable");
        assert!(error.to_string().contains("make them agree"));
    }

    #[test]
    fn the_many_to_many_defaults_are_the_conventional_names() {
        let model = parse(
            "struct Post {
                 #[entity(pk)] id: i64,
                 #[entity(many_to_many = Tag)] tags: Related<Vec<Tag>>,
             }",
        )
        .expect("a many-to-many");
        let relation = &model.relations[0];
        assert_eq!(relation.through.as_deref(), Some("post_tags"));
        assert_eq!(relation.left.as_deref(), Some("post_id"));
        assert_eq!(relation.right.as_deref(), Some("tag_id"));
    }

    #[test]
    fn an_unknown_setting_gets_one_error_with_a_suggestion() {
        let error = parse("#[entity(tabel = \"users\")] struct User { #[entity(pk)] id: i64 }")
            .expect_err("`tabel` is not a setting");
        let text = error.to_string();
        assert!(text.contains("`tabel` is not"), "{text}");
        assert!(text.contains("did you mean `table`?"), "{text}");
        assert_eq!(text.matches("help:").count(), 1, "{text}");
    }

    #[test]
    fn a_nonsense_setting_lists_the_real_ones_instead_of_guessing() {
        let error = parse("#[entity(bananas = \"yes\")] struct User { #[entity(pk)] id: i64 }")
            .expect_err("`bananas` is not a setting");
        let text = error.to_string();
        assert!(text.contains("the settings are:"), "{text}");
        assert!(text.contains("soft_delete"), "{text}");
    }

    #[test]
    fn an_enum_is_refused_with_the_reason_and_the_fix() {
        let error = parse("enum Kind { A, B }").expect_err("an enum is not a table");
        let text = error.to_string();
        assert!(text.contains("named columns"), "{text}");
        assert!(text.contains("help:"), "{text}");
    }

    #[test]
    fn a_tuple_struct_is_refused_because_a_column_needs_a_name() {
        let error = parse("struct User(i64);").expect_err("a tuple struct has no column names");
        assert!(error.to_string().contains("named fields"));
    }

    #[test]
    fn an_entity_without_a_key_names_the_fix() {
        let error = parse("struct User { email: String }").expect_err("no key");
        let text = error.to_string();
        assert!(text.contains("no primary key"), "{text}");
        assert!(text.contains("#[entity(pk)]"), "{text}");
    }

    #[test]
    fn a_composite_key_is_refused_with_the_reason_rather_than_a_bound_failure() {
        let error =
            parse("struct PostTag { #[entity(pk)] post_id: i64, #[entity(pk)] tag_id: i64 }")
                .expect_err("two keys");
        let text = error.to_string();
        assert!(text.contains("2 fields"), "{text}");
        assert!(text.contains("single-column key"), "{text}");
    }

    #[test]
    fn the_insert_struct_omits_the_columns_the_database_supplies() {
        let model = parse(
            "#[entity(timestamps)]
             struct User {
                 #[entity(pk, default = \"uuid_generate_v7()\")] id: i64,
                 email: String,
                 #[entity(readonly)] search: String,
                 created_at: i64,
                 updated_at: i64,
             }",
        )
        .expect("an entity");

        let insertable: Vec<&str> = model
            .insertable()
            .iter()
            .map(|column| column.column.as_str())
            .collect();
        assert_eq!(insertable, ["email"]);
    }

    #[test]
    fn a_nullable_soft_delete_column_is_required() {
        let error = parse(
            "#[entity(soft_delete = \"deleted_at\")]
             struct User { #[entity(pk)] id: i64, deleted_at: i64 }",
        )
        .expect_err("not nullable");
        assert!(error.to_string().contains("nullable"));
    }

    #[test]
    fn the_expansion_implements_the_trait_and_names_only_the_private_path() {
        let out = expand_str(
            "#[entity(table = \"users\")]
             struct User { #[entity(pk)] id: i64, email: String }",
        );
        assert!(
            out.contains("impl :: moso :: __private :: Entity for User"),
            "{out}"
        );
        assert!(out.contains("const TABLE"), "{out}");
        assert!(out.contains("from_static (\"users\")"), "{out}");
        assert!(out.contains("fn from_row"), "{out}");
        assert!(out.contains("fn descriptor"), "{out}");
        assert!(
            !out.contains("moso_orm"),
            "D6: never a runtime crate: {out}"
        );
        assert!(!out.contains("moso_sql"), "{out}");
        assert!(!out.contains("compile_error"), "{out}");
    }

    #[test]
    fn from_row_decodes_positionally_with_literal_indices() {
        let out = expand_str("struct User { #[entity(pk)] id: i64, email: String }");
        assert!(out.contains("decode (__row , 0usize)"), "{out}");
        assert!(out.contains("decode (__row , 1usize)"), "{out}");
        assert!(!out.contains("column_name"), "no name lookup: {out}");
    }

    #[test]
    fn a_decode_failure_names_the_entity_and_the_field() {
        let out = expand_str("struct User { #[entity(pk)] id: i64, email: String }");
        assert!(
            out.contains("in_entity (\"User\") . in_field (\"email\")"),
            "{out}"
        );
    }

    #[test]
    fn the_column_constants_carry_the_entity_and_the_rust_type() {
        let out = expand_str("struct User { #[entity(pk)] id: i64, email: String }");
        assert!(
            out.contains("const EMAIL : :: moso :: __private :: Column < User , String >"),
            "{out}"
        );
        assert!(out.contains("Column :: new (\"email\")"), "{out}");
    }

    #[test]
    fn a_json_column_goes_through_the_json_wrapper_on_both_sides() {
        let out =
            expand_str("struct User { #[entity(pk)] id: i64, #[entity(json)] prefs: Preferences }");
        assert!(out.contains("Json < Preferences >"), "{out}");
        assert!(out.contains(". into_inner ()"), "{out}");
        assert!(out.contains("Json :: new (self . prefs)"), "{out}");
    }

    #[test]
    fn a_defaulted_column_is_optional_and_binds_the_default_keyword() {
        let out = expand_str(
            "struct User { #[entity(pk)] id: i64, #[entity(default = \"false\")] admin: bool }",
        );
        assert!(out.contains("Option < bool >"), "{out}");
        assert!(out.contains("Expr :: Default"), "{out}");
    }

    #[test]
    fn the_relation_constants_and_accessors_are_generated() {
        let out = expand_str(
            "struct Post {
                 #[entity(pk)] id: i64,
                 #[entity(has_many = Comment, fk = \"post_id\")] comments: Related<Vec<Comment>>,
             }",
        );
        assert!(
            out.contains("const COMMENTS : :: moso :: __private :: HasMany < Post , Comment >"),
            "{out}"
        );
        assert!(
            out.contains("HasMany :: new (\"comments\" , \"post_id\")"),
            "{out}"
        );
        assert!(
            out.contains("NotLoaded :: of (\"Post\" , \"comments\" , \"Post::COMMENTS\")"),
            "{out}"
        );
        assert!(out.contains("Related :: NotLoaded"), "{out}");
    }

    #[test]
    fn a_tenant_scoped_entity_starts_its_queries_owing_a_tenant() {
        let out = expand_str(
            "#[entity(tenant = \"tenant_id\")]
             struct Invoice { #[entity(pk)] id: i64, tenant_id: i64 }",
        );
        assert!(
            out.contains("Select < Self , :: moso :: __private :: NeedsTenant >"),
            "{out}"
        );
    }

    #[test]
    fn an_untenanted_entity_starts_ready() {
        let out = expand_str("struct User { #[entity(pk)] id: i64 }");
        assert!(out.contains("Select < Self , () >"), "{out}");
    }

    #[test]
    fn the_descriptor_carries_what_the_migration_differ_needs() {
        let out = expand_str(
            "#[entity(table = \"users\", timestamps, soft_delete = \"deleted_at\",
                      index(columns(\"email\"), unique, where = \"deleted_at is null\"),
                      check(name = \"email_shape\", expr = \"email like '%@%'\"))]
             struct User {
                 #[entity(pk, default = \"uuid_generate_v7()\")] id: i64,
                 #[entity(unique, len = 255)] email: String,
                 deleted_at: Option<i64>,
                 created_at: i64,
                 updated_at: i64,
             }",
        );
        assert!(
            out.contains("EntityDescriptor :: builder (\"User\""),
            "{out}"
        );
        assert!(
            out.contains("ColumnDefault :: sql (\"uuid_generate_v7()\")"),
            "{out}"
        );
        assert!(
            out.contains("IndexDescriptor :: builder (\"users_email_idx\")"),
            "{out}"
        );
        assert!(out.contains(". unique ()"), "{out}");
        assert!(
            out.contains("RawExpr :: new (\"deleted_at is null\")"),
            "{out}"
        );
        assert!(
            out.contains("CheckDescriptor :: new (\"email_shape\""),
            "{out}"
        );
        assert!(out.contains(". soft_delete (\"deleted_at\")"), "{out}");
        assert!(
            out.contains(". timestamps (\"created_at\" , \"updated_at\")"),
            "{out}"
        );
        assert!(out.contains("VarChar"), "{out}");
    }

    #[test]
    fn a_belongs_to_emits_a_real_foreign_key_constraint() {
        let out = expand_str(
            "struct Post {
                 #[entity(pk)] id: i64,
                 author_id: i64,
                 #[entity(belongs_to = User, fk = \"author_id\", on_delete = \"cascade\")]
                 author: Related<User>,
             }",
        );
        assert!(
            out.contains("ForeignKeyDescriptor :: builder (\"posts_author_id_fkey\""),
            "{out}"
        );
        assert!(out.contains("ReferentialAction :: Cascade"), "{out}");
        assert!(out.contains("primary_key_columns ()"), "{out}");
    }

    #[test]
    fn a_cascade_from_a_soft_deleted_parent_is_refused() {
        let error = parse(
            "#[entity(soft_delete = \"deleted_at\")]
             struct User {
                 #[entity(pk)] id: i64,
                 deleted_at: Option<i64>,
                 #[entity(has_many = Post, on_delete = \"cascade\")] posts: Related<Vec<Post>>,
             }",
        )
        .expect_err("a soft delete does not cascade");
        let text = error.to_string();
        assert!(text.contains("can never fire"), "{text}");
        assert!(text.contains("hard-delete"), "{text}");
    }

    #[test]
    fn a_polymorphic_relation_generates_the_reference_enum_and_the_reader() {
        let out = expand_str(
            "struct Comment {
                 #[entity(pk)] id: i64,
                 target_type: String,
                 target_id: i64,
                 #[entity(belongs_to_any(types(Post, Tag), type_column = \"target_type\",
                          id_column = \"target_id\"))]
                 target: Related<CommentTargetRef>,
             }",
        );
        assert!(out.contains("enum CommentTargetRef"), "{out}");
        assert!(out.contains("fn discriminant"), "{out}");
        assert!(out.contains("BelongsToAny :: new"), "{out}");
        assert!(
            out.contains("PolymorphicVariant :: to :: < Post >"),
            "{out}"
        );
        assert!(out.contains("const TARGET_KEY"), "{out}");
        assert!(out.contains("PolymorphicDescriptor :: new"), "{out}");
    }

    #[test]
    fn every_relation_constant_carries_the_setter_the_preloader_calls() {
        let out = expand_str(
            "struct Post {
                 #[entity(pk)] id: i64,
                 author_id: i64,
                 #[entity(belongs_to = User, fk = \"author_id\")] author: Related<User>,
                 #[entity(has_many = Comment)] comments: Related<Vec<Comment>>,
             }",
        );
        assert!(out.contains(". linking ("), "{out}");
        assert!(out.contains("into_required_row :: < User >"), "{out}");
        assert!(out.contains("into_rows :: < Comment >"), "{out}");
        // Only a `belongs_to` needs the reader; everything else batches on the
        // parent's primary key, which the preloader already has.
        assert_eq!(out.matches(". keyed_by (").count(), 1, "{out}");
        assert!(out.contains("__entity . author_id"), "{out}");
    }

    #[test]
    fn a_count_field_is_written_by_the_same_setter_and_read_by_an_accessor() {
        let out = expand_str(
            "struct Post {
                 #[entity(pk)] id: i64,
                 #[entity(has_many = Comment)] comments: Related<Vec<Comment>>,
                 #[entity(count_of = \"comments\")] comments_count: Option<i64>,
             }",
        );
        assert!(out.contains("LoadedRows :: is_count (& __rows)"), "{out}");
        assert!(
            out.contains("__entity . comments_count = :: core :: option :: Option :: Some"),
            "{out}"
        );
        assert!(out.contains("fn comments_count (& self)"), "{out}");
        // It is not a column, so it is neither selected nor inserted.
        assert!(
            !out.contains("ColumnDef :: new (\"comments_count\""),
            "{out}"
        );
    }

    #[test]
    fn a_count_field_that_names_no_relation_is_refused() {
        let error = parse(
            "struct Post { #[entity(pk)] id: i64, #[entity(count_of = \"tags\")] n: Option<i64> }",
        )
        .expect_err("`tags` is not a relation");
        assert!(error.to_string().contains("is not a relation of"));
    }

    #[test]
    fn an_embedded_value_object_splices_its_columns_in() {
        let out = expand_str(
            "struct Order {
                 #[entity(pk)] id: i64,
                 #[entity(embedded)] address: Address,
                 total: i64,
             }",
        );
        assert!(out.contains("MOSO_COLUMNS"), "{out}");
        assert!(out.contains("concat_columns"), "{out}");
        assert!(out.contains("total_columns"), "{out}");
        assert!(out.contains("moso_from_row"), "{out}");
        assert!(out.contains("moso_descriptors"), "{out}");
    }

    #[test]
    fn an_index_method_that_moso_does_not_know_still_works() {
        let out = expand_str(
            "#[entity(index(columns(\"embedding\"), method = \"hnsw\"))]
             struct Doc { #[entity(pk)] id: i64, embedding: Vec<u8> }",
        );
        assert!(out.contains("IndexMethod :: Custom"), "{out}");
        assert!(out.contains("\"hnsw\""), "{out}");
    }

    #[test]
    fn a_precision_that_keeps_more_decimals_than_digits_is_refused() {
        let error =
            parse("struct Item { #[entity(pk)] id: i64, #[entity(precision(2, 10))] price: i64 }")
                .expect_err("more scale than precision");
        assert!(error.to_string().contains("more decimals than digits"));
    }

    #[test]
    fn a_column_name_a_server_would_truncate_is_refused_at_expansion() {
        let long = "x".repeat(64);
        let error = parse(&format!(
            "struct User {{ #[entity(pk)] id: i64, #[entity(column = \"{long}\")] a: i64 }}"
        ))
        .expect_err("too long");
        assert!(error.to_string().contains("63"));
    }

    #[test]
    fn two_fields_that_write_the_same_column_are_refused() {
        let error = parse(
            "struct User {
                 #[entity(pk)] id: i64,
                 #[entity(column = \"email\")] a: String,
                 #[entity(column = \"email\")] b: String,
             }",
        )
        .expect_err("a duplicate column");
        assert!(error.to_string().contains("declared twice"));
    }

    #[test]
    fn a_relation_that_would_shadow_a_generated_method_is_refused() {
        let error = parse("struct User { #[entity(pk)] id: i64, query: Related<Vec<Post>> }")
            .expect_err("`query` is generated");
        assert!(error.to_string().contains("shadow"));
    }

    #[test]
    fn an_attribute_mistake_is_reported_instead_of_an_expansion() {
        let out = expand_str("#[entity(tabel = \"users\")] struct User { #[entity(pk)] id: i64 }");
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("did you mean"), "{out}");
        assert!(
            !out.contains("fn from_row"),
            "one error, not a fake impl: {out}"
        );
    }
}
