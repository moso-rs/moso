//! What a Rust type is worth in SQL: [`SqlType`] and the marker traits that
//! decide which column operations exist.
//!
//! # The shape of the mapping
//!
//! One trait does three jobs, and it does them together on purpose:
//!
//! | Job | Member | Consumer |
//! | --- | --- | --- |
//! | bind a value | [`SqlType::to_value`] | every `filter`, `set`, `insert` |
//! | read a value | [`SqlType::decode`] | the generated `from_row` |
//! | declare a column | [`SqlType::data_type`] | `moso-migrate`, `moso-admin` |
//!
//! Splitting them would let a type be bindable and not decodable, which is a
//! state nothing wants and everything would have to handle.
//!
//! # Marker traits gate the operations
//!
//! `Column<E, String>` has `.ilike(..)`; `Column<E, i64>` does not, because
//! [`TextLike`] is the bound on that `impl` block. A misuse is therefore a
//! *missing method*, which rustc reports with the column's own type in it,
//! rather than a runtime cast error from the server.

use core::fmt;

use moso_schema::types::{Email, Id, IdMarker, Slug};
use moso_sql::{DataType, Value, ValueKind};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::row::{DecodeError, Row};

/// A Rust type that maps to exactly one column.
///
/// Implemented here for the primitives, the `chrono` calendar types,
/// `uuid::Uuid`, `moso_sql::Decimal`, `moso_schema`'s constrained types,
/// `Option<T>` and [`Json<T>`]. `#[derive(DbEnum)]` implements it for an enum,
/// and applications implement it by hand for a newtype over any of those.
///
/// ```
/// use moso_orm::{DecodeError, Row, SqlType};
/// use moso_sql::{DataType, Value, ValueKind};
///
/// /// Cents, so that money is never an `f64`.
/// #[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// pub struct Cents(pub i64);
///
/// impl SqlType for Cents {
///     const KIND: ValueKind = ValueKind::I64;
///     const TYPE_NAME: &'static str = "Cents";
///
///     fn data_type() -> DataType {
///         DataType::BigInt
///     }
///
///     fn to_value(&self) -> Value {
///         Value::I64(self.0)
///     }
///
///     fn decode(row: &Row, index: usize) -> Result<Self, DecodeError> {
///         row.get_i64(index).map(Cents)
///     }
/// }
///
/// assert_eq!(Cents(250).to_value(), Value::I64(250));
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be stored in a database column",
    label = "not a column type",
    note = "a column type must be one of the primitives, `String`, `Vec<u8>`, `uuid::Uuid`, a \
            `chrono` calendar type, `moso_sql::Decimal`, `Id<E>`, `Email`, `Slug`, an \
            `Option<..>` of any of those, or a type that implements `SqlType`",
    note = "for a struct or a map, store it as JSON: wrap the field in `moso_orm::Json<{Self}>`",
    note = "for an enum, write `#[derive(moso::DbEnum)]` above `{Self}`",
    note = "help: implement `SqlType for {Self}` — it needs `to_value`, `decode` and `data_type`"
)]
pub trait SqlType: Sized + Send + Sync + 'static {
    /// The parameter type this binds as.
    ///
    /// Carried so that a `None` still produces a *typed* `NULL`, which is what
    /// lets PostgreSQL infer a cast for `column = $1`.
    const KIND: ValueKind;

    /// The type's name as a user writes it, for diagnostics.
    ///
    /// `core::any::type_name` would print `alloc::string::String`; the
    /// diagnostics style guide says to print what the user typed.
    const TYPE_NAME: &'static str;

    /// Whether the column may be `NULL`. Only `Option<T>` sets this.
    const NULLABLE: bool = false;

    /// The column type the migration generator emits.
    ///
    /// ```
    /// use moso_orm::SqlType;
    /// use moso_sql::DataType;
    ///
    /// assert_eq!(<i64 as SqlType>::data_type(), DataType::BigInt);
    /// ```
    fn data_type() -> DataType;

    /// Binds the value as a statement parameter.
    ///
    /// ```
    /// use moso_orm::SqlType;
    /// use moso_sql::Value;
    ///
    /// assert_eq!(true.to_value(), Value::Bool(true));
    /// ```
    fn to_value(&self) -> Value;

    /// Binds by value, so an owned `String` need not be cloned.
    ///
    /// ```
    /// use moso_orm::SqlType;
    /// use moso_sql::Value;
    ///
    /// assert_eq!(String::from("hi").into_value(), Value::text("hi"));
    /// ```
    #[must_use]
    fn into_value(self) -> Value {
        self.to_value()
    }

    /// Reads column `index` of `row`.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] naming the column and both types.
    ///
    /// ```
    /// use moso_orm::{DecodeError, Row, SqlType};
    ///
    /// fn read(row: &Row) -> Result<i64, DecodeError> {
    ///     <i64 as SqlType>::decode(row, 0)
    /// }
    /// ```
    fn decode(row: &Row, index: usize) -> Result<Self, DecodeError>;
}

