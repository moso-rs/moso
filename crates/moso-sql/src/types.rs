//! SQL data types, as named in a `CAST` and in DDL.
//!
//! One enum serves both, because a migration that adds a column and a query
//! that casts to that column's type must agree, and two enums drift.

use crate::ident::TypeRef;

/// A SQL data type.
///
/// The spelling is the dialect's business: `DataType::Timestamp { with_time_zone: true }`
/// renders as `timestamptz` on PostgreSQL and as `text` on SQLite, which stores
/// no such type. Ask [`Dialect::capabilities`](crate::Dialect::capabilities)
/// before assuming a type exists.
///
/// ```
/// use moso_sql::DataType;
///
/// let tags = DataType::array_of(DataType::Text);
/// assert_eq!(tags.element(), Some(&DataType::Text));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DataType {
    /// `boolean`.
    Boolean,
    /// `smallint`, 16-bit.
    SmallInt,
    /// `integer`, 32-bit.
    Integer,
    /// `bigint`, 64-bit.
    BigInt,
    /// `smallserial` — DDL only; an auto-incrementing `smallint`.
    SmallSerial,
    /// `serial` — DDL only; an auto-incrementing `integer`.
    Serial,
    /// `bigserial` — DDL only; an auto-incrementing `bigint`.
    BigSerial,
    /// `real`, 32-bit floating point.
    Real,
    /// `double precision`, 64-bit floating point.
    DoublePrecision,
    /// `numeric(p, s)` — exact decimal.
    Numeric {
        /// Total significant digits, if constrained.
        precision: Option<u8>,
        /// Digits after the decimal point, if constrained.
        scale: Option<u8>,
    },
    /// `text` — unbounded.
    Text,
    /// `varchar(n)`, or unbounded `varchar` when the length is `None`.
    VarChar(Option<u32>),
    /// `char(n)` — blank-padded.
    Char(Option<u32>),
    /// `bytea` on PostgreSQL, `blob` on SQLite.
    Bytea,
    /// `uuid`.
    Uuid,
    /// `json` — stored as received, including whitespace and key order.
    Json,
    /// `jsonb` — parsed and normalised. The default for entity JSON columns.
    JsonB,
    /// `date`.
    Date,
    /// `time` / `timetz`.
    Time {
        /// Whether the type carries a UTC offset.
        with_time_zone: bool,
    },
    /// `timestamp` / `timestamptz`.
    Timestamp {
        /// Whether the type carries a UTC offset. Entity timestamps should
        /// always set this: `timestamp` without a zone is a recurring source
        /// of production incidents.
        with_time_zone: bool,
    },
    /// `interval`.
    Interval,
    /// `inet`.
    Inet,
    /// `cidr`.
    Cidr,
    /// `macaddr`.
    MacAddr,
    /// `tsvector` — a parsed full-text document.
    TsVector,
    /// `tsquery` — a parsed full-text query.
    TsQuery,
    /// A one-dimensional array of another type.
    Array(Box<DataType>),
    /// A user-defined enum type, created by
    /// [`ddl::CreateType`](crate::ddl::CreateType).
    Enum(TypeRef),
    /// Any other named type: a domain, a composite, an extension type such as
    /// `vector` or `ltree`.
    Custom(TypeRef),
}

impl DataType {
    /// An array of `element`.
    ///
    /// ```
    /// use moso_sql::DataType;
    ///
    /// assert_eq!(
    ///     DataType::array_of(DataType::Integer),
    ///     DataType::Array(Box::new(DataType::Integer)),
    /// );
    /// ```
    #[must_use]
    pub fn array_of(element: DataType) -> Self {
        Self::Array(Box::new(element))
    }

    /// The element type, if this is an array.
    ///
    /// ```
    /// use moso_sql::DataType;
    ///
    /// assert_eq!(DataType::array_of(DataType::Uuid).element(), Some(&DataType::Uuid));
    /// assert_eq!(DataType::Uuid.element(), None);
    /// ```
    #[must_use]
    pub fn element(&self) -> Option<&DataType> {
        match self {
            Self::Array(inner) => Some(inner),
            _ => None,
        }
    }

    /// Whether this type carries its own sequence, so a column of it must not
    /// appear in an `INSERT` column list unless a value is given explicitly.
    ///
    /// ```
    /// use moso_sql::DataType;
    ///
    /// assert!(DataType::BigSerial.is_auto_increment());
    /// assert!(!DataType::BigInt.is_auto_increment());
    /// ```
    #[must_use]
    pub const fn is_auto_increment(&self) -> bool {
        matches!(self, Self::SmallSerial | Self::Serial | Self::BigSerial)
    }

    /// Whether values of this type are stored as JSON documents, which is what
    /// decides whether the `jsonb` operators apply.
    ///
    /// ```
    /// use moso_sql::DataType;
    ///
    /// assert!(DataType::JsonB.is_json());
    /// assert!(!DataType::Text.is_json());
    /// ```
    #[must_use]
    pub const fn is_json(&self) -> bool {
        matches!(self, Self::Json | Self::JsonB)
    }
}
