//! Bound-parameter values, and the Moso-owned scalar types they carry.
//!
//! Everything a statement can send to the server travels as a [`Value`]. The
//! scalar types here — [`Uuid`], [`Decimal`], [`Timestamp`], [`Date`],
//! [`Time`], [`DateTime`], [`Interval`], [`Json`] — are Moso's own, not
//! `chrono`'s or `uuid`'s, because ADR-0005 requires that no foreign type
//! appear in this crate's public API. They are deliberately plain: they carry
//! the bytes the wire protocol needs and nothing else. Calendar arithmetic and
//! conversion to the ecosystem's types belong to `moso-orm`, which is allowed
//! to name `chrono` and `uuid`.

use core::fmt;
use core::str::FromStr;

/// A value bound as a statement parameter.
///
/// A `Value` is never interpolated into SQL text. Dialects emit a placeholder
/// (`$1` for PostgreSQL, `?` for SQLite) and push the value onto
/// [`Sql::args`](crate::Sql::args), so a value can never become syntax.
///
/// ```
/// use moso_sql::{Value, ValueKind};
///
/// let name = Value::text("Ada");
/// assert_eq!(name.kind(), ValueKind::Text);
/// assert_eq!(name.as_str(), Some("Ada"));
///
/// // NULL carries the type it stands in for, so PostgreSQL can infer a cast.
/// assert!(Value::null(ValueKind::I64).is_null());
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// `NULL`, remembering the type it stands in for.
    Null(ValueKind),
    /// `boolean`.
    Bool(bool),
    /// An 8-bit signed integer. PostgreSQL widens it to `smallint`.
    I8(i8),
    /// `smallint`.
    I16(i16),
    /// `integer`.
    I32(i32),
    /// `bigint`.
    I64(i64),
    /// An 8-bit unsigned integer. PostgreSQL has no unsigned types and widens.
    U8(u8),
    /// A 16-bit unsigned integer.
    U16(u16),
    /// A 32-bit unsigned integer.
    U32(u32),
    /// A 64-bit unsigned integer. Values above `i64::MAX` are rejected by
    /// PostgreSQL at bind time, which is reported rather than truncated.
    U64(u64),
    /// `real`.
    F32(f32),
    /// `double precision`.
    F64(f64),
    /// `numeric` / `decimal`.
    Decimal(Decimal),
    /// `text` / `varchar`.
    Text(String),
    /// `bytea` / `blob`.
    Bytes(Vec<u8>),
    /// `uuid`.
    Uuid(Uuid),
    /// `json` / `jsonb`.
    Json(Json),
    /// `timestamptz` — an instant, always stored as UTC.
    Timestamp(Timestamp),
    /// `timestamp` — a calendar date and time with no zone.
    DateTime(DateTime),
    /// `date`.
    Date(Date),
    /// `time`.
    Time(Time),
    /// `interval`.
    Interval(Interval),
    /// A one-dimensional array. PostgreSQL only.
    Array(Array),
}

impl Value {
    /// A typed `NULL`.
    ///
    /// ```
    /// use moso_sql::{Value, ValueKind};
    ///
    /// let v = Value::null(ValueKind::Text);
    /// assert!(v.is_null());
    /// assert_eq!(v.kind(), ValueKind::Text);
    /// ```
    #[must_use]
    pub const fn null(kind: ValueKind) -> Self {
        Self::Null(kind)
    }

    /// A text value.
    ///
    /// ```
    /// assert_eq!(moso_sql::Value::text("hi").as_str(), Some("hi"));
    /// ```
    #[must_use]
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    /// A byte-string value.
    ///
    /// ```
    /// assert_eq!(moso_sql::Value::bytes([1, 2]).as_bytes(), Some(&[1u8, 2][..]));
    /// ```
    #[must_use]
    pub fn bytes(value: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(value.into())
    }

    /// Binds anything that implements [`Bindable`].
    ///
    /// ```
    /// use moso_sql::{Value, ValueKind};
    ///
    /// assert_eq!(Value::bind(7_i32), Value::I32(7));
    /// // `None` keeps the type, which is what lets PostgreSQL infer a cast.
    /// assert_eq!(Value::bind(None::<i32>), Value::Null(ValueKind::I32));
    /// ```
    #[must_use]
    pub fn bind<T: Bindable>(value: T) -> Self {
        value.into_value()
    }

    /// Binds JSON text as a `jsonb` parameter.
    ///
    /// Serialisation happens in the caller: see [`Json`] for why this crate
    /// has no `Serialize` bound anywhere in its public API.
    ///
    /// # Errors
    ///
    /// [`ValueError::Json`] if the text is not valid JSON.
    ///
    /// ```
    /// use moso_sql::Value;
    ///
    /// let v = Value::json("[1,2,3]")?;
    /// assert!(matches!(v, Value::Json(_)));
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    pub fn json(text: &str) -> Result<Self, ValueError> {
        Json::parse(text).map(Self::Json)
    }

    /// The type this value binds as, whether or not it is `NULL`.
    ///
    /// ```
    /// use moso_sql::{Value, ValueKind};
    ///
    /// assert_eq!(Value::Bool(true).kind(), ValueKind::Bool);
    /// assert_eq!(Value::null(ValueKind::Bool).kind(), ValueKind::Bool);
    /// ```
    #[must_use]
    pub const fn kind(&self) -> ValueKind {
        match self {
            Self::Null(kind) => *kind,
            Self::Bool(_) => ValueKind::Bool,
            Self::I8(_) => ValueKind::I8,
            Self::I16(_) => ValueKind::I16,
            Self::I32(_) => ValueKind::I32,
            Self::I64(_) => ValueKind::I64,
            Self::U8(_) => ValueKind::U8,
            Self::U16(_) => ValueKind::U16,
            Self::U32(_) => ValueKind::U32,
            Self::U64(_) => ValueKind::U64,
            Self::F32(_) => ValueKind::F32,
            Self::F64(_) => ValueKind::F64,
            Self::Decimal(_) => ValueKind::Decimal,
            Self::Text(_) => ValueKind::Text,
            Self::Bytes(_) => ValueKind::Bytes,
            Self::Uuid(_) => ValueKind::Uuid,
            Self::Json(_) => ValueKind::Json,
            Self::Timestamp(_) => ValueKind::Timestamp,
            Self::DateTime(_) => ValueKind::DateTime,
            Self::Date(_) => ValueKind::Date,
            Self::Time(_) => ValueKind::Time,
            Self::Interval(_) => ValueKind::Interval,
            Self::Array(_) => ValueKind::Array,
        }
    }