/// A column type that behaves like text, and therefore has pattern matching and
/// full-text search.
///
/// Implemented for `String`, [`Email`], [`Slug`] and `Option` of each.
///
/// ```
/// use moso_orm::TextLike;
///
/// fn is_texty<T: TextLike>() {}
/// is_texty::<String>();
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a text column, so it has no `like` / `ilike` / `contains`",
    label = "not text",
    note = "pattern matching exists on `String`, `Email` and `Slug` columns only",
    note = "help: compare instead — `.eq(value)`, or `.is_in([a, b])`"
)]
pub trait TextLike: SqlType {}

/// A column type stored as `json`/`jsonb`, and therefore having path and
/// containment operators.
///
/// ```
/// use moso_orm::{Json, JsonLike};
///
/// fn is_json<T: JsonLike>() {}
/// is_json::<Json<serde_json::Value>>();
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a JSON column, so it has no `path` / `has_key` / `contains_json`",
    label = "not JSON",
    note = "declare the field as `Json<{Self}>` to store it as `jsonb`",
    note = "help: `#[entity(json)] pub preferences: Json<Preferences>`"
)]
pub trait JsonLike: SqlType {}

/// A column type that can hold `NULL`, and therefore has `is_null`.
///
/// Implemented for `Option<T>` and nothing else, which is what makes
/// `User::NAME.is_null()` a compile error when `name` is `NOT NULL`.
///
/// ```
/// use moso_orm::Nullable;
///
/// fn nullable<T: Nullable>() {}
/// nullable::<Option<String>>();
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a nullable column, so it can never be NULL",
    label = "declared NOT NULL",
    note = "`is_null()` exists only on `Option<..>` columns — the schema says this one is required",
    note = "help: if the column really is nullable, declare the field as `Option<{Self}>`"
)]
pub trait Nullable: SqlType {
    /// The type inside the `Option`.
    type Inner: SqlType;
}

/// A column type whose ordering the database and Rust agree on, and which can
/// therefore be a keyset-pagination key.
///
/// ```
/// use moso_orm::Sortable;
///
/// fn sortable<T: Sortable>() {}
/// sortable::<i64>();
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot order a keyset page",
    label = "not sortable",
    note = "a cursor encodes the ordering key, so its type must have a total order the database \
            and Rust agree on",
    note = "help: paginate by `created_at`, an integer, or the primary key"
)]
pub trait Sortable: SqlType {}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

macro_rules! sql_type {
    (
        $rust:ty,
        name = $name:literal,
        kind = $kind:ident,
        data_type = $data_type:expr,
        to_value = |$value:ident| $to_value:expr,
        decode = |$row:ident, $index:ident| $decode:expr
        $(, sortable = $sortable:literal)?
        $(,)?
    ) => {
        impl SqlType for $rust {
            const KIND: ValueKind = ValueKind::$kind;
            const TYPE_NAME: &'static str = $name;

            fn data_type() -> DataType {
                $data_type
            }

            fn to_value(&self) -> Value {
                let $value = self;
                $to_value
            }

            fn decode($row: &Row, $index: usize) -> Result<Self, DecodeError> {
                $decode
            }
        }
    };
}

