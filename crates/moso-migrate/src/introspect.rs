//! Reading a live database back into a [`Schema`].
//!
//! This is what makes `moso db check` possible in both directions, and it is
//! what the round-trip test asserts against: a fresh database, every migration
//! applied, read back — and the result must equal the snapshot the migrations
//! were generated from. A differ that only ever sees its own output proves
//! nothing; this is the half that makes it a real test.
//!
//! # Fidelity
//!
//! The target is not "describe any schema": it is "describe, exactly, the
//! schemas Moso generates". A hand-written `EXCLUDE` constraint or a
//! multi-dimensional array will be read back approximately, and drift will
//! report it. That is the honest failure mode — the alternative, silently
//! ignoring what it cannot read, would make `moso db check` say "no drift" for
//! a database that has none of your indexes.
//!
//! ```no_run
//! use moso_migrate::introspect::read_schema;
//!
//! # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
//! let live = read_schema(connection).await?;
//! println!("{} tables", live.tables().count());
//! # Ok(())
//! # }
//! ```

use moso_orm::Backend;

use crate::conn::Connection;
use crate::error::Result;
use crate::schema::{
    Action, Check, Column, EnumType, ForeignKey, Index, IndexPart, NullsOrder, Schema, Sort, Table,
};

/// Reads the whole schema of the database on the other end of `connection`.
///
/// # Errors
///
/// [`Error::Database`](crate::Error::Database) when a catalogue query fails,
/// [`Error::Snapshot`](crate::Error::Snapshot) when the database has a type
/// this build has no name for.
///
/// ```no_run
/// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
/// let live = moso_migrate::introspect::read_schema(connection).await?;
/// assert!(live.table("moso_migrations").is_none(), "the ledger is not part of the schema");
/// # Ok(())
/// # }
/// ```
pub async fn read_schema(connection: &mut Connection) -> Result<Schema> {
    read_schema_including(connection, &[]).await
}

/// The same, also reading the named schemas in `extra`.
///
/// PostgreSQL introspection is scoped to the connection's **search path**,
/// because a database frequently holds more than one application's tables and
/// reporting somebody else's as drift is worse than useless. An entity in a
/// named schema — `#[entity(schema = "analytics")]` — is off the search path by
/// default, so `moso db check` passes the schemas the snapshot declares.
///
/// # Errors
///
/// As [`read_schema`].
///
/// ```no_run
/// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
/// let live = moso_migrate::introspect::read_schema_including(
///     connection,
///     &["analytics".to_owned()],
/// ).await?;
/// # let _ = live;
/// # Ok(())
/// # }
/// ```
pub async fn read_schema_including(
    connection: &mut Connection,
    extra: &[String],
) -> Result<Schema> {
    match connection.backend() {
        Backend::Sqlite => sqlite::read(connection).await,
        _ => postgres::read(connection, extra).await,
    }
}

/// Tables Moso owns and which are therefore not part of the application's
/// schema.
///
/// ```
/// assert!(moso_migrate::introspect::is_moso_table("moso_migrations"));
/// assert!(!moso_migrate::introspect::is_moso_table("users"));
/// ```
#[must_use]
pub fn is_moso_table(name: &str) -> bool {
    matches!(name, "moso_migrations" | "moso_migrations_lock")
        || name.starts_with("sqlite_")
        || name.ends_with(crate::plan::REBUILD_SUFFIX)
}

/// Splits an index key part into its expression and its ordering.
///
/// `pg_get_indexdef` and `sqlite_master` both hand back `created_at DESC NULLS
/// LAST`, and the snapshot stores those three things separately.
///
/// ```
/// use moso_migrate::introspect::parse_index_part;
///
/// let part = parse_index_part("created_at DESC NULLS LAST");
/// assert_eq!(part.column_name(), Some("created_at"));
/// assert_eq!(part.sort(), Some(moso_migrate::schema::Sort::Desc));
/// ```
#[must_use]
pub fn parse_index_part(raw: &str) -> IndexPart {
    let mut rest = raw.trim();
    let mut nulls = None;
    let mut sort = None;

    for (suffix, value) in [
        (" NULLS LAST", NullsOrder::Last),
        (" NULLS FIRST", NullsOrder::First),
    ] {
        if let Some(head) = strip_suffix_ignore_case(rest, suffix) {
            nulls = Some(value);
            rest = head.trim_end();
            break;
        }
    }
    for (suffix, value) in [(" DESC", Sort::Desc), (" ASC", Sort::Asc)] {
        if let Some(head) = strip_suffix_ignore_case(rest, suffix) {
            sort = Some(value);
            rest = head.trim_end();
            break;
        }
    }

    // An operator class is a trailing bare identifier after a column, which is
    // ambiguous with a two-word type name; only the shapes Moso emits are
    // recognised.
    let mut ops = None;
    if let Some((head, tail)) = rest.rsplit_once(' ')
        && tail.ends_with("_ops")
        && is_bare_identifier(head)
    {
        ops = Some(tail.to_owned());
        rest = head;
    }

    // PostgreSQL wraps an expression index's key in its own parentheses:
    // `((lower(email)))`. Only a pair that wraps the *whole* expression may be
    // removed — stripping greedily turns `lower(email)` into `lower(email`.
    while rest.len() > 1 && rest.starts_with('(') && rest.ends_with(')') && balanced(rest) {
        rest = rest[1..rest.len() - 1].trim();
    }

    let unquoted = unquote(rest);
    let mut part = if is_bare_identifier(&unquoted) || rest.starts_with('"') {
        IndexPart::column(unquoted)
    } else {
        IndexPart::expression(rest)
    };
    if let Some(sort) = sort {
        part = part.sorted(sort);
    }
    if let Some(nulls) = nulls {
        part = part.nulls(nulls);
    }
    if let Some(ops) = ops {
        part = part.operator_class(ops);
    }
    part
}