    /// Whether this value is `NULL`.
    ///
    /// ```
    /// use moso_sql::{Value, ValueKind};
    ///
    /// assert!(Value::null(ValueKind::Text).is_null());
    /// assert!(!Value::text("").is_null());
    /// ```
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null(_))
    }

    /// The value as a `bool`, if it is one.
    ///
    /// ```
    /// assert_eq!(moso_sql::Value::Bool(true).as_bool(), Some(true));
    /// ```
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// The value as an `i64`, widening any signed or unsigned integer that
    /// fits.
    ///
    /// ```
    /// assert_eq!(moso_sql::Value::I16(3).as_i64(), Some(3));
    /// assert_eq!(moso_sql::Value::text("3").as_i64(), None);
    /// ```
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::I8(value) => Some(i64::from(*value)),
            Self::I16(value) => Some(i64::from(*value)),
            Self::I32(value) => Some(i64::from(*value)),
            Self::I64(value) => Some(*value),
            Self::U8(value) => Some(i64::from(*value)),
            Self::U16(value) => Some(i64::from(*value)),
            Self::U32(value) => Some(i64::from(*value)),
            Self::U64(value) => i64::try_from(*value).ok(),
            _ => None,
        }
    }

    /// The value as a string slice, if it is text.
    ///
    /// ```
    /// assert_eq!(moso_sql::Value::text("x").as_str(), Some("x"));
    /// ```
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }

    /// The value as bytes, if it is a byte string.
    ///
    /// ```
    /// assert_eq!(moso_sql::Value::bytes([7]).as_bytes(), Some(&[7u8][..]));
    /// ```
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(value) => Some(value),
            _ => None,
        }
    }
}

/// The type of a [`Value`], with the payload removed.
///
/// Carried by `NULL` so a placeholder can be cast, and by [`Array`] so an empty
/// array still knows its element type.
///
/// ```
/// use moso_sql::{Value, ValueKind};
///
/// assert_eq!(Value::I32(1).kind(), ValueKind::I32);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ValueKind {
    /// The type is not known — an untyped `NULL`.
    Unknown,
    /// [`Value::Bool`].
    Bool,
    /// [`Value::I8`].
    I8,
    /// [`Value::I16`].
    I16,
    /// [`Value::I32`].
    I32,
    /// [`Value::I64`].
    I64,
    /// [`Value::U8`].
    U8,
    /// [`Value::U16`].
    U16,
    /// [`Value::U32`].
    U32,
    /// [`Value::U64`].
    U64,
    /// [`Value::F32`].
    F32,
    /// [`Value::F64`].
    F64,
    /// [`Value::Decimal`].
    Decimal,
    /// [`Value::Text`].
    Text,
    /// [`Value::Bytes`].
    Bytes,
    /// [`Value::Uuid`].
    Uuid,
    /// [`Value::Json`].
    Json,
    /// [`Value::Timestamp`].
    Timestamp,
    /// [`Value::DateTime`].
    DateTime,
    /// [`Value::Date`].
    Date,
    /// [`Value::Time`].
    Time,
    /// [`Value::Interval`].
    Interval,
    /// [`Value::Array`].
    Array,
}

/// A Rust type that can be bound as a statement parameter.
///
/// Implemented for the primitives, `String`, `&str`, `Vec<u8>`, `&[u8]`, this
/// crate's scalar types, and `Option<T>` for any bindable `T`. `moso-orm`
/// implements it for domain types on top.
///
/// `Vec<T>` is deliberately **not** implemented: `Vec<u8>` has to mean `bytea`,
/// so a blanket impl would be ambiguous. Build a PostgreSQL array with
/// [`Array::of`] instead.
///
/// ```
/// use moso_sql::{Bindable, Value, ValueKind};
///
/// assert_eq!(true.into_value(), Value::Bool(true));
/// assert_eq!(<Option<i32> as Bindable>::KIND, ValueKind::I32);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be bound as a SQL parameter",
    label = "not a bindable value",
    note = "a parameter must be a primitive, `String`, `&str`, `Vec<u8>`, or one of \
            `moso_sql`'s scalar types (`Uuid`, `Decimal`, `Timestamp`, `Date`, `Time`, \
            `DateTime`, `Interval`, `Json`, `Array`)",
    note = "for a list, build a PostgreSQL array: `Array::of(values)`",
    note = "for a struct, serialise it first and bind the text: \
            `Value::json(&serde_json::to_string(&value)?)?`",
    note = "help: implement `Bindable for {Self}` if this type maps to one column"
)]
pub trait Bindable: Sized {
    /// The type this value binds as. Used so that `None` still produces a
    /// typed `NULL`.
    const KIND: ValueKind;

    /// Converts into a bound parameter.
    fn into_value(self) -> Value;
}

macro_rules! bindable {
    ($($rust:ty => $kind:ident, $variant:ident;)*) => {
        $(
            impl Bindable for $rust {
                const KIND: ValueKind = ValueKind::$kind;

                fn into_value(self) -> Value {
                    Value::$variant(self)
                }
            }

            impl From<$rust> for Value {
                fn from(value: $rust) -> Self {
                    Value::$variant(value)
                }
            }
        )*
    };
}

bindable! {
    bool => Bool, Bool;
    i8 => I8, I8;
    i16 => I16, I16;
    i32 => I32, I32;
    i64 => I64, I64;
    u8 => U8, U8;
    u16 => U16, U16;
    u32 => U32, U32;
    u64 => U64, U64;
    f32 => F32, F32;
    f64 => F64, F64;
    String => Text, Text;
    Decimal => Decimal, Decimal;
    Uuid => Uuid, Uuid;
    Json => Json, Json;
    Timestamp => Timestamp, Timestamp;
    DateTime => DateTime, DateTime;
    Date => Date, Date;
    Time => Time, Time;
    Interval => Interval, Interval;
    Array => Array, Array;
}

impl Bindable for &str {
    const KIND: ValueKind = ValueKind::Text;

    fn into_value(self) -> Value {
        Value::Text(self.to_owned())
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::Text(value.to_owned())
    }
}

