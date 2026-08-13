//! Rendering DDL as the text that goes into a migration file.
//!
//! # Why this is not `Dialect::build`
//!
//! [`moso_sql::Dialect`] renders a statement as `Sql { text, args }`: a
//! parameterised query with `$1` where a value goes. That is exactly right for
//! a query and exactly wrong for a migration. A migration file is *text a human
//! reviews and can paste into `psql`*, so every literal has to be in it. There
//! is no parameter list to send alongside a file on disk.
//!
//! So this module renders the same [`Ddl`] intermediate representation with
//! literals inlined. Identifiers still come from [`Ident`], which validates
//! them and which this module always quotes, so the injection guarantee is
//! unchanged: a runtime string cannot become an identifier, and a runtime value
//! reaches the text only through [`literal`], which escapes it.
//!
//! ```
//! use moso_migrate::emit::render;
//! use moso_orm::Backend;
//! use moso_sql::ddl::{ColumnSpec, CreateTable, Ddl};
//! use moso_sql::{DataType, Ident, TableRef};
//!
//! let create = Ddl::CreateTable(
//!     CreateTable::new(TableRef::from_static("users"))
//!         .column(
//!             ColumnSpec::new(Ident::from_static("id"), DataType::BigSerial)
//!                 .not_null()
//!                 .primary_key(),
//!         ),
//! );
//! assert_eq!(
//!     render(&create, Backend::Postgres)?,
//!     "CREATE TABLE \"users\" (\n    \"id\" bigserial NOT NULL PRIMARY KEY\n)",
//! );
//! # Ok::<(), moso_migrate::Error>(())
//! ```

use std::fmt::Write as _;

use moso_orm::Backend;
use moso_sql::ddl::{
    AlterTable, AlterTableAction, AlterType, AlterTypeAction, ColumnSpec, CommentOn, CommentTarget,
    CreateExtension, CreateIndex, CreateSchema, CreateTable, CreateType, Ddl, DropIndex,
    DropSchema, DropTable, DropType, ForeignKey, Generated, Identity, IndexMethod, IndexTarget,
    PartitionStrategy, Partitioning, ReferentialAction, RenameIndex, RenameTable, TableConstraint,
    Truncate, TypeBody,
};
use moso_sql::{
    BinOp, Case, DataType, Expr, Function, Ident, JsonOp, Nulls, Order, RawExpr, TableRef,
    TrimMode, TypeRef, UnOp, Value,
};

use crate::error::{Error, Result};

/// Renders one schema-changing statement, with no trailing semicolon.
///
/// # Errors
///
/// [`Error::Unsupported`] when the backend has no such construct and there is
/// no honest substitute — `CREATE INDEX CONCURRENTLY` on SQLite, for instance.
/// A silently dropped clause is how a migration passes review and then fails
/// to do what it says.
///
/// ```
/// use moso_migrate::emit::render;
/// use moso_orm::Backend;
/// use moso_sql::ddl::{Ddl, DropTable};
/// use moso_sql::TableRef;
///
/// let drop = Ddl::DropTable(DropTable::new([TableRef::from_static("legacy")]));
/// assert_eq!(render(&drop, Backend::Postgres)?, "DROP TABLE \"legacy\"");
/// # Ok::<(), moso_migrate::Error>(())
/// ```
pub fn render(ddl: &Ddl, backend: Backend) -> Result<String> {
    let mut out = String::with_capacity(128);
    match ddl {
        Ddl::CreateTable(create) => create_table(create, backend, &mut out)?,
        Ddl::AlterTable(alter) => alter_table(alter, backend, &mut out)?,
        Ddl::DropTable(drop) => drop_table(drop, backend, &mut out),
        Ddl::RenameTable(rename) => rename_table(rename, &mut out),
        Ddl::Truncate(truncate) => truncate_tables(truncate, backend, &mut out)?,
        Ddl::CreateIndex(create) => create_index(create, backend, &mut out)?,
        Ddl::DropIndex(drop) => drop_index(drop, backend, &mut out)?,
        Ddl::RenameIndex(rename) => rename_index(rename, backend, &mut out)?,
        Ddl::CreateType(create) => create_type(create, backend, &mut out)?,
        Ddl::AlterType(alter) => alter_type(alter, backend, &mut out)?,
        Ddl::DropType(drop) => drop_type(drop, backend, &mut out)?,
        Ddl::CreateSchema(create) => create_schema(create, backend, &mut out)?,
        Ddl::DropSchema(drop) => drop_schema(drop, backend, &mut out)?,
        Ddl::CreateExtension(create) => create_extension(create, backend, &mut out)?,
        Ddl::Comment(comment) => comment_on(comment, backend, &mut out)?,
        Ddl::Raw(raw) => out.push_str(raw.text()),
        // `Ddl` is `#[non_exhaustive]`. Refusing an unknown variant is the only
        // safe answer: emitting nothing would produce a migration that claims
        // to do something and does not.
        other => {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: format!("render `{other:?}`"),
                help: "this version of `moso-migrate` is older than the `moso-sql` it is \
                       compiled against; upgrade the workspace together"
                    .to_owned(),
            });
        }
    }
    Ok(out)
}

/// A SQL literal for a bound value.
///
/// Text is single-quoted with doubled internal quotes; bytes become a
/// `bytea` hex literal on PostgreSQL and an `X'..'` blob literal on SQLite.
/// There is no path by which a value becomes anything other than a literal, so
/// a migration generated from data — a fill value for a new `NOT NULL` column,
/// say — cannot carry SQL with it.
///
/// ```
/// use moso_migrate::emit::literal;
/// use moso_orm::Backend;
/// use moso_sql::Value;
///
/// assert_eq!(literal(&Value::text("it's"), Backend::Postgres), "'it''s'");
/// assert_eq!(literal(&Value::Bool(true), Backend::Sqlite), "1");
/// ```
#[must_use]
pub fn literal(value: &Value, backend: Backend) -> String {
    match value {
        Value::Null(_) => "NULL".to_owned(),
        Value::Bool(true) => match backend {
            Backend::Sqlite => "1".to_owned(),
            _ => "true".to_owned(),
        },
        Value::Bool(false) => match backend {
            Backend::Sqlite => "0".to_owned(),
            _ => "false".to_owned(),
        },
        Value::I8(n) => n.to_string(),
        Value::I16(n) => n.to_string(),
        Value::I32(n) => n.to_string(),
        Value::I64(n) => n.to_string(),
        Value::U8(n) => n.to_string(),
        Value::U16(n) => n.to_string(),
        Value::U32(n) => n.to_string(),
        Value::U64(n) => n.to_string(),
        Value::F32(n) => render_float(f64::from(*n)),
        Value::F64(n) => render_float(*n),
        Value::Decimal(decimal) => decimal.to_string(),
        Value::Text(text) => quote_literal(text),
        Value::Bytes(bytes) => {
            let mut hex = String::with_capacity(bytes.len() * 2);
            for byte in bytes {
                let _ = write!(hex, "{byte:02x}");
            }
            match backend {
                Backend::Sqlite => format!("X'{hex}'"),
                _ => format!("'\\x{hex}'"),
            }
        }
        Value::Uuid(uuid) => quote_literal(&uuid.to_string()),
        Value::Json(json) => quote_literal(json.as_json_str()),
        Value::Timestamp(timestamp) => {
            let rendered =
                chrono::DateTime::from_timestamp(timestamp.unix_seconds(), timestamp.nanoseconds())
                    .map_or_else(
                        || timestamp.unix_seconds().to_string(),
                        |when| when.format("%Y-%m-%d %H:%M:%S%.6f+00").to_string(),
                    );
            quote_literal(&rendered)
        }
        Value::DateTime(datetime) => quote_literal(&datetime.to_string()),
        Value::Date(date) => quote_literal(&date.to_string()),
        Value::Time(time) => quote_literal(&time.to_string()),
        Value::Interval(interval) => quote_literal(&interval.to_string()),
        Value::Array(array) => {
            let items: Vec<String> = array
                .items()
                .iter()
                .map(|item| literal(item, backend))
                .collect();
            format!("ARRAY[{}]", items.join(", "))
        }
        // `Value` is `#[non_exhaustive]`; an unknown variant becomes `NULL`
        // only if it already *is* null, and otherwise a placeholder a reviewer
        // cannot miss.
        other => format!("/* unrenderable value: {other:?} */ NULL"),
    }
}

