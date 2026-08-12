//! Building a [`Schema`] from the entity graph.
//!
//! This is non-negotiable N6 made concrete: `#[derive(Entity)]` produces an
//! [`EntityDescriptor`], and the desired schema is a pure function of the set
//! of descriptors. Nothing here touches a database.
//!
//! ```
//! use moso_migrate::Schema;
//! use moso_orm::descriptor::{ColumnDescriptor, EntityDescriptor};
//! use moso_sql::{DataType, Ident, TableRef};
//!
//! let user = EntityDescriptor::builder("User", TableRef::from_static("users"))
//!     .column(
//!         ColumnDescriptor::builder(Ident::from_static("id"), DataType::BigSerial)
//!             .primary_key()
//!             .build(),
//!     )
//!     .column(
//!         ColumnDescriptor::builder(Ident::from_static("email"), DataType::Text)
//!             .unique()
//!             .build(),
//!     )
//!     .build();
//!
//! let schema = Schema::from_entities([&user])?;
//! let users = schema.table("users").expect("one table per entity");
//! assert_eq!(users.primary_key(), ["id"]);
//! assert!(users.index("users_email_key").is_some(), "a unique column is a unique index");
//! # Ok::<(), moso_migrate::Error>(())
//! ```

use std::collections::BTreeMap;

use moso_orm::descriptor::{
    CheckDescriptor, ColumnDefault, ColumnDescriptor, EntityDescriptor, EnumTypeDescriptor,
    ForeignKeyDescriptor, IndexDescriptor, RelationDescriptor, RelationKind,
};
use moso_orm::{Backend, EnumStorage};
use moso_sql::{DataType, Ident, TableRef};

use crate::emit;
use crate::error::{Error, Result};
use crate::schema::{
    Action, Check, Column, EnumType, ForeignKey, Generated, Index, IndexPart, NullsOrder, Schema,
    Sort, Table, qualify,
};

/// Two entities that cannot both be true.
///
/// Building a schema is the first place a mistake in an entity graph becomes
/// visible — the derive sees one entity at a time and cannot know that another
/// one claims the same table.
///
/// ```
/// use moso_migrate::schema::EntityGraphError;
///
/// let clash = EntityGraphError::DuplicateTable {
///     table: "users".to_owned(),
///     first: "User".to_owned(),
///     second: "Account".to_owned(),
/// };
/// assert!(clash.to_string().contains("help:"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EntityGraphError {
    /// Two entities map to the same table.
    #[error(
        "`{first}` and `{second}` both map to the table `{table}`\n\
         help: give one of them its own table with `#[entity(table = \"...\")]`\n\
         help: if they are deliberately two views of one table, only one of them may own the \
         schema — mark the other `#[entity(managed = false)]`"
    )]
    DuplicateTable {
        /// The table both claim.
        table: String,
        /// The first entity.
        first: String,
        /// The second entity.
        second: String,
    },

    /// A relation points at an entity that is not in the graph.
    #[error(
        "`{entity}`'s relation `{relation}` targets `{target}`, which is not in the entity list\n\
         help: add `{target}` to the entities passed to `Schema::from_entities`; \
         `moso db make-migration` collects them from the `entities!` list in your composition root"
    )]
    UnknownTarget {
        /// The entity declaring the relation.
        entity: String,
        /// The relation's name.
        relation: String,
        /// The target entity's name.
        target: String,
    },

    /// Two enum types share a name with different variants.
    #[error(
        "the enum type `{name}` is declared twice with different variants:\n  \
         {first}\n  {second}\n\
         help: two Rust enums that map to one PostgreSQL type must have the same variants in the \
         same order; rename one with `#[db_enum(type_name = \"...\")]`"
    )]
    ConflictingEnum {
        /// The type name.
        name: String,
        /// The first variant list.
        first: String,
        /// The second variant list.
        second: String,
    },
}

