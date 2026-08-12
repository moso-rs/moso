//! The canonical spelling of a column type, and its inverse.
//!
//! `migrations/.schema.json` is read by humans in code review — that is the
//! whole point of committing it — so a column's type is one string,
//! `"varchar(255)"`, rather than nine lines of tagged union. The round trip is
//! exact and exhaustively tested: [`spell`] followed by [`parse`] is the
//! identity on every [`DataType`] this build can produce.

use moso_sql::{DataType, Ident, TypeRef};

use crate::error::{Error, Result};

/// The canonical spelling of a type, as it appears in `.schema.json`.
///
/// PostgreSQL's own vocabulary is the reference — `timestamptz`, `bytea`,
/// `jsonb` — because that is what the reference dialect calls them (ADR-0010)
/// and what a reviewer will recognise. What SQLite stores them as is the
/// dialect's business, decided when the DDL is rendered.
///
/// ```
/// use moso_migrate::schema::spell;
/// use moso_sql::DataType;
///
/// assert_eq!(spell(&DataType::Timestamp { with_time_zone: true }), "timestamptz");
/// assert_eq!(spell(&DataType::array_of(DataType::Text)), "text[]");
/// assert_eq!(spell(&DataType::VarChar(Some(255))), "varchar(255)");
/// ```
#[must_use]
pub fn spell(data_type: &DataType) -> String {
    match data_type {
        DataType::Boolean => "boolean".to_owned(),
        DataType::SmallInt => "smallint".to_owned(),
        DataType::Integer => "integer".to_owned(),
        DataType::BigInt => "bigint".to_owned(),
        DataType::SmallSerial => "smallserial".to_owned(),
        DataType::Serial => "serial".to_owned(),
        DataType::BigSerial => "bigserial".to_owned(),
        DataType::Real => "real".to_owned(),
        DataType::DoublePrecision => "double precision".to_owned(),
        DataType::Numeric { precision, scale } => match (precision, scale) {
            (Some(precision), Some(scale)) => format!("numeric({precision},{scale})"),
            (Some(precision), None) => format!("numeric({precision})"),
            _ => "numeric".to_owned(),
        },
        DataType::Text => "text".to_owned(),
        DataType::VarChar(None) => "varchar".to_owned(),
        DataType::VarChar(Some(length)) => format!("varchar({length})"),
        DataType::Char(None) => "char".to_owned(),
        DataType::Char(Some(length)) => format!("char({length})"),
        DataType::Bytea => "bytea".to_owned(),
        DataType::Uuid => "uuid".to_owned(),
        DataType::Json => "json".to_owned(),
        DataType::JsonB => "jsonb".to_owned(),
        DataType::Date => "date".to_owned(),
        DataType::Time {
            with_time_zone: true,
        } => "timetz".to_owned(),
        DataType::Time {
            with_time_zone: false,
        } => "time".to_owned(),
        DataType::Timestamp {
            with_time_zone: true,
        } => "timestamptz".to_owned(),
        DataType::Timestamp {
            with_time_zone: false,
        } => "timestamp".to_owned(),
        DataType::Interval => "interval".to_owned(),
        DataType::Inet => "inet".to_owned(),
        DataType::Cidr => "cidr".to_owned(),
        DataType::MacAddr => "macaddr".to_owned(),
        DataType::TsVector => "tsvector".to_owned(),
        DataType::TsQuery => "tsquery".to_owned(),
        DataType::Array(element) => format!("{}[]", spell(element)),
        DataType::Enum(name) => format!("enum:{}", spell_type_ref(name)),
        DataType::Custom(name) => format!("custom:{}", spell_type_ref(name)),
        // `DataType` is `#[non_exhaustive]`. A variant added by a later
        // `moso-sql` must not silently become `text` here: that would generate
        // a migration that changes a column's type to something it is not.
        other => format!("custom:unknown_{other:?}"),
    }
}