/// Renders a float without losing round-trip precision and without producing
/// `inf`, which no dialect accepts as a literal.
fn render_float(value: f64) -> String {
    if value.is_nan() {
        return "'NaN'".to_owned();
    }
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "'Infinity'".to_owned()
        } else {
            "'-Infinity'".to_owned()
        };
    }
    let rendered = format!("{value:?}");
    rendered
}

/// Single-quotes a string, doubling any internal quote.
///
/// ```
/// assert_eq!(moso_migrate::emit::quote_literal("it's"), "'it''s'");
/// ```
#[must_use]
pub fn quote_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for ch in text.chars() {
        if ch == '\'' {
            out.push('\'');
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

/// Double-quotes an identifier. Both supported backends accept the SQL
/// standard's spelling, and [`Ident`] already forbids the quote character, so
/// there is nothing to escape.
///
/// ```
/// use moso_sql::Ident;
///
/// assert_eq!(moso_migrate::emit::quote(&Ident::from_static("order")), "\"order\"");
/// ```
#[must_use]
pub fn quote(ident: &Ident) -> String {
    format!("\"{}\"", ident.as_str())
}

/// Double-quotes a name that has already been validated elsewhere — one read
/// back from `.schema.json` or from a live catalogue.
///
/// ```
/// assert_eq!(moso_migrate::emit::quote_name("users"), "\"users\"");
/// assert_eq!(moso_migrate::emit::quote_name("app.users"), "\"app\".\"users\"");
/// ```
#[must_use]
pub fn quote_name(name: &str) -> String {
    name.split('.')
        .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(".")
}

/// The backend's spelling of a type.
///
/// # SQLite
///
/// SQLite has five storage classes and derives a column's *affinity* from
/// substrings of whatever type name you declare, so it accepts — and remembers
/// verbatim — the PostgreSQL spellings. Moso therefore declares the same name
/// on both backends, with one exception: `bytea` becomes `blob`, because
/// `bytea` would get NUMERIC affinity and byte strings belong in BLOB.
///
/// Keeping the declared names identical is what makes drift detection work on
/// SQLite at all: `pragma table_info` reports the declared type, so it comes
/// back exactly as the snapshot spells it.
///
/// ```
/// use moso_migrate::emit::type_name;
/// use moso_orm::Backend;
/// use moso_sql::DataType;
///
/// assert_eq!(type_name(&DataType::Bytea, Backend::Postgres), "bytea");
/// assert_eq!(type_name(&DataType::Bytea, Backend::Sqlite), "blob");
/// assert_eq!(type_name(&DataType::JsonB, Backend::Sqlite), "jsonb");
/// ```
#[must_use]
pub fn type_name(data_type: &DataType, backend: Backend) -> String {
    match (backend, data_type) {
        (Backend::Sqlite, DataType::Bytea) => "blob".to_owned(),
        (Backend::Sqlite, DataType::SmallSerial | DataType::Serial | DataType::BigSerial) => {
            "integer".to_owned()
        }
        (Backend::Sqlite, DataType::Array(_)) => "text".to_owned(),
        (Backend::Sqlite, DataType::Enum(_)) => "text".to_owned(),
        (_, DataType::Enum(name) | DataType::Custom(name)) => qualified_type(name),
        (_, DataType::Array(element)) => format!("{}[]", type_name(element, backend)),
        _ => crate::schema::spell(data_type),
    }
}

fn qualified_type(name: &TypeRef) -> String {
    name.schema().map_or_else(
        || quote(name.name()),
        |schema| format!("{}.{}", quote(schema), quote(name.name())),
    )
}

fn table_ref(table: &TableRef) -> String {
    table.schema().map_or_else(
        || quote(table.name()),
        |schema| format!("{}.{}", quote(schema), quote(table.name())),
    )
}

// ── CREATE TABLE ────────────────────────────────────────────────────────────

fn create_table(create: &CreateTable, backend: Backend, out: &mut String) -> Result<()> {
    out.push_str("CREATE ");
    if create.is_temporary() {
        out.push_str("TEMPORARY ");
    }
    // SQLite has no unlogged tables and the distinction is meaningless there:
    // the whole database is one file. Dropping the keyword is documented
    // divergence, not a silent loss of meaning.
    if create.is_unlogged() && backend == Backend::Postgres {
        out.push_str("UNLOGGED ");
    }
    out.push_str("TABLE ");
    if create.is_if_not_exists() {
        out.push_str("IF NOT EXISTS ");
    }
    out.push_str(&table_ref(create.table()));
    out.push_str(" (");

    let mut parts: Vec<String> = Vec::with_capacity(create.columns().len() + 2);
    for column in create.columns() {
        parts.push(column_spec(column, backend, create)?);
    }
    for constraint in create.constraints() {
        parts.push(table_constraint(constraint, backend)?);
    }
    if parts.is_empty() {
        out.push(')');
        return Ok(());
    }
    for (index, part) in parts.iter().enumerate() {
        out.push_str("\n    ");
        out.push_str(part);
        if index + 1 < parts.len() {
            out.push(',');
        }
    }
    out.push_str("\n)");

    if let Some(partitioning) = create.partitioning() {
        if backend != Backend::Postgres {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: "partition a table".to_owned(),
                help:
                    "SQLite has no declarative partitioning; drop `#[entity(partition_by = ..)]` \
                       or keep this table on PostgreSQL only"
                        .to_owned(),
            });
        }
        out.push(' ');
        out.push_str(&partition_clause(partitioning));
    }
    Ok(())
}

fn partition_clause(partitioning: &Partitioning) -> String {
    let strategy = match partitioning.strategy() {
        PartitionStrategy::List => "LIST",
        PartitionStrategy::Hash => "HASH",
        _ => "RANGE",
    };
    let columns: Vec<String> = partitioning.columns().iter().map(quote).collect();
    format!("PARTITION BY {strategy} ({})", columns.join(", "))
}

fn column_spec(column: &ColumnSpec, backend: Backend, table: &CreateTable) -> Result<String> {
    let mut out = quote(column.name());
    out.push(' ');

    // SQLite's rowid alias is spelled `INTEGER PRIMARY KEY AUTOINCREMENT` and
    // nothing else: `bigint primary key autoincrement` is a syntax error. This
    // is the one place the two backends' DDL genuinely diverges rather than
    // merely differing in spelling.
    let sqlite_rowid = backend == Backend::Sqlite
        && column.data_type().is_auto_increment()
        && (column.is_primary_key() || sole_primary_key(table, column.name()));

    if sqlite_rowid {
        out.push_str("integer PRIMARY KEY AUTOINCREMENT");
        return Ok(out);
    }

    out.push_str(&type_name(column.data_type(), backend));

    if let Some(collation) = column.collation() {
        out.push_str(" COLLATE ");
        out.push_str(&quote(collation));
    }
    if let Some(generated) = column.generation() {
        out.push_str(&generated_clause(generated, backend)?);
    } else if let Some(identity) = column.identity_kind() {
        if backend == Backend::Postgres {
            out.push_str(match identity {
                Identity::Always => " GENERATED ALWAYS AS IDENTITY",
                _ => " GENERATED BY DEFAULT AS IDENTITY",
            });
        } else {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: format!("make `{}` an identity column", column.name().as_str()),
                help: "SQLite's equivalent is `integer primary key autoincrement`; declare the \
                       field as the entity's primary key with a `serial` type"
                    .to_owned(),
            });
        }
    }
    if !column.is_nullable() {
        out.push_str(" NOT NULL");
    }
    if let Some(default) = column.default_value() {
        out.push_str(" DEFAULT ");
        out.push_str(&expression(default, backend)?);
    }
    if column.is_primary_key() {
        out.push_str(" PRIMARY KEY");
    }
    if column.is_unique() {
        out.push_str(" UNIQUE");
    }
    if let Some(check) = column.check_expr() {
        out.push_str(" CHECK (");
        out.push_str(&expression(check, backend)?);
        out.push(')');
    }
    if let Some(foreign_key) = column.foreign_key() {
        out.push(' ');
        out.push_str(&references_clause(foreign_key, backend));
    }
    Ok(out)
}

/// Whether the only `PRIMARY KEY` constraint on the table names exactly this
/// column, which is what SQLite's rowid alias requires.
fn sole_primary_key(table: &CreateTable, column: &Ident) -> bool {
    table.constraints().iter().any(|constraint| {
        matches!(
            constraint,
            TableConstraint::PrimaryKey { columns, .. }
                if columns.len() == 1 && columns[0] == *column
        )
    })
}