impl Schema {
    /// Builds the desired schema from a set of entity descriptors.
    ///
    /// Everything the migration generator knows comes from here: tables,
    /// columns, primary keys, unique constraints (as unique indexes), other
    /// indexes, check constraints, foreign keys, enum types, and the join
    /// tables that many-to-many relations imply.
    ///
    /// # Errors
    ///
    /// [`Error::Snapshot`] wrapping an [`EntityGraphError`] when the graph is
    /// inconsistent: two entities on one table, a relation with no target, two
    /// spellings of one enum type.
    ///
    /// ```
    /// use moso_migrate::Schema;
    ///
    /// assert!(Schema::from_entities([])?.is_empty());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn from_entities<'a>(
        entities: impl IntoIterator<Item = &'a EntityDescriptor>,
    ) -> Result<Self> {
        let descriptors: Vec<&EntityDescriptor> = entities.into_iter().collect();
        let mut schema = Self::empty();
        let mut by_entity: BTreeMap<&str, &EntityDescriptor> = BTreeMap::new();
        let mut table_owners: BTreeMap<String, &str> = BTreeMap::new();

        for descriptor in &descriptors {
            by_entity.insert(descriptor.entity(), descriptor);
            let qualified = qualified_table(descriptor.table());
            if let Some(first) = table_owners.insert(qualified.clone(), descriptor.entity()) {
                return Err(graph_error(EntityGraphError::DuplicateTable {
                    table: qualified,
                    first: first.to_owned(),
                    second: descriptor.entity().to_owned(),
                }));
            }
        }

        for descriptor in &descriptors {
            for enum_type in descriptor.enum_types() {
                add_enum(&mut schema, enum_type)?;
            }
            schema.add_table(build_table(descriptor)?);
        }

        // Join tables come last: they reference two tables that must exist
        // first, and two entities can name the same join table (both sides of
        // one many-to-many), so the second declaration must not duplicate it.
        for descriptor in &descriptors {
            for relation in descriptor.relations() {
                if let Some(join) = join_table(descriptor, relation, &by_entity)?
                    && schema.table(&join.qualified_name()).is_none()
                {
                    schema.add_table(join);
                }
            }
        }

        Ok(schema)
    }
}

fn graph_error(error: EntityGraphError) -> Error {
    Error::Snapshot {
        path: "entity graph".into(),
        reason: error.to_string(),
    }
}

fn qualified_table(table: &TableRef) -> String {
    qualify(table.schema().map(Ident::as_str), table.name().as_str())
}

fn add_enum(schema: &mut Schema, descriptor: &EnumTypeDescriptor) -> Result<()> {
    if !descriptor.needs_a_type() {
        // A `text` or `int` enum needs no database type at all, which is why it
        // is the default: adding a variant is then a code change, not a
        // migration.
        debug_assert!(matches!(
            descriptor.storage(),
            EnumStorage::Text | EnumStorage::Int
        ));
        return Ok(());
    }
    let name = descriptor.name();
    let mut enum_type = EnumType::new(name.name().as_str(), descriptor.variants().iter().copied());
    if let Some(schema_name) = name.schema() {
        enum_type = enum_type.in_schema(schema_name.as_str());
    }
    if let Some(existing) = schema.enum_type(&enum_type.qualified_name()) {
        if existing.labels() != enum_type.labels() {
            return Err(graph_error(EntityGraphError::ConflictingEnum {
                name: enum_type.qualified_name(),
                first: existing.labels().join(", "),
                second: enum_type.labels().join(", "),
            }));
        }
        return Ok(());
    }
    schema.add_enum(enum_type);
    Ok(())
}