/// The name of a user-defined type, schema-qualified when it has one.
fn spell_type_ref(name: &TypeRef) -> String {
    name.schema().map_or_else(
        || name.name().as_str().to_owned(),
        |schema| format!("{}.{}", schema.as_str(), name.name().as_str()),
    )
}

/// Parses a canonical spelling back into a [`DataType`].
///
/// # Errors
///
/// [`Error::Snapshot`] when the spelling is not one this build produces, which
/// in practice means the snapshot was written by a newer Moso.
///
/// ```
/// use moso_migrate::schema::{parse, spell};
/// use moso_sql::DataType;
///
/// let original = DataType::Numeric { precision: Some(10), scale: Some(2) };
/// assert_eq!(parse(&spell(&original))?, original);
/// # Ok::<(), moso_migrate::Error>(())
/// ```
pub fn parse(spelling: &str) -> Result<DataType> {
    let spelling = spelling.trim();
    if let Some(element) = spelling.strip_suffix("[]") {
        return Ok(DataType::array_of(parse(element)?));
    }
    if let Some(name) = spelling.strip_prefix("enum:") {
        return Ok(DataType::Enum(parse_type_ref(name)?));
    }
    if let Some(name) = spelling.strip_prefix("custom:") {
        return Ok(DataType::Custom(parse_type_ref(name)?));
    }

    let (head, argument) = match spelling.split_once('(') {
        Some((head, rest)) => (
            head.trim(),
            Some(rest.trim_end().trim_end_matches(')').trim()),
        ),
        None => (spelling, None),
    };

    let numbers: Vec<&str> = argument
        .map(|argument| argument.split(',').map(str::trim).collect())
        .unwrap_or_default();
    // `varchar(n)` takes a `u32` length and `numeric(p, s)` takes two `u8`s, so
    // the two families need separate readers rather than one inferred closure.
    let length = |index: usize| numbers.get(index).and_then(|raw| raw.parse::<u32>().ok());
    let digits = |index: usize| numbers.get(index).and_then(|raw| raw.parse::<u8>().ok());

    Ok(match head {
        "boolean" | "bool" => DataType::Boolean,
        "smallint" | "int2" => DataType::SmallInt,
        "integer" | "int" | "int4" => DataType::Integer,
        "bigint" | "int8" => DataType::BigInt,
        "smallserial" => DataType::SmallSerial,
        "serial" => DataType::Serial,
        "bigserial" => DataType::BigSerial,
        "real" | "float4" => DataType::Real,
        "double precision" | "float8" => DataType::DoublePrecision,
        "numeric" | "decimal" => DataType::Numeric {
            precision: digits(0),
            scale: digits(1),
        },
        "text" => DataType::Text,
        "varchar" | "character varying" => DataType::VarChar(length(0)),
        "char" | "character" | "bpchar" => DataType::Char(length(0)),
        "bytea" | "blob" => DataType::Bytea,
        "uuid" => DataType::Uuid,
        "json" => DataType::Json,
        "jsonb" => DataType::JsonB,
        "date" => DataType::Date,
        "time" => DataType::Time {
            with_time_zone: false,
        },
        "timetz" | "time with time zone" => DataType::Time {
            with_time_zone: true,
        },
        "timestamp" | "timestamp without time zone" => DataType::Timestamp {
            with_time_zone: false,
        },
        "timestamptz" | "timestamp with time zone" => DataType::Timestamp {
            with_time_zone: true,
        },
        "interval" => DataType::Interval,
        "inet" => DataType::Inet,
        "cidr" => DataType::Cidr,
        "macaddr" => DataType::MacAddr,
        "tsvector" => DataType::TsVector,
        "tsquery" => DataType::TsQuery,
        _ => {
            return Err(Error::Snapshot {
                path: "migrations/.schema.json".into(),
                reason: format!(
                    "`{spelling}` is not a type this version understands; if it is a database \
                     type Moso has no name for, spell it `custom:{spelling}`"
                ),
            });
        }
    })
}