sql_type!(
    bool,
    name = "bool",
    kind = Bool,
    data_type = DataType::Boolean,
    to_value = |v| Value::Bool(*v),
    decode = |row, index| row.get_bool(index),
);
sql_type!(
    i16,
    name = "i16",
    kind = I16,
    data_type = DataType::SmallInt,
    to_value = |v| Value::I16(*v),
    decode = |row, index| row.get_i16(index),
);
sql_type!(
    i32,
    name = "i32",
    kind = I32,
    data_type = DataType::Integer,
    to_value = |v| Value::I32(*v),
    decode = |row, index| row.get_i32(index),
);
sql_type!(
    i64,
    name = "i64",
    kind = I64,
    data_type = DataType::BigInt,
    to_value = |v| Value::I64(*v),
    decode = |row, index| row.get_i64(index),
);
sql_type!(
    f32,
    name = "f32",
    kind = F32,
    data_type = DataType::Real,
    to_value = |v| Value::F32(*v),
    decode = |row, index| row.get_f32(index),
);
sql_type!(
    f64,
    name = "f64",
    kind = F64,
    data_type = DataType::DoublePrecision,
    to_value = |v| Value::F64(*v),
    decode = |row, index| row.get_f64(index),
);
sql_type!(
    String,
    name = "String",
    kind = Text,
    data_type = DataType::Text,
    to_value = |v| Value::Text(v.clone()),
    decode = |row, index| row.get_string(index),
);
sql_type!(
    Vec<u8>,
    name = "Vec<u8>",
    kind = Bytes,
    data_type = DataType::Bytea,
    to_value = |v| Value::Bytes(v.clone()),
    decode = |row, index| row.get_bytes(index).map(<[u8]>::to_vec),
);
sql_type!(
    uuid::Uuid,
    name = "Uuid",
    kind = Uuid,
    data_type = DataType::Uuid,
    to_value = |v| Value::Uuid(moso_sql::Uuid::from_bytes(*v.as_bytes())),
    decode = |row, index| row.get_uuid(index),
);
sql_type!(
    moso_sql::Decimal,
    name = "Decimal",
    kind = Decimal,
    data_type = DataType::Numeric {
        precision: None,
        scale: None
    },
    to_value = |v| Value::Decimal(*v),
    decode = |row, index| row.get_decimal(index),
);
sql_type!(
    chrono::DateTime<chrono::Utc>,
    name = "DateTime<Utc>",
    kind = Timestamp,
    data_type = DataType::Timestamp {
        with_time_zone: true
    },
    to_value = |v| {
        // `Timestamp::new` only rejects a nanosecond count above one second,
        // which `chrono` cannot produce outside a leap second; a leap second is
        // clamped rather than dropped.
        let nanoseconds = v.timestamp_subsec_nanos().min(999_999_999);
        Value::Timestamp(
            moso_sql::Timestamp::new(v.timestamp(), nanoseconds)
                .unwrap_or(moso_sql::Timestamp::UNIX_EPOCH),
        )
    },
    decode = |row, index| row.get_timestamp(index),
);
sql_type!(
    chrono::NaiveDateTime,
    name = "NaiveDateTime",
    kind = DateTime,
    data_type = DataType::Timestamp {
        with_time_zone: false
    },
    to_value = |v| Value::DateTime(naive_datetime_to_sql(*v)),
    decode = |row, index| row.get_datetime(index),
);
sql_type!(
    chrono::NaiveDate,
    name = "NaiveDate",
    kind = Date,
    data_type = DataType::Date,
    to_value = |v| Value::Date(naive_date_to_sql(*v)),
    decode = |row, index| row.get_date(index),
);
sql_type!(
    chrono::NaiveTime,
    name = "NaiveTime",
    kind = Time,
    data_type = DataType::Time {
        with_time_zone: false
    },
    to_value = |v| Value::Time(naive_time_to_sql(*v)),
    decode = |row, index| row.get_time(index),
);
sql_type!(
    Email,
    name = "Email",
    kind = Text,
    data_type = DataType::VarChar(Some(254)),
    to_value = |v| Value::text(v.as_str()),
    decode = |row, index| {
        let text = row.get_str(index)?;
        Email::new(text).map_err(|error| DecodeError::malformed(index, "Email", error.to_string()))
    },
);
sql_type!(
    Slug,
    name = "Slug",
    kind = Text,
    data_type = DataType::VarChar(Some(255)),
    to_value = |v| Value::text(v.as_str()),
    decode = |row, index| {
        let text = row.get_str(index)?;
        Slug::new(text).map_err(|error| DecodeError::malformed(index, "Slug", error.to_string()))
    },
);