impl Bindable for Vec<u8> {
    const KIND: ValueKind = ValueKind::Bytes;

    fn into_value(self) -> Value {
        Value::Bytes(self)
    }
}

impl From<Vec<u8>> for Value {
    fn from(value: Vec<u8>) -> Self {
        Value::Bytes(value)
    }
}

impl Bindable for &[u8] {
    const KIND: ValueKind = ValueKind::Bytes;

    fn into_value(self) -> Value {
        Value::Bytes(self.to_vec())
    }
}

impl From<&[u8]> for Value {
    fn from(value: &[u8]) -> Self {
        Value::Bytes(value.to_vec())
    }
}

impl Bindable for Value {
    const KIND: ValueKind = ValueKind::Unknown;

    fn into_value(self) -> Value {
        self
    }
}

#[diagnostic::do_not_recommend]
impl<T: Bindable> Bindable for Option<T> {
    const KIND: ValueKind = T::KIND;

    fn into_value(self) -> Value {
        match self {
            Some(value) => value.into_value(),
            None => Value::Null(T::KIND),
        }
    }
}

#[diagnostic::do_not_recommend]
impl<T: Bindable> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.into_value()
    }
}

/// A 128-bit UUID.
///
/// Deliberately not `uuid::Uuid`: ADR-0005 keeps foreign types out of this
/// crate's public API. Convert at the `moso-orm` boundary through
/// [`Uuid::into_bytes`] and [`Uuid::from_bytes`], which are exactly the bytes
/// `uuid::Uuid::as_bytes` gives.
///
/// ```
/// use moso_sql::Uuid;
///
/// let id: Uuid = "f81d4fae-7dec-11d0-a765-00a0c91e6bf6".parse()?;
/// assert_eq!(id.to_string(), "f81d4fae-7dec-11d0-a765-00a0c91e6bf6");
/// # Ok::<(), moso_sql::ValueError>(())
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uuid([u8; 16]);

impl Uuid {
    /// The all-zero UUID.
    ///
    /// ```
    /// assert_eq!(moso_sql::Uuid::NIL.to_string(), "00000000-0000-0000-0000-000000000000");
    /// ```
    pub const NIL: Self = Self([0; 16]);

    /// Wraps the 16 big-endian bytes of a UUID.
    ///
    /// ```
    /// assert_eq!(moso_sql::Uuid::from_bytes([0; 16]), moso_sql::Uuid::NIL);
    /// ```
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The 16 big-endian bytes of the UUID.
    ///
    /// ```
    /// assert_eq!(moso_sql::Uuid::NIL.into_bytes(), [0; 16]);
    /// ```
    #[must_use]
    pub const fn into_bytes(self) -> [u8; 16] {
        self.0
    }

    /// Builds a UUID from its big-endian integer value.
    ///
    /// ```
    /// assert_eq!(moso_sql::Uuid::from_u128(0), moso_sql::Uuid::NIL);
    /// ```
    #[must_use]
    pub const fn from_u128(value: u128) -> Self {
        Self(value.to_be_bytes())
    }

    /// The UUID's big-endian integer value.
    ///
    /// ```
    /// assert_eq!(moso_sql::Uuid::NIL.as_u128(), 0);
    /// ```
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        u128::from_be_bytes(self.0)
    }

    /// Parses the hyphenated or unhyphenated 32-hex-digit form.
    ///
    /// # Errors
    ///
    /// [`ValueError::Uuid`] if the string is not 32 hexadecimal digits with
    /// optional hyphens.
    ///
    /// ```
    /// use moso_sql::Uuid;
    ///
    /// let a = Uuid::parse("f81d4fae7dec11d0a76500a0c91e6bf6")?;
    /// let b = Uuid::parse("f81d4fae-7dec-11d0-a765-00a0c91e6bf6")?;
    /// assert_eq!(a, b);
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    pub fn parse(text: &str) -> Result<Self, ValueError> {
        let mut bytes = [0_u8; 16];
        let mut nibble = 0_usize;
        let mut high: Option<u8> = None;
        for character in text.chars() {
            if character == '-' {
                continue;
            }
            let digit = character
                .to_digit(16)
                .ok_or_else(|| ValueError::Uuid(text.to_owned()))?;
            // A hexadecimal digit is 0..=15, so the narrowing cannot lose bits.
            let digit = u8::try_from(digit).unwrap_or_default();
            match high {
                None => high = Some(digit),
                Some(top) => {
                    if nibble >= 16 {
                        return Err(ValueError::Uuid(text.to_owned()));
                    }
                    bytes[nibble] = (top << 4) | digit;
                    nibble += 1;
                    high = None;
                }
            }
        }
        if nibble != 16 || high.is_some() {
            return Err(ValueError::Uuid(text.to_owned()));
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({self})")
    }
}

impl FromStr for Uuid {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// An exact decimal number: `mantissa × 10⁻ˢᶜᵃˡᵉ`.
///
/// This is the representation `numeric` columns round-trip through. It is
/// deliberately not a foreign decimal type (ADR-0005); the execution layer
/// converts at the edge.
///
/// ```
/// use moso_sql::Decimal;
///
/// let price = Decimal::new(1999, 2)?;
/// assert_eq!(price.to_string(), "19.99");
/// assert_eq!(price.mantissa(), 1999);
/// assert_eq!(price.scale(), 2);
/// # Ok::<(), moso_sql::ValueError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Decimal {
    mantissa: i128,
    scale: u32,
}

impl Decimal {
    /// The largest scale that survives a round trip through the execution
    /// layer's decimal type.
    ///
    /// ```
    /// assert_eq!(moso_sql::Decimal::MAX_SCALE, 28);
    /// ```
    pub const MAX_SCALE: u32 = 28;

    /// Zero, with scale zero.
    ///
    /// ```
    /// assert_eq!(moso_sql::Decimal::ZERO.to_string(), "0");
    /// ```
    pub const ZERO: Self = Self {
        mantissa: 0,
        scale: 0,
    };