fn generated_clause(generated: &Generated, backend: Backend) -> Result<String> {
    let expr = expression(generated.expr(), backend)?;
    Ok(if generated.is_stored() {
        format!(" GENERATED ALWAYS AS ({expr}) STORED")
    } else if backend == Backend::Sqlite {
        format!(" GENERATED ALWAYS AS ({expr}) VIRTUAL")
    } else {
        // PostgreSQL has only stored generated columns. Emitting VIRTUAL would
        // be a syntax error at apply time, on a production database.
        format!(" GENERATED ALWAYS AS ({expr}) STORED")
    })
}

fn table_constraint(constraint: &TableConstraint, backend: Backend) -> Result<String> {
    let mut out = String::new();
    if let Some(name) = constraint.name() {
        out.push_str("CONSTRAINT ");
        out.push_str(&quote(name));
        out.push(' ');
    }
    match constraint {
        TableConstraint::PrimaryKey { columns, .. } => {
            let _ = write!(out, "PRIMARY KEY ({})", column_list(columns));
        }
        TableConstraint::Unique {
            columns,
            nulls_not_distinct,
            ..
        } => {
            out.push_str("UNIQUE ");
            if *nulls_not_distinct {
                if backend == Backend::Postgres {
                    out.push_str("NULLS NOT DISTINCT ");
                } else {
                    return Err(Error::Unsupported {
                        backend: backend.as_str(),
                        operation: "declare a unique constraint NULLS NOT DISTINCT".to_owned(),
                        help: "SQLite treats every NULL as distinct in a unique index; enforce it \
                               with a partial unique index on the non-null case instead"
                            .to_owned(),
                    });
                }
            }
            let _ = write!(out, "({})", column_list(columns));
        }
        TableConstraint::ForeignKey(foreign_key) => {
            let _ = write!(
                out,
                "FOREIGN KEY ({}) {}",
                column_list(foreign_key.columns()),
                references_clause(foreign_key, backend)
            );
        }
        TableConstraint::Check {
            expr, not_valid, ..
        } => {
            let _ = write!(out, "CHECK ({})", expression(expr, backend)?);
            if *not_valid && backend == Backend::Postgres {
                out.push_str(" NOT VALID");
            }
        }
        TableConstraint::Exclude {
            method,
            elements,
            predicate,
            ..
        } => {
            if backend != Backend::Postgres {
                return Err(Error::Unsupported {
                    backend: backend.as_str(),
                    operation: "declare an EXCLUDE constraint".to_owned(),
                    help: "SQLite has no exclusion constraints; enforce non-overlap in the \
                           application, inside the transaction that writes the row"
                        .to_owned(),
                });
            }
            out.push_str("EXCLUDE ");
            if let Some(method) = method {
                let _ = write!(out, "USING {} ", method.as_str());
            }
            let rendered: Vec<String> = elements
                .iter()
                .map(|(expr, operator)| {
                    Ok(format!(
                        "{} WITH {}",
                        expression(expr, backend)?,
                        operator.as_str()
                    ))
                })
                .collect::<Result<_>>()?;
            let _ = write!(out, "({})", rendered.join(", "));
            if let Some(predicate) = predicate {
                let _ = write!(out, " WHERE ({})", expression(predicate, backend)?);
            }
        }
        other => {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: format!("render the constraint `{other:?}`"),
                help: "this version of `moso-migrate` is older than its `moso-sql`".to_owned(),
            });
        }
    }
    Ok(out)
}

fn references_clause(foreign_key: &ForeignKey, backend: Backend) -> String {
    let mut out = format!(
        "REFERENCES {} ({})",
        table_ref(foreign_key.target_table()),
        column_list(foreign_key.target_columns())
    );
    if let Some(action) = foreign_key.delete_action() {
        let _ = write!(out, " ON DELETE {}", referential_action(action));
    }
    if let Some(action) = foreign_key.update_action() {
        let _ = write!(out, " ON UPDATE {}", referential_action(action));
    }
    if foreign_key.is_deferrable() {
        out.push_str(" DEFERRABLE");
        if foreign_key.is_initially_deferred() {
            out.push_str(" INITIALLY DEFERRED");
        }
    }
    if foreign_key.is_not_valid() && backend == Backend::Postgres {
        out.push_str(" NOT VALID");
    }
    out
}

fn referential_action(action: ReferentialAction) -> &'static str {
    match action {
        ReferentialAction::Restrict => "RESTRICT",
        ReferentialAction::Cascade => "CASCADE",
        ReferentialAction::SetNull => "SET NULL",
        ReferentialAction::SetDefault => "SET DEFAULT",
        _ => "NO ACTION",
    }
}

fn column_list(columns: &[Ident]) -> String {
    columns.iter().map(quote).collect::<Vec<_>>().join(", ")
}

// ── ALTER TABLE ─────────────────────────────────────────────────────────────

fn alter_table(alter: &AlterTable, backend: Backend, out: &mut String) -> Result<()> {
    let actions = alter.actions();
    if actions.is_empty() {
        return Err(Error::Sql(moso_sql::Error::Incomplete {
            statement: "ALTER TABLE",
            missing: "any action",
            help: "call `.action(..)`, `.add_column(..)` or `.drop_column(..)`",
        }));
    }
    let _ = write!(out, "ALTER TABLE {} ", table_ref(alter.table()));
    let rendered: Vec<String> = actions
        .iter()
        .map(|action| alter_action(action, backend, alter))
        .collect::<Result<_>>()?;
    out.push_str(&rendered.join(", "));
    Ok(())
}