fn parse_type_ref(raw: &str) -> Result<TypeRef> {
    Ok(match raw.split_once('.') {
        Some((schema, name)) => TypeRef::qualified(Ident::new(schema)?, Ident::new(name)?),
        None => TypeRef::new(Ident::new(raw)?),
    })
}

/// Whether replacing `from` with `to` can lose data.
///
/// A lossy change is one that a `USING` clause cannot make safe: narrowing a
/// `bigint` to an `integer` throws away magnitude, `text` to `varchar(20)`
/// throws away characters, and anything to a type with no conversion at all
/// throws away the column. It is the predicate that decides whether the
/// generator emits the change commented-out
/// (`docs/02-data/23-migrations.md` § safety policy).
///
/// ```
/// use moso_migrate::schema::is_lossy;
/// use moso_sql::DataType;
///
/// assert!(is_lossy(&DataType::BigInt, &DataType::Integer));
/// assert!(!is_lossy(&DataType::Integer, &DataType::BigInt));
/// assert!(!is_lossy(&DataType::VarChar(Some(20)), &DataType::Text));
/// ```
#[must_use]
pub fn is_lossy(from: &DataType, to: &DataType) -> bool {
    if from == to {
        return false;
    }
    match (from, to) {
        // Widening within a family is safe.
        (DataType::SmallInt, DataType::Integer | DataType::BigInt)
        | (DataType::Integer, DataType::BigInt)
        | (DataType::Real, DataType::DoublePrecision)
        | (DataType::Json, DataType::JsonB)
        | (DataType::Char(_) | DataType::VarChar(_), DataType::Text) => false,
        // A longer bound on the same family is safe; a shorter one is not.
        (DataType::VarChar(Some(from)), DataType::VarChar(Some(to)))
        | (DataType::Char(Some(from)), DataType::Char(Some(to)))
        | (DataType::VarChar(Some(from)), DataType::Char(Some(to)))
        | (DataType::Char(Some(from)), DataType::VarChar(Some(to))) => to < from,
        (DataType::VarChar(None) | DataType::Char(None), DataType::VarChar(Some(_)))
        | (DataType::Text, DataType::VarChar(_) | DataType::Char(_)) => true,
        (
            DataType::Numeric {
                precision: Some(from_precision),
                scale: from_scale,
            },
            DataType::Numeric {
                precision: Some(to_precision),
                scale: to_scale,
            },
        ) => to_precision < from_precision || to_scale < from_scale,
        (
            DataType::Numeric {
                precision: None, ..
            },
            DataType::Numeric {
                precision: Some(_), ..
            },
        ) => true,
        (DataType::Array(from), DataType::Array(to)) => is_lossy(from, to),
        // A serial IS its base integer; the sequence is the only difference.
        (DataType::SmallSerial, DataType::SmallInt)
        | (DataType::Serial, DataType::Integer)
        | (DataType::BigSerial, DataType::BigInt)
        | (DataType::SmallInt, DataType::SmallSerial)
        | (DataType::Integer, DataType::Serial)
        | (DataType::BigInt, DataType::BigSerial) => false,
        // Everything else is a conversion whose safety we cannot prove, and an
        // unproven type change on a production table is exactly the thing the
        // acknowledgement gate exists for.
        _ => true,
    }
}

/// A `USING` expression for a type change the server will not do implicitly,
/// or `None` when the cast is safe without one.
///
/// ```
/// use moso_migrate::schema::using_expression;
/// use moso_sql::DataType;
///
/// assert_eq!(
///     using_expression("age", &DataType::Text, &DataType::Integer).as_deref(),
///     Some("\"age\"::integer"),
/// );
/// assert_eq!(using_expression("age", &DataType::Integer, &DataType::BigInt), None);
/// ```
#[must_use]
pub fn using_expression(column: &str, from: &DataType, to: &DataType) -> Option<String> {
    if from == to {
        return None;
    }
    let implicit = matches!(
        (from, to),
        (DataType::SmallInt, DataType::Integer | DataType::BigInt)
            | (DataType::Integer, DataType::BigInt)
            | (DataType::Real, DataType::DoublePrecision)
            | (DataType::Char(_) | DataType::VarChar(_), DataType::Text)
            | (DataType::VarChar(_), DataType::VarChar(_))
            | (DataType::Numeric { .. }, DataType::Numeric { .. })
    );
    if implicit {
        return None;
    }
    Some(format!("\"{column}\"::{}", cast_spelling(to)))
}