    /// Builds `mantissa × 10⁻ˢᶜᵃˡᵉ`.
    ///
    /// # Errors
    ///
    /// [`ValueError::Scale`] if `scale` exceeds [`Decimal::MAX_SCALE`].
    ///
    /// ```
    /// assert_eq!(moso_sql::Decimal::new(-5, 1)?.to_string(), "-0.5");
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    pub const fn new(mantissa: i128, scale: u32) -> Result<Self, ValueError> {
        if scale > Self::MAX_SCALE {
            return Err(ValueError::Scale {
                scale,
                max: Self::MAX_SCALE,
            });
        }
        Ok(Self { mantissa, scale })
    }

    /// The unscaled integer value.
    ///
    /// ```
    /// assert_eq!(moso_sql::Decimal::new(1999, 2)?.mantissa(), 1999);
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    #[must_use]
    pub const fn mantissa(self) -> i128 {
        self.mantissa
    }

    /// The number of digits after the decimal point.
    ///
    /// ```
    /// assert_eq!(moso_sql::Decimal::new(1999, 2)?.scale(), 2);
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    #[must_use]
    pub const fn scale(self) -> u32 {
        self.scale
    }

    /// Whether the value is zero, at any scale.
    ///
    /// ```
    /// assert!(moso_sql::Decimal::new(0, 4)?.is_zero());
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.mantissa == 0
    }

    /// Parses a plain decimal string. Exponent notation is not accepted:
    /// `numeric` literals in migrations should be written out.
    ///
    /// # Errors
    ///
    /// [`ValueError::Decimal`] if the text is not a plain decimal number, and
    /// [`ValueError::Scale`] if it has more than [`Decimal::MAX_SCALE`]
    /// fractional digits.
    ///
    /// ```
    /// use moso_sql::Decimal;
    ///
    /// assert_eq!(Decimal::parse("-19.990")?.scale(), 3);
    /// assert!(Decimal::parse("1e5").is_err());
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    pub fn parse(text: &str) -> Result<Self, ValueError> {
        let trimmed = text.trim();
        let (negative, digits) = match trimmed.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
        };
        let (integer, fraction) = match digits.split_once('.') {
            Some((integer, fraction)) => (integer, fraction),
            None => (digits, ""),
        };
        if integer.is_empty() && fraction.is_empty() {
            return Err(ValueError::Decimal(text.to_owned()));
        }
        let mut mantissa: i128 = 0;
        for character in integer.chars().chain(fraction.chars()) {
            let digit = character
                .to_digit(10)
                .ok_or_else(|| ValueError::Decimal(text.to_owned()))?;
            mantissa = mantissa
                .checked_mul(10)
                .and_then(|value| value.checked_add(i128::from(digit)))
                .ok_or_else(|| ValueError::Decimal(text.to_owned()))?;
        }
        let scale =
            u32::try_from(fraction.len()).map_err(|_| ValueError::Decimal(text.to_owned()))?;
        Self::new(if negative { -mantissa } else { mantissa }, scale)
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale == 0 {
            return write!(f, "{}", self.mantissa);
        }
        let sign = if self.mantissa < 0 { "-" } else { "" };
        let digits = self.mantissa.unsigned_abs().to_string();
        let scale = usize::try_from(self.scale).unwrap_or(usize::MAX);
        if digits.len() > scale {
            let split = digits.len() - scale;
            write!(f, "{sign}{}.{}", &digits[..split], &digits[split..])
        } else {
            write!(f, "{sign}0.{:0>scale$}", digits)
        }
    }
}

impl FromStr for Decimal {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl From<i32> for Decimal {
    fn from(value: i32) -> Self {
        Self {
            mantissa: i128::from(value),
            scale: 0,
        }
    }
}

impl From<i64> for Decimal {
    fn from(value: i64) -> Self {
        Self {
            mantissa: i128::from(value),
            scale: 0,
        }
    }
}

/// An instant, as `timestamptz` stores one: seconds since the Unix epoch plus
/// a sub-second remainder, always UTC.
///
/// There is no calendar arithmetic here on purpose. `moso-orm` converts to and
/// from `chrono::DateTime<Utc>` at its boundary.
///
/// ```
/// use moso_sql::Timestamp;
///
/// let t = Timestamp::new(1_700_000_000, 500_000_000)?;
/// assert_eq!(t.unix_seconds(), 1_700_000_000);
/// assert_eq!(t.to_unix_millis(), 1_700_000_000_500);
/// # Ok::<(), moso_sql::ValueError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    unix_seconds: i64,
    nanoseconds: u32,
}

impl Timestamp {
    /// 1970-01-01T00:00:00Z.
    ///
    /// ```
    /// assert_eq!(moso_sql::Timestamp::UNIX_EPOCH.unix_seconds(), 0);
    /// ```
    pub const UNIX_EPOCH: Self = Self {
        unix_seconds: 0,
        nanoseconds: 0,
    };

    /// Builds an instant.
    ///
    /// # Errors
    ///
    /// [`ValueError::Nanoseconds`] if `nanoseconds` is not below one second.
    ///
    /// ```
    /// assert!(moso_sql::Timestamp::new(0, 1_000_000_000).is_err());
    /// ```
    pub const fn new(unix_seconds: i64, nanoseconds: u32) -> Result<Self, ValueError> {
        if nanoseconds >= 1_000_000_000 {
            return Err(ValueError::Nanoseconds(nanoseconds));
        }
        Ok(Self {
            unix_seconds,
            nanoseconds,
        })
    }

    /// Whole seconds since the Unix epoch.
    ///
    /// ```
    /// assert_eq!(moso_sql::Timestamp::UNIX_EPOCH.unix_seconds(), 0);
    /// ```
    #[must_use]
    pub const fn unix_seconds(self) -> i64 {
        self.unix_seconds
    }

    /// The sub-second remainder, below one second.
    ///
    /// ```
    /// assert_eq!(moso_sql::Timestamp::UNIX_EPOCH.nanoseconds(), 0);
    /// ```
    #[must_use]
    pub const fn nanoseconds(self) -> u32 {
        self.nanoseconds
    }

    /// Builds an instant from milliseconds since the Unix epoch.
    ///
    /// ```
    /// assert_eq!(moso_sql::Timestamp::from_unix_millis(-1).to_unix_millis(), -1);
    /// ```
    #[must_use]
    pub const fn from_unix_millis(millis: i64) -> Self {
        let seconds = millis.div_euclid(1_000);
        // `rem_euclid` with a positive divisor lands in `0..1_000`, so the
        // narrowing below is exact for every input.
        let remainder = millis.rem_euclid(1_000);
        Self {
            unix_seconds: seconds,
            nanoseconds: (remainder as u32) * 1_000_000,
        }
    }