fn strip_suffix_ignore_case<'a>(haystack: &'a str, suffix: &str) -> Option<&'a str> {
    if haystack.len() < suffix.len() {
        return None;
    }
    let split = haystack.len() - suffix.len();
    haystack
        .get(split..)
        .filter(|tail| tail.eq_ignore_ascii_case(suffix))
        .and_then(|_| haystack.get(..split))
}

/// Whether a catalogue value that stands for a boolean is true.
///
/// PostgreSQL's `cast(x as text)` renders a boolean as `true`, not as the `t`
/// that `psql` prints; SQLite's `PRAGMA` output uses `1`. Accepting all three
/// is the difference between reading a schema and reading a schema in which
/// every column is nullable and no index is unique.
///
/// ```
/// assert!(moso_migrate::introspect::reads_as_true(Some("true")));
/// assert!(moso_migrate::introspect::reads_as_true(Some("t")));
/// assert!(moso_migrate::introspect::reads_as_true(Some("1")));
/// assert!(!moso_migrate::introspect::reads_as_true(Some("false")));
/// assert!(!moso_migrate::introspect::reads_as_true(None));
/// ```
#[must_use]
pub fn reads_as_true(value: Option<&str>) -> bool {
    matches!(value, Some("t" | "true" | "1" | "TRUE" | "T"))
}

fn is_bare_identifier(raw: &str) -> bool {
    !raw.is_empty()
        && raw
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && !raw.starts_with(|ch: char| ch.is_ascii_digit())
}

fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        return trimmed[1..trimmed.len() - 1].replace("\"\"", "\"");
    }
    trimmed.to_owned()
}

/// Normalises a default expression so that two spellings of the same default
/// compare equal.
///
/// PostgreSQL hands back `'en'::text` for a default written `'en'`, and
/// `CURRENT_TIMESTAMP` for one written `now()` — neither is a schema change,
/// and reporting them as drift trains people to ignore `moso db check`.
///
/// ```
/// use moso_migrate::introspect::normalise_default;
///
/// assert_eq!(normalise_default("'en'::text"), "'en'");
/// assert_eq!(normalise_default("now()"), "now()");
/// assert_eq!(normalise_default("(0)"), "0");
/// ```
#[must_use]
pub fn normalise_default(raw: &str) -> String {
    let mut value = raw.trim();

    // Strip one layer of the parentheses PostgreSQL adds around anything
    // compound, and SQLite adds around everything.
    while value.len() > 1 && value.starts_with('(') && value.ends_with(')') && balanced(value) {
        value = value[1..value.len() - 1].trim();
    }

    // Strip a trailing cast, which PostgreSQL always adds.
    let mut owned = value.to_owned();
    while let Some(at) = owned.rfind("::") {
        let tail = &owned[at + 2..];
        if tail.is_empty()
            || !tail.chars().all(|ch| {
                ch.is_ascii_alphanumeric() || ch == '_' || ch == ' ' || ch == '[' || ch == ']'
            })
        {
            break;
        }
        owned.truncate(at);
        owned = owned.trim().to_owned();
    }

    match owned.to_ascii_lowercase().as_str() {
        "current_timestamp" | "now()" => "now()".to_owned(),
        "true" | "false" | "null" => owned.to_ascii_lowercase(),
        "1" if raw.contains("bool") => "true".to_owned(),
        _ => owned,
    }
}