fn alter_action(action: &AlterTableAction, backend: Backend, alter: &AlterTable) -> Result<String> {
    let sqlite_refusal = |operation: String, help: &str| Error::Unsupported {
        backend: Backend::Sqlite.as_str(),
        operation,
        help: help.to_owned(),
    };

    Ok(match action {
        AlterTableAction::AddColumn {
            column,
            if_not_exists,
        } => {
            let mut out = String::from("ADD COLUMN ");
            if *if_not_exists {
                out.push_str("IF NOT EXISTS ");
            }
            // `CreateTable` is only used here for the sole-primary-key probe,
            // which cannot apply to a column being added to an existing table.
            let probe = CreateTable::new(alter.table().clone());
            out.push_str(&column_spec(column, backend, &probe)?);
            out
        }
        AlterTableAction::DropColumn {
            name,
            if_exists,
            cascade,
        } => {
            let mut out = String::from("DROP COLUMN ");
            if *if_exists {
                out.push_str("IF EXISTS ");
            }
            out.push_str(&quote(name));
            if *cascade && backend == Backend::Postgres {
                out.push_str(" CASCADE");
            }
            out
        }
        AlterTableAction::RenameColumn { from, to } => {
            format!("RENAME COLUMN {} TO {}", quote(from), quote(to))
        }
        AlterTableAction::AlterColumnType {
            name,
            data_type,
            using,
            ..
        } => {
            if backend == Backend::Sqlite {
                return Err(sqlite_refusal(
                    format!("change the type of `{}`", name.as_str()),
                    "SQLite cannot alter a column's type; `moso db make-migration` emits the \
                     12-step table rebuild for SQLite instead — see `Plan::for_backend`",
                ));
            }
            let mut out = format!(
                "ALTER COLUMN {} TYPE {}",
                quote(name),
                type_name(data_type, backend)
            );
            if let Some(using) = using {
                let _ = write!(out, " USING {}", expression(using, backend)?);
            }
            out
        }
        AlterTableAction::SetNotNull(name) => {
            if backend == Backend::Sqlite {
                return Err(sqlite_refusal(
                    format!("add NOT NULL to `{}`", name.as_str()),
                    "SQLite needs the 12-step table rebuild for a nullability change",
                ));
            }
            format!("ALTER COLUMN {} SET NOT NULL", quote(name))
        }
        AlterTableAction::DropNotNull(name) => {
            if backend == Backend::Sqlite {
                return Err(sqlite_refusal(
                    format!("drop NOT NULL from `{}`", name.as_str()),
                    "SQLite needs the 12-step table rebuild for a nullability change",
                ));
            }
            format!("ALTER COLUMN {} DROP NOT NULL", quote(name))
        }
        AlterTableAction::SetDefault { name, value } => {
            if backend == Backend::Sqlite {
                return Err(sqlite_refusal(
                    format!("change the default of `{}`", name.as_str()),
                    "SQLite needs the 12-step table rebuild for a default change",
                ));
            }
            format!(
                "ALTER COLUMN {} SET DEFAULT {}",
                quote(name),
                expression(value, backend)?
            )
        }
        AlterTableAction::DropDefault(name) => {
            if backend == Backend::Sqlite {
                return Err(sqlite_refusal(
                    format!("drop the default of `{}`", name.as_str()),
                    "SQLite needs the 12-step table rebuild for a default change",
                ));
            }
            format!("ALTER COLUMN {} DROP DEFAULT", quote(name))
        }
        AlterTableAction::AddConstraint(constraint) => {
            if backend == Backend::Sqlite {
                return Err(sqlite_refusal(
                    "add a constraint to an existing table".to_owned(),
                    "SQLite needs the 12-step table rebuild to add a constraint",
                ));
            }
            format!("ADD {}", table_constraint(constraint, backend)?)
        }
        AlterTableAction::DropConstraint {
            name,
            if_exists,
            cascade,
        } => {
            if backend == Backend::Sqlite {
                return Err(sqlite_refusal(
                    format!("drop the constraint `{}`", name.as_str()),
                    "SQLite needs the 12-step table rebuild to drop a constraint",
                ));
            }
            let mut out = String::from("DROP CONSTRAINT ");
            if *if_exists {
                out.push_str("IF EXISTS ");
            }
            out.push_str(&quote(name));
            if *cascade {
                out.push_str(" CASCADE");
            }
            out
        }
        AlterTableAction::ValidateConstraint(name) => {
            if backend != Backend::Postgres {
                return Err(sqlite_refusal(
                    "validate a constraint".to_owned(),
                    "SQLite validates a constraint when it is created; there is no second step",
                ));
            }
            format!("VALIDATE CONSTRAINT {}", quote(name))
        }
        AlterTableAction::RenameConstraint { from, to } => {
            if backend != Backend::Postgres {
                return Err(sqlite_refusal(
                    "rename a constraint".to_owned(),
                    "SQLite needs the 12-step table rebuild to rename a constraint",
                ));
            }
            format!("RENAME CONSTRAINT {} TO {}", quote(from), quote(to))
        }
        AlterTableAction::AddPrimaryKeyUsingIndex { name, index } => {
            constraint_using_index("PRIMARY KEY", name.as_ref(), index, backend)?
        }
        AlterTableAction::AddUniqueUsingIndex { name, index } => {
            constraint_using_index("UNIQUE", name.as_ref(), index, backend)?
        }
        AlterTableAction::SetSchema(schema) => {
            if backend != Backend::Postgres {
                return Err(sqlite_refusal(
                    "move a table to another schema".to_owned(),
                    "SQLite attaches whole databases rather than schemas",
                ));
            }
            format!("SET SCHEMA {}", quote(schema))
        }
        AlterTableAction::AttachPartition { partition, bounds } => {
            format!("ATTACH PARTITION {} {bounds}", table_ref(partition))
        }
        AlterTableAction::DetachPartition {
            partition,
            concurrently,
        } => {
            let mut out = format!("DETACH PARTITION {}", table_ref(partition));
            if *concurrently {
                out.push_str(" CONCURRENTLY");
            }
            out
        }
        other => {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: format!("render the action `{other:?}`"),
                help: "this version of `moso-migrate` is older than its `moso-sql`".to_owned(),
            });
        }
    })
}

fn constraint_using_index(
    kind: &str,
    name: Option<&Ident>,
    index: &Ident,
    backend: Backend,
) -> Result<String> {
    if backend != Backend::Postgres {
        return Err(Error::Unsupported {
            backend: backend.as_str(),
            operation: format!("promote an index to a {kind} constraint"),
            help: "SQLite has no `USING INDEX` form; a unique index IS the constraint there"
                .to_owned(),
        });
    }
    let mut out = String::from("ADD ");
    if let Some(name) = name {
        let _ = write!(out, "CONSTRAINT {} ", quote(name));
    }
    let _ = write!(out, "{kind} USING INDEX {}", quote(index));
    Ok(out)
}

// ── the rest of the DDL ─────────────────────────────────────────────────────

fn drop_table(drop: &DropTable, backend: Backend, out: &mut String) {
    out.push_str("DROP TABLE ");
    if drop.is_if_exists() {
        out.push_str("IF EXISTS ");
    }
    let tables: Vec<String> = drop.tables().iter().map(table_ref).collect();
    out.push_str(&tables.join(", "));
    if drop.is_cascade() && backend == Backend::Postgres {
        out.push_str(" CASCADE");
    }
}

fn rename_table(rename: &RenameTable, out: &mut String) {
    let _ = write!(
        out,
        "ALTER TABLE {} RENAME TO {}",
        table_ref(rename.from()),
        quote(rename.to())
    );
}

fn truncate_tables(truncate: &Truncate, backend: Backend, out: &mut String) -> Result<()> {
    if backend == Backend::Sqlite {
        // SQLite has no TRUNCATE. `DELETE FROM t` with no WHERE is optimised
        // into the same thing, and saying so is better than refusing.
        let tables: Vec<String> = truncate
            .tables()
            .iter()
            .map(|table| format!("DELETE FROM {}", table_ref(table)))
            .collect();
        out.push_str(&tables.join(";\n"));
        return Ok(());
    }
    out.push_str("TRUNCATE ");
    let tables: Vec<String> = truncate.tables().iter().map(table_ref).collect();
    out.push_str(&tables.join(", "));
    if truncate.restarts_identity() {
        out.push_str(" RESTART IDENTITY");
    }
    if truncate.is_cascade() {
        out.push_str(" CASCADE");
    }
    Ok(())
}

fn create_index(create: &CreateIndex, backend: Backend, out: &mut String) -> Result<()> {
    out.push_str("CREATE ");
    if create.is_unique() {
        out.push_str("UNIQUE ");
    }
    out.push_str("INDEX ");
    if create.is_concurrent() {
        if backend != Backend::Postgres {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: "build an index concurrently".to_owned(),
                help: "SQLite holds a write lock for the whole database anyway; the plan for \
                       SQLite drops CONCURRENTLY and stays transactional"
                    .to_owned(),
            });
        }
        out.push_str("CONCURRENTLY ");
    }
    if create.is_if_not_exists() {
        out.push_str("IF NOT EXISTS ");
    }
    let _ = write!(
        out,
        "{} ON {}",
        quote(create.name()),
        table_ref(create.table())
    );
    // SQLite has one index type. Dropping `USING btree` is exact; dropping
    // `USING gin` is not, which is why the planner refuses a non-btree index
    // for SQLite before it gets here.
    if let Some(method) = create.method()
        && backend == Backend::Postgres
    {
        let _ = write!(out, " USING {}", index_method(method));
    }
    let targets: Vec<String> = create
        .targets()
        .iter()
        .map(|target| index_target(target, backend))
        .collect::<Result<_>>()?;
    let _ = write!(out, " ({})", targets.join(", "));

    if !create.included().is_empty() {
        if backend != Backend::Postgres {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: "add INCLUDE columns to an index".to_owned(),
                help: "SQLite has no covering-index syntax; add the columns to the key instead"
                    .to_owned(),
            });
        }
        let _ = write!(out, " INCLUDE ({})", column_list(create.included()));
    }
    if create.has_nulls_not_distinct() && backend == Backend::Postgres {
        out.push_str(" NULLS NOT DISTINCT");
    }
    if let Some(predicate) = create.predicate() {
        let _ = write!(out, " WHERE {}", expression(predicate, backend)?);
    }
    Ok(())
}

fn index_method(method: &IndexMethod) -> String {
    match method {
        IndexMethod::BTree => "btree".to_owned(),
        IndexMethod::Hash => "hash".to_owned(),
        IndexMethod::Gin => "gin".to_owned(),
        IndexMethod::Gist => "gist".to_owned(),
        IndexMethod::SpGist => "spgist".to_owned(),
        IndexMethod::Brin => "brin".to_owned(),
        IndexMethod::Custom(name) => name.as_str().to_owned(),
        _ => "btree".to_owned(),
    }
}