    /// Milliseconds since the Unix epoch, truncating the sub-millisecond part.
    ///
    /// ```
    /// assert_eq!(moso_sql::Timestamp::UNIX_EPOCH.to_unix_millis(), 0);
    /// ```
    #[must_use]
    pub const fn to_unix_millis(self) -> i64 {
        self.unix_seconds * 1_000 + (self.nanoseconds / 1_000_000) as i64
    }

    /// The instant as whole microseconds since the PostgreSQL epoch
    /// (2000-01-01T00:00:00Z), which is the wire representation of
    /// `timestamptz`.
    ///
    /// ```
    /// assert_eq!(moso_sql::Timestamp::UNIX_EPOCH.to_postgres_micros(), -946_684_800_000_000);
    /// ```
    #[must_use]
    pub const fn to_postgres_micros(self) -> i64 {
        const POSTGRES_EPOCH_UNIX_SECONDS: i64 = 946_684_800;
        (self.unix_seconds - POSTGRES_EPOCH_UNIX_SECONDS) * 1_000_000
            + (self.nanoseconds / 1_000) as i64
    }
}

/// A calendar date, as `date` stores one.
///
/// ```
/// use moso_sql::Date;
///
/// let d = Date::new(2026, 7, 30)?;
/// assert_eq!(d.to_string(), "2026-07-30");
/// assert!(Date::new(2025, 2, 29).is_err());
/// # Ok::<(), moso_sql::ValueError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: i32,
    month: u8,
    day: u8,
}

impl Date {
    /// Builds a date, checking the month and the day against the calendar.
    ///
    /// # Errors
    ///
    /// [`ValueError::Month`] or [`ValueError::Day`].
    ///
    /// ```
    /// assert!(moso_sql::Date::new(2024, 2, 29).is_ok());
    /// assert!(moso_sql::Date::new(2024, 13, 1).is_err());
    /// ```
    pub const fn new(year: i32, month: u8, day: u8) -> Result<Self, ValueError> {
        if month == 0 || month > 12 {
            return Err(ValueError::Month(month));
        }
        if day == 0 || day > days_in_month(year, month) {
            return Err(ValueError::Day { year, month, day });
        }
        Ok(Self { year, month, day })
    }

    /// The year. Negative years are BCE, as PostgreSQL renders them.
    ///
    /// ```
    /// assert_eq!(moso_sql::Date::new(2026, 1, 1)?.year(), 2026);
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    #[must_use]
    pub const fn year(self) -> i32 {
        self.year
    }

    /// The month, `1..=12`.
    ///
    /// ```
    /// assert_eq!(moso_sql::Date::new(2026, 1, 1)?.month(), 1);
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    #[must_use]
    pub const fn month(self) -> u8 {
        self.month
    }

    /// The day of the month, `1..=31`.
    ///
    /// ```
    /// assert_eq!(moso_sql::Date::new(2026, 1, 1)?.day(), 1);
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    #[must_use]
    pub const fn day(self) -> u8 {
        self.day
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Whether `year` is a leap year in the proleptic Gregorian calendar, which is
/// the calendar PostgreSQL uses.
const fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// The number of days in `month`. `month` must be `1..=12`.
const fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        _ => 28,
    }
}

/// A wall-clock time, as `time` stores one.
///
/// ```
/// use moso_sql::Time;
///
/// assert_eq!(Time::new(9, 5, 0, 0)?.to_string(), "09:05:00");
/// assert_eq!(Time::new(9, 5, 0, 250_000_000)?.to_string(), "09:05:00.250000000");
/// # Ok::<(), moso_sql::ValueError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time {
    hour: u8,
    minute: u8,
    second: u8,
    nanosecond: u32,
}

impl Time {
    /// Midnight.
    ///
    /// ```
    /// assert_eq!(moso_sql::Time::MIDNIGHT.to_string(), "00:00:00");
    /// ```
    pub const MIDNIGHT: Self = Self {
        hour: 0,
        minute: 0,
        second: 0,
        nanosecond: 0,
    };

    /// Builds a time of day.
    ///
    /// # Errors
    ///
    /// [`ValueError::Hour`], [`ValueError::Minute`], [`ValueError::Second`] or
    /// [`ValueError::Nanoseconds`].
    ///
    /// ```
    /// assert!(moso_sql::Time::new(24, 0, 0, 0).is_err());
    /// ```
    pub const fn new(
        hour: u8,
        minute: u8,
        second: u8,
        nanosecond: u32,
    ) -> Result<Self, ValueError> {
        if hour > 23 {
            return Err(ValueError::Hour(hour));
        }
        if minute > 59 {
            return Err(ValueError::Minute(minute));
        }
        if second > 59 {
            return Err(ValueError::Second(second));
        }
        if nanosecond >= 1_000_000_000 {
            return Err(ValueError::Nanoseconds(nanosecond));
        }
        Ok(Self {
            hour,
            minute,
            second,
            nanosecond,
        })
    }

    /// The hour, `0..=23`.
    ///
    /// ```
    /// assert_eq!(moso_sql::Time::MIDNIGHT.hour(), 0);
    /// ```
    #[must_use]
    pub const fn hour(self) -> u8 {
        self.hour
    }

    /// The minute, `0..=59`.
    ///
    /// ```
    /// assert_eq!(moso_sql::Time::MIDNIGHT.minute(), 0);
    /// ```
    #[must_use]
    pub const fn minute(self) -> u8 {
        self.minute
    }

    /// The second, `0..=59`.
    ///
    /// ```
    /// assert_eq!(moso_sql::Time::MIDNIGHT.second(), 0);
    /// ```
    #[must_use]
    pub const fn second(self) -> u8 {
        self.second
    }

    /// The sub-second remainder, below one second.
    ///
    /// ```
    /// assert_eq!(moso_sql::Time::MIDNIGHT.nanosecond(), 0);
    /// ```
    #[must_use]
    pub const fn nanosecond(self) -> u32 {
        self.nanosecond
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}:{:02}", self.hour, self.minute, self.second)?;
        if self.nanosecond != 0 {
            write!(f, ".{:09}", self.nanosecond)?;
        }
        Ok(())
    }
}