/// The date a clamped conversion falls back to.
///
/// `moso_sql::Date` has no `UNIX_EPOCH` constant and its constructor is
/// fallible, so the fallback is spelled out once here rather than at three call
/// sites. It is only reachable for a `chrono` date outside the SQL calendar,
/// which `chrono`'s own API cannot construct.
fn epoch_date() -> moso_sql::Date {
    moso_sql::Date::new(1970, 1, 1).unwrap_or_else(|_| unreachable!("1970-01-01 is a real date"))
}

/// `chrono::NaiveDate` into `moso_sql::Date`, clamping rather than panicking.
///
/// `chrono`'s calendar range is wider than the SQL one only at the extremes,
/// where a year is outside `Date`'s validation; clamping keeps `to_value`
/// infallible, which is what makes `filter(..)` chainable.
fn naive_date_to_sql(value: chrono::NaiveDate) -> moso_sql::Date {
    use chrono::Datelike as _;
    let (year, month, day) = (value.year(), value.month(), value.day());
    #[expect(
        clippy::cast_possible_truncation,
        reason = "chrono::Datelike guarantees month in 1..=12 and day in 1..=31"
    )]
    moso_sql::Date::new(year, month as u8, day as u8).unwrap_or_else(|_| epoch_date())
}

/// `chrono::NaiveTime` into `moso_sql::Time`.
fn naive_time_to_sql(value: chrono::NaiveTime) -> moso_sql::Time {
    use chrono::Timelike as _;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "chrono::Timelike guarantees hour < 24, minute < 60 and second < 61"
    )]
    let (hour, minute, second) = (
        value.hour() as u8,
        value.minute() as u8,
        value.second().min(59) as u8,
    );
    moso_sql::Time::new(hour, minute, second, value.nanosecond().min(999_999_999))
        .unwrap_or(moso_sql::Time::MIDNIGHT)
}

/// `chrono::NaiveDateTime` into `moso_sql::DateTime`.
fn naive_datetime_to_sql(value: chrono::NaiveDateTime) -> moso_sql::DateTime {
    moso_sql::DateTime::new(
        naive_date_to_sql(value.date()),
        naive_time_to_sql(value.time()),
    )
}

impl TextLike for String {}
impl TextLike for Email {}
impl TextLike for Slug {}

impl Sortable for i16 {}
impl Sortable for i32 {}
impl Sortable for i64 {}
impl Sortable for String {}
impl Sortable for uuid::Uuid {}
impl Sortable for chrono::DateTime<chrono::Utc> {}
impl Sortable for chrono::NaiveDateTime {}
impl Sortable for chrono::NaiveDate {}
impl Sortable for moso_sql::Decimal {}
impl Sortable for Slug {}
impl Sortable for Email {}

// ---------------------------------------------------------------------------
// Id<E>
// ---------------------------------------------------------------------------

impl<E: IdMarker> SqlType for Id<E> {
    const KIND: ValueKind = ValueKind::Uuid;
    const TYPE_NAME: &'static str = "Id<_>";

    fn data_type() -> DataType {
        DataType::Uuid
    }

    fn to_value(&self) -> Value {
        Value::Uuid(moso_sql::Uuid::from_bytes(*self.as_uuid().as_bytes()))
    }

    fn decode(row: &Row, index: usize) -> Result<Self, DecodeError> {
        row.get_uuid(index).map(Id::from_uuid)
    }
}

impl<E: IdMarker> Sortable for Id<E> {}

// ---------------------------------------------------------------------------
// Option<T>
// ---------------------------------------------------------------------------

#[diagnostic::do_not_recommend]
impl<T: SqlType> SqlType for Option<T> {
    const KIND: ValueKind = T::KIND;
    const TYPE_NAME: &'static str = T::TYPE_NAME;
    const NULLABLE: bool = true;