fn index_target(target: &IndexTarget, backend: Backend) -> Result<String> {
    let mut out = match target.target_expr() {
        Expr::Column(column) => quote(column.name()),
        other => format!("({})", expression(other, backend)?),
    };
    if let Some(collation) = target.collation() {
        let _ = write!(out, " COLLATE {}", quote(collation));
    }
    if let Some(ops) = target.operator_class_name()
        && backend == Backend::Postgres
    {
        let _ = write!(out, " {}", ops.as_str());
    }
    if let Some(order) = target.sort_order() {
        out.push_str(match order {
            Order::Desc => " DESC",
            _ => " ASC",
        });
    }
    if let Some(nulls) = target.nulls_placement() {
        out.push_str(match nulls {
            Nulls::First => " NULLS FIRST",
            _ => " NULLS LAST",
        });
    }
    Ok(out)
}

fn drop_index(drop: &DropIndex, backend: Backend, out: &mut String) -> Result<()> {
    out.push_str("DROP INDEX ");
    if drop.is_concurrent() {
        if backend != Backend::Postgres {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: "drop an index concurrently".to_owned(),
                help: "SQLite drops an index instantly; the plan for SQLite drops CONCURRENTLY"
                    .to_owned(),
            });
        }
        out.push_str("CONCURRENTLY ");
    }
    if drop.is_if_exists() {
        out.push_str("IF EXISTS ");
    }
    match drop.schema() {
        Some(schema) => {
            let _ = write!(out, "{}.{}", quote(schema), quote(drop.name()));
        }
        None => out.push_str(&quote(drop.name())),
    }
    if drop.is_cascade() && backend == Backend::Postgres {
        out.push_str(" CASCADE");
    }
    Ok(())
}

fn rename_index(rename: &RenameIndex, backend: Backend, out: &mut String) -> Result<()> {
    if backend != Backend::Postgres {
        return Err(Error::Unsupported {
            backend: backend.as_str(),
            operation: "rename an index".to_owned(),
            help: "SQLite has no `ALTER INDEX`; the plan for SQLite drops and recreates it"
                .to_owned(),
        });
    }
    let _ = write!(
        out,
        "ALTER INDEX {} RENAME TO {}",
        quote(rename.from()),
        quote(rename.to())
    );
    Ok(())
}

fn create_type(create: &CreateType, backend: Backend, out: &mut String) -> Result<()> {
    refuse_types_on_sqlite(backend, "create an enum type")?;
    let _ = write!(out, "CREATE TYPE {} AS ", qualified_type(create.name()));
    match create.body() {
        TypeBody::Enum(labels) => {
            let rendered: Vec<String> = labels.iter().map(|label| quote_literal(label)).collect();
            let _ = write!(out, "ENUM ({})", rendered.join(", "));
        }
        other => {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: format!("create the type body `{other:?}`"),
                help: "only enum types are generated from entities".to_owned(),
            });
        }
    }
    Ok(())
}

fn alter_type(alter: &AlterType, backend: Backend, out: &mut String) -> Result<()> {
    refuse_types_on_sqlite(backend, "alter an enum type")?;
    let name = qualified_type(alter.name());
    match alter.action() {
        AlterTypeAction::AddValue {
            value,
            before,
            after,
            if_not_exists,
        } => {
            let _ = write!(out, "ALTER TYPE {name} ADD VALUE ");
            if *if_not_exists {
                out.push_str("IF NOT EXISTS ");
            }
            out.push_str(&quote_literal(value));
            if let Some(before) = before {
                let _ = write!(out, " BEFORE {}", quote_literal(before));
            } else if let Some(after) = after {
                let _ = write!(out, " AFTER {}", quote_literal(after));
            }
        }
        AlterTypeAction::RenameValue { from, to } => {
            let _ = write!(
                out,
                "ALTER TYPE {name} RENAME VALUE {} TO {}",
                quote_literal(from),
                quote_literal(to)
            );
        }
        AlterTypeAction::Rename(to) => {
            let _ = write!(out, "ALTER TYPE {name} RENAME TO {}", quote(to));
        }
        other => {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: format!("render the type action `{other:?}`"),
                help: "this version of `moso-migrate` is older than its `moso-sql`".to_owned(),
            });
        }
    }
    Ok(())
}

fn drop_type(drop: &DropType, backend: Backend, out: &mut String) -> Result<()> {
    refuse_types_on_sqlite(backend, "drop an enum type")?;
    out.push_str("DROP TYPE ");
    if drop.is_if_exists() {
        out.push_str("IF EXISTS ");
    }
    out.push_str(&qualified_type(drop.name()));
    if drop.is_cascade() {
        out.push_str(" CASCADE");
    }
    Ok(())
}

fn refuse_types_on_sqlite(backend: Backend, operation: &str) -> Result<()> {
    if backend == Backend::Postgres {
        return Ok(());
    }
    Err(Error::Unsupported {
        backend: backend.as_str(),
        operation: operation.to_owned(),
        help: "SQLite has no user-defined types; declare the enum with `#[db_enum(storage = \
               \"text\")]`, which stores the variant name and needs no type"
            .to_owned(),
    })
}

fn create_schema(create: &CreateSchema, backend: Backend, out: &mut String) -> Result<()> {
    if backend != Backend::Postgres {
        return Err(Error::Unsupported {
            backend: backend.as_str(),
            operation: "create a schema".to_owned(),
            help: "SQLite has no schemas; drop `#[entity(schema = ..)]` or keep this entity on \
                   PostgreSQL only"
                .to_owned(),
        });
    }
    out.push_str("CREATE SCHEMA ");
    if create.is_if_not_exists() {
        out.push_str("IF NOT EXISTS ");
    }
    out.push_str(&quote(create.name()));
    if let Some(owner) = create.owner() {
        let _ = write!(out, " AUTHORIZATION {}", quote(owner));
    }
    Ok(())
}

fn drop_schema(drop: &DropSchema, backend: Backend, out: &mut String) -> Result<()> {
    if backend != Backend::Postgres {
        return Err(Error::Unsupported {
            backend: backend.as_str(),
            operation: "drop a schema".to_owned(),
            help: "SQLite has no schemas".to_owned(),
        });
    }
    out.push_str("DROP SCHEMA ");
    if drop.is_if_exists() {
        out.push_str("IF EXISTS ");
    }
    out.push_str(&quote(drop.name()));
    if drop.is_cascade() {
        out.push_str(" CASCADE");
    }
    Ok(())
}

fn create_extension(create: &CreateExtension, backend: Backend, out: &mut String) -> Result<()> {
    if backend != Backend::Postgres {
        return Err(Error::Unsupported {
            backend: backend.as_str(),
            operation: format!("create the extension `{}`", create.name().as_str()),
            help: "SQLite has no extensions in this sense; remove it from `app.extensions` or \
                   guard the entity that needs it behind a PostgreSQL-only feature"
                .to_owned(),
        });
    }
    out.push_str("CREATE EXTENSION ");
    if create.is_if_not_exists() {
        out.push_str("IF NOT EXISTS ");
    }
    out.push_str(&quote(create.name()));
    if let Some(schema) = create.target_schema() {
        let _ = write!(out, " SCHEMA {}", quote(schema));
    }
    if let Some(version) = create.required_version() {
        let _ = write!(out, " VERSION {}", quote_literal(version));
    }
    Ok(())
}

fn comment_on(comment: &CommentOn, backend: Backend, out: &mut String) -> Result<()> {
    if backend != Backend::Postgres {
        return Err(Error::Unsupported {
            backend: backend.as_str(),
            operation: "attach a comment to a schema object".to_owned(),
            help: "SQLite stores no object comments; the doc comment stays in the Rust source"
                .to_owned(),
        });
    }
    let target = match comment.target() {
        CommentTarget::Table(table) => format!("TABLE {}", table_ref(table)),
        CommentTarget::Column { table, column } => {
            format!("COLUMN {}.{}", table_ref(table), quote(column))
        }
        CommentTarget::Index(name) => format!("INDEX {}", quote(name)),
        CommentTarget::Type(name) => format!("TYPE {}", qualified_type(name)),
        other => {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: format!("comment on `{other:?}`"),
                help: "this version of `moso-migrate` is older than its `moso-sql`".to_owned(),
            });
        }
    };
    let body = comment
        .text()
        .map_or_else(|| "NULL".to_owned(), quote_literal);
    let _ = write!(out, "COMMENT ON {target} IS {body}");
    Ok(())
}

// ── expressions ─────────────────────────────────────────────────────────────