/// A calendar date and time with no time zone, as `timestamp` stores one.
///
/// ```
/// use moso_sql::{Date, DateTime, Time};
///
/// let at = DateTime::new(Date::new(2026, 7, 30)?, Time::new(9, 5, 0, 0)?);
/// assert_eq!(at.to_string(), "2026-07-30 09:05:00");
/// # Ok::<(), moso_sql::ValueError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DateTime {
    date: Date,
    time: Time,
}

impl DateTime {
    /// Pairs a date with a time.
    ///
    /// ```
    /// use moso_sql::{Date, DateTime, Time};
    ///
    /// let at = DateTime::new(Date::new(2026, 1, 1)?, Time::MIDNIGHT);
    /// assert_eq!(at.time(), Time::MIDNIGHT);
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    #[must_use]
    pub const fn new(date: Date, time: Time) -> Self {
        Self { date, time }
    }

    /// The date part.
    ///
    /// ```
    /// use moso_sql::{Date, DateTime, Time};
    ///
    /// let d = Date::new(2026, 1, 1)?;
    /// assert_eq!(DateTime::new(d, Time::MIDNIGHT).date(), d);
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    #[must_use]
    pub const fn date(self) -> Date {
        self.date
    }

    /// The time part.
    ///
    /// ```
    /// use moso_sql::{Date, DateTime, Time};
    ///
    /// let at = DateTime::new(Date::new(2026, 1, 1)?, Time::MIDNIGHT);
    /// assert_eq!(at.time().hour(), 0);
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    #[must_use]
    pub const fn time(self) -> Time {
        self.time
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.date, self.time)
    }
}

/// A PostgreSQL `interval`, in its three independent components.
///
/// Months, days and microseconds are kept apart because they are not
/// interchangeable: a month is not 30 days and a day is not always 24 hours
/// across a daylight-saving boundary. This is exactly how the server stores
/// the type.
///
/// ```
/// use moso_sql::Interval;
///
/// let a_fortnight = Interval::from_days(14);
/// assert_eq!(a_fortnight.day_component(), 14);
/// assert_eq!(a_fortnight.to_string(), "14 days");
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Interval {
    months: i32,
    days: i32,
    microseconds: i64,
}

impl Interval {
    /// The zero interval.
    ///
    /// ```
    /// assert_eq!(moso_sql::Interval::ZERO.to_string(), "0 seconds");
    /// ```
    pub const ZERO: Self = Self {
        months: 0,
        days: 0,
        microseconds: 0,
    };

    /// Builds an interval from its three components.
    ///
    /// ```
    /// assert_eq!(moso_sql::Interval::new(1, 0, 0).months(), 1);
    /// ```
    #[must_use]
    pub const fn new(months: i32, days: i32, microseconds: i64) -> Self {
        Self {
            months,
            days,
            microseconds,
        }
    }

    /// An interval of whole days.
    ///
    /// ```
    /// assert_eq!(moso_sql::Interval::from_days(7).day_component(), 7);
    /// ```
    #[must_use]
    pub const fn from_days(days: i32) -> Self {
        Self::new(0, days, 0)
    }

    /// An interval of whole months.
    ///
    /// ```
    /// assert_eq!(moso_sql::Interval::from_months(3).months(), 3);
    /// ```
    #[must_use]
    pub const fn from_months(months: i32) -> Self {
        Self::new(months, 0, 0)
    }

    /// An interval of whole seconds.
    ///
    /// ```
    /// assert_eq!(moso_sql::Interval::from_seconds(90).microseconds(), 90_000_000);
    /// ```
    #[must_use]
    pub const fn from_seconds(seconds: i64) -> Self {
        Self::new(0, 0, seconds * 1_000_000)
    }

    /// The month component.
    ///
    /// ```
    /// assert_eq!(moso_sql::Interval::new(3, 0, 0).months(), 3);
    /// ```
    #[must_use]
    pub const fn months(self) -> i32 {
        self.months
    }

    /// The day component. Named `day_component` rather than `days` because
    /// [`Interval::from_days`] already owns the shorter reading.
    ///
    /// ```
    /// assert_eq!(moso_sql::Interval::new(0, 3, 0).day_component(), 3);
    /// ```
    #[must_use]
    pub const fn day_component(self) -> i32 {
        self.days
    }

    /// The sub-day component, in microseconds.
    ///
    /// ```
    /// assert_eq!(moso_sql::Interval::from_seconds(1).microseconds(), 1_000_000);
    /// ```
    #[must_use]
    pub const fn microseconds(self) -> i64 {
        self.microseconds
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut wrote = false;
        if self.months != 0 {
            write!(f, "{} mons", self.months)?;
            wrote = true;
        }
        if self.days != 0 {
            if wrote {
                f.write_str(" ")?;
            }
            write!(f, "{} days", self.days)?;
            wrote = true;
        }
        if self.microseconds != 0 || !wrote {
            if wrote {
                f.write_str(" ")?;
            }
            let seconds = self.microseconds / 1_000_000;
            let remainder = (self.microseconds % 1_000_000).abs();
            if remainder == 0 {
                write!(f, "{seconds} seconds")?;
            } else {
                write!(f, "{seconds}.{remainder:06} seconds")?;
            }
        }
        Ok(())
    }
}

/// A one-dimensional SQL array.
///
/// The element type is carried separately so that an empty array still binds
/// as `int[]` rather than as an untyped literal.
///
/// ```
/// use moso_sql::{Array, ValueKind};
///
/// let tags = Array::of(["rust", "sql"]);
/// assert_eq!(tags.element_kind(), ValueKind::Text);
/// assert_eq!(tags.len(), 2);
///
/// let empty = Array::empty(ValueKind::I32);
/// assert!(empty.is_empty());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Array {
    element: ValueKind,
    items: Vec<Value>,
}

impl Array {
    /// An array of already-built values, with an explicit element type.
    ///
    /// ```
    /// use moso_sql::{Array, Value, ValueKind};
    ///
    /// let a = Array::new(ValueKind::I32, [Value::I32(1), Value::I32(2)]);
    /// assert_eq!(a.len(), 2);
    /// ```
    #[must_use]
    pub fn new(element: ValueKind, items: impl IntoIterator<Item = Value>) -> Self {
        Self {
            element,
            items: items.into_iter().collect(),
        }
    }

