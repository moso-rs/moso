//! The four things every table-backed store in this module needs, in one place.
//!
//! Four stores now write SQL against PostgreSQL and SQLite from the same
//! statement text: [`TableSessionStore`](crate::store::TableSessionStore),
//! [`TableRefreshStore`](crate::store::TableRefreshStore),
//! [`TableApiKeyStore`](crate::store::TableApiKeyStore) and
//! `TablePasskeyStore` (behind the `passkeys` feature). Each of them has to
//! answer the same four questions, and four copies of an answer is three chances
//! to get it wrong:
//!
//! | Question | Answer |
//! | --- | --- |
//! | How is the *n*th bind parameter spelled? | [`placeholder`] |
//! | How is an instant written so text comparison orders it? | [`stamp`] |
//! | How is one read back? | [`unstamp`], [`unstamp_opt`] |
//! | What is a database failure, to an authentication store? | [`unavailable`] |
//!
//! # Why the placeholder is a function
//!
//! PostgreSQL numbers its parameters and SQLite does not. Getting it wrong is a
//! syntax error on one backend and *silently the wrong parameter* on the other,
//! which is the kind of bug that reaches production because the test suite ran
//! on SQLite. One function, called from every statement, is the only shape where
//! a fix lands everywhere.
//!
//! # Why timestamps are text
//!
//! RFC 3339 with a fixed sub-second width sorts lexicographically, so
//! `expires_at > $now` is the same predicate on both backends and no `timestamptz`
//! / `datetime` divergence enters the statement. It is the decision
//! [`SESSIONS_SCHEMA`](crate::store::SESSIONS_SCHEMA) already made, and the three
//! new tables inherit it rather than inventing a second convention.

use chrono::{DateTime, SecondsFormat, Utc};
use moso_orm::{Backend, Executor, RawQuery, Row};

use crate::{Error, Result};

/// The `n`th bind placeholder in `backend`'s spelling.
///
/// ```text
/// Postgres → $1 $2 $3 …        SQLite → ? ? ? …
/// ```
pub(super) fn placeholder(backend: Backend, n: usize) -> String {
    match backend {
        Backend::Sqlite => "?".to_owned(),
        // PostgreSQL is the reference dialect (ADR-0010), so a backend this
        // build has never heard of gets its spelling rather than SQLite's.
        Backend::Postgres | _ => format!("${n}"),
    }
}

/// `count` placeholders, comma-separated, starting at 1.
///
/// The `values (…)` list of an insert. Written once because an off-by-one in it
/// binds every column one position to the left.
pub(super) fn placeholders(backend: Backend, count: usize) -> String {
    (1..=count)
        .map(|n| placeholder(backend, n))
        .collect::<Vec<_>>()
        .join(", ")
}

/// An RFC 3339 timestamp, fixed width and in UTC, so text comparison orders it.
pub(super) fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// The inverse of [`stamp`].
///
/// A column that does not parse is an availability failure, not a credential
/// failure: the row was written by something other than this crate, and the
/// honest answer is "this store is not usable" rather than "you are not logged
/// in".
pub(super) fn unstamp(component: &'static str, text: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|error| Error::Unavailable {
            component,
            detail: format!("`{text}` is not an RFC 3339 timestamp: {error}"),
            source: None,
        })
}

/// [`unstamp`] over a nullable column.
pub(super) fn unstamp_opt(
    component: &'static str,
    text: Option<String>,
) -> Result<Option<DateTime<Utc>>> {
    text.map(|text| unstamp(component, &text)).transpose()
}

/// Every database failure is an availability failure to an authentication store.
///
/// Never a 401: a store that cannot be read has not said the credential is
/// invalid, and treating it as if it had logs every user out on a blip.
pub(super) fn unavailable(component: &'static str, error: moso_orm::Error) -> Error {
    Error::Unavailable {
        component,
        detail: error.to_string(),
        source: Some(Box::new(error)),
    }
}

/// An availability failure with a message this crate wrote rather than one the
/// driver did.
pub(super) fn malformed(component: &'static str, detail: impl Into<String>) -> Error {
    Error::Unavailable {
        component,
        detail: detail.into(),
        source: None,
    }
}

/// Run a query on `executor` and hand back the rows.
pub(super) async fn fetch<'e>(
    executor: impl Executor<'e>,
    component: &'static str,
    query: RawQuery,
) -> Result<Vec<Row>> {
    executor
        .handle()
        .fetch_all_sql(query.into_sql())
        .await
        .map_err(|error| unavailable(component, error))
}

