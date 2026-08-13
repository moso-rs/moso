//! Reading and writing `migrations/.schema.json`.
//!
//! Two properties matter and neither is negotiable. **Byte stability**: the
//! same schema must serialise to the same bytes on every machine, or every
//! `make-migration` produces a spurious diff in version control. **A refusal on
//! the way in**: a snapshot written by a newer Moso is rejected rather than
//! partially understood, because a field this build ignores is a schema element
//! it would then propose to drop.

use crate::error::{Error, Result};
use crate::schema::{FORMAT_VERSION, Schema};

/// The path a snapshot lives at, relative to the migrations directory.
pub(crate) const FILE_NAME: &str = ".schema.json";

/// Serialises with two-space indentation and a trailing newline.
pub(super) fn to_json(schema: &Schema) -> String {
    // `serde_json::to_string_pretty` cannot fail for a type with no maps keyed
    // by a non-string and no non-finite floats, which describes every type in
    // this module. The fallback keeps the signature infallible without a panic.
    let mut text = serde_json::to_string_pretty(schema)
        .unwrap_or_else(|_| String::from("{\n  \"format\": 1\n}"));
    text.push('\n');
    text
}

/// Parses a snapshot, refusing one from the future.
pub(super) fn from_json(text: &str) -> Result<Schema> {
    #[derive(serde::Deserialize)]
    struct JustTheFormat {
        #[serde(default = "one")]
        format: u32,
    }
    const fn one() -> u32 {
        1
    }

    let probe: JustTheFormat = serde_json::from_str(text).map_err(|error| Error::Snapshot {
        path: FILE_NAME.into(),
        reason: format!("the file is not valid JSON: {error}"),
    })?;
    if probe.format > FORMAT_VERSION {
        return Err(Error::Snapshot {
            path: FILE_NAME.into(),
            reason: format!(
                "it is format {} and this build understands format {FORMAT_VERSION}; \
                 upgrading Moso is the fix, not regenerating the snapshot",
                probe.format
            ),
        });
    }
    serde_json::from_str(text).map_err(|error| Error::Snapshot {
        path: FILE_NAME.into(),
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use moso_sql::DataType;

    use super::*;
    use crate::schema::{Column, Table};

    #[test]
    fn output_is_pretty_and_newline_terminated() {
        let mut schema = Schema::empty();
        let mut users = Table::new("users");
        users.add_column(Column::new("id", DataType::BigSerial));
        schema.add_table(users);

        let json = schema.to_json();
        assert!(json.ends_with("}\n"), "{json}");
        assert!(json.contains("\n  \"format\": 1"), "{json}");
        assert!(json.contains("\"type\": \"bigserial\""), "{json}");
    }

    #[test]
    fn a_newer_format_is_refused_with_the_fix() {
        let error = from_json("{\"format\": 99}").expect_err("from the future");
        let text = error.to_string();
        assert!(text.contains("format 99"), "{text}");
        assert!(text.contains("Upgrad") || text.contains("upgrad"), "{text}");
    }

    #[test]
    fn malformed_json_names_the_file() {
        let error = from_json("{").expect_err("not JSON");
        assert!(error.to_string().contains(".schema.json"), "{error}");
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        let error = from_json("{\"format\": 1, \"tabels\": {}}").expect_err("typo");
        assert!(error.to_string().contains("tabels"), "{error}");
    }

    #[test]
    fn serialisation_is_byte_stable() {
        let mut schema = Schema::empty();
        schema.add_table(Table::new("z"));
        schema.add_table(Table::new("a"));
        let first = schema.to_json();
        for _ in 0..8 {
            assert_eq!(schema.to_json(), first);
        }
        let reparsed = from_json(&first).expect("round trip");
        assert_eq!(reparsed.to_json(), first);
    }
}