    /// An array built from bindable values, taking the element type from `T`.
    ///
    /// ```
    /// use moso_sql::{Array, ValueKind};
    ///
    /// assert_eq!(Array::of([1_i64, 2]).element_kind(), ValueKind::I64);
    /// ```
    #[must_use]
    pub fn of<T: Bindable>(items: impl IntoIterator<Item = T>) -> Self {
        Self {
            element: T::KIND,
            items: items.into_iter().map(Bindable::into_value).collect(),
        }
    }

    /// An empty array of a known element type.
    ///
    /// ```
    /// use moso_sql::{Array, ValueKind};
    ///
    /// assert!(Array::empty(ValueKind::Uuid).is_empty());
    /// ```
    #[must_use]
    pub const fn empty(element: ValueKind) -> Self {
        Self {
            element,
            items: Vec::new(),
        }
    }

    /// The element type.
    ///
    /// ```
    /// use moso_sql::{Array, ValueKind};
    ///
    /// assert_eq!(Array::empty(ValueKind::Bool).element_kind(), ValueKind::Bool);
    /// ```
    #[must_use]
    pub const fn element_kind(&self) -> ValueKind {
        self.element
    }

    /// The elements.
    ///
    /// ```
    /// use moso_sql::{Array, Value};
    ///
    /// assert_eq!(Array::of([1_i32]).items(), &[Value::I32(1)]);
    /// ```
    #[must_use]
    pub fn items(&self) -> &[Value] {
        &self.items
    }

    /// How many elements the array has.
    ///
    /// ```
    /// assert_eq!(moso_sql::Array::of([1_i32, 2, 3]).len(), 3);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the array has no elements.
    ///
    /// ```
    /// assert!(moso_sql::Array::of(Vec::<i32>::new()).is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Consumes the array and returns its elements.
    ///
    /// ```
    /// assert_eq!(moso_sql::Array::of([1_i32]).into_items().len(), 1);
    /// ```
    #[must_use]
    pub fn into_items(self) -> Vec<Value> {
        self.items
    }
}

/// A JSON document bound as a `json` or `jsonb` parameter.
///
/// # Why there is no `Serialize` bound here
///
/// A `Json::from_serialize<T: Serialize>` would be more convenient and would
/// put `serde` into this crate's public API, which [ADR-0005] forbids — the
/// point of the facade is that *nothing* foreign is reachable from it, not
/// "nothing except the crates we happen to trust today". Serialisation is the
/// caller's job, and `moso-orm`, which is allowed to name `serde`, does it:
///
/// ```
/// # use moso_sql::{Json, Value};
/// # #[derive(serde::Serialize)]
/// # struct Preferences { theme: &'static str }
/// let preferences = Preferences { theme: "dark" };
/// let text = serde_json::to_string(&preferences).expect("serialisable");
/// let document = Json::from_json_string(text)?;
/// assert_eq!(document.as_json_str(), r#"{"theme":"dark"}"#);
/// # let _ = Value::Json(document);
/// # Ok::<(), moso_sql::ValueError>(())
/// ```
///
/// The text is validated on the way in and held as compact JSON, so a `Json`
/// is always a document the server will accept.
///
/// ```
/// use moso_sql::Json;
///
/// let document = Json::parse("[1,  2, 3]")?;
/// assert_eq!(document.as_json_str(), "[1,2,3]");
/// assert!(Json::parse("{").is_err());
/// # Ok::<(), moso_sql::ValueError>(())
/// ```
///
/// [ADR-0005]: https://github.com/lowsbarrel/moso/blob/main/docs/adr/0005-sealed-sql-facade.md
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Json(String);

impl Json {
    /// The JSON `null` document — which is not the same as a SQL `NULL`.
    ///
    /// ```
    /// assert_eq!(moso_sql::Json::null().as_json_str(), "null");
    /// ```
    #[must_use]
    pub fn null() -> Self {
        Self("null".to_owned())
    }