    fn data_type() -> DataType {
        T::data_type()
    }

    fn to_value(&self) -> Value {
        match self {
            Some(value) => value.to_value(),
            None => Value::null(T::KIND),
        }
    }

    fn into_value(self) -> Value {
        match self {
            Some(value) => value.into_value(),
            None => Value::null(T::KIND),
        }
    }

    fn decode(row: &Row, index: usize) -> Result<Self, DecodeError> {
        if row.is_null(index)? {
            return Ok(None);
        }
        T::decode(row, index).map(Some)
    }
}

#[diagnostic::do_not_recommend]
impl<T: SqlType> Nullable for Option<T> {
    type Inner = T;
}

#[diagnostic::do_not_recommend]
impl<T: TextLike> TextLike for Option<T> {}

#[diagnostic::do_not_recommend]
impl<T: JsonLike> JsonLike for Option<T> {}

#[diagnostic::do_not_recommend]
impl<T: Sortable> Sortable for Option<T> {}

// ---------------------------------------------------------------------------
// Json<T>
// ---------------------------------------------------------------------------

/// A field stored as a `jsonb` column.
///
/// The inner type is serialised on the way in and deserialised on the way out,
/// so anything `serde` handles can be a column. This is the honest version of
/// `#[entity(json)]`: the wrapper is visible in the entity, so nobody has to
/// remember which fields are secretly JSON.
///
/// ```
/// use moso_orm::Json;
/// use serde::{Deserialize, Serialize};
///
/// /// What a user chose in the settings page.
/// #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
/// pub struct Preferences {
///     /// Whether to send the weekly digest.
///     pub digest: bool,
/// }
///
/// let stored = Json(Preferences { digest: true });
/// assert!(stored.get().digest);
/// assert_eq!(stored.into_inner(), Preferences { digest: true });
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Json<T>(pub T);

impl<T> Json<T> {
    /// Wraps a value.
    ///
    /// ```
    /// use moso_orm::Json;
    ///
    /// assert_eq!(Json::new(3).into_inner(), 3);
    /// ```
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Borrows the value.
    ///
    /// ```
    /// use moso_orm::Json;
    ///
    /// assert_eq!(Json(7).get(), &7);
    /// ```
    #[must_use]
    pub const fn get(&self) -> &T {
        &self.0
    }

    /// Borrows the value mutably.
    ///
    /// ```
    /// use moso_orm::Json;
    ///
    /// let mut json = Json(1);
    /// *json.get_mut() += 1;
    /// assert_eq!(json.into_inner(), 2);
    /// ```
    #[must_use]
    pub const fn get_mut(&mut self) -> &mut T {
        &mut self.0
    }

    /// Unwraps the value.
    ///
    /// ```
    /// use moso_orm::Json;
    ///
    /// assert_eq!(Json("x").into_inner(), "x");
    /// ```
    #[must_use]
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Json<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> SqlType for Json<T> {
    const KIND: ValueKind = ValueKind::Json;
    const TYPE_NAME: &'static str = "Json<_>";

    fn data_type() -> DataType {
        DataType::JsonB
    }

    fn to_value(&self) -> Value {
        // A `Serialize` that fails is a bug in the type, not a state the query
        // builder can report — `filter(..)` is infallible by design (N4). A
        // failure binds SQL `null`, and the statement's own `NOT NULL` catches
        // it at the server with a message naming the column.
        match serde_json::to_string(&self.0) {
            Ok(text) => Value::json(&text).unwrap_or_else(|_| Value::null(ValueKind::Json)),
            Err(_) => Value::null(ValueKind::Json),
        }
    }

    fn decode(row: &Row, index: usize) -> Result<Self, DecodeError> {
        let text = row.get_json_text(index)?;
        serde_json::from_str(&text)
            .map(Json)
            .map_err(|error| DecodeError::malformed(index, "Json<_>", error.to_string()))
    }
}

impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> JsonLike for Json<T> {}

impl<T: fmt::Display> fmt::Display for Json<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// How an enum is stored in its column.
///
/// ```
/// use moso_orm::EnumStorage;
///
/// assert!(EnumStorage::PgEnum.needs_a_type());
/// assert!(!EnumStorage::Text.needs_a_type());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EnumStorage {
    /// One `text` column holding the variant's name. The default: readable in
    /// `psql`, and adding a variant needs no migration.
    #[default]
    Text,
    /// One `integer` column holding the variant's discriminant. Compact, and
    /// unreadable without the code.
    Int,
    /// A PostgreSQL `CREATE TYPE ... AS ENUM`. Checked by the server, and
    /// adding a variant is a migration.
    PgEnum,
}