/// Renders an expression as standalone SQL text, with literals inlined.
///
/// # Errors
///
/// [`Error::Unsupported`] for the expression kinds that cannot legally appear
/// in DDL — a subquery inside a `CHECK`, a window function inside a default.
/// Refusing is the point: a `DEFAULT` that quietly loses its `CASE` is a column
/// full of nulls in production.
///
/// ```
/// use moso_migrate::emit::expression;
/// use moso_orm::Backend;
/// use moso_sql::{Expr, Ident};
///
/// let predicate = Expr::col(Ident::from_static("deleted_at")).is_null();
/// assert_eq!(expression(&predicate, Backend::Postgres)?, "\"deleted_at\" IS NULL");
/// # Ok::<(), moso_migrate::Error>(())
/// ```
pub fn expression(expr: &Expr, backend: Backend) -> Result<String> {
    let unsupported = |what: &str| Error::Unsupported {
        backend: backend.as_str(),
        operation: format!("put {what} in a schema definition"),
        help: "a DEFAULT, a CHECK and an index predicate must be a self-contained expression; \
               move the logic into the application or into a generated column"
            .to_owned(),
    };

    Ok(match expr {
        Expr::Value(value) => literal(value, backend),
        Expr::Column(column) => match column.qualifier() {
            Some(qualifier) => format!("{}.{}", quote(qualifier), quote(column.name())),
            None => quote(column.name()),
        },
        Expr::Tuple(items) => format!("({})", expression_list(items, backend)?),
        Expr::Array(items) => format!("ARRAY[{}]", expression_list(items, backend)?),
        Expr::Nested(inner) => format!("({})", expression(inner, backend)?),
        Expr::Binary { lhs, op, rhs } => format!(
            "{} {} {}",
            expression(lhs, backend)?,
            binary_operator(*op, backend)?,
            expression(rhs, backend)?
        ),
        Expr::Unary { op, operand } => {
            let operand = expression(operand, backend)?;
            match op {
                UnOp::Not => format!("NOT {operand}"),
                UnOp::Neg => format!("-{operand}"),
                UnOp::BitNot => format!("~{operand}"),
                _ => return Err(unsupported("that operator")),
            }
        }
        Expr::IsNull { operand, negated } => format!(
            "{} IS {}NULL",
            expression(operand, backend)?,
            if *negated { "NOT " } else { "" }
        ),
        Expr::Between {
            operand,
            low,
            high,
            negated,
        } => format!(
            "{} {}BETWEEN {} AND {}",
            expression(operand, backend)?,
            if *negated { "NOT " } else { "" },
            expression(low, backend)?,
            expression(high, backend)?
        ),
        Expr::Like {
            operand,
            pattern,
            case_insensitive,
            negated,
            escape,
        } => {
            let keyword = match (*case_insensitive, backend) {
                (true, Backend::Postgres) => "ILIKE",
                _ => "LIKE",
            };
            let (left, right) = if *case_insensitive && backend != Backend::Postgres {
                (
                    format!("lower({})", expression(operand, backend)?),
                    format!("lower({})", expression(pattern, backend)?),
                )
            } else {
                (expression(operand, backend)?, expression(pattern, backend)?)
            };
            let mut out = format!(
                "{left} {}{keyword} {right}",
                if *negated { "NOT " } else { "" }
            );
            if let Some(escape) = escape {
                let _ = write!(out, " ESCAPE {}", quote_literal(&escape.to_string()));
            }
            out
        }
        Expr::InList {
            operand,
            items,
            negated,
        } => {
            if items.is_empty() {
                return Ok(if *negated {
                    "true".to_owned()
                } else {
                    "false".to_owned()
                });
            }
            format!(
                "{} {}IN ({})",
                expression(operand, backend)?,
                if *negated { "NOT " } else { "" },
                expression_list(items, backend)?
            )
        }
        Expr::Cast { operand, data_type } => format!(
            "CAST({} AS {})",
            expression(operand, backend)?,
            type_name(data_type, backend)
        ),
        Expr::Function(function) => render_function(function, backend)?,
        Expr::Json { lhs, op, rhs } => {
            if backend != Backend::Postgres {
                return Err(unsupported("a JSON operator"));
            }
            format!(
                "{} {} {}",
                expression(lhs, backend)?,
                json_operator(*op),
                expression(rhs, backend)?
            )
        }
        Expr::Case(case) => render_case(case, backend)?,
        Expr::Raw(raw) => render_raw(raw, backend),
        Expr::Default => "DEFAULT".to_owned(),
        Expr::InSubquery { .. } | Expr::Exists { .. } | Expr::Scalar(_) => {
            return Err(unsupported("a subquery"));
        }
        Expr::Aggregate(_) => return Err(unsupported("an aggregate")),
        Expr::Window(_) => return Err(unsupported("a window function")),
        Expr::Quantified { .. } => return Err(unsupported("an ANY/ALL comparison")),
        other => return Err(unsupported(&format!("`{other:?}`"))),
    })
}

fn expression_list(items: &[Expr], backend: Backend) -> Result<String> {
    let rendered: Vec<String> = items
        .iter()
        .map(|item| expression(item, backend))
        .collect::<Result<_>>()?;
    Ok(rendered.join(", "))
}

fn binary_operator(op: BinOp, backend: Backend) -> Result<&'static str> {
    Ok(match op {
        BinOp::Eq => "=",
        BinOp::NotEq => "<>",
        BinOp::Lt => "<",
        BinOp::LtEq => "<=",
        BinOp::Gt => ">",
        BinOp::GtEq => ">=",
        BinOp::IsDistinctFrom => "IS DISTINCT FROM",
        BinOp::IsNotDistinctFrom => "IS NOT DISTINCT FROM",
        BinOp::And => "AND",
        BinOp::Or => "OR",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Concat => "||",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::ShiftLeft => "<<",
        BinOp::ShiftRight => ">>",
        BinOp::Exp if backend == Backend::Postgres => "^",
        BinOp::BitXor if backend == Backend::Postgres => "#",
        BinOp::Regex if backend == Backend::Postgres => "~",
        BinOp::RegexCaseInsensitive if backend == Backend::Postgres => "~*",
        BinOp::NotRegex if backend == Backend::Postgres => "!~",
        BinOp::NotRegexCaseInsensitive if backend == Backend::Postgres => "!~*",
        BinOp::TextMatch if backend == Backend::Postgres => "@@",
        BinOp::ArrayContains if backend == Backend::Postgres => "@>",
        BinOp::ArrayContainedBy if backend == Backend::Postgres => "<@",
        BinOp::ArrayOverlaps if backend == Backend::Postgres => "&&",
        other => {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: format!("use the operator `{other:?}` in a schema definition"),
                help: "PostgreSQL has it and SQLite does not; keep the expression out of the \
                       schema, or make the entity PostgreSQL-only"
                    .to_owned(),
            });
        }
    })
}

fn json_operator(op: JsonOp) -> &'static str {
    match op {
        JsonOp::Get => "->",
        JsonOp::GetText => "->>",
        JsonOp::GetPath => "#>",
        JsonOp::GetPathText => "#>>",
        JsonOp::Contains => "@>",
        JsonOp::ContainedBy => "<@",
        JsonOp::HasKey => "?",
        JsonOp::HasAnyKey => "?|",
        JsonOp::HasAllKeys => "?&",
        JsonOp::Concat => "||",
        JsonOp::Remove => "-",
        _ => "#-",
    }
}

