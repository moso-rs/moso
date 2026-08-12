//! A row, and the errors that come out of reading one.
//!
//! # Positional, always
//!
//! [`Entity::from_row`](crate::Entity::from_row) reads column **0, 1, 2 …** in
//! the order the entity declares them, because the query that produced the row
//! was built from the same constant list. Name-based lookup would hash a string
//! per column per row for information the program already has at compile time —
//! and it is the reason SeaORM's decode path shows up in profiles.
//!
//! The one place names are used is diagnostics: a [`DecodeError`] carries the
//! column's name so the message can say `users.created_at` rather than
//! `column 6`.

use core::fmt;

use crate::db::Backend;

/// Reads column `index` out of whichever driver row this is, as `$ty`.
///
/// The two arms are byte-for-byte the same call; they cannot be one arm,
/// because `sqlx::Row` is implemented per database and `PgRow` and `SqliteRow`
/// are unrelated types. Written as a macro rather than a generic function so
/// that the `#[cfg]`s stay on the arms — a build with one backend turned off
/// must not mention the other's types at all.
///
/// Yields `Result<$ty, sqlx::Error>`; [`Row::finish`] turns that into a
/// [`DecodeError`] that names the column.
macro_rules! try_get {
    ($row:expr, $index:expr, $ty:ty) => {
        match &$row.repr {
            #[cfg(feature = "postgres")]
            RowRepr::Postgres(inner) => sqlx::Row::try_get::<$ty, _>(inner, $index),
            #[cfg(feature = "sqlite")]
            RowRepr::Sqlite(inner) => sqlx::Row::try_get::<$ty, _>(inner, $index),
        }
    };
}

/// One row of a result set, from any supported backend.
///
/// Obtained from an [`Executor`](crate::Executor) and consumed by
/// [`SqlType::decode`](crate::SqlType::decode). Applications rarely name it:
/// the derive writes the `from_row` that reads it.
///
/// ```
/// use moso_orm::{DecodeError, Row};
///
/// /// Decode `(id, email)` from the first two columns.
/// fn read(row: &Row) -> Result<(i64, String), DecodeError> {
///     Ok((row.get_i64(0)?, row.get_string(1)?))
/// }
/// ```
pub struct Row {
    repr: RowRepr,
}

/// The driver row behind a [`Row`].
///
/// Private on purpose: the backend is an implementation detail of the handle
/// the row came from, and widening this enum must not be a breaking change.
enum RowRepr {
    /// A PostgreSQL row.
    #[cfg(feature = "postgres")]
    Postgres(sqlx::postgres::PgRow),
    /// A SQLite row.
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::sqlite::SqliteRow),
}

impl Row {
    /// Wraps a PostgreSQL row.
    ///
    /// Public to `moso-orm` only; the executor is the only caller.
    #[cfg(feature = "postgres")]
    pub(crate) const fn postgres(row: sqlx::postgres::PgRow) -> Self {
        Self {
            repr: RowRepr::Postgres(row),
        }
    }

    /// Wraps a SQLite row.
    #[cfg(feature = "sqlite")]
    pub(crate) const fn sqlite(row: sqlx::sqlite::SqliteRow) -> Self {
        Self {
            repr: RowRepr::Sqlite(row),
        }
    }

    /// Which backend produced the row.
    ///
    /// Decoders branch on this where the wire formats differ — SQLite has no
    /// native `uuid` or `timestamptz`, so those arrive as text or integers.
    ///
    /// ```
    /// use moso_orm::{Backend, Row};
    ///
    /// fn is_pg(row: &Row) -> bool {
    ///     row.backend() == Backend::Postgres
    /// }
    /// ```
    #[must_use]
    pub fn backend(&self) -> Backend {
        match &self.repr {
            #[cfg(feature = "postgres")]
            RowRepr::Postgres(_) => Backend::Postgres,
            #[cfg(feature = "sqlite")]
            RowRepr::Sqlite(_) => Backend::Sqlite,
        }
    }