fn balanced(value: &str) -> bool {
    let mut depth = 0_i32;
    for (index, ch) in value.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && index + 1 != value.len() {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

mod postgres {
    use super::{
        Action, Check, Column, EnumType, ForeignKey, Index, Schema, Table, is_moso_table,
        normalise_default, parse_index_part, unquote,
    };
    use crate::conn::Connection;
    use crate::error::Result;
    use moso_sql::DataType;
    use std::collections::BTreeMap;

    pub(super) async fn read(connection: &mut Connection, extra: &[String]) -> Result<Schema> {
        let scope = scope(extra);
        let mut schema = Schema::empty();
        read_enums(connection, &scope, &mut schema).await?;
        read_extensions(connection, &mut schema).await?;
        let mut tables = read_columns(connection, &scope).await?;
        read_indexes(connection, &scope, &mut tables).await?;
        read_constraints(connection, &scope, &mut tables).await?;
        for (_, table) in tables {
            schema.add_table(table);
        }
        Ok(schema)
    }

    /// The `WHERE` fragment that limits every catalogue query to the search
    /// path plus the schemas the caller named.
    fn scope(extra: &[String]) -> String {
        let mut clause = String::from("n.nspname = ANY(current_schemas(false))");
        for name in extra {
            let _ = std::fmt::Write::write_fmt(
                &mut clause,
                format_args!(" OR n.nspname = {}", crate::emit::quote_literal(name)),
            );
        }
        format!("({clause})")
    }

    async fn read_enums(
        connection: &mut Connection,
        scope: &str,
        schema: &mut Schema,
    ) -> Result<()> {
        let sql = format!(
            "SELECT n.nspname, t.typname, \
             string_agg(e.enumlabel, chr(1) ORDER BY e.enumsortorder) \
             FROM pg_type t \
             JOIN pg_enum e ON e.enumtypid = t.oid \
             JOIN pg_namespace n ON n.oid = t.typnamespace \
             WHERE {scope} \
             GROUP BY 1, 2 ORDER BY 1, 2"
        );
        for row in connection.fetch_text(&sql).await? {
            let (Some(namespace), Some(name), Some(labels)) = (
                row.first().cloned().flatten(),
                row.get(1).cloned().flatten(),
                row.get(2).cloned().flatten(),
            ) else {
                continue;
            };
            let mut enum_type = EnumType::new(name, labels.split('\u{1}').map(ToOwned::to_owned));
            if namespace != "public" {
                enum_type = enum_type.in_schema(namespace);
            }
            schema.add_enum(enum_type);
        }
        Ok(())
    }

    async fn read_extensions(connection: &mut Connection, schema: &mut Schema) -> Result<()> {
        let sql = "SELECT extname FROM pg_extension WHERE extname <> 'plpgsql' ORDER BY extname";
        for row in connection.fetch_text(sql).await? {
            if let Some(name) = row.first().cloned().flatten() {
                schema.add_extension(name);
            }
        }
        Ok(())
    }

    async fn read_columns(
        connection: &mut Connection,
        scope: &str,
    ) -> Result<BTreeMap<String, Table>> {
        let sql = format!(
            "SELECT n.nspname, c.relname, a.attname, \
                   format_type(a.atttypid, a.atttypmod), \
                   cast(a.attnotnull as text), \
                   pg_get_expr(d.adbin, d.adrelid), \
                   cast(a.attidentity as text), \
                   cast(a.attgenerated as text), \
                   obj_description(c.oid, 'pg_class'), \
                   col_description(c.oid, a.attnum) \
                   FROM pg_attribute a \
                   JOIN pg_class c ON c.oid = a.attrelid \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                   LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                   WHERE c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped \
                   AND {scope} \
                   ORDER BY n.nspname, c.relname, a.attnum"
        );

        let mut tables: BTreeMap<String, Table> = BTreeMap::new();
        for row in connection.fetch_text(&sql).await? {
            let get = |index: usize| row.get(index).cloned().flatten();
            let (Some(namespace), Some(table_name), Some(column_name), Some(type_name)) =
                (get(0), get(1), get(2), get(3))
            else {
                continue;
            };
            if is_moso_table(&table_name) {
                continue;
            }

            let key = if namespace == "public" {
                table_name.clone()
            } else {
                format!("{namespace}.{table_name}")
            };
            let table = tables.entry(key).or_insert_with(|| {
                let mut table = Table::new(table_name.clone());
                if namespace != "public" {
                    table = table.in_schema(namespace.clone());
                }
                if let Some(comment) = get(8) {
                    table = table.with_comment(comment);
                }
                table
            });

            let default = get(5);
            let identity = get(6).filter(|value| !value.is_empty());
            let generated = get(7).filter(|value| !value.is_empty());

            // A `bigserial` is a `bigint` whose default is its own sequence.
            // Reporting the pair as PostgreSQL stores it would make every
            // snapshot look drifted.
            let is_serial = default
                .as_deref()
                .is_some_and(|value| value.starts_with("nextval("));
            let data_type = match (is_serial, type_name.as_str()) {
                (true, "smallint") => DataType::SmallSerial,
                (true, "integer") => DataType::Serial,
                (true, "bigint") => DataType::BigSerial,
                _ => crate::schema::parse(&type_name)?,
            };

            let mut column = Column::new(column_name, data_type);
            if !super::reads_as_true(get(4).as_deref()) {
                column = column.nullable();
            }
            match (&generated, &default) {
                // A generated column's "default" IS its generation expression.
                (Some(_), Some(expression)) => {
                    column =
                        column.generated_as(crate::schema::Generated::stored(expression.clone()));
                }
                (None, Some(default)) if !is_serial => {
                    column = column.with_default(normalise_default(default));
                }
                _ => {}
            }
            if let Some(kind) = identity {
                column = column.identity(if kind == "a" {
                    crate::schema::IdentityKind::Always
                } else {
                    crate::schema::IdentityKind::ByDefault
                });
            }
            if let Some(comment) = get(9) {
                column = column.with_comment(comment);
            }
            table.add_column(column);
        }
        Ok(tables)
    }

    async fn read_indexes(
        connection: &mut Connection,
        scope: &str,
        tables: &mut BTreeMap<String, Table>,
    ) -> Result<()> {
        let sql = format!(
            "SELECT n.nspname, t.relname, i.relname, \
                   cast(ix.indisunique as text), cast(ix.indisprimary as text), am.amname, \
                   pg_get_expr(ix.indpred, ix.indrelid), \
                   cast(ix.indnkeyatts as text), \
                   (SELECT conname FROM pg_constraint WHERE conindid = i.oid LIMIT 1), \
                   (SELECT string_agg(pg_get_indexdef(i.oid, k, true), chr(1) ORDER BY k) \
                      FROM generate_series(1, ix.indnkeyatts) k), \
                   cast(ix.indoption as text) \
                   FROM pg_index ix \
                   JOIN pg_class i ON i.oid = ix.indexrelid \
                   JOIN pg_class t ON t.oid = ix.indrelid \
                   JOIN pg_namespace n ON n.oid = t.relnamespace \
                   JOIN pg_am am ON am.oid = i.relam \
                   WHERE {scope} \
                   ORDER BY n.nspname, t.relname, i.relname"
        );

        for row in connection.fetch_text(&sql).await? {
            let get = |index: usize| row.get(index).cloned().flatten();
            let (Some(namespace), Some(table_name), Some(index_name)) = (get(0), get(1), get(2))
            else {
                continue;
            };
            if is_moso_table(&table_name) {
                continue;
            }
            let key = if namespace == "public" {
                table_name
            } else {
                format!("{namespace}.{table_name}")
            };
            let Some(table) = tables.get_mut(&key) else {
                continue;
            };

            // `pg_get_indexdef(oid, n, …)` returns the key expression *only*:
            // the direction and the nulls placement live in `indoption`, a bit
            // per key column. Reading the text alone loses every `DESC`.
            let options: Vec<i64> = get(10)
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|raw| raw.parse().ok())
                .collect();
            let parts: Vec<_> = get(9)
                .unwrap_or_default()
                .split('\u{1}')
                .filter(|part| !part.is_empty())
                .enumerate()
                .map(|(at, part)| {
                    let mut key = parse_index_part(part);
                    if let Some(option) = options.get(at) {
                        // Bit 0 is DESC, bit 1 is NULLS FIRST.
                        if option & 1 == 1 {
                            key = key.sorted(super::Sort::Desc);
                        }
                        key = key.nulls(if option & 2 == 2 {
                            super::NullsOrder::First
                        } else {
                            super::NullsOrder::Last
                        });
                    }
                    key
                })
                .collect();

            if super::reads_as_true(get(4).as_deref()) {
                table.set_primary_key(
                    parts
                        .iter()
                        .filter_map(|part| part.column_name().map(ToOwned::to_owned))
                        .collect::<Vec<_>>(),
                );
                continue;
            }

            let mut index = Index::over(index_name, parts);
            if super::reads_as_true(get(3).as_deref()) {
                index = index.unique();
            }
            if get(8).is_some() {
                index = index.backing_a_constraint();
            }
            if let Some(method) = get(5)
                && method != "btree"
            {
                index = index.using(method);
            }
            if let Some(predicate) = get(6) {
                index = index.r#where(normalise_predicate(&predicate));
            }
            table.add_index(index);
        }
        Ok(())
    }

    async fn read_constraints(
        connection: &mut Connection,
        scope: &str,
        tables: &mut BTreeMap<String, Table>,
    ) -> Result<()> {
        let sql = format!(
            "SELECT n.nspname, t.relname, con.conname, cast(con.contype as text), \
                   pg_get_constraintdef(con.oid) \
                   FROM pg_constraint con \
                   JOIN pg_class t ON t.oid = con.conrelid \
                   JOIN pg_namespace n ON n.oid = t.relnamespace \
                   WHERE con.contype IN ('f', 'c') \
                   AND {scope} \
                   ORDER BY n.nspname, t.relname, con.conname"
        );

        for row in connection.fetch_text(&sql).await? {
            let get = |index: usize| row.get(index).cloned().flatten();
            let (Some(namespace), Some(table_name), Some(name), Some(kind), Some(definition)) =
                (get(0), get(1), get(2), get(3), get(4))
            else {
                continue;
            };
            if is_moso_table(&table_name) {
                continue;
            }
            let key = if namespace == "public" {
                table_name
            } else {
                format!("{namespace}.{table_name}")
            };
            let Some(table) = tables.get_mut(&key) else {
                continue;
            };

            if kind == "c" {
                // `CHECK ((n > 0))` — one layer of the parentheses is the
                // catalogue's, and one may be the author's.
                let body = definition
                    .trim_start_matches("CHECK")
                    .trim()
                    .trim_end_matches("NOT VALID")
                    .trim();
                table.add_check(Check::new(name, normalise_predicate(body)));
                continue;
            }
            if let Some(foreign_key) = parse_foreign_key(&name, &definition) {
                table.add_foreign_key(foreign_key);
            }
        }
        Ok(())
    }

    /// `FOREIGN KEY (author_id) REFERENCES users(id) ON DELETE CASCADE`
    fn parse_foreign_key(name: &str, definition: &str) -> Option<ForeignKey> {
        let rest = definition.strip_prefix("FOREIGN KEY ")?;
        let (columns, rest) = take_parenthesised(rest)?;
        let rest = rest.trim().strip_prefix("REFERENCES ")?;
        let open = rest.find('(')?;
        let target = unquote(rest[..open].trim());
        let (target_columns, tail) = take_parenthesised(&rest[open..])?;

        let mut foreign_key = ForeignKey::new(
            name,
            columns.split(',').map(|c| unquote(c.trim())),
            target,
            target_columns.split(',').map(|c| unquote(c.trim())),
        );
        for (clause, set) in [("ON DELETE ", true), ("ON UPDATE ", false)] {
            if let Some(at) = tail.find(clause) {
                let action = parse_action(&tail[at + clause.len()..]);
                foreign_key = if set {
                    foreign_key.on_delete(action)
                } else {
                    foreign_key.on_update(action)
                };
            }
        }
        if tail.contains("DEFERRABLE") {
            foreign_key = foreign_key.deferrable(tail.contains("INITIALLY DEFERRED"));
        }
        Some(foreign_key)
    }

    fn take_parenthesised(raw: &str) -> Option<(&str, &str)> {
        let rest = raw.trim().strip_prefix('(')?;
        let close = rest.find(')')?;
        Some((&rest[..close], &rest[close + 1..]))
    }

    fn parse_action(tail: &str) -> Action {
        let tail = tail.trim_start();
        if tail.starts_with("CASCADE") {
            Action::Cascade
        } else if tail.starts_with("SET NULL") {
            Action::SetNull
        } else if tail.starts_with("SET DEFAULT") {
            Action::SetDefault
        } else if tail.starts_with("RESTRICT") {
            Action::Restrict
        } else {
            Action::NoAction
        }
    }

    /// PostgreSQL re-prints a predicate from its parse tree, so `deleted_at is
    /// null` comes back as `(deleted_at IS NULL)`. Comparing those two as
    /// strings would report drift on a schema that has none, so both sides are
    /// squashed to the same shape before they meet.
    fn normalise_predicate(raw: &str) -> String {
        let mut value = raw.trim();
        while value.len() > 1
            && value.starts_with('(')
            && value.ends_with(')')
            && super::balanced(value)
        {
            value = value[1..value.len() - 1].trim();
        }
        value
            .split_whitespace()
            .map(|word| {
                let bare = word.trim_matches('"');
                if matches!(
                    bare.to_ascii_uppercase().as_str(),
                    "IS" | "NOT" | "NULL" | "AND" | "OR" | "TRUE" | "FALSE" | "IN" | "LIKE"
                ) {
                    bare.to_ascii_lowercase()
                } else {
                    bare.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

mod sqlite {
    use super::{
        Check, Column, ForeignKey, Index, Schema, Table, is_moso_table, normalise_default,
        parse_index_part, unquote,
    };
    use crate::conn::Connection;
    use crate::emit::quote_literal;
    use crate::error::Result;
    use moso_sql::DataType;

    pub(super) async fn read(connection: &mut Connection) -> Result<Schema> {
        let mut schema = Schema::empty();
        let listing = connection
            .fetch_text("SELECT name, sql FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .await?;

        for row in listing {
            let (Some(name), definition) = (
                row.first().cloned().flatten(),
                row.get(1).cloned().flatten().unwrap_or_default(),
            ) else {
                continue;
            };
            if is_moso_table(&name) {
                continue;
            }
            schema.add_table(read_table(connection, &name, &definition).await?);
        }
        Ok(schema)
    }

    async fn read_table(
        connection: &mut Connection,
        name: &str,
        definition: &str,
    ) -> Result<Table> {
        let mut table = Table::new(name);
        let mut primary_key: Vec<(i64, String)> = Vec::new();

        let info = connection
            .fetch_text(&format!("PRAGMA table_info({})", quote_literal(name)))
            .await?;
        let autoincrement = definition.to_ascii_uppercase().contains("AUTOINCREMENT");

        for row in info {
            let get = |index: usize| row.get(index).cloned().flatten();
            let (Some(column_name), Some(declared)) = (get(1), get(2)) else {
                continue;
            };
            let not_null = super::reads_as_true(get(3).as_deref());
            let default = get(4);
            let pk_position: i64 = get(5).and_then(|raw| raw.parse().ok()).unwrap_or(0);

            let declared = declared.trim().to_ascii_lowercase();
            let data_type = if autoincrement && pk_position == 1 && declared == "integer" {
                // `INTEGER PRIMARY KEY AUTOINCREMENT` is how SQLite spells a
                // serial, and it is what the emitter writes for one.
                DataType::BigSerial
            } else {
                crate::schema::parse(&declared)?
            };

            let mut column = Column::new(column_name.clone(), data_type);
            if !not_null && pk_position == 0 {
                column = column.nullable();
            }
            if let Some(default) = default {
                column = column.with_default(normalise_default(&default));
            }
            table.add_column(column);
            if pk_position > 0 {
                primary_key.push((pk_position, column_name));
            }
        }

        primary_key.sort_by_key(|(position, _)| *position);
        table.set_primary_key(
            primary_key
                .into_iter()
                .map(|(_, name)| name)
                .collect::<Vec<_>>(),
        );

        read_indexes(connection, name, &mut table).await?;
        read_foreign_keys(connection, name, &mut table).await?;
        for check in parse_checks(definition) {
            table.add_check(check);
        }
        Ok(table)
    }

    async fn read_indexes(
        connection: &mut Connection,
        table_name: &str,
        table: &mut Table,
    ) -> Result<()> {
        let list = connection
            .fetch_text(&format!("PRAGMA index_list({})", quote_literal(table_name)))
            .await?;

        for row in list {
            let get = |index: usize| row.get(index).cloned().flatten();
            let Some(index_name) = get(1) else { continue };
            let unique = super::reads_as_true(get(2).as_deref());
            let origin = get(3).unwrap_or_default();
            let partial = super::reads_as_true(get(4).as_deref());

            // A `pk` index is the primary key, which `table_info` already gave
            // us; an implicit index over an expression cannot be described by
            // `index_info`, so it is read from `sqlite_master` instead.
            if origin == "pk" {
                continue;
            }

            let definition = connection
                .fetch_text(&format!(
                    "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = {}",
                    quote_literal(&index_name)
                ))
                .await?
                .first()
                .and_then(|row| row.first().cloned().flatten());

            let parts = match definition.as_deref().and_then(index_columns) {
                Some(parts) => parts,
                None => {
                    let info = connection
                        .fetch_text(&format!(
                            "PRAGMA index_info({})",
                            quote_literal(&index_name)
                        ))
                        .await?;
                    info.iter()
                        .filter_map(|row| row.get(2).cloned().flatten())
                        .map(super::IndexPart::column)
                        .collect()
                }
            };
            if parts.is_empty() {
                continue;
            }

            let mut index = Index::over(index_name, parts);
            if unique {
                index = index.unique();
            }
            if origin == "u" {
                index = index.backing_a_constraint();
            }
            if partial && let Some(predicate) = definition.as_deref().and_then(index_predicate) {
                index = index.r#where(predicate);
            }
            table.add_index(index);
        }
        Ok(())
    }

    async fn read_foreign_keys(
        connection: &mut Connection,
        table_name: &str,
        table: &mut Table,
    ) -> Result<()> {
        let list = connection
            .fetch_text(&format!(
                "PRAGMA foreign_key_list({})",
                quote_literal(table_name)
            ))
            .await?;

        // SQLite stores no constraint names, so the name is synthesised the
        // same way the generator names one. That is what lets a round trip
        // compare equal.
        /// One row of `PRAGMA foreign_key_list`: the target table, the local
        /// column, the target column, and the two referential actions.
        type ForeignKeyPart = (String, String, String, String, String);

        let mut grouped: std::collections::BTreeMap<String, Vec<ForeignKeyPart>> =
            std::collections::BTreeMap::new();
        for row in list {
            let get = |index: usize| row.get(index).cloned().flatten().unwrap_or_default();
            grouped
                .entry(get(0))
                .or_default()
                .push((get(2), get(3), get(4), get(5), get(6)));
        }

        for parts in grouped.into_values() {
            let Some((target, _, _, on_update, on_delete)) = parts.first().cloned() else {
                continue;
            };
            let columns: Vec<String> = parts.iter().map(|part| part.1.clone()).collect();
            let target_columns: Vec<String> = parts.iter().map(|part| part.2.clone()).collect();
            let name = format!("{table_name}_{}_fkey", columns.join("_"));

            let mut foreign_key = ForeignKey::new(name, columns, target, target_columns);
            if on_delete != "NO ACTION" {
                foreign_key = foreign_key.on_delete(action(&on_delete));
            }
            if on_update != "NO ACTION" {
                foreign_key = foreign_key.on_update(action(&on_update));
            }
            table.add_foreign_key(foreign_key);
        }
        Ok(())
    }

    fn action(raw: &str) -> super::Action {
        match raw {
            "CASCADE" => super::Action::Cascade,
            "SET NULL" => super::Action::SetNull,
            "SET DEFAULT" => super::Action::SetDefault,
            "RESTRICT" => super::Action::Restrict,
            _ => super::Action::NoAction,
        }
    }

    /// The key columns from a `CREATE INDEX … ON t (a, b DESC)` statement.
    fn index_columns(definition: &str) -> Option<Vec<super::IndexPart>> {
        let on = definition.find(" ON ")?;
        let rest = &definition[on + 4..];
        let open = rest.find('(')?;
        let close = matching_paren(rest, open)?;
        Some(
            split_top_level(&rest[open + 1..close])
                .into_iter()
                .map(|part| parse_index_part(&part))
                .collect(),
        )
    }

    fn index_predicate(definition: &str) -> Option<String> {
        let at = definition.to_ascii_uppercase().rfind(" WHERE ")?;
        Some(definition[at + 7..].trim().to_owned())
    }

    /// Named `CHECK` constraints from a `CREATE TABLE` statement.
    fn parse_checks(definition: &str) -> Vec<Check> {
        let mut checks = Vec::new();
        let upper = definition.to_ascii_uppercase();
        let mut cursor = 0;
        while let Some(at) = upper[cursor..].find("CONSTRAINT ") {
            let start = cursor + at + "CONSTRAINT ".len();
            let rest = &definition[start..];
            let Some((name, tail)) = rest.split_once(' ') else {
                break;
            };
            cursor = start;
            if !tail.trim_start().to_ascii_uppercase().starts_with("CHECK") {
                continue;
            }
            let tail = tail.trim_start();
            let Some(open) = tail.find('(') else { continue };
            let Some(close) = matching_paren(tail, open) else {
                continue;
            };
            checks.push(Check::new(
                unquote(name),
                tail[open + 1..close].trim().to_owned(),
            ));
        }
        checks
    }

    fn matching_paren(raw: &str, open: usize) -> Option<usize> {
        let mut depth = 0_i32;
        for (index, ch) in raw.char_indices().skip(open) {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(index);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn split_top_level(raw: &str) -> Vec<String> {
        let mut parts = Vec::new();
        let mut depth = 0_i32;
        let mut current = String::new();
        for ch in raw.chars() {
            match ch {
                '(' => {
                    depth += 1;
                    current.push(ch);
                }
                ')' => {
                    depth -= 1;
                    current.push(ch);
                }
                ',' if depth == 0 => parts.push(std::mem::take(&mut current).trim().to_owned()),
                _ => current.push(ch),
            }
        }
        let last = current.trim();
        if !last.is_empty() {
            parts.push(last.to_owned());
        }
        parts
    }
}

/// The table names in a schema, for a caller that wants to compare two sets
/// without holding both whole schemas.
///
/// ```
/// use moso_migrate::introspect::table_names;
/// use moso_migrate::schema::{Schema, Table};
///
/// let mut schema = Schema::empty();
/// schema.add_table(Table::new("users"));
/// assert_eq!(table_names(&schema), ["users"]);
/// ```
#[must_use]
pub fn table_names(schema: &Schema) -> Vec<String> {
    schema.tables().map(Table::qualified_name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moso_tables_are_not_part_of_the_application_schema() {
        assert!(is_moso_table("moso_migrations"));
        assert!(is_moso_table("moso_migrations_lock"));
        assert!(is_moso_table("sqlite_sequence"));
        assert!(is_moso_table("users__moso_new"));
        assert!(!is_moso_table("users"));
    }

    #[test]
    fn index_parts_split_into_expression_and_ordering() {
        let plain = parse_index_part("email");
        assert_eq!(plain.column_name(), Some("email"));
        assert_eq!(plain.sort(), None);

        let sorted = parse_index_part("created_at DESC NULLS LAST");
        assert_eq!(sorted.column_name(), Some("created_at"));
        assert_eq!(sorted.sort(), Some(Sort::Desc));
        assert_eq!(sorted.nulls_order(), Some(NullsOrder::Last));

        let expression = parse_index_part("lower(email)");
        assert_eq!(expression.column_name(), None);
        assert_eq!(expression.expr(), "lower(email)");

        let quoted = parse_index_part("\"order\"");
        assert_eq!(quoted.column_name(), Some("order"));

        let ops = parse_index_part("doc jsonb_path_ops");
        assert_eq!(ops.column_name(), Some("doc"));
        assert_eq!(ops.ops(), Some("jsonb_path_ops"));
    }

    #[test]
    fn defaults_normalise_across_the_two_catalogues() {
        assert_eq!(normalise_default("'en'::text"), "'en'");
        assert_eq!(normalise_default("'en'::character varying"), "'en'");
        assert_eq!(normalise_default("(0)"), "0");
        assert_eq!(normalise_default("CURRENT_TIMESTAMP"), "now()");
        assert_eq!(normalise_default("now()"), "now()");
        assert_eq!(normalise_default("TRUE"), "true");
        assert_eq!(normalise_default("'{}'::jsonb"), "'{}'");
        // A sequence default is left alone: the cast is inside the call, and
        // the introspector discards the whole default for a serial column
        // anyway.
        assert_eq!(
            normalise_default("nextval('users_id_seq'::regclass)"),
            "nextval('users_id_seq'::regclass)"
        );
    }

    #[test]
    fn balanced_parentheses_are_recognised() {
        assert!(balanced("(a)"));
        assert!(balanced("(a(b))"));
        assert!(!balanced("(a)(b)"));
        assert!(!balanced("(a"));
    }

    #[test]
    fn bare_identifiers_are_distinguished_from_expressions() {
        assert!(is_bare_identifier("email"));
        assert!(is_bare_identifier("created_at"));
        assert!(!is_bare_identifier("lower(email)"));
        assert!(!is_bare_identifier("1"));
        assert!(!is_bare_identifier(""));
    }

    #[test]
    fn quoted_identifiers_unquote() {
        assert_eq!(unquote("\"order\""), "order");
        assert_eq!(unquote("order"), "order");
        assert_eq!(unquote("\"a\"\"b\""), "a\"b");
    }

    #[tokio::test]
    async fn a_sqlite_schema_reads_back() {
        let mut connection = crate::conn::Connection::open("sqlite::memory:")
            .await
            .expect("opens");
        connection
            .execute(
                "CREATE TABLE \"users\" (\n  \
                 \"id\" integer PRIMARY KEY AUTOINCREMENT,\n  \
                 \"email\" text NOT NULL,\n  \
                 \"bio\" text,\n  \
                 \"locale\" text NOT NULL DEFAULT 'en',\n  \
                 CONSTRAINT \"users_email_key\" UNIQUE (\"email\")\n)",
            )
            .await
            .expect("creates");
        connection
            .execute("CREATE INDEX \"idx_users_locale\" ON \"users\" (\"locale\")")
            .await
            .expect("indexes");

        let schema = read_schema(&mut connection).await.expect("reads");
        let users = schema.table("users").expect("users");
        assert_eq!(users.primary_key(), ["id"]);
        assert_eq!(users.columns().len(), 4);
        assert_eq!(users.column("id").expect("id").type_name(), "bigserial");
        assert!(!users.column("email").expect("email").is_nullable());
        assert!(users.column("bio").expect("bio").is_nullable());
        assert_eq!(
            users.column("locale").expect("locale").default(),
            Some("'en'")
        );
        assert!(users.index("idx_users_locale").is_some());
        let unique = users
            .index("sqlite_autoindex_users_1")
            .or_else(|| users.indexes().find(|index| index.is_unique()));
        assert!(
            unique.is_some_and(Index::is_unique),
            "the unique constraint is an index"
        );

        connection.close().await;
    }

    #[tokio::test]
    async fn sqlite_foreign_keys_and_checks_read_back() {
        let mut connection = crate::conn::Connection::open("sqlite::memory:")
            .await
            .expect("opens");
        connection
            .execute("CREATE TABLE \"users\" (\"id\" integer PRIMARY KEY AUTOINCREMENT)")
            .await
            .expect("creates");
        connection
            .execute(
                "CREATE TABLE \"posts\" (\n  \
                 \"id\" integer PRIMARY KEY AUTOINCREMENT,\n  \
                 \"author_id\" bigint NOT NULL,\n  \
                 CONSTRAINT \"posts_id_positive\" CHECK (\"id\" > 0),\n  \
                 CONSTRAINT \"posts_author_id_fkey\" FOREIGN KEY (\"author_id\") \
                 REFERENCES \"users\" (\"id\") ON DELETE CASCADE\n)",
            )
            .await
            .expect("creates");

        let schema = read_schema(&mut connection).await.expect("reads");
        let posts = schema.table("posts").expect("posts");
        let fk = posts
            .foreign_key("posts_author_id_fkey")
            .expect("the synthesised name matches the generator's");
        assert_eq!(fk.target_table(), "users");
        assert_eq!(fk.delete_action(), Some(Action::Cascade));
        assert_eq!(
            posts
                .check("posts_id_positive")
                .expect("check")
                .expression(),
            "\"id\" > 0"
        );
        connection.close().await;
    }

    #[tokio::test]
    async fn the_ledger_is_not_part_of_the_schema() {
        let mut connection = crate::conn::Connection::open("sqlite::memory:")
            .await
            .expect("opens");
        crate::ledger::Ledger::ensure(&mut connection)
            .await
            .expect("creates");
        let schema = read_schema(&mut connection).await.expect("reads");
        assert!(schema.table("moso_migrations").is_none());
        connection.close().await;
    }
}