fn render_function(function: &Function, backend: Backend) -> Result<String> {
    let one = |name: &str, arg: &Expr| -> Result<String> {
        Ok(format!("{name}({})", expression(arg, backend)?))
    };
    Ok(match function {
        Function::Coalesce(args) => format!("coalesce({})", expression_list(args, backend)?),
        Function::NullIf(a, b) => format!(
            "nullif({}, {})",
            expression(a, backend)?,
            expression(b, backend)?
        ),
        Function::Greatest(args) => format!("greatest({})", expression_list(args, backend)?),
        Function::Least(args) => format!("least({})", expression_list(args, backend)?),
        Function::Abs(arg) => one("abs", arg)?,
        Function::Round { operand, decimals } => match decimals {
            Some(decimals) => format!(
                "round({}, {})",
                expression(operand, backend)?,
                expression(decimals, backend)?
            ),
            None => one("round", operand)?,
        },
        Function::Floor(arg) => one("floor", arg)?,
        Function::Ceil(arg) => one("ceil", arg)?,
        Function::Lower(arg) => one("lower", arg)?,
        Function::Upper(arg) => one("upper", arg)?,
        Function::Length(arg) => one("length", arg)?,
        Function::Trim {
            operand,
            mode,
            characters,
        } => {
            let keyword = match mode {
                TrimMode::Leading => "LEADING",
                TrimMode::Trailing => "TRAILING",
                _ => "BOTH",
            };
            match characters {
                Some(characters) => format!(
                    "trim({keyword} {} FROM {})",
                    expression(characters, backend)?,
                    expression(operand, backend)?
                ),
                None => format!("trim({keyword} FROM {})", expression(operand, backend)?),
            }
        }
        Function::Substring {
            operand,
            from,
            length,
        } => {
            let mut out = format!("substring({}", expression(operand, backend)?);
            if let Some(from) = from {
                let _ = write!(out, " FROM {}", expression(from, backend)?);
            }
            if let Some(length) = length {
                let _ = write!(out, " FOR {}", expression(length, backend)?);
            }
            out.push(')');
            out
        }
        Function::Replace { operand, from, to } => format!(
            "replace({}, {}, {})",
            expression(operand, backend)?,
            expression(from, backend)?,
            expression(to, backend)?
        ),
        Function::Concat(args) => {
            // SQLite's `concat()` arrived in 3.44; `||` works everywhere and
            // means the same thing for non-null operands.
            let rendered: Vec<String> = args
                .iter()
                .map(|arg| expression(arg, backend))
                .collect::<Result<_>>()?;
            if backend == Backend::Sqlite {
                rendered.join(" || ")
            } else {
                format!("concat({})", rendered.join(", "))
            }
        }
        Function::ConcatWs { separator, items } => format!(
            "concat_ws({}, {})",
            expression(separator, backend)?,
            expression_list(items, backend)?
        ),
        Function::Now | Function::CurrentTimestamp => match backend {
            Backend::Sqlite => "CURRENT_TIMESTAMP".to_owned(),
            _ => "now()".to_owned(),
        },
        Function::CurrentDate => "CURRENT_DATE".to_owned(),
        Function::CurrentTime => "CURRENT_TIME".to_owned(),
        Function::Random => match backend {
            Backend::Sqlite => "random()".to_owned(),
            _ => "random()".to_owned(),
        },
        Function::ToTsVector { config, document } => {
            let document = expression(document, backend)?;
            match config {
                Some(config) => format!(
                    "to_tsvector({}, {document})",
                    quote_literal(config.as_str())
                ),
                None => format!("to_tsvector({document})"),
            }
        }
        Function::Custom { name, args } => {
            format!("{}({})", name.as_str(), expression_list(args, backend)?)
        }
        other => {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: format!("call `{other:?}` in a schema definition"),
                help: "full-text ranking and highlighting belong in a query, not in DDL".to_owned(),
            });
        }
    })
}

fn render_case(case: &Case, backend: Backend) -> Result<String> {
    let mut out = String::from("CASE");
    if let Some(operand) = case.operand() {
        let _ = write!(out, " {}", expression(operand, backend)?);
    }
    for (condition, result) in case.branches() {
        let _ = write!(
            out,
            " WHEN {} THEN {}",
            expression(condition, backend)?,
            expression(result, backend)?
        );
    }
    if let Some(otherwise) = case.default_result() {
        let _ = write!(out, " ELSE {}", expression(otherwise, backend)?);
    }
    out.push_str(" END");
    Ok(out)
}