fn build_table(descriptor: &EntityDescriptor) -> Result<Table> {
    let mut table = Table::new(descriptor.table().name().as_str()).for_entity(descriptor.entity());
    if let Some(schema) = descriptor.table().schema() {
        table = table.in_schema(schema.as_str());
    }
    if let Some(comment) = descriptor.comment() {
        table = table.with_comment(comment);
    }

    for column in descriptor.columns() {
        table.add_column(build_column(column)?);
    }

    let primary_key: Vec<&str> = descriptor
        .primary_key()
        .iter()
        .map(|column| column.name().as_str())
        .collect();
    table.set_primary_key(primary_key);

    // A column marked `unique` is recorded as the unique index PostgreSQL would
    // have created for it, under PostgreSQL's own naming convention. Modelling
    // it as an index rather than as a column flag is what makes the
    // zero-downtime path — build the index concurrently, then promote it —
    // expressible at all.
    for column in descriptor.columns() {
        if column.is_unique() && !column.is_primary_key() {
            let name = format!(
                "{}_{}_key",
                descriptor.table().name().as_str(),
                column.name().as_str()
            );
            table.add_index(
                Index::new(name, [column.name().as_str()])
                    .unique()
                    .backing_a_constraint(),
            );
        }
    }

    for index in descriptor.indexes() {
        table.add_index(build_index(index)?);
    }
    for check in descriptor.checks() {
        table.add_check(build_check(check));
    }
    for foreign_key in descriptor.foreign_keys() {
        table.add_foreign_key(build_foreign_key(foreign_key));
    }

    Ok(table)
}

fn build_column(descriptor: &ColumnDescriptor) -> Result<Column> {
    let mut column = Column::new(descriptor.name().as_str(), refine_type(descriptor));
    if let Some(field) = descriptor.field() {
        column = column.for_field(field);
    }
    if descriptor.is_nullable() {
        column = column.nullable();
    }
    if let Some(default) = descriptor.default() {
        column = column.with_default(render_default(default));
    }
    if let Some(expression) = descriptor.generated() {
        column = column.generated_as(Generated::stored(expression));
    }
    if let Some(comment) = descriptor.comment() {
        column = column.with_comment(comment);
    }
    Ok(column)
}

/// Applies the descriptor's length and precision hints to its declared type.
///
/// `#[schema(max_length = 255)]` on a `String` field means `varchar(255)`, and
/// a `Decimal` with `#[schema(precision = 10, scale = 2)]` means
/// `numeric(10,2)`. The derive records both as hints beside the type rather
/// than folding them in, so this is where they meet.
fn refine_type(descriptor: &ColumnDescriptor) -> DataType {
    match (
        descriptor.data_type(),
        descriptor.max_length(),
        descriptor.numeric(),
    ) {
        (DataType::Text, Some(length), _) => DataType::VarChar(Some(length)),
        (DataType::VarChar(None), Some(length), _) => DataType::VarChar(Some(length)),
        (
            DataType::Numeric {
                precision: None,
                scale: None,
            },
            _,
            Some((precision, scale)),
        ) => DataType::Numeric {
            precision: Some(precision),
            scale: Some(scale),
        },
        (other, _, _) => other.clone(),
    }
}

fn render_default(default: &ColumnDefault) -> String {
    match (default.as_sql(), default.as_value()) {
        (Some(sql), _) => sql.to_owned(),
        // The snapshot is dialect-neutral, and PostgreSQL is the reference
        // dialect (ADR-0010), so a literal is spelled the PostgreSQL way. The
        // SQLite renderer re-spells booleans when it emits the DDL.
        (None, Some(value)) => emit::literal(value, Backend::Postgres),
        (None, None) => "NULL".to_owned(),
    }
}