/// Run a statement on `executor` for its effect, returning the affected row
/// count.
///
/// That count is not bookkeeping: it is the answer to every compare-and-set in
/// this module.
pub(super) async fn run<'e>(
    executor: impl Executor<'e>,
    component: &'static str,
    query: RawQuery,
) -> Result<u64> {
    query
        .execute(executor)
        .await
        .map_err(|error| unavailable(component, error))
}

/// Read a required text column, naming it when it does not decode.
pub(super) fn text(component: &'static str, row: &Row, index: usize, name: &str) -> Result<String> {
    row.get_string(index).map_err(|error| {
        malformed(
            component,
            format!("column `{name}` did not decode: {error}"),
        )
    })
}

/// Read a nullable text column. A column that does not decode reads as absent,
/// which is what every optional column in this module means anyway.
pub(super) fn text_opt(row: &Row, index: usize) -> Option<String> {
    row.get_opt::<String>(index).ok().flatten()
}

/// Read a required boolean column.
pub(super) fn flag(component: &'static str, row: &Row, index: usize, name: &str) -> Result<bool> {
    row.get_bool(index).map_err(|error| {
        malformed(
            component,
            format!("column `{name}` did not decode: {error}"),
        )
    })
}

/// Read a required integer column.
///
/// Only the passkey store reads a `bigint` column (`sign_count`, `algorithm`),
/// so this is gated with it — every other table in this battery is text, bool
/// or a primary key.
#[cfg(feature = "passkeys")]
pub(super) fn integer(component: &'static str, row: &Row, index: usize, name: &str) -> Result<i64> {
    row.get_i64(index).map_err(|error| {
        malformed(
            component,
            format!("column `{name}` did not decode: {error}"),
        )
    })
}

/// Read a JSON column, which is `text` on both backends.
///
/// A value that does not parse reads as `fallback` rather than failing the
/// request: the column holds data the *application* put there, and one
/// unreadable session payload must not take out the login path.
pub(super) fn json(row: &Row, index: usize, fallback: serde_json::Value) -> serde_json::Value {
    row.get_string(index)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(fallback)
}

/// Read a JSON array of strings, which is how every list column here is stored.
///
/// A value that does not decode reads as *empty* rather than as an error. Both
/// callers — an API key's scopes and a passkey's transports — fail safe that
/// way: a key with no scopes can do nothing, and a credential with no declared
/// transports just gets a browser prompt with no hint.
pub(super) fn string_array(row: &Row, index: usize) -> Vec<String> {
    json(row, index, serde_json::Value::Array(Vec::new()))
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Write a list column: a JSON array, in the shape [`string_array`] reads.
pub(super) fn encode_strings(component: &'static str, values: &[String]) -> Result<String> {
    serde_json::to_string(values)
        .map_err(|error| malformed(component, format!("a list column does not encode: {error}")))
}

/// Create every object in `statements`, tolerating a concurrent creator.
///
/// PostgreSQL's `if not exists` is a check, not a lock: two processes starting
/// at once both see the object absent and the loser gets a unique violation on
/// the catalogue. The object exists either way, which is all the caller asked
/// for.
pub(super) async fn create_objects(
    db: &moso_orm::Db,
    component: &'static str,
    statements: &[&str],
) -> Result<()> {
    for statement in statements {
        match RawQuery::new(*statement).execute(db).await {
            Ok(_) | Err(moso_orm::Error::UniqueViolation(_)) => {}
            Err(error) => return Err(unavailable(component, error)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_numbers_its_placeholders_and_sqlite_does_not() {
        assert_eq!(placeholder(Backend::Postgres, 3), "$3");
        assert_eq!(placeholder(Backend::Sqlite, 3), "?");
        assert_eq!(placeholders(Backend::Postgres, 3), "$1, $2, $3");
        assert_eq!(placeholders(Backend::Sqlite, 3), "?, ?, ?");
    }

    #[test]
    fn a_stamp_sorts_the_way_the_instant_does() {
        let earlier = stamp(Utc::now() - chrono::Duration::hours(1));
        let later = stamp(Utc::now());
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn a_stamp_round_trips_to_the_microsecond() {
        let now = Utc::now();
        let back = unstamp("test", &stamp(now)).expect("parses");
        assert!((back - now).num_microseconds().unwrap_or(i64::MAX).abs() <= 1);
    }

    #[test]
    fn an_unparseable_timestamp_is_an_outage_not_a_rejection() {
        let error = unstamp("test", "yesterday").expect_err("refused");
        assert!(matches!(error, Error::Unavailable { .. }), "{error}");
    }

    #[test]
    fn an_absent_optional_timestamp_is_not_an_error() {
        assert_eq!(unstamp_opt("test", None).expect("no column"), None);
        assert!(unstamp_opt("test", Some("nope".to_owned())).is_err());
    }
}