    /// Parses JSON text and stores it in compact form.
    ///
    /// # Errors
    ///
    /// [`ValueError::Json`] if the text is not valid JSON.
    ///
    /// ```
    /// assert!(moso_sql::Json::parse("{").is_err());
    /// assert_eq!(moso_sql::Json::parse(" 7 ")?.as_json_str(), "7");
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    pub fn parse(text: &str) -> Result<Self, ValueError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|error| ValueError::Json(error.to_string()))?;
        Ok(Self(value.to_string()))
    }

    /// Takes ownership of JSON text, validating it but keeping it byte for
    /// byte rather than re-serialising it.
    ///
    /// This is the constructor a write path should use: the caller has just
    /// produced the text with its own serialiser, and [`Json::parse`] would
    /// throw that string away and build another one.
    ///
    /// # Errors
    ///
    /// [`ValueError::Json`] if the text is not valid JSON.
    ///
    /// ```
    /// let document = moso_sql::Json::from_json_string("[1, 2]".to_owned())?;
    /// // Kept as written — only `parse` normalises.
    /// assert_eq!(document.as_json_str(), "[1, 2]");
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    pub fn from_json_string(text: String) -> Result<Self, ValueError> {
        serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|error| ValueError::Json(error.to_string()))?;
        Ok(Self(text))
    }

    /// The document as compact JSON text.
    ///
    /// ```
    /// assert_eq!(moso_sql::Json::parse("[1, 2]")?.as_json_str(), "[1,2]");
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    #[must_use]
    pub fn as_json_str(&self) -> &str {
        &self.0
    }

    /// Consumes the document and returns its text.
    ///
    /// ```
    /// assert_eq!(moso_sql::Json::null().into_json_string(), "null");
    /// ```
    #[must_use]
    pub fn into_json_string(self) -> String {
        self.0
    }

    /// Whether the document is JSON `null`.
    ///
    /// ```
    /// assert!(moso_sql::Json::null().is_json_null());
    /// assert!(!moso_sql::Json::parse("0")?.is_json_null());
    /// # Ok::<(), moso_sql::ValueError>(())
    /// ```
    #[must_use]
    pub fn is_json_null(&self) -> bool {
        self.0 == "null"
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Json {
    type Err = ValueError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// Why a value could not be built.
///
/// ```
/// use moso_sql::{Decimal, ValueError};
///
/// let error = Decimal::parse("nope").expect_err("not a number");
/// assert!(matches!(error, ValueError::Decimal(_)));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ValueError {
    /// A decimal scale beyond [`Decimal::MAX_SCALE`].
    #[error(
        "a decimal scale of {scale} is more than the {max} digits that survive a round trip\n\
         help: round the value before binding it, or store it as text"
    )]
    Scale {
        /// The rejected scale.
        scale: u32,
        /// The accepted maximum.
        max: u32,
    },

    /// A sub-second component of one second or more.
    #[error(
        "{0} nanoseconds is not below one second\n\
         help: carry the whole seconds in the seconds field"
    )]
    Nanoseconds(u32),

    /// A month outside `1..=12`.
    #[error("{0} is not a month; months are 1..=12")]
    Month(u8),

    /// A day that does not exist in that month of that year.
    #[error("{year:04}-{month:02}-{day:02} is not a date; that month has fewer days")]
    Day {
        /// The year that was given.
        year: i32,
        /// The month that was given.
        month: u8,
        /// The day that was given.
        day: u8,
    },

    /// An hour outside `0..=23`.
    #[error("{0} is not an hour; hours are 0..=23")]
    Hour(u8),

    /// A minute outside `0..=59`.
    #[error("{0} is not a minute; minutes are 0..=59")]
    Minute(u8),

    /// A second outside `0..=59`.
    #[error("{0} is not a second; seconds are 0..=59, and leap seconds are not representable")]
    Second(u8),

    /// A string that is not a UUID.
    #[error(
        "`{0}` is not a UUID\n\
         help: a UUID is 32 hexadecimal digits, optionally grouped 8-4-4-4-12 with hyphens"
    )]
    Uuid(String),

    /// A string that is not a plain decimal number.
    #[error(
        "`{0}` is not a decimal number\n\
         help: write it out in full — `19.99`, not `1.999e1`"
    )]
    Decimal(String),

    /// JSON that could not be serialised, parsed or deserialised.
    #[error("the JSON value is not usable: {0}")]
    Json(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_none_keeps_its_column_type() {
        assert_eq!(Value::bind(None::<i32>), Value::Null(ValueKind::I32));
        assert_eq!(Value::bind(Some(3_i32)), Value::I32(3));
        assert_eq!(<Option<String> as Bindable>::KIND, ValueKind::Text);
    }

    #[test]
    fn kind_survives_null() {
        for value in [
            Value::Bool(true),
            Value::I64(1),
            Value::text("x"),
            Value::bytes([1]),
            Value::Uuid(Uuid::NIL),
        ] {
            let kind = value.kind();
            assert_eq!(Value::null(kind).kind(), kind);
        }
    }

    #[test]
    fn uuid_round_trips_through_both_spellings() {
        let id = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let text = id.to_string();
        assert_eq!(text.len(), 36);
        assert_eq!(Uuid::parse(&text).expect("hyphenated"), id);
        assert_eq!(
            Uuid::parse(&text.replace('-', "")).expect("unhyphenated"),
            id
        );
        assert!(Uuid::parse("not-a-uuid").is_err());
        assert!(Uuid::parse("0123").is_err());
    }

    #[test]
    fn decimal_renders_the_way_a_migration_should_read() {
        let cases = [
            (0_i128, 0_u32, "0"),
            (1999, 2, "19.99"),
            (-1999, 2, "-19.99"),
            (5, 3, "0.005"),
            (-5, 1, "-0.5"),
            (100, 2, "1.00"),
        ];
        for (mantissa, scale, expected) in cases {
            let decimal = Decimal::new(mantissa, scale).expect("in range");
            assert_eq!(decimal.to_string(), expected);
            assert_eq!(Decimal::parse(expected).expect("round trip"), decimal);
        }
    }

    #[test]
    fn decimal_rejects_exponents_and_overlong_scales() {
        assert!(Decimal::parse("1e5").is_err());
        assert!(Decimal::parse("").is_err());
        assert!(Decimal::new(1, Decimal::MAX_SCALE + 1).is_err());
    }

    #[test]
    fn the_calendar_is_gregorian() {
        assert!(Date::new(2024, 2, 29).is_ok());
        assert!(Date::new(2100, 2, 29).is_err());
        assert!(Date::new(2000, 2, 29).is_ok());
        assert!(Date::new(2026, 0, 1).is_err());
        assert!(Date::new(2026, 4, 31).is_err());
    }

    #[test]
    fn time_renders_fractional_seconds_only_when_present() {
        assert_eq!(
            Time::new(1, 2, 3, 0).expect("valid").to_string(),
            "01:02:03"
        );
        assert_eq!(
            Time::new(1, 2, 3, 4).expect("valid").to_string(),
            "01:02:03.000000004"
        );
    }

    #[test]
    fn intervals_keep_their_components_apart() {
        let interval = Interval::new(1, 2, 3_500_000);
        assert_eq!(interval.months(), 1);
        assert_eq!(interval.day_component(), 2);
        assert_eq!(interval.microseconds(), 3_500_000);
        assert_eq!(interval.to_string(), "1 mons 2 days 3.500000 seconds");
        assert_eq!(Interval::ZERO.to_string(), "0 seconds");
    }

    #[test]
    fn an_empty_array_still_knows_its_element_type() {
        let empty = Array::of(Vec::<i64>::new());
        assert!(empty.is_empty());
        assert_eq!(empty.element_kind(), ValueKind::I64);
    }

    #[test]
    fn json_round_trips() {
        let document = Json::parse("[1,  2,\n3]").expect("valid JSON");
        assert_eq!(document.as_json_str(), "[1,2,3]");
        assert_eq!(
            Json::from_json_string("[1,2,3]".to_owned()).expect("valid JSON"),
            document,
            "text that is already compact round-trips without re-serialising"
        );
        assert!(Json::parse("{").is_err());
        assert!(Json::from_json_string("{".to_owned()).is_err());
        assert!(Json::null().is_json_null());
        assert!(!Json::parse("0").expect("valid JSON").is_json_null());
    }

    #[test]
    fn timestamp_millis_round_trip_across_the_epoch() {
        for millis in [-1_500_i64, -1, 0, 1, 1_500] {
            assert_eq!(Timestamp::from_unix_millis(millis).to_unix_millis(), millis);
        }
    }
}