    /// How many columns the row has.
    ///
    /// ```
    /// use moso_orm::Row;
    ///
    /// fn arity(row: &Row) -> usize {
    ///     row.len()
    /// }
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.repr {
            #[cfg(feature = "postgres")]
            RowRepr::Postgres(row) => sqlx::Row::columns(row).len(),
            #[cfg(feature = "sqlite")]
            RowRepr::Sqlite(row) => sqlx::Row::columns(row).len(),
        }
    }

    /// Whether the row has no columns at all.
    ///
    /// ```
    /// use moso_orm::Row;
    ///
    /// fn empty(row: &Row) -> bool {
    ///     row.is_empty()
    /// }
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The name the server gave column `index`, for diagnostics.
    ///
    /// ```
    /// use moso_orm::Row;
    ///
    /// fn first_name(row: &Row) -> Option<&str> {
    ///     row.column_name(0)
    /// }
    /// ```
    #[must_use]
    pub fn column_name(&self, index: usize) -> Option<&str> {
        match &self.repr {
            #[cfg(feature = "postgres")]
            RowRepr::Postgres(row) => sqlx::Row::columns(row).get(index).map(sqlx::Column::name),
            #[cfg(feature = "sqlite")]
            RowRepr::Sqlite(row) => sqlx::Row::columns(row).get(index).map(sqlx::Column::name),
        }
    }

    /// Whether column `index` is `NULL`.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when `index` is past the end of the row.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn optional(row: &Row) -> Result<Option<String>, DecodeError> {
    ///     if row.is_null(3)? { Ok(None) } else { row.get_string(3).map(Some) }
    /// }
    /// ```
    pub fn is_null(&self, index: usize) -> Result<bool, DecodeError> {
        if index >= self.len() {
            return Err(self.named(DecodeError::missing_column(index, "a value")));
        }
        Ok(self.raw_is_null(index))
    }

    /// Whether column `index` is `NULL`, without the bounds check.
    ///
    /// A column the driver refuses to hand over raw is reported as *not* null,
    /// because the failure is then a decode failure and saying "it was NULL"
    /// would send the reader to fix the wrong thing.
    fn raw_is_null(&self, index: usize) -> bool {
        match &self.repr {
            #[cfg(feature = "postgres")]
            RowRepr::Postgres(row) => sqlx::Row::try_get_raw(row, index)
                .map(|value| sqlx::ValueRef::is_null(&value))
                .unwrap_or(false),
            #[cfg(feature = "sqlite")]
            RowRepr::Sqlite(row) => sqlx::Row::try_get_raw(row, index)
                .map(|value| sqlx::ValueRef::is_null(&value))
                .unwrap_or(false),
        }
    }

    /// Turns a driver result into one this crate's callers can act on.
    ///
    /// The three failures are told apart here rather than by matching sqlx's
    /// error enum, because the distinction a reader needs — "the row is
    /// shorter than the entity", "it was NULL", "it is the wrong type" — is
    /// answerable from the row itself and is stable across driver versions.
    fn finish<T>(
        &self,
        index: usize,
        expected: &'static str,
        result: core::result::Result<T, sqlx::Error>,
    ) -> Result<T, DecodeError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => Err(self.decode_failure(index, expected, &error)),
        }
    }

    /// Classifies a driver decode failure.
    fn decode_failure(
        &self,
        index: usize,
        expected: &'static str,
        error: &sqlx::Error,
    ) -> DecodeError {
        if index >= self.len() {
            return self.named(DecodeError::missing_column(index, expected));
        }
        if self.raw_is_null(index) {
            return self.named(DecodeError::unexpected_null(index, expected));
        }
        self.named(DecodeError::type_mismatch(
            index,
            expected,
            error.to_string(),
        ))
    }

    /// Attaches the server's own column name, when the row carries one.
    fn named(&self, error: DecodeError) -> DecodeError {
        match self.column_name(error.index()) {
            Some(name) => error.with_column_name(name),
            None => error,
        }
    }

    /// Decodes column `index` into `T`.
    ///
    /// The generic entry point every generated `from_row` uses.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] naming the column, the expected type and what was found.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn read_flag(row: &Row) -> Result<bool, DecodeError> {
    ///     row.get::<bool>(4)
    /// }
    /// ```
    pub fn get<T: crate::SqlType>(&self, index: usize) -> Result<T, DecodeError> {
        T::decode(self, index)
    }

    /// Decodes column `index` into `Option<T>`, mapping `NULL` to `None`.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column exists but is not a `T`.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn read_deleted_at(row: &Row) -> Result<Option<i64>, DecodeError> {
    ///     row.get_opt::<i64>(7)
    /// }
    /// ```
    pub fn get_opt<T: crate::SqlType>(&self, index: usize) -> Result<Option<T>, DecodeError> {
        if self.is_null(index)? {
            return Ok(None);
        }
        T::decode(self, index).map(Some)
    }

    /// Decodes a `boolean` column.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not a boolean.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn admin(row: &Row) -> Result<bool, DecodeError> {
    ///     row.get_bool(2)
    /// }
    /// ```
    pub fn get_bool(&self, index: usize) -> Result<bool, DecodeError> {
        self.finish(index, "bool", try_get!(self, index, bool))
    }

    /// Decodes a `smallint` column.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not an integer.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn rank(row: &Row) -> Result<i16, DecodeError> {
    ///     row.get_i16(0)
    /// }
    /// ```
    pub fn get_i16(&self, index: usize) -> Result<i16, DecodeError> {
        self.finish(index, "i16", try_get!(self, index, i16))
    }

    /// Decodes an `integer` column.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not an integer.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn count(row: &Row) -> Result<i32, DecodeError> {
    ///     row.get_i32(0)
    /// }
    /// ```
    pub fn get_i32(&self, index: usize) -> Result<i32, DecodeError> {
        self.finish(index, "i32", try_get!(self, index, i32))
    }

    /// Decodes a `bigint` column.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not an integer.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn total(row: &Row) -> Result<i64, DecodeError> {
    ///     row.get_i64(0)
    /// }
    /// ```
    pub fn get_i64(&self, index: usize) -> Result<i64, DecodeError> {
        self.finish(index, "i64", try_get!(self, index, i64))
    }

    /// Decodes a `real` column.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not a float.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn ratio(row: &Row) -> Result<f32, DecodeError> {
    ///     row.get_f32(0)
    /// }
    /// ```
    pub fn get_f32(&self, index: usize) -> Result<f32, DecodeError> {
        self.finish(index, "f32", try_get!(self, index, f32))
    }

    /// Decodes a `double precision` column.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not a float.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn score(row: &Row) -> Result<f64, DecodeError> {
    ///     row.get_f64(0)
    /// }
    /// ```
    pub fn get_f64(&self, index: usize) -> Result<f64, DecodeError> {
        self.finish(index, "f64", try_get!(self, index, f64))
    }

    /// Decodes a text column, borrowing from the row's buffer.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not text.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn slug(row: &Row) -> Result<&str, DecodeError> {
    ///     row.get_str(1)
    /// }
    /// ```
    pub fn get_str(&self, index: usize) -> Result<&str, DecodeError> {
        self.finish(index, "String", try_get!(self, index, &str))
    }

    /// Decodes a text column into an owned `String`.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not text.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn title(row: &Row) -> Result<String, DecodeError> {
    ///     row.get_string(1)
    /// }
    /// ```
    pub fn get_string(&self, index: usize) -> Result<String, DecodeError> {
        self.get_str(index).map(ToOwned::to_owned)
    }

    /// Decodes a `bytea`/`blob` column, borrowing from the row's buffer.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not binary.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn digest(row: &Row) -> Result<&[u8], DecodeError> {
    ///     row.get_bytes(0)
    /// }
    /// ```
    pub fn get_bytes(&self, index: usize) -> Result<&[u8], DecodeError> {
        self.finish(index, "Vec<u8>", try_get!(self, index, &[u8]))
    }

    /// Decodes a `uuid` column.
    ///
    /// SQLite has no native UUID; there the column is text or a 16-byte blob
    /// and both spellings are accepted.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not a UUID.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn id(row: &Row) -> Result<uuid::Uuid, DecodeError> {
    ///     row.get_uuid(0)
    /// }
    /// ```
    pub fn get_uuid(&self, index: usize) -> Result<uuid::Uuid, DecodeError> {
        // The driver's own `Uuid` decoder first, which is native on PostgreSQL
        // and the sixteen-byte blob `bind_sqlite` writes on SQLite. A column
        // written by something *else* — a migration that used `text`, an
        // application that predates Moso — is accepted as its text spelling
        // rather than refused, because both are unambiguous.
        if let Ok(value) = try_get!(self, index, uuid::Uuid) {
            return Ok(value);
        }
        let text = self.finish(index, "Uuid", try_get!(self, index, &str))?;
        uuid::Uuid::parse_str(text)
            .map_err(|error| self.named(DecodeError::malformed(index, "Uuid", error.to_string())))
    }

    /// Decodes a `timestamptz` column as an instant in UTC.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not a timestamp.
    ///
    /// ```
    /// use chrono::{DateTime, Utc};
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn created(row: &Row) -> Result<DateTime<Utc>, DecodeError> {
    ///     row.get_timestamp(5)
    /// }
    /// ```
    pub fn get_timestamp(
        &self,
        index: usize,
    ) -> Result<chrono::DateTime<chrono::Utc>, DecodeError> {
        self.finish(
            index,
            "DateTime<Utc>",
            try_get!(self, index, chrono::DateTime<chrono::Utc>),
        )
    }

    /// Decodes a `timestamp` (no zone) column.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not a timestamp.
    ///
    /// ```
    /// use chrono::NaiveDateTime;
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn at(row: &Row) -> Result<NaiveDateTime, DecodeError> {
    ///     row.get_datetime(5)
    /// }
    /// ```
    pub fn get_datetime(&self, index: usize) -> Result<chrono::NaiveDateTime, DecodeError> {
        self.finish(
            index,
            "NaiveDateTime",
            try_get!(self, index, chrono::NaiveDateTime),
        )
    }

    /// Decodes a `date` column.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not a date.
    ///
    /// ```
    /// use chrono::NaiveDate;
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn born(row: &Row) -> Result<NaiveDate, DecodeError> {
    ///     row.get_date(2)
    /// }
    /// ```
    pub fn get_date(&self, index: usize) -> Result<chrono::NaiveDate, DecodeError> {
        self.finish(index, "NaiveDate", try_get!(self, index, chrono::NaiveDate))
    }

    /// Decodes a `time` column.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not a time.
    ///
    /// ```
    /// use chrono::NaiveTime;
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn opens(row: &Row) -> Result<NaiveTime, DecodeError> {
    ///     row.get_time(3)
    /// }
    /// ```
    pub fn get_time(&self, index: usize) -> Result<chrono::NaiveTime, DecodeError> {
        self.finish(index, "NaiveTime", try_get!(self, index, chrono::NaiveTime))
    }

    /// Decodes a `numeric` column into [`moso_sql::Decimal`].
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not numeric, and
    /// when the value does not fit `moso-sql`'s 128-bit mantissa.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    /// use moso_sql::Decimal;
    ///
    /// fn total(row: &Row) -> Result<Decimal, DecodeError> {
    ///     row.get_decimal(4)
    /// }
    /// ```
    pub fn get_decimal(&self, index: usize) -> Result<moso_sql::Decimal, DecodeError> {
        // The text spelling is the exchange format in both directions: on
        // PostgreSQL the driver's `numeric` decoder produces it, and on SQLite
        // it *is* the storage (`bind_sqlite` writes `TEXT`, because `REAL`
        // loses digits). Going through text also keeps the two 128-bit
        // mantissas from disagreeing about a value neither would round.
        let text = match &self.repr {
            #[cfg(feature = "postgres")]
            RowRepr::Postgres(row) => self
                .finish(
                    index,
                    "Decimal",
                    sqlx::Row::try_get::<sqlx::types::Decimal, _>(row, index),
                )?
                .to_string(),
            #[cfg(feature = "sqlite")]
            RowRepr::Sqlite(row) => match sqlx::Row::try_get::<&str, _>(row, index) {
                Ok(text) => text.to_owned(),
                // A column somebody wrote as `REAL` rather than `TEXT`. Read
                // it rather than refuse it; the loss already happened when it
                // was written.
                Err(_) => self
                    .finish(index, "Decimal", sqlx::Row::try_get::<f64, _>(row, index))?
                    .to_string(),
            },
        };
        moso_sql::Decimal::parse(&text).map_err(|error| {
            self.named(DecodeError::malformed(index, "Decimal", error.to_string()))
        })
    }

    /// Decodes a `json`/`jsonb` column as its compact text form.
    ///
    /// The caller deserialises. This keeps `serde_json::Value` out of the hot
    /// path when the target is a concrete struct.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the column is absent, `NULL`, or not JSON.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row};
    ///
    /// fn preferences(row: &Row) -> Result<String, DecodeError> {
    ///     row.get_json_text(6)
    /// }
    /// ```
    pub fn get_json_text(&self, index: usize) -> Result<String, DecodeError> {
        match &self.repr {
            // `jsonb` is a binary format with a version byte in front, so it
            // has to go through the driver's decoder rather than be read as
            // text. `serde_json::Value`'s `Display` is the compact form, which
            // is what `bind_postgres` sent.
            #[cfg(feature = "postgres")]
            RowRepr::Postgres(row) => self
                .finish(
                    index,
                    "Json<..>",
                    sqlx::Row::try_get::<serde_json::Value, _>(row, index),
                )
                .map(|value| value.to_string()),
            // SQLite stores the compact text as it is (`bind_sqlite`), so the
            // bytes are already the answer.
            #[cfg(feature = "sqlite")]
            RowRepr::Sqlite(row) => self
                .finish(index, "Json<..>", sqlx::Row::try_get::<&str, _>(row, index))
                .map(ToOwned::to_owned),
        }
    }
}

impl fmt::Debug for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Row").finish_non_exhaustive()
    }
}

/// A column that could not become the Rust type the entity declares.
///
/// Every variant names the column, and every message ends in something the
/// reader can act on — usually "change the column type" or "make the field
/// `Option<..>`".
///
/// ```
/// use moso_orm::DecodeError;
///
/// let error = DecodeError::unexpected_null(3, "String")
///     .in_entity("User")
///     .in_field("name")
///     .with_column_name("name");
/// assert!(error.to_string().contains("User::name"));
/// assert!(error.to_string().contains("Option<String>"));
/// ```
#[derive(Clone, Debug, thiserror::Error)]
pub struct DecodeError {
    kind: DecodeErrorKind,
    index: usize,
    expected: &'static str,
    column: Option<String>,
    entity: Option<&'static str>,
    field: Option<&'static str>,
    detail: Option<String>,
}

impl DecodeError {
    /// The row has fewer columns than the entity declares.
    ///
    /// Almost always a hand-written query whose `select` list drifted from the
    /// entity.
    ///
    /// ```
    /// use moso_orm::DecodeError;
    ///
    /// let error = DecodeError::missing_column(9, "String");
    /// assert!(error.to_string().contains("column 9"));
    /// ```
    #[must_use]
    pub const fn missing_column(index: usize, expected: &'static str) -> Self {
        Self::of(DecodeErrorKind::MissingColumn, index, expected)
    }

    /// The column was `NULL` and the field is not `Option`.
    ///
    /// ```
    /// use moso_orm::DecodeError;
    ///
    /// assert!(DecodeError::unexpected_null(1, "i64").to_string().contains("Option<i64>"));
    /// ```
    #[must_use]
    pub const fn unexpected_null(index: usize, expected: &'static str) -> Self {
        Self::of(DecodeErrorKind::UnexpectedNull, index, expected)
    }

    /// The column's SQL type cannot become the Rust type.
    ///
    /// ```
    /// use moso_orm::DecodeError;
    ///
    /// let error = DecodeError::type_mismatch(2, "i32", "text");
    /// assert!(error.to_string().contains("text"));
    /// ```
    #[must_use]
    pub fn type_mismatch(index: usize, expected: &'static str, found: impl Into<String>) -> Self {
        Self::of(DecodeErrorKind::TypeMismatch, index, expected).with_detail(found)
    }

    /// The bytes were the right type and the wrong shape — an unparsable UUID,
    /// a `numeric` too wide for the mantissa, invalid JSON.
    ///
    /// ```
    /// use moso_orm::DecodeError;
    ///
    /// let error = DecodeError::malformed(0, "Uuid", "invalid length: 7");
    /// assert!(error.to_string().contains("invalid length"));
    /// ```
    #[must_use]
    pub fn malformed(index: usize, expected: &'static str, detail: impl Into<String>) -> Self {
        Self::of(DecodeErrorKind::Malformed, index, expected).with_detail(detail)
    }

    /// The row has a different number of columns than the projection expects.
    ///
    /// ```
    /// use moso_orm::DecodeError;
    ///
    /// let error = DecodeError::arity(3, 2);
    /// assert!(error.to_string().contains("3"));
    /// ```
    #[must_use]
    pub fn arity(expected_columns: usize, found: usize) -> Self {
        Self::of(DecodeErrorKind::Arity, found, "row").with_detail(format!(
            "the projection reads {expected_columns} columns and the row has {found}"
        ))
    }

    /// Shared constructor.
    const fn of(kind: DecodeErrorKind, index: usize, expected: &'static str) -> Self {
        Self {
            kind,
            index,
            expected,
            column: None,
            entity: None,
            field: None,
            detail: None,
        }
    }

    /// Records the column name the server sent.
    ///
    /// ```
    /// use moso_orm::DecodeError;
    ///
    /// let error = DecodeError::unexpected_null(0, "i64").with_column_name("id");
    /// assert!(error.to_string().contains("`id`"));
    /// ```
    #[must_use]
    pub fn with_column_name(mut self, name: impl Into<String>) -> Self {
        self.column = Some(name.into());
        self
    }

    /// Records the entity being decoded. The derive adds this.
    ///
    /// ```
    /// use moso_orm::DecodeError;
    ///
    /// assert_eq!(DecodeError::arity(1, 0).in_entity("User").entity(), Some("User"));
    /// ```
    #[must_use]
    pub const fn in_entity(mut self, entity: &'static str) -> Self {
        self.entity = Some(entity);
        self
    }

    /// Records the Rust field being decoded. The derive adds this.
    ///
    /// ```
    /// use moso_orm::DecodeError;
    ///
    /// assert_eq!(DecodeError::arity(1, 0).in_field("name").field(), Some("name"));
    /// ```
    #[must_use]
    pub const fn in_field(mut self, field: &'static str) -> Self {
        self.field = Some(field);
        self
    }

    /// Adds what the driver said.
    ///
    /// ```
    /// use moso_orm::DecodeError;
    ///
    /// let error = DecodeError::arity(1, 0).with_detail("short row");
    /// assert!(error.to_string().contains("short row"));
    /// ```
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Which kind of decode failure this is.
    ///
    /// ```
    /// use moso_orm::{DecodeError, DecodeErrorKind};
    ///
    /// assert_eq!(DecodeError::unexpected_null(0, "i64").kind(), DecodeErrorKind::UnexpectedNull);
    /// ```
    #[must_use]
    pub const fn kind(&self) -> DecodeErrorKind {
        self.kind
    }

    /// The zero-based column position.
    ///
    /// ```
    /// assert_eq!(moso_orm::DecodeError::unexpected_null(4, "i64").index(), 4);
    /// ```
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    /// The Rust type that was expected, as it is spelled in the entity.
    ///
    /// ```
    /// assert_eq!(moso_orm::DecodeError::unexpected_null(0, "i64").expected(), "i64");
    /// ```
    #[must_use]
    pub const fn expected(&self) -> &'static str {
        self.expected
    }

    /// The column name, when it was recorded.
    ///
    /// ```
    /// assert!(moso_orm::DecodeError::arity(1, 0).column_name().is_none());
    /// ```
    #[must_use]
    pub fn column_name(&self) -> Option<&str> {
        self.column.as_deref()
    }

    /// The entity, when it was recorded.
    ///
    /// ```
    /// assert!(moso_orm::DecodeError::arity(1, 0).entity().is_none());
    /// ```
    #[must_use]
    pub const fn entity(&self) -> Option<&'static str> {
        self.entity
    }

    /// The field, when it was recorded.
    ///
    /// ```
    /// assert!(moso_orm::DecodeError::arity(1, 0).field().is_none());
    /// ```
    #[must_use]
    pub const fn field(&self) -> Option<&'static str> {
        self.field
    }

    /// What the driver reported, when anything was reported.
    ///
    /// ```
    /// assert!(moso_orm::DecodeError::unexpected_null(0, "i64").detail().is_none());
    /// ```
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// How the position reads in a message: `User::name`, `` `email` `` or
    /// `column 4`, in that order of preference.
    fn where_(&self) -> String {
        match (self.entity, self.field, &self.column) {
            (Some(entity), Some(field), _) => format!("{entity}::{field}"),
            (_, _, Some(column)) => format!("`{column}`"),
            _ => format!("column {}", self.index),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let position = self.where_();
        match self.kind {
            DecodeErrorKind::MissingColumn => write!(
                f,
                "{position} reads column {index}, and the row has fewer columns\n  \
                 help: the query's select list does not match the entity; rebuild it with \
                 `Entity::query()`, or fix the projection",
                index = self.index
            )?,
            DecodeErrorKind::UnexpectedNull => write!(
                f,
                "{position} is NULL and `{expected}` cannot hold NULL\n  \
                 help: make the field `Option<{expected}>`, or add `NOT NULL` to the column",
                expected = self.expected
            )?,
            DecodeErrorKind::TypeMismatch => write!(
                f,
                "{position} cannot be read as `{expected}`\n  \
                 help: change the field's type, or change the column's",
                expected = self.expected
            )?,
            DecodeErrorKind::Malformed => write!(
                f,
                "{position} holds a value that is not a valid `{expected}`",
                expected = self.expected
            )?,
            DecodeErrorKind::Arity => write!(f, "the row has the wrong number of columns")?,
        }
        if let Some(detail) = &self.detail {
            write!(f, "\n  note: {detail}")?;
        }
        Ok(())
    }
}