fn build_index(descriptor: &IndexDescriptor) -> Result<Index> {
    let parts: Vec<IndexPart> = descriptor
        .columns()
        .iter()
        .map(|column| {
            let mut part = match column.column_name() {
                Some(name) => IndexPart::column(name.as_str()),
                None => IndexPart::expression(emit::expression(column.expr(), Backend::Postgres)?),
            };
            if let Some(order) = column.sort_order() {
                part = part.sorted(match order {
                    moso_sql::Order::Desc => Sort::Desc,
                    _ => Sort::Asc,
                });
            }
            if let Some(nulls) = column.nulls_placement() {
                part = part.nulls(match nulls {
                    moso_sql::Nulls::First => NullsOrder::First,
                    _ => NullsOrder::Last,
                });
            }
            if let Some(ops) = column.operator_class_name() {
                part = part.operator_class(ops.as_str());
            }
            Ok(part)
        })
        .collect::<Result<_>>()?;

    let mut index = Index::over(descriptor.name().as_str(), parts);
    if descriptor.is_unique() {
        index = index.unique();
    }
    if let Some(method) = descriptor.method() {
        index = index.using(index_method_name(method));
    }
    if let Some(predicate) = descriptor.predicate() {
        index = index.r#where(emit::expression(predicate, Backend::Postgres)?);
    }
    if !descriptor.included().is_empty() {
        index = index.include(descriptor.included().iter().map(Ident::as_str));
    }
    if descriptor.nulls_not_distinct() {
        index = index.nulls_not_distinct();
    }
    Ok(index)
}

fn index_method_name(method: &moso_sql::ddl::IndexMethod) -> String {
    use moso_sql::ddl::IndexMethod as M;
    match method {
        M::BTree => "btree".to_owned(),
        M::Hash => "hash".to_owned(),
        M::Gin => "gin".to_owned(),
        M::Gist => "gist".to_owned(),
        M::SpGist => "spgist".to_owned(),
        M::Brin => "brin".to_owned(),
        M::Custom(name) => name.as_str().to_owned(),
        _ => "btree".to_owned(),
    }
}

fn build_check(descriptor: &CheckDescriptor) -> Check {
    Check::new(descriptor.name().as_str(), descriptor.expression())
}

fn build_foreign_key(descriptor: &ForeignKeyDescriptor) -> ForeignKey {
    let mut foreign_key = ForeignKey::new(
        descriptor.name().as_str(),
        descriptor.columns().iter().map(Ident::as_str),
        qualified_table(descriptor.target()),
        descriptor.target_columns().iter().map(Ident::as_str),
    );
    if let Some(action) = descriptor.on_delete() {
        foreign_key = foreign_key.on_delete(Action::from_sql_action(action));
    }
    if let Some(action) = descriptor.on_update() {
        foreign_key = foreign_key.on_update(Action::from_sql_action(action));
    }
    if descriptor.is_deferrable() {
        foreign_key = foreign_key.deferrable(descriptor.is_initially_deferred());
    }
    foreign_key
}

/// The join table a many-to-many relation implies: two foreign keys and a
/// composite primary key, exactly as the operation table specifies.
fn join_table(
    owner: &EntityDescriptor,
    relation: &RelationDescriptor,
    by_entity: &BTreeMap<&str, &EntityDescriptor>,
) -> Result<Option<Table>> {
    if relation.kind() != RelationKind::ManyToMany {
        return Ok(None);
    }
    let Some(through) = relation.through() else {
        return Ok(None);
    };
    let target = by_entity.get(relation.target()).ok_or_else(|| {
        graph_error(EntityGraphError::UnknownTarget {
            entity: owner.entity().to_owned(),
            relation: relation.name().to_owned(),
            target: relation.target().to_owned(),
        })
    })?;

    let owner_key = sole_primary_key(owner);
    let target_key = sole_primary_key(target);

    let table_name = through.table().name().as_str();
    let mut table = Table::new(table_name);
    if let Some(schema) = through.table().schema() {
        table = table.in_schema(schema.as_str());
    }

    table.add_column(Column::new(through.left().as_str(), owner_key.1));
    table.add_column(Column::new(through.right().as_str(), target_key.1));
    table.set_primary_key([through.left().as_str(), through.right().as_str()]);

    table.add_foreign_key(
        ForeignKey::new(
            format!("{table_name}_{}_fkey", through.left().as_str()),
            [through.left().as_str()],
            qualified_table(owner.table()),
            [owner_key.0.as_str()],
        )
        .on_delete(Action::Cascade),
    );
    table.add_foreign_key(
        ForeignKey::new(
            format!("{table_name}_{}_fkey", through.right().as_str()),
            [through.right().as_str()],
            qualified_table(target.table()),
            [target_key.0.as_str()],
        )
        .on_delete(Action::Cascade),
    );
    // The reverse lookup: the composite primary key already indexes
    // (left, right), and a join table without an index on `right` turns every
    // reverse traversal into a sequential scan.
    table.add_index(Index::new(
        format!("{table_name}_{}_idx", through.right().as_str()),
        [through.right().as_str()],
    ));

    Ok(Some(table))
}