/// Substitutes a raw fragment's `?` placeholders with its bound values as
/// literals, honouring the `??` escape [`RawExpr`] documents.
fn render_raw(raw: &RawExpr, backend: Backend) -> String {
    let fragment = raw.fragment();
    let mut out = String::with_capacity(fragment.len());
    let mut args = raw.args().iter();
    let mut chars = fragment.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '?' {
            out.push(ch);
            continue;
        }
        if chars.peek() == Some(&'?') {
            chars.next();
            out.push('?');
            continue;
        }
        match args.next() {
            Some(value) => out.push_str(&literal(value, backend)),
            // `RawExpr` validates arity at build time; if one slipped through,
            // leaving the placeholder visible is better than losing it.
            None => out.push('?'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use moso_sql::ddl::{CreateIndex, IndexTarget};
    use moso_sql::{Ident, TableRef};

    use super::*;

    fn pg(ddl: &Ddl) -> String {
        render(ddl, Backend::Postgres).expect("renders on postgres")
    }

    fn sqlite(ddl: &Ddl) -> String {
        render(ddl, Backend::Sqlite).expect("renders on sqlite")
    }

    #[test]
    fn create_table_renders_on_both_dialects() {
        let create = Ddl::CreateTable(
            CreateTable::new(TableRef::from_static("users"))
                .column(
                    ColumnSpec::new(Ident::from_static("id"), DataType::BigSerial)
                        .not_null()
                        .primary_key(),
                )
                .column(ColumnSpec::new(Ident::from_static("email"), DataType::Text).not_null())
                .column(ColumnSpec::new(Ident::from_static("bio"), DataType::Text)),
        );
        assert_eq!(
            pg(&create),
            "CREATE TABLE \"users\" (\n    \"id\" bigserial NOT NULL PRIMARY KEY,\n    \
             \"email\" text NOT NULL,\n    \"bio\" text\n)"
        );
        assert_eq!(
            sqlite(&create),
            "CREATE TABLE \"users\" (\n    \"id\" integer PRIMARY KEY AUTOINCREMENT,\n    \
             \"email\" text NOT NULL,\n    \"bio\" text\n)"
        );
    }

    #[test]
    fn a_composite_primary_key_is_a_table_constraint() {
        let create = Ddl::CreateTable(
            CreateTable::new(TableRef::from_static("post_tags"))
                .column(ColumnSpec::new(
                    Ident::from_static("post_id"),
                    DataType::BigInt,
                ))
                .column(ColumnSpec::new(
                    Ident::from_static("tag_id"),
                    DataType::BigInt,
                ))
                .constraint(TableConstraint::primary_key(
                    Some(Ident::from_static("post_tags_pkey")),
                    [Ident::from_static("post_id"), Ident::from_static("tag_id")],
                )),
        );
        for text in [pg(&create), sqlite(&create)] {
            assert!(
                text.contains(
                    "CONSTRAINT \"post_tags_pkey\" PRIMARY KEY (\"post_id\", \"tag_id\")"
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn identifiers_are_always_quoted() {
        let create = Ddl::CreateTable(CreateTable::new(TableRef::from_static("order")).column(
            ColumnSpec::new(Ident::from_static("select"), DataType::Text),
        ));
        assert!(pg(&create).contains("\"order\""));
        assert!(pg(&create).contains("\"select\""));
    }

    #[test]
    fn concurrent_index_is_postgres_only_and_says_so() {
        let index = Ddl::CreateIndex(
            CreateIndex::new(
                Ident::from_static("idx_users_email"),
                TableRef::from_static("users"),
                [IndexTarget::column(Ident::from_static("email"))],
            )
            .concurrently(),
        );
        assert_eq!(
            pg(&index),
            "CREATE INDEX CONCURRENTLY \"idx_users_email\" ON \"users\" (\"email\")"
        );
        let error = render(&index, Backend::Sqlite).expect_err("no CONCURRENTLY on sqlite");
        assert!(error.to_string().contains("help:"), "{error}");
    }

    #[test]
    fn a_partial_unique_index_renders_its_predicate() {
        let index = Ddl::CreateIndex(
            CreateIndex::new(
                Ident::from_static("users_email_key"),
                TableRef::from_static("users"),
                [IndexTarget::column(Ident::from_static("email"))],
            )
            .unique()
            .where_(Expr::col(Ident::from_static("deleted_at")).is_null()),
        );
        assert_eq!(
            pg(&index),
            "CREATE UNIQUE INDEX \"users_email_key\" ON \"users\" (\"email\") \
             WHERE \"deleted_at\" IS NULL"
        );
        assert_eq!(
            sqlite(&index),
            "CREATE UNIQUE INDEX \"users_email_key\" ON \"users\" (\"email\") \
             WHERE \"deleted_at\" IS NULL"
        );
    }

    #[test]
    fn foreign_keys_render_the_not_valid_idiom() {
        let alter = Ddl::AlterTable(
            AlterTable::new(TableRef::from_static("posts")).action(
                AlterTableAction::AddConstraint(TableConstraint::ForeignKey(
                    ForeignKey::new(
                        Some(Ident::from_static("posts_author_id_fkey")),
                        [Ident::from_static("author_id")],
                        TableRef::from_static("users"),
                        [Ident::from_static("id")],
                    )
                    .on_delete(ReferentialAction::Cascade)
                    .not_valid(),
                )),
            ),
        );
        assert_eq!(
            pg(&alter),
            "ALTER TABLE \"posts\" ADD CONSTRAINT \"posts_author_id_fkey\" \
             FOREIGN KEY (\"author_id\") REFERENCES \"users\" (\"id\") ON DELETE CASCADE NOT VALID"
        );
    }

    #[test]
    fn validate_constraint_is_its_own_statement() {
        let validate = Ddl::AlterTable(AlterTable::new(TableRef::from_static("posts")).action(
            AlterTableAction::ValidateConstraint(Ident::from_static("posts_author_id_fkey")),
        ));
        assert_eq!(
            pg(&validate),
            "ALTER TABLE \"posts\" VALIDATE CONSTRAINT \"posts_author_id_fkey\""
        );
    }

    #[test]
    fn add_unique_using_index_is_the_zero_downtime_idiom() {
        let promote = Ddl::AlterTable(AlterTable::new(TableRef::from_static("users")).action(
            AlterTableAction::AddUniqueUsingIndex {
                name: Some(Ident::from_static("users_email_key")),
                index: Ident::from_static("users_email_key"),
            },
        ));
        assert_eq!(
            pg(&promote),
            "ALTER TABLE \"users\" ADD CONSTRAINT \"users_email_key\" UNIQUE USING INDEX \
             \"users_email_key\""
        );
    }

    #[test]
    fn enum_types_are_postgres_only_with_a_named_alternative() {
        let create = Ddl::CreateType(CreateType::new(
            TypeRef::from_static("user_role"),
            TypeBody::enumeration(["admin", "member"]),
        ));
        assert_eq!(
            pg(&create),
            "CREATE TYPE \"user_role\" AS ENUM ('admin', 'member')"
        );
        let error = render(&create, Backend::Sqlite).expect_err("no types on sqlite");
        assert!(error.to_string().contains("db_enum"), "{error}");
    }

    #[test]
    fn alter_type_add_value_renders_with_position() {
        let alter = Ddl::AlterType(AlterType::new(
            TypeRef::from_static("user_role"),
            AlterTypeAction::AddValue {
                value: "auditor".to_owned(),
                before: None,
                after: Some("member".to_owned()),
                if_not_exists: true,
            },
        ));
        assert_eq!(
            pg(&alter),
            "ALTER TYPE \"user_role\" ADD VALUE IF NOT EXISTS 'auditor' AFTER 'member'"
        );
    }

    #[test]
    fn literals_escape_quotes_and_pick_the_dialect_spelling() {
        assert_eq!(literal(&Value::text("it's"), Backend::Postgres), "'it''s'");
        assert_eq!(literal(&Value::Bool(true), Backend::Postgres), "true");
        assert_eq!(literal(&Value::Bool(true), Backend::Sqlite), "1");
        assert_eq!(literal(&Value::I64(-3), Backend::Postgres), "-3");
        assert_eq!(
            literal(&Value::bytes([0xde, 0xad]), Backend::Postgres),
            "'\\xdead'"
        );
        assert_eq!(
            literal(&Value::bytes([0xde, 0xad]), Backend::Sqlite),
            "X'dead'"
        );
        assert_eq!(
            literal(&Value::Null(moso_sql::ValueKind::Text), Backend::Postgres),
            "NULL"
        );
    }

    #[test]
    fn a_text_literal_cannot_escape_its_quotes() {
        let hostile = Value::text("'; DROP TABLE users; --");
        let rendered = literal(&hostile, Backend::Postgres);
        assert_eq!(rendered, "'''; DROP TABLE users; --'");
        assert!(rendered.starts_with('\'') && rendered.ends_with('\''));
    }

    #[test]
    fn expressions_render_the_ddl_subset() {
        let cases: Vec<(Expr, &str)> = vec![
            (
                Expr::col(Ident::from_static("n")).gt(Expr::value(0_i32)),
                "\"n\" > 0",
            ),
            (
                Expr::col(Ident::from_static("a")).is_null(),
                "\"a\" IS NULL",
            ),
            (
                Expr::Function(Function::Lower(Box::new(Expr::col(Ident::from_static(
                    "email",
                ))))),
                "lower(\"email\")",
            ),
            (Expr::Function(Function::Now), "now()"),
        ];
        for (expr, expected) in cases {
            assert_eq!(
                expression(&expr, Backend::Postgres).expect("renders"),
                expected
            );
        }
    }

    #[test]
    fn now_is_spelled_for_the_dialect() {
        assert_eq!(
            expression(&Expr::Function(Function::Now), Backend::Sqlite).expect("renders"),
            "CURRENT_TIMESTAMP"
        );
    }

    #[test]
    fn a_subquery_in_ddl_is_refused_with_a_reason() {
        let expr = Expr::Exists {
            query: Box::new(moso_sql::Select::from_table(TableRef::from_static("t"))),
            negated: false,
        };
        let error = expression(&expr, Backend::Postgres).expect_err("no subqueries in DDL");
        assert!(error.to_string().contains("self-contained"), "{error}");
    }

    #[test]
    fn raw_fragments_substitute_their_bound_values() {
        let raw = RawExpr::with_args("coalesce(?, ?) || '??'", [Value::text("a"), Value::I32(1)]);
        assert_eq!(
            expression(&Expr::Raw(raw), Backend::Postgres).expect("renders"),
            "coalesce('a', 1) || '?'"
        );
    }

    #[test]
    fn qualified_names_quote_each_part() {
        assert_eq!(quote_name("app.users"), "\"app\".\"users\"");
        assert_eq!(quote_name("users"), "\"users\"");
    }

    #[test]
    fn truncate_becomes_delete_on_sqlite() {
        let truncate = Ddl::Truncate(Truncate::new([TableRef::from_static("events")]));
        assert_eq!(pg(&truncate), "TRUNCATE \"events\"");
        assert_eq!(sqlite(&truncate), "DELETE FROM \"events\"");
    }

    #[test]
    fn comments_are_postgres_only() {
        let comment = Ddl::Comment(CommentOn::new(
            CommentTarget::Table(TableRef::from_static("users")),
            Some("People who can log in".to_owned()),
        ));
        assert_eq!(
            pg(&comment),
            "COMMENT ON TABLE \"users\" IS 'People who can log in'"
        );
        assert!(render(&comment, Backend::Sqlite).is_err());
    }

    #[test]
    fn partitioning_renders_on_postgres_and_is_refused_on_sqlite() {
        let create = Ddl::CreateTable(
            CreateTable::new(TableRef::from_static("events"))
                .column(ColumnSpec::new(
                    Ident::from_static("created_at"),
                    DataType::Timestamp {
                        with_time_zone: true,
                    },
                ))
                .partition_by(Partitioning::new(
                    PartitionStrategy::Range,
                    [Ident::from_static("created_at")],
                )),
        );
        assert!(pg(&create).ends_with("PARTITION BY RANGE (\"created_at\")"));
        assert!(render(&create, Backend::Sqlite).is_err());
    }

    #[test]
    fn generated_columns_render_stored_on_postgres() {
        let create = Ddl::CreateTable(CreateTable::new(TableRef::from_static("posts")).column(
            ColumnSpec::new(Ident::from_static("slug_upper"), DataType::Text).generated(
                Generated::stored(Expr::Function(Function::Upper(Box::new(Expr::col(
                    Ident::from_static("slug"),
                ))))),
            ),
        ));
        assert!(
            pg(&create).contains("GENERATED ALWAYS AS (upper(\"slug\")) STORED"),
            "{}",
            pg(&create)
        );
    }

    #[test]
    fn float_literals_round_trip() {
        assert_eq!(literal(&Value::F64(1.5), Backend::Postgres), "1.5");
        assert_eq!(literal(&Value::F64(1.0), Backend::Postgres), "1.0");
        assert_eq!(
            literal(&Value::F64(f64::INFINITY), Backend::Postgres),
            "'Infinity'"
        );
    }
}