/// Which kind of decode failure a [`DecodeError`] is.
///
/// ```
/// use moso_orm::DecodeErrorKind;
///
/// assert!(DecodeErrorKind::UnexpectedNull.is_schema_drift());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DecodeErrorKind {
    /// The row is shorter than the entity.
    MissingColumn,
    /// The column was `NULL` and the field is not `Option`.
    UnexpectedNull,
    /// The column's SQL type cannot become the Rust type.
    TypeMismatch,
    /// The bytes were the right type and an invalid value.
    Malformed,
    /// A projection expected a different number of columns.
    Arity,
}

impl DecodeErrorKind {
    /// Whether this means the database and the entity have drifted apart —
    /// which is what a pending migration looks like at runtime.
    ///
    /// ```
    /// use moso_orm::DecodeErrorKind;
    ///
    /// assert!(DecodeErrorKind::TypeMismatch.is_schema_drift());
    /// assert!(!DecodeErrorKind::Malformed.is_schema_drift());
    /// ```
    #[must_use]
    pub const fn is_schema_drift(self) -> bool {
        matches!(
            self,
            Self::MissingColumn | Self::UnexpectedNull | Self::TypeMismatch | Self::Arity
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_in_a_non_optional_field_suggests_option() {
        let error = DecodeError::unexpected_null(3, "String")
            .in_entity("User")
            .in_field("name");
        let text = error.to_string();
        assert!(text.contains("User::name"), "{text}");
        assert!(text.contains("Option<String>"), "{text}");
    }

    #[test]
    fn the_position_prefers_the_entity_field_then_the_column_then_the_index() {
        let bare = DecodeError::unexpected_null(3, "i64");
        assert!(bare.to_string().contains("column 3"));

        let named = bare.clone().with_column_name("created_at");
        assert!(named.to_string().contains("`created_at`"));

        let full = named.in_entity("Post").in_field("created_at");
        assert!(full.to_string().contains("Post::created_at"));
    }

    #[test]
    fn schema_drift_is_distinguishable_from_a_bad_value() {
        assert!(DecodeErrorKind::MissingColumn.is_schema_drift());
        assert!(DecodeErrorKind::Arity.is_schema_drift());
        assert!(!DecodeErrorKind::Malformed.is_schema_drift());
    }

    #[test]
    fn every_message_carries_a_help_line_or_a_note() {
        for error in [
            DecodeError::missing_column(1, "i64"),
            DecodeError::unexpected_null(1, "i64"),
            DecodeError::type_mismatch(1, "i64", "text"),
            DecodeError::malformed(1, "Uuid", "bad length"),
            DecodeError::arity(2, 1),
        ] {
            let text = error.to_string();
            assert!(
                text.contains("help:") || text.contains("note:"),
                "no fix offered: {text}"
            );
        }
    }
}