impl EnumStorage {
    /// Whether the migration generator has to emit a `CREATE TYPE`.
    ///
    /// ```
    /// use moso_orm::EnumStorage;
    ///
    /// assert!(EnumStorage::PgEnum.needs_a_type());
    /// ```
    #[must_use]
    pub const fn needs_a_type(self) -> bool {
        matches!(self, Self::PgEnum)
    }

    /// The column type for this storage strategy.
    ///
    /// ```
    /// use moso_orm::EnumStorage;
    /// use moso_sql::DataType;
    ///
    /// assert_eq!(EnumStorage::Int.data_type(None), DataType::Integer);
    /// ```
    #[must_use]
    pub fn data_type(self, type_name: Option<moso_sql::TypeRef>) -> DataType {
        match (self, type_name) {
            (Self::Int, _) => DataType::Integer,
            (Self::PgEnum, Some(name)) => DataType::Enum(name),
            _ => DataType::Text,
        }
    }
}

/// An enum that is one column.
///
/// `#[derive(DbEnum)]` implements this *and* [`SqlType`], because the two
/// together are what a column needs and implementing one without the other is
/// never useful.
///
/// ```
/// use moso_orm::{DbEnum, EnumStorage};
///
/// /// Where an order is in its lifecycle.
/// #[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// pub enum Status {
///     /// Awaiting payment.
///     Pending,
///     /// Paid for.
///     Paid,
/// }
///
/// impl DbEnum for Status {
///     const VARIANTS: &'static [&'static str] = &["pending", "paid"];
///     const STORAGE: EnumStorage = EnumStorage::Text;
///     const TYPE_NAME: &'static str = "order_status";
///
///     fn as_db_str(&self) -> &'static str {
///         match self {
///             Self::Pending => "pending",
///             Self::Paid => "paid",
///         }
///     }
///
///     fn from_db_str(value: &str) -> Option<Self> {
///         match value {
///             "pending" => Some(Self::Pending),
///             "paid" => Some(Self::Paid),
///             _ => None,
///         }
///     }
///
///     fn as_db_int(&self) -> i32 {
///         match self {
///             Self::Pending => 0,
///             Self::Paid => 1,
///         }
///     }
///
///     fn from_db_int(value: i32) -> Option<Self> {
///         match value {
///             0 => Some(Self::Pending),
///             1 => Some(Self::Paid),
///             _ => None,
///         }
///     }
/// }
///
/// assert_eq!(Status::Paid.as_db_str(), "paid");
/// assert_eq!(Status::from_db_int(0), Some(Status::Pending));
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a database enum",
    label = "not a database enum",
    note = "an enum column needs to know its variants' stored spellings, and how to read one back",
    note = "help: write `#[derive(moso::DbEnum)]` above `{Self}`",
    note = "help: choose the storage with `#[db_enum(as = \"text\")]`, `\"int\"` or `\"pg_enum\"`"
)]
pub trait DbEnum: Sized + Send + Sync + 'static {
    /// Every variant's stored spelling, in declaration order.
    ///
    /// The migration generator writes these into `CREATE TYPE`, and
    /// `moso-admin` renders them as a `<select>`.
    const VARIANTS: &'static [&'static str];

    /// How the column stores a variant.
    const STORAGE: EnumStorage;

    /// The PostgreSQL type name, used when [`EnumStorage::PgEnum`] is chosen
    /// and ignored otherwise.
    const TYPE_NAME: &'static str;

    /// This variant's stored spelling.
    fn as_db_str(&self) -> &'static str;

    /// The variant with this stored spelling, if there is one.
    fn from_db_str(value: &str) -> Option<Self>;

    /// This variant's stored discriminant.
    fn as_db_int(&self) -> i32;

    /// The variant with this discriminant, if there is one.
    fn from_db_int(value: i32) -> Option<Self>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_primitive_mapping_is_the_obvious_one() {
        assert_eq!(<i64 as SqlType>::data_type(), DataType::BigInt);
        assert_eq!(<i32 as SqlType>::data_type(), DataType::Integer);
        assert_eq!(<bool as SqlType>::data_type(), DataType::Boolean);
        assert_eq!(<String as SqlType>::data_type(), DataType::Text);
        assert_eq!(<uuid::Uuid as SqlType>::data_type(), DataType::Uuid);
        assert_eq!(<Vec<u8> as SqlType>::data_type(), DataType::Bytea);
    }

    #[test]
    fn option_is_the_only_thing_that_is_nullable() {
        const { assert!(!<String as SqlType>::NULLABLE) };
        const { assert!(<Option<String> as SqlType>::NULLABLE) };
        // …and it keeps the inner type's kind, so `NULL` is still typed.
        assert_eq!(<Option<i32> as SqlType>::KIND, ValueKind::I32);
        assert_eq!(<Option<i32> as SqlType>::data_type(), DataType::Integer);
    }

    #[test]
    fn none_binds_a_typed_null() {
        assert_eq!(None::<i64>.to_value(), Value::null(ValueKind::I64));
        assert_eq!(Some(7_i64).to_value(), Value::I64(7));
    }

    #[test]
    fn a_uuid_round_trips_through_the_sealed_scalar() {
        let id = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let Value::Uuid(bound) = id.to_value() else {
            panic!("a Uuid must bind as Value::Uuid");
        };
        assert_eq!(bound.into_bytes(), *id.as_bytes());
    }

    #[test]
    fn an_instant_binds_as_a_timestamp_with_the_same_epoch_seconds() {
        let at = chrono::DateTime::from_timestamp(1_700_000_000, 123).expect("a valid instant");
        let Value::Timestamp(bound) = at.to_value() else {
            panic!("a DateTime<Utc> must bind as Value::Timestamp");
        };
        assert_eq!(bound.unix_seconds(), 1_700_000_000);
        assert_eq!(bound.nanoseconds(), 123);
    }

    #[test]
    fn a_calendar_date_survives_the_conversion() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 7, 30).expect("a real date");
        let Value::Date(bound) = date.to_value() else {
            panic!("a NaiveDate must bind as Value::Date");
        };
        assert_eq!((bound.year(), bound.month(), bound.day()), (2026, 7, 30));
    }

    #[test]
    fn json_serialises_on_the_way_in() {
        let value = Json(vec![1_u8, 2, 3]);
        let Value::Json(bound) = value.to_value() else {
            panic!("Json<T> must bind as Value::Json");
        };
        assert_eq!(bound.as_json_str(), "[1,2,3]");
    }

    #[test]
    fn the_type_names_are_the_ones_a_user_would_write() {
        assert_eq!(<String as SqlType>::TYPE_NAME, "String");
        assert_eq!(<Option<String> as SqlType>::TYPE_NAME, "String");
        assert_eq!(
            <chrono::DateTime<chrono::Utc> as SqlType>::TYPE_NAME,
            "DateTime<Utc>"
        );
        // Style-guide rule 2: nothing a diagnostic prints is long.
        for name in [
            <String as SqlType>::TYPE_NAME,
            <Id<()> as SqlType>::TYPE_NAME,
            <Json<()> as SqlType>::TYPE_NAME,
        ] {
            assert!(name.len() <= 80, "{name}");
        }
    }

    #[test]
    fn enum_storage_decides_the_column_type() {
        assert_eq!(EnumStorage::Text.data_type(None), DataType::Text);
        assert_eq!(EnumStorage::Int.data_type(None), DataType::Integer);
        let name = moso_sql::TypeRef::from_static("order_status");
        assert_eq!(
            EnumStorage::PgEnum.data_type(Some(name.clone())),
            DataType::Enum(name)
        );
    }
}