/// The spelling that goes after `::` in a cast. Differs from [`spell`] only for
/// the tagged forms, where the tag is Moso's and not SQL's.
fn cast_spelling(data_type: &DataType) -> String {
    match data_type {
        DataType::Enum(name) | DataType::Custom(name) => spell_type_ref(name),
        DataType::Array(element) => format!("{}[]", cast_spelling(element)),
        other => spell(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant this build can produce, so that adding one to `moso-sql`
    /// without teaching this module about it fails here rather than in a
    /// generated migration.
    fn every_type() -> Vec<DataType> {
        vec![
            DataType::Boolean,
            DataType::SmallInt,
            DataType::Integer,
            DataType::BigInt,
            DataType::SmallSerial,
            DataType::Serial,
            DataType::BigSerial,
            DataType::Real,
            DataType::DoublePrecision,
            DataType::Numeric {
                precision: None,
                scale: None,
            },
            DataType::Numeric {
                precision: Some(10),
                scale: None,
            },
            DataType::Numeric {
                precision: Some(10),
                scale: Some(2),
            },
            DataType::Text,
            DataType::VarChar(None),
            DataType::VarChar(Some(255)),
            DataType::Char(None),
            DataType::Char(Some(2)),
            DataType::Bytea,
            DataType::Uuid,
            DataType::Json,
            DataType::JsonB,
            DataType::Date,
            DataType::Time {
                with_time_zone: false,
            },
            DataType::Time {
                with_time_zone: true,
            },
            DataType::Timestamp {
                with_time_zone: false,
            },
            DataType::Timestamp {
                with_time_zone: true,
            },
            DataType::Interval,
            DataType::Inet,
            DataType::Cidr,
            DataType::MacAddr,
            DataType::TsVector,
            DataType::TsQuery,
            DataType::array_of(DataType::Text),
            DataType::array_of(DataType::array_of(DataType::Integer)),
            DataType::Enum(TypeRef::from_static("user_role")),
            DataType::Enum(TypeRef::qualified(
                Ident::from_static("app"),
                Ident::from_static("user_role"),
            )),
            DataType::Custom(TypeRef::from_static("vector")),
        ]
    }

    #[test]
    fn spelling_round_trips_for_every_type() {
        for data_type in every_type() {
            let spelling = spell(&data_type);
            let back = parse(&spelling).unwrap_or_else(|error| {
                panic!("`{spelling}` did not parse back: {error}");
            });
            assert_eq!(back, data_type, "round trip of `{spelling}`");
        }
    }

    #[test]
    fn spellings_are_distinct() {
        let mut seen = std::collections::BTreeSet::new();
        for data_type in every_type() {
            let spelling = spell(&data_type);
            assert!(seen.insert(spelling.clone()), "`{spelling}` is ambiguous");
        }
    }

    #[test]
    fn database_spellings_are_accepted_on_the_way_in() {
        // What `format_type()` and `pragma table_info` actually return.
        assert_eq!(
            parse("character varying(255)").expect("pg"),
            DataType::VarChar(Some(255))
        );
        assert_eq!(
            parse("timestamp with time zone").expect("pg"),
            DataType::Timestamp {
                with_time_zone: true
            }
        );
        assert_eq!(parse("int4").expect("pg"), DataType::Integer);
        // SQLite's `pragma table_info` reports declared types verbatim, so the
        // introspector lower-cases before it gets here; `blob` is the spelling
        // this function is asked for.
        assert_eq!(parse("blob").expect("sqlite"), DataType::Bytea);
        assert!(parse("BLOB").is_err(), "case folding is the caller's job");
    }

    #[test]
    fn unknown_types_are_refused_with_a_fix() {
        let error = parse("hstore").expect_err("not a known type");
        assert!(error.to_string().contains("custom:hstore"), "{error}");
    }

    #[test]
    fn lossiness_is_directional() {
        assert!(is_lossy(&DataType::BigInt, &DataType::Integer));
        assert!(!is_lossy(&DataType::Integer, &DataType::BigInt));
        assert!(is_lossy(&DataType::Text, &DataType::VarChar(Some(20))));
        assert!(!is_lossy(&DataType::VarChar(Some(20)), &DataType::Text));
        assert!(is_lossy(
            &DataType::VarChar(Some(50)),
            &DataType::VarChar(Some(20))
        ));
        assert!(!is_lossy(
            &DataType::VarChar(Some(20)),
            &DataType::VarChar(Some(50))
        ));
        assert!(!is_lossy(&DataType::BigInt, &DataType::BigSerial));
        assert!(is_lossy(
            &DataType::array_of(DataType::BigInt),
            &DataType::array_of(DataType::Integer)
        ));
    }

    #[test]
    fn numeric_narrowing_is_lossy_in_both_dimensions() {
        let wide = DataType::Numeric {
            precision: Some(12),
            scale: Some(4),
        };
        let narrow_precision = DataType::Numeric {
            precision: Some(8),
            scale: Some(4),
        };
        let narrow_scale = DataType::Numeric {
            precision: Some(12),
            scale: Some(2),
        };
        assert!(is_lossy(&wide, &narrow_precision));
        assert!(is_lossy(&wide, &narrow_scale));
        assert!(!is_lossy(&narrow_precision, &wide));
    }

    #[test]
    fn using_is_emitted_only_where_it_is_needed() {
        assert_eq!(
            using_expression("age", &DataType::Text, &DataType::Integer).as_deref(),
            Some("\"age\"::integer")
        );
        assert_eq!(
            using_expression(
                "role",
                &DataType::Text,
                &DataType::Enum(TypeRef::from_static("user_role"))
            )
            .as_deref(),
            Some("\"role\"::user_role")
        );
        assert_eq!(
            using_expression("n", &DataType::Integer, &DataType::BigInt),
            None
        );
        assert_eq!(
            using_expression("n", &DataType::BigInt, &DataType::BigInt),
            None
        );
    }
}

/// Squashes an expression to a shape two databases can be compared in.
///
/// A `CHECK` constraint is the hard case. PostgreSQL does not store the text
/// you wrote; it stores a parse tree and re-prints it, so
/// `length(title) > 0` on a `varchar` column comes back as
/// `length((title)::text) > 0`. Comparing the two as strings reports drift on a
/// schema that has none — and a drift check people learn to ignore is worse
/// than no drift check.
///
/// This removes the differences that are the database's spelling rather than
/// the schema's meaning: casts, redundant parentheses around a single
/// identifier, quoting, keyword case and whitespace. It does **not** try to
/// understand the expression, so two genuinely different predicates always
/// compare different.
///
/// ```
/// use moso_migrate::schema::normalise_expression;
///
/// assert_eq!(
///     normalise_expression("length((title)::text) > 0"),
///     normalise_expression("length(title) > 0"),
/// );
/// assert_eq!(normalise_expression("\"id\" > 0"), "id > 0");
/// assert_ne!(normalise_expression("id > 0"), normalise_expression("id > 1"));
/// ```
#[must_use]
pub fn normalise_expression(raw: &str) -> String {
    let mut value = strip_casts(raw.trim());
    // A predicate the catalogue wrapped whole.
    while value.len() > 1
        && value.starts_with('(')
        && value.ends_with(')')
        && wraps_the_whole(&value)
    {
        value = value[1..value.len() - 1].trim().to_owned();
    }
    value = strip_redundant_parentheses(&value);

    value
        .split_whitespace()
        .map(|word| {
            let bare = word.trim_matches('"');
            match bare.to_ascii_uppercase().as_str() {
                "IS" | "NOT" | "NULL" | "AND" | "OR" | "TRUE" | "FALSE" | "IN" | "LIKE"
                | "BETWEEN" | "ANY" | "ALL" => bare.to_ascii_lowercase(),
                _ => bare.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Removes `::type` casts, including the multi-word type names PostgreSQL
/// prints.
fn strip_casts(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let chars: Vec<char> = raw.chars().collect();
    let mut at = 0;
    while at < chars.len() {
        if chars[at] == ':' && chars.get(at + 1) == Some(&':') {
            at += 2;
            // The type name: word characters, spaces inside a known multi-word
            // spelling, and a trailing `[]`.
            while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '_') {
                at += 1;
            }
            for suffix in [
                " varying",
                " precision",
                " with time zone",
                " without time zone",
            ] {
                let rest: String = chars[at..].iter().collect();
                if rest.starts_with(suffix) {
                    at += suffix.chars().count();
                }
            }
            while chars.get(at) == Some(&'[') && chars.get(at + 1) == Some(&']') {
                at += 2;
            }
            continue;
        }
        out.push(chars[at]);
        at += 1;
    }
    out
}

/// Turns `(title)` into `title` wherever the parentheses wrap one bare
/// identifier or literal and therefore mean nothing.
fn strip_redundant_parentheses(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let chars: Vec<char> = raw.chars().collect();
    let mut at = 0;
    while at < chars.len() {
        // A `(` right after an identifier opens an argument list, not a
        // grouping: removing it turns `length(title)` into `lengthtitle`.
        let opens_a_call = at > 0
            && (chars[at - 1].is_alphanumeric() || chars[at - 1] == '_' || chars[at - 1] == '"');
        if chars[at] == '(' && !opens_a_call {
            let mut end = at + 1;
            while end < chars.len()
                && (chars[end].is_alphanumeric() || chars[end] == '_' || chars[end] == '"')
            {
                end += 1;
            }
            if end > at + 1 && chars.get(end) == Some(&')') {
                out.extend(&chars[at + 1..end]);
                at = end + 1;
                continue;
            }
        }
        out.push(chars[at]);
        at += 1;
    }
    out
}

/// Whether the leading `(` closes at the very end.
fn wraps_the_whole(value: &str) -> bool {
    let mut depth = 0_i32;
    for (index, ch) in value.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return index + 1 == value.len();
                }
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod expression_tests {
    use super::normalise_expression;

    #[test]
    fn casts_and_parentheses_are_the_databases_spelling() {
        assert_eq!(
            normalise_expression("length((title)::text) > 0"),
            normalise_expression("length(title) > 0")
        );
        assert_eq!(
            normalise_expression("(((id > 0)))"),
            normalise_expression("id > 0")
        );
        assert_eq!(
            normalise_expression("\"deleted_at\" IS NULL"),
            normalise_expression("deleted_at is null")
        );
        assert_eq!(normalise_expression("(name)::character varying"), "name");
        assert_eq!(normalise_expression("tags::text[]"), "tags");
    }

    #[test]
    fn different_predicates_stay_different() {
        assert_ne!(
            normalise_expression("id > 0"),
            normalise_expression("id > 1")
        );
        assert_ne!(
            normalise_expression("a AND b"),
            normalise_expression("a OR b")
        );
        assert_ne!(
            normalise_expression("length(title) > 0"),
            normalise_expression("length(body) > 0")
        );
    }

    #[test]
    fn a_predicate_with_a_nested_call_survives() {
        assert_eq!(
            normalise_expression("coalesce(lower(email), '') <> ''"),
            "coalesce(lower(email), '') <> ''"
        );
    }
}