/// The primary-key column of an entity, and the type a foreign key to it must
/// have. A composite key falls back to the first column, which is wrong for a
/// composite-keyed many-to-many — a case the derive refuses before it gets
/// here.
fn sole_primary_key(descriptor: &EntityDescriptor) -> (String, DataType) {
    descriptor.primary_key().first().map_or_else(
        || ("id".to_owned(), DataType::BigInt),
        |column| {
            (
                column.name().as_str().to_owned(),
                // A foreign key must be the *base* type, not the serial: a
                // `bigserial` column on the referencing side would get its own
                // sequence, which is nonsense.
                match column.data_type() {
                    DataType::SmallSerial => DataType::SmallInt,
                    DataType::Serial => DataType::Integer,
                    DataType::BigSerial => DataType::BigInt,
                    other => other.clone(),
                },
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use moso_orm::descriptor::{IndexColumn, JoinTableDescriptor, RelationDescriptor};
    use moso_sql::{Expr, Ident, TableRef};

    use super::*;

    fn user() -> EntityDescriptor {
        EntityDescriptor::builder("User", TableRef::from_static("users"))
            .column(
                ColumnDescriptor::builder(Ident::from_static("id"), DataType::BigSerial)
                    .primary_key()
                    .field("id")
                    .build(),
            )
            .column(
                ColumnDescriptor::builder(Ident::from_static("email"), DataType::Text)
                    .unique()
                    .field("email")
                    .build(),
            )
            .column(
                ColumnDescriptor::builder(Ident::from_static("bio"), DataType::Text)
                    .nullable()
                    .field("bio")
                    .build(),
            )
            .build()
    }

    fn post() -> EntityDescriptor {
        EntityDescriptor::builder("Post", TableRef::from_static("posts"))
            .column(
                ColumnDescriptor::builder(Ident::from_static("id"), DataType::BigSerial)
                    .primary_key()
                    .build(),
            )
            .column(
                ColumnDescriptor::builder(Ident::from_static("author_id"), DataType::BigInt)
                    .build(),
            )
            .foreign_key(
                ForeignKeyDescriptor::builder(
                    "posts_author_id_fkey",
                    TableRef::from_static("users"),
                )
                .column(Ident::from_static("author_id"), Ident::from_static("id"))
                .on_delete(moso_sql::ddl::ReferentialAction::Cascade)
                .build(),
            )
            .index(
                IndexDescriptor::builder("idx_posts_author")
                    .column(Ident::from_static("author_id"))
                    .build(),
            )
            .check(CheckDescriptor::new("posts_id_positive", "id > 0"))
            .build()
    }

    #[test]
    fn one_entity_becomes_one_table() {
        let schema = Schema::from_entities([&user()]).expect("builds");
        let users = schema.table("users").expect("the users table");
        assert_eq!(users.entity(), Some("User"));
        assert_eq!(users.columns().len(), 3);
        assert_eq!(users.primary_key(), ["id"]);
        assert!(users.column("bio").expect("bio").is_nullable());
        assert!(!users.column("email").expect("email").is_nullable());
    }

    #[test]
    fn a_unique_column_becomes_a_unique_index_a_constraint_owns() {
        let schema = Schema::from_entities([&user()]).expect("builds");
        let index = schema
            .table("users")
            .expect("users")
            .index("users_email_key")
            .expect("the implicit unique index");
        assert!(index.is_unique());
        assert!(index.backs_a_constraint());
        assert_eq!(index.columns()[0].column_name(), Some("email"));
    }

    #[test]
    fn foreign_keys_indexes_and_checks_all_arrive() {
        let schema = Schema::from_entities([&user(), &post()]).expect("builds");
        let posts = schema.table("posts").expect("posts");
        let fk = posts.foreign_key("posts_author_id_fkey").expect("the fk");
        assert_eq!(fk.target_table(), "users");
        assert_eq!(fk.delete_action(), Some(Action::Cascade));
        assert!(posts.index("idx_posts_author").is_some());
        assert_eq!(
            posts
                .check("posts_id_positive")
                .expect("check")
                .expression(),
            "id > 0"
        );
    }

    #[test]
    fn two_entities_on_one_table_is_refused_by_name() {
        let clash = EntityDescriptor::builder("Account", TableRef::from_static("users")).build();
        let error = Schema::from_entities([&user(), &clash]).expect_err("clash");
        let text = error.to_string();
        assert!(text.contains("User"), "{text}");
        assert!(text.contains("Account"), "{text}");
        assert!(text.contains("help:"), "{text}");
    }

    #[test]
    fn many_to_many_produces_the_join_table() {
        let tag = EntityDescriptor::builder("Tag", TableRef::from_static("tags"))
            .column(
                ColumnDescriptor::builder(Ident::from_static("id"), DataType::BigSerial)
                    .primary_key()
                    .build(),
            )
            .build();
        let post = EntityDescriptor::builder("Post", TableRef::from_static("posts"))
            .column(
                ColumnDescriptor::builder(Ident::from_static("id"), DataType::BigSerial)
                    .primary_key()
                    .build(),
            )
            .relation(
                RelationDescriptor::builder("tags", RelationKind::ManyToMany, "Tag")
                    .through(JoinTableDescriptor::new(
                        TableRef::from_static("post_tags"),
                        Ident::from_static("post_id"),
                        Ident::from_static("tag_id"),
                    ))
                    .build(),
            )
            .build();

        let schema = Schema::from_entities([&post, &tag]).expect("builds");
        let join = schema.table("post_tags").expect("the join table");
        assert_eq!(join.primary_key(), ["post_id", "tag_id"]);
        assert_eq!(join.foreign_keys().len(), 2);
        // The foreign key points at `bigint`, not `bigserial`.
        assert_eq!(
            join.column("post_id").expect("post_id").type_name(),
            "bigint"
        );
        assert!(join.index("post_tags_tag_id_idx").is_some());
    }

    #[test]
    fn a_relation_to_an_unlisted_entity_names_the_fix() {
        let post = EntityDescriptor::builder("Post", TableRef::from_static("posts"))
            .relation(
                RelationDescriptor::builder("tags", RelationKind::ManyToMany, "Tag")
                    .through(JoinTableDescriptor::new(
                        TableRef::from_static("post_tags"),
                        Ident::from_static("post_id"),
                        Ident::from_static("tag_id"),
                    ))
                    .build(),
            )
            .build();
        let error = Schema::from_entities([&post]).expect_err("no Tag");
        assert!(error.to_string().contains("Tag"), "{error}");
        assert!(error.to_string().contains("from_entities"), "{error}");
    }

    #[test]
    fn a_pg_enum_becomes_a_type_and_a_text_enum_does_not() {
        let with_type = EntityDescriptor::builder("A", TableRef::from_static("a"))
            .enum_type(EnumTypeDescriptor::new(
                "user_role",
                EnumStorage::PgEnum,
                ["admin", "member"],
            ))
            .build();
        let without = EntityDescriptor::builder("B", TableRef::from_static("b"))
            .enum_type(EnumTypeDescriptor::new(
                "status",
                EnumStorage::Text,
                ["open", "closed"],
            ))
            .build();
        let schema = Schema::from_entities([&with_type, &without]).expect("builds");
        assert_eq!(schema.enums().len(), 1);
        assert_eq!(
            schema.enum_type("user_role").expect("the type").labels(),
            ["admin", "member"]
        );
    }

    #[test]
    fn two_spellings_of_one_enum_are_refused() {
        let first = EntityDescriptor::builder("A", TableRef::from_static("a"))
            .enum_type(EnumTypeDescriptor::new(
                "role",
                EnumStorage::PgEnum,
                ["admin"],
            ))
            .build();
        let second = EntityDescriptor::builder("B", TableRef::from_static("b"))
            .enum_type(EnumTypeDescriptor::new(
                "role",
                EnumStorage::PgEnum,
                ["member"],
            ))
            .build();
        let error = Schema::from_entities([&first, &second]).expect_err("conflict");
        assert!(error.to_string().contains("db_enum"), "{error}");
    }

    #[test]
    fn max_length_and_precision_refine_the_type() {
        let entity = EntityDescriptor::builder("Product", TableRef::from_static("products"))
            .column(
                ColumnDescriptor::builder(Ident::from_static("sku"), DataType::Text)
                    .max_length(64)
                    .build(),
            )
            .column(
                ColumnDescriptor::builder(
                    Ident::from_static("price"),
                    DataType::Numeric {
                        precision: None,
                        scale: None,
                    },
                )
                .numeric(10, 2)
                .build(),
            )
            .build();
        let schema = Schema::from_entities([&entity]).expect("builds");
        let products = schema.table("products").expect("products");
        assert_eq!(
            products.column("sku").expect("sku").type_name(),
            "varchar(64)"
        );
        assert_eq!(
            products.column("price").expect("price").type_name(),
            "numeric(10,2)"
        );
    }

    #[test]
    fn defaults_are_rendered_as_sql_text() {
        let entity = EntityDescriptor::builder("Setting", TableRef::from_static("settings"))
            .column(
                ColumnDescriptor::builder(Ident::from_static("locale"), DataType::Text)
                    .default(ColumnDefault::value(moso_sql::Value::text("en")))
                    .build(),
            )
            .column(
                ColumnDescriptor::builder(
                    Ident::from_static("created_at"),
                    DataType::Timestamp {
                        with_time_zone: true,
                    },
                )
                .default(ColumnDefault::sql("now()"))
                .build(),
            )
            .build();
        let schema = Schema::from_entities([&entity]).expect("builds");
        let settings = schema.table("settings").expect("settings");
        assert_eq!(
            settings.column("locale").expect("locale").default(),
            Some("'en'")
        );
        assert_eq!(
            settings.column("created_at").expect("created_at").default(),
            Some("now()")
        );
    }

    #[test]
    fn expression_indexes_survive_the_round_trip() {
        let entity = EntityDescriptor::builder("User", TableRef::from_static("users"))
            .index(
                IndexDescriptor::builder("idx_users_lower_email")
                    .target(IndexColumn::expression(Expr::Function(
                        moso_sql::Function::Lower(Box::new(Expr::col(Ident::from_static("email")))),
                    )))
                    .unique()
                    .build(),
            )
            .build();
        let schema = Schema::from_entities([&entity]).expect("builds");
        let index = schema
            .table("users")
            .expect("users")
            .index("idx_users_lower_email")
            .expect("the index");
        assert!(index.is_unique());
        assert_eq!(index.columns()[0].expr(), "lower(\"email\")");
        assert!(!index.columns()[0].is_column());
    }

    #[test]
    fn building_is_deterministic() {
        let first = Schema::from_entities([&user(), &post()]).expect("builds");
        let second = Schema::from_entities([&post(), &user()]).expect("builds");
        assert_eq!(first.to_json(), second.to_json());
    }
}
