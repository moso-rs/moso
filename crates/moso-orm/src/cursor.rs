//! [`PageCursor`] — what a keyset cursor carries, and what seals it.
//!
//! A cursor is the sort-key tuple of the last row a client saw, plus a
//! fingerprint of the ordering it was produced for. It travels in a query
//! string, so it is three things at once:
//!
//! 1. **Opaque.** The client sees base64url and nothing else, which is what lets
//!    the pagination key change without breaking every client.
//! 2. **Authenticated.** The tuple goes straight into a `WHERE` clause. An
//!    unsigned cursor is a query parameter an attacker can edit, so every cursor
//!    carries a truncated HMAC-SHA256 tag over the payload *and* a scope label.
//! 3. **Self-describing about its ordering.** Resuming a `created_at DESC` scan
//!    with a cursor minted for `title ASC` would skip and repeat rows in equal
//!    measure, so the ordering is fingerprinted into the payload and checked.
//!
//! # The signing is [`CursorCodec`]'s, not this module's
//!
//! `moso-core` already owns the MAC, the wire format, the scope binding and the
//! one deliberately-indistinguishable rejection. This module does **not** fork
//! it: it produces the payload bytes and hands them to
//! [`CursorCodec::sign`], and gets them back from [`CursorCodec::verify`].
//! Everything below the `payload` line in `moso-core`'s diagram belongs to that
//! type.
//!
//! ```
//! use moso_core::response::cursor::CursorCodec;
//! use moso_orm::cursor::PageCursor;
//! use moso_sql::Value;
//!
//! let codec = CursorCodec::new("an application secret that is long enough");
//!
//! // The last row of page one: `created_at = 1700000000`, `id = 42`.
//! let minted = PageCursor::new(0x1234_5678, [Value::I64(1_700_000_000), Value::I64(42)]);
//! let token = minted.seal(&codec, "Post").unwrap();
//!
//! // Page two opens it and gets the tuple back, exactly.
//! let opened = PageCursor::open(&codec, "Post", &token).unwrap();
//! assert_eq!(opened.key(), minted.key());
//! assert_eq!(opened.ordering(), 0x1234_5678);
//!
//! // A cursor minted for one listing does not open against another …
//! assert!(PageCursor::open(&codec, "Comment", &token).is_err());
//! // … and neither does one issued by a server with a different secret.
//! let theirs = CursorCodec::new("a completely different application secret");
//! assert!(PageCursor::open(&theirs, "Post", &token).is_err());
//! ```
//!
//! # The payload
//!
//! ```text
//! ┌─────────┬──────────────────────┬────────┬───────────────────────┐
//! │ format  │ ordering fingerprint │ arity  │  arity × tagged value │
//! │ 1 byte  │       8 bytes        │ 1 byte │                       │
//! └─────────┴──────────────────────┴────────┴───────────────────────┘
//! ```
//!
//! Every value is length- or width-prefixed by its own tag, so decoding never
//! trusts a length it has not bounds-checked, and a payload that runs out mid
//! value is a rejection rather than a panic.
//!
//! # Why the ordering is a 64-bit fingerprint and not the ordering itself
//!
//! Spelling `posts.created_at desc nulls first, posts.id desc` into every cursor
//! would cost forty bytes of a token that has to fit in a URL, and would tell a
//! client the column names it is being sorted by. A fingerprint costs eight
//! bytes and says only "not this ordering".
//!
//! It is **not** a security primitive: a cursor cannot be minted without the
//! secret, so nobody can grind a collision. It only has to separate the
//! orderings one application actually uses, which is what FNV-1a is for.

use moso_core::response::cursor::CursorCodec;
use moso_schema::types::Cursor;
use moso_sql::{Date, DateTime, Decimal, Interval, Json, Time, Timestamp, Uuid, Value, ValueKind};

use crate::error::{CursorError, Error, Result};

/// The payload's format byte.
///
/// A future change of layout is a clean [`CursorError::Malformed`] rather than a
/// confusing decode of the wrong shape.
///
/// ```
/// assert_eq!(moso_orm::cursor::FORMAT, 1);
/// ```
pub const FORMAT: u8 = 1;

/// The most sort-key columns a cursor will carry.
///
/// Sixteen is far past any real ordering; the limit is here so that a corrupt
/// arity byte cannot make the decoder allocate.
///
/// ```
/// assert!(moso_orm::cursor::MAX_KEY_COLUMNS >= 4);
/// ```
pub const MAX_KEY_COLUMNS: usize = 16;

/// The sort-key tuple of one row, and the ordering it belongs to.
///
/// This is the *inside* of a pagination cursor. [`PageCursor::seal`] turns it
/// into an opaque [`Cursor`] and [`PageCursor::open`] turns one back, refusing
/// anything this application did not mint.
///
/// ```
/// use moso_core::response::cursor::CursorCodec;
/// use moso_orm::cursor::PageCursor;
/// use moso_sql::Value;
///
/// let codec = CursorCodec::new("an application secret that is long enough");
/// let cursor = PageCursor::new(7, [Value::text("ada"), Value::I64(1)]);
///
/// let token = cursor.seal(&codec, "User").unwrap();
/// assert_eq!(PageCursor::open(&codec, "User", &token).unwrap().len(), 2);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct PageCursor {
    ordering: u64,
    key: Vec<Value>,
}

impl PageCursor {
    /// The tuple `key`, issued for the ordering whose fingerprint is `ordering`.
    ///
    /// ```
    /// use moso_orm::cursor::PageCursor;
    /// use moso_sql::Value;
    ///
    /// let cursor = PageCursor::new(1, [Value::I64(9)]);
    /// assert_eq!(cursor.key(), [Value::I64(9)]);
    /// ```
    #[must_use]
    pub fn new(ordering: u64, key: impl IntoIterator<Item = Value>) -> Self {
        Self {
            ordering,
            key: key.into_iter().collect(),
        }
    }

    /// The fingerprint of the ordering this cursor was issued for.
    ///
    /// ```
    /// use moso_orm::cursor::PageCursor;
    ///
    /// assert_eq!(PageCursor::new(42, []).ordering(), 42);
    /// ```
    #[must_use]
    pub const fn ordering(&self) -> u64 {
        self.ordering
    }

    /// The sort-key tuple, in ordering-term order.
    ///
    /// ```
    /// use moso_orm::cursor::PageCursor;
    /// use moso_sql::Value;
    ///
    /// assert_eq!(PageCursor::new(0, [Value::Bool(true)]).key().len(), 1);
    /// ```
    #[must_use]
    pub fn key(&self) -> &[Value] {
        &self.key
    }

    /// The sort-key tuple, consuming the cursor.
    ///
    /// ```
    /// use moso_orm::cursor::PageCursor;
    /// use moso_sql::Value;
    ///
    /// assert_eq!(PageCursor::new(0, [Value::I32(3)]).into_key(), vec![Value::I32(3)]);
    /// ```
    #[must_use]
    pub fn into_key(self) -> Vec<Value> {
        self.key
    }

    /// How many columns the sort key has.
    ///
    /// ```
    /// use moso_orm::cursor::PageCursor;
    /// use moso_sql::Value;
    ///
    /// assert_eq!(PageCursor::new(0, [Value::I64(1), Value::I64(2)]).len(), 2);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.key.len()
    }

    /// Whether the sort key is empty, which no query should produce.
    ///
    /// ```
    /// use moso_orm::cursor::PageCursor;
    ///
    /// assert!(PageCursor::new(0, []).is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.key.is_empty()
    }

    /// Whether this cursor was issued for the ordering fingerprinted as
    /// `ordering`, with the same number of columns.
    ///
    /// ```
    /// use moso_orm::cursor::PageCursor;
    /// use moso_sql::Value;
    ///
    /// let cursor = PageCursor::new(5, [Value::I64(1)]);
    /// assert!(cursor.matches(5, 1));
    /// assert!(!cursor.matches(6, 1));
    /// assert!(!cursor.matches(5, 2));
    /// ```
    #[must_use]
    pub fn matches(&self, ordering: u64, columns: usize) -> bool {
        self.ordering == ordering && self.key.len() == columns
    }

    /// Signs the cursor under `scope` and returns the opaque token.
    ///
    /// `scope` is the listing the cursor belongs to — the entity's name is the
    /// default [`Paginated`](crate::Paginated) uses. It is mixed into the MAC
    /// and never transmitted, so a cursor from one listing cannot be replayed
    /// against another.
    ///
    /// # Errors
    ///
    /// [`Error::Build`] when a sort-key value has no cursor encoding (an array
    /// column is not a sort key) or when the tuple is too large to fit in a
    /// token that survives a URL.
    ///
    /// ```
    /// use moso_core::response::cursor::CursorCodec;
    /// use moso_orm::cursor::PageCursor;
    /// use moso_sql::Value;
    ///
    /// let codec = CursorCodec::new("an application secret that is long enough");
    /// let token = PageCursor::new(1, [Value::I64(7)]).seal(&codec, "Post").unwrap();
    /// assert!(!token.encode().is_empty());
    /// ```
    pub fn seal(&self, codec: &CursorCodec, scope: &str) -> Result<Cursor> {
        let payload = self.to_payload()?;
        codec.sign(scope, &payload).map_err(|_| too_large())
    }

    /// Verifies `cursor` against `codec` and `scope`, and returns its contents.
    ///
    /// # Errors
    ///
    /// [`CursorError::Tampered`] for a cursor that was edited, truncated,
    /// issued under a different scope, or issued by a server with a different
    /// secret — the four are deliberately indistinguishable, because telling an
    /// attacker *which* part of a token failed is how a forgery oracle starts.
    /// [`CursorError::Malformed`] for an authentic tag over a payload this
    /// version cannot read, which is what an older deployment's cursor looks
    /// like after the key tuple changed shape.
    ///
    /// ```
    /// use moso_core::response::cursor::CursorCodec;
    /// use moso_orm::cursor::PageCursor;
    /// use moso_sql::Value;
    ///
    /// let codec = CursorCodec::new("an application secret that is long enough");
    /// let token = PageCursor::new(1, [Value::I64(7)]).seal(&codec, "Post").unwrap();
    ///
    /// assert_eq!(PageCursor::open(&codec, "Post", &token).unwrap().key(), [Value::I64(7)]);
    /// assert!(PageCursor::open(&codec, "Tag", &token).is_err());
    /// ```
    pub fn open(
        codec: &CursorCodec,
        scope: &str,
        cursor: &Cursor,
    ) -> core::result::Result<Self, CursorError> {
        let payload = codec
            .verify(scope, cursor)
            .map_err(|_| CursorError::Tampered)?;
        Self::from_payload(&payload)
    }

    /// The unsigned payload bytes.
    ///
    /// Exposed for a caller that signs cursors with its own key management;
    /// [`PageCursor::seal`] is what an application wants.
    ///
    /// # Errors
    ///
    /// [`Error::Build`] as [`PageCursor::seal`].
    ///
    /// ```
    /// use moso_orm::cursor::PageCursor;
    /// use moso_sql::Value;
    ///
    /// let payload = PageCursor::new(0, [Value::Bool(true)]).to_payload().unwrap();
    /// assert_eq!(payload[0], moso_orm::cursor::FORMAT);
    /// ```
    pub fn to_payload(&self) -> Result<Vec<u8>> {
        if self.key.len() > MAX_KEY_COLUMNS {
            return Err(too_large());
        }
        let mut out = Vec::with_capacity(10 + self.key.len() * 12);
        out.push(FORMAT);
        out.extend_from_slice(&self.ordering.to_be_bytes());
        // Bounded by `MAX_KEY_COLUMNS` immediately above, so this cannot wrap.
        out.push(self.key.len() as u8);
        for value in &self.key {
            write_value(&mut out, value)?;
        }
        if out.len() > CursorCodec::MAX_PAYLOAD {
            return Err(too_large());
        }
        Ok(out)
    }

    /// Reads a payload produced by [`PageCursor::to_payload`].
    ///
    /// The caller is responsible for having authenticated `payload` first.
    ///
    /// # Errors
    ///
    /// [`CursorError::Malformed`] for a payload of the wrong format, a
    /// truncated one, or one carrying a value this version cannot read.
    ///
    /// ```
    /// use moso_orm::cursor::PageCursor;
    /// use moso_sql::Value;
    ///
    /// let cursor = PageCursor::new(3, [Value::text("x")]);
    /// let payload = cursor.to_payload().unwrap();
    /// assert_eq!(PageCursor::from_payload(&payload).unwrap(), cursor);
    /// assert!(PageCursor::from_payload(&payload[..2]).is_err());
    /// ```
    pub fn from_payload(payload: &[u8]) -> core::result::Result<Self, CursorError> {
        let mut reader = Reader::new(payload);
        if reader.byte()? != FORMAT {
            return Err(CursorError::Malformed);
        }
        let ordering = u64::from_be_bytes(reader.array::<8>()?);
        let arity = usize::from(reader.byte()?);
        if arity > MAX_KEY_COLUMNS {
            return Err(CursorError::Malformed);
        }
        let mut key = Vec::with_capacity(arity);
        for _ in 0..arity {
            key.push(read_value(&mut reader)?);
        }
        if !reader.is_exhausted() {
            // Trailing bytes mean the payload is not the one this version
            // writes, and a lenient reader here would accept two spellings of
            // the same cursor.
            return Err(CursorError::Malformed);
        }
        Ok(Self { ordering, key })
    }
}

/// The 64-bit fingerprint of an ordering, from its canonical terms.
///
/// FNV-1a over the terms joined by a byte that cannot appear in one, so that
/// `["ab", "c"]` and `["a", "bc"]` do not collide by concatenation.
///
/// ```
/// use moso_orm::cursor::fingerprint;
///
/// let by_date = fingerprint(["posts.created_at desc nulls first", "posts.id desc nulls first"]);
/// assert_eq!(by_date, fingerprint(["posts.created_at desc nulls first", "posts.id desc nulls first"]));
/// assert_ne!(by_date, fingerprint(["posts.title asc nulls last", "posts.id asc nulls last"]));
/// ```
#[must_use]
pub fn fingerprint<S: AsRef<str>>(terms: impl IntoIterator<Item = S>) -> u64 {
    /// The FNV-1a 64-bit offset basis.
    const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    /// The FNV-1a 64-bit prime.
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = BASIS;
    let mut mix = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    for term in terms {
        mix(term.as_ref().as_bytes());
        // A separator that no canonical term contains, so the joined form is
        // unambiguous.
        mix(&[0]);
    }
    hash
}

/// The one error for a sort key that will not fit in, or cannot be spelled by,
/// a URL-safe token.
fn too_large() -> Error {
    Error::Build(moso_sql::Error::InvalidClause {
        clause: "ORDER BY",
        reason: "this sort key cannot be carried in a pagination cursor — it is either too large \
                 for a URL or contains a column type that has no cursor encoding",
        help: "sort by fewer or narrower columns (an id and a timestamp is the usual pair), or \
               use `paginate_offset(page, per_page)`",
    })
}

/// The tag byte each [`Value`] variant is written under.
///
/// Assigned explicitly rather than derived from `ValueKind`'s discriminants,
/// which are not part of `moso-sql`'s promise: a cursor minted by one release
/// must open in the next.
mod tag {
    /// `NULL`, followed by the tag of the type it stands in for.
    pub const NULL: u8 = 0;
    /// `bool`.
    pub const BOOL: u8 = 1;
    /// `i8`.
    pub const I8: u8 = 2;
    /// `i16`.
    pub const I16: u8 = 3;
    /// `i32`.
    pub const I32: u8 = 4;
    /// `i64`.
    pub const I64: u8 = 5;
    /// `u8`.
    pub const U8: u8 = 6;
    /// `u16`.
    pub const U16: u8 = 7;
    /// `u32`.
    pub const U32: u8 = 8;
    /// `u64`.
    pub const U64: u8 = 9;
    /// `f32`.
    pub const F32: u8 = 10;
    /// `f64`.
    pub const F64: u8 = 11;
    /// `numeric`.
    pub const DECIMAL: u8 = 12;
    /// `text`.
    pub const TEXT: u8 = 13;
    /// `bytea`.
    pub const BYTES: u8 = 14;
    /// `uuid`.
    pub const UUID: u8 = 15;
    /// `json` / `jsonb`.
    pub const JSON: u8 = 16;
    /// `timestamptz`.
    pub const TIMESTAMP: u8 = 17;
    /// `timestamp`.
    pub const DATETIME: u8 = 18;
    /// `date`.
    pub const DATE: u8 = 19;
    /// `time`.
    pub const TIME: u8 = 20;
    /// `interval`.
    pub const INTERVAL: u8 = 21;
    /// An untyped `NULL`.
    pub const UNKNOWN: u8 = 255;
}

/// The tag a `NULL` of this kind is written under.
const fn tag_of_kind(kind: ValueKind) -> Option<u8> {
    Some(match kind {
        ValueKind::Unknown => tag::UNKNOWN,
        ValueKind::Bool => tag::BOOL,
        ValueKind::I8 => tag::I8,
        ValueKind::I16 => tag::I16,
        ValueKind::I32 => tag::I32,
        ValueKind::I64 => tag::I64,
        ValueKind::U8 => tag::U8,
        ValueKind::U16 => tag::U16,
        ValueKind::U32 => tag::U32,
        ValueKind::U64 => tag::U64,
        ValueKind::F32 => tag::F32,
        ValueKind::F64 => tag::F64,
        ValueKind::Decimal => tag::DECIMAL,
        ValueKind::Text => tag::TEXT,
        ValueKind::Bytes => tag::BYTES,
        ValueKind::Uuid => tag::UUID,
        ValueKind::Json => tag::JSON,
        ValueKind::Timestamp => tag::TIMESTAMP,
        ValueKind::DateTime => tag::DATETIME,
        ValueKind::Date => tag::DATE,
        ValueKind::Time => tag::TIME,
        ValueKind::Interval => tag::INTERVAL,
        // `Array`, and anything a later `moso-sql` adds. A column of that type
        // is not a sort key, so refusing is the honest answer.
        _ => return None,
    })
}

/// The kind a `NULL` tag stands for.
const fn kind_of_tag(tag: u8) -> Option<ValueKind> {
    Some(match tag {
        tag::UNKNOWN => ValueKind::Unknown,
        tag::BOOL => ValueKind::Bool,
        tag::I8 => ValueKind::I8,
        tag::I16 => ValueKind::I16,
        tag::I32 => ValueKind::I32,
        tag::I64 => ValueKind::I64,
        tag::U8 => ValueKind::U8,
        tag::U16 => ValueKind::U16,
        tag::U32 => ValueKind::U32,
        tag::U64 => ValueKind::U64,
        tag::F32 => ValueKind::F32,
        tag::F64 => ValueKind::F64,
        tag::DECIMAL => ValueKind::Decimal,
        tag::TEXT => ValueKind::Text,
        tag::BYTES => ValueKind::Bytes,
        tag::UUID => ValueKind::Uuid,
        tag::JSON => ValueKind::Json,
        tag::TIMESTAMP => ValueKind::Timestamp,
        tag::DATETIME => ValueKind::DateTime,
        tag::DATE => ValueKind::Date,
        tag::TIME => ValueKind::Time,
        tag::INTERVAL => ValueKind::Interval,
        _ => return None,
    })
}

/// Appends one tagged value.
///
/// One arm per [`Value`] variant, in the enum's own order: a table, not a
/// function, and splitting it would only hide that.
fn write_value(out: &mut Vec<u8>, value: &Value) -> Result<()> {
    /// Writes a length-prefixed byte string.
    fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
        let length = u32::try_from(bytes.len()).map_err(|_| too_large())?;
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(bytes);
        Ok(())
    }

    match value {
        Value::Null(kind) => {
            out.push(tag::NULL);
            out.push(tag_of_kind(*kind).ok_or_else(too_large)?);
        }
        Value::Bool(flag) => {
            out.push(tag::BOOL);
            out.push(u8::from(*flag));
        }
        Value::I8(number) => {
            out.push(tag::I8);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::I16(number) => {
            out.push(tag::I16);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::I32(number) => {
            out.push(tag::I32);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::I64(number) => {
            out.push(tag::I64);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::U8(number) => {
            out.push(tag::U8);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::U16(number) => {
            out.push(tag::U16);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::U32(number) => {
            out.push(tag::U32);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::U64(number) => {
            out.push(tag::U64);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::F32(number) => {
            out.push(tag::F32);
            out.extend_from_slice(&number.to_bits().to_be_bytes());
        }
        Value::F64(number) => {
            out.push(tag::F64);
            out.extend_from_slice(&number.to_bits().to_be_bytes());
        }
        Value::Decimal(decimal) => {
            out.push(tag::DECIMAL);
            out.extend_from_slice(&decimal.mantissa().to_be_bytes());
            out.extend_from_slice(&decimal.scale().to_be_bytes());
        }
        Value::Text(text) => {
            out.push(tag::TEXT);
            write_bytes(out, text.as_bytes())?;
        }
        Value::Bytes(bytes) => {
            out.push(tag::BYTES);
            write_bytes(out, bytes)?;
        }
        Value::Uuid(uuid) => {
            out.push(tag::UUID);
            out.extend_from_slice(&uuid.into_bytes());
        }
        Value::Json(json) => {
            out.push(tag::JSON);
            write_bytes(out, json.as_json_str().as_bytes())?;
        }
        Value::Timestamp(timestamp) => {
            out.push(tag::TIMESTAMP);
            out.extend_from_slice(&timestamp.unix_seconds().to_be_bytes());
            out.extend_from_slice(&timestamp.nanoseconds().to_be_bytes());
        }
        Value::DateTime(datetime) => {
            out.push(tag::DATETIME);
            write_date(out, datetime.date());
            write_time(out, datetime.time());
        }
        Value::Date(date) => {
            out.push(tag::DATE);
            write_date(out, *date);
        }
        Value::Time(time) => {
            out.push(tag::TIME);
            write_time(out, *time);
        }
        Value::Interval(interval) => {
            out.push(tag::INTERVAL);
            out.extend_from_slice(&interval.months().to_be_bytes());
            out.extend_from_slice(&interval.day_component().to_be_bytes());
            out.extend_from_slice(&interval.microseconds().to_be_bytes());
        }
        // `Value::Array`, and anything a later `moso-sql` adds.
        _ => return Err(too_large()),
    }
    Ok(())
}

/// Appends a date, without its tag.
fn write_date(out: &mut Vec<u8>, date: Date) {
    out.extend_from_slice(&date.year().to_be_bytes());
    out.push(date.month());
    out.push(date.day());
}

/// Appends a time, without its tag.
fn write_time(out: &mut Vec<u8>, time: Time) {
    out.push(time.hour());
    out.push(time.minute());
    out.push(time.second());
    out.extend_from_slice(&time.nanosecond().to_be_bytes());
}

/// Reads one tagged value.
fn read_value(reader: &mut Reader<'_>) -> core::result::Result<Value, CursorError> {
    let tag = reader.byte()?;
    Ok(match tag {
        tag::NULL => Value::Null(kind_of_tag(reader.byte()?).ok_or(CursorError::Malformed)?),
        tag::BOOL => Value::Bool(reader.byte()? != 0),
        tag::I8 => Value::I8(i8::from_be_bytes(reader.array::<1>()?)),
        tag::I16 => Value::I16(i16::from_be_bytes(reader.array::<2>()?)),
        tag::I32 => Value::I32(i32::from_be_bytes(reader.array::<4>()?)),
        tag::I64 => Value::I64(i64::from_be_bytes(reader.array::<8>()?)),
        tag::U8 => Value::U8(u8::from_be_bytes(reader.array::<1>()?)),
        tag::U16 => Value::U16(u16::from_be_bytes(reader.array::<2>()?)),
        tag::U32 => Value::U32(u32::from_be_bytes(reader.array::<4>()?)),
        tag::U64 => Value::U64(u64::from_be_bytes(reader.array::<8>()?)),
        tag::F32 => Value::F32(f32::from_bits(u32::from_be_bytes(reader.array::<4>()?))),
        tag::F64 => Value::F64(f64::from_bits(u64::from_be_bytes(reader.array::<8>()?))),
        tag::DECIMAL => {
            let mantissa = i128::from_be_bytes(reader.array::<16>()?);
            let scale = u32::from_be_bytes(reader.array::<4>()?);
            Value::Decimal(Decimal::new(mantissa, scale).map_err(|_| CursorError::Malformed)?)
        }
        tag::TEXT => Value::Text(reader.string()?),
        tag::BYTES => Value::Bytes(reader.blob()?.to_vec()),
        tag::UUID => Value::Uuid(Uuid::from_bytes(reader.array::<16>()?)),
        tag::JSON => Value::Json(
            Json::from_json_string(reader.string()?).map_err(|_| CursorError::Malformed)?,
        ),
        tag::TIMESTAMP => {
            let seconds = i64::from_be_bytes(reader.array::<8>()?);
            let nanoseconds = u32::from_be_bytes(reader.array::<4>()?);
            Value::Timestamp(
                Timestamp::new(seconds, nanoseconds).map_err(|_| CursorError::Malformed)?,
            )
        }
        tag::DATETIME => Value::DateTime(DateTime::new(read_date(reader)?, read_time(reader)?)),
        tag::DATE => Value::Date(read_date(reader)?),
        tag::TIME => Value::Time(read_time(reader)?),
        tag::INTERVAL => {
            let months = i32::from_be_bytes(reader.array::<4>()?);
            let days = i32::from_be_bytes(reader.array::<4>()?);
            let microseconds = i64::from_be_bytes(reader.array::<8>()?);
            Value::Interval(Interval::new(months, days, microseconds))
        }
        _ => return Err(CursorError::Malformed),
    })
}

/// Reads a date, without its tag.
fn read_date(reader: &mut Reader<'_>) -> core::result::Result<Date, CursorError> {
    let year = i32::from_be_bytes(reader.array::<4>()?);
    let month = reader.byte()?;
    let day = reader.byte()?;
    Date::new(year, month, day).map_err(|_| CursorError::Malformed)
}

/// Reads a time, without its tag.
fn read_time(reader: &mut Reader<'_>) -> core::result::Result<Time, CursorError> {
    let hour = reader.byte()?;
    let minute = reader.byte()?;
    let second = reader.byte()?;
    let nanosecond = u32::from_be_bytes(reader.array::<4>()?);
    Time::new(hour, minute, second, nanosecond).map_err(|_| CursorError::Malformed)
}

/// A bounds-checked cursor over the payload.
///
/// Every read is fallible, so a truncated payload is a rejection rather than a
/// panic — which matters because the payload arrived from the network, even
/// though it was authenticated first.
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    /// A reader at the start of `bytes`.
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    /// The next byte.
    fn byte(&mut self) -> core::result::Result<u8, CursorError> {
        Ok(self.take(1)?[0])
    }

    /// The next `N` bytes, as an array.
    fn array<const N: usize>(&mut self) -> core::result::Result<[u8; N], CursorError> {
        let slice = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    /// A `u32`-length-prefixed byte string.
    fn blob(&mut self) -> core::result::Result<&'a [u8], CursorError> {
        let length = u32::from_be_bytes(self.array::<4>()?);
        let length = usize::try_from(length).map_err(|_| CursorError::Malformed)?;
        self.take(length)
    }

    /// A `u32`-length-prefixed UTF-8 string.
    fn string(&mut self) -> core::result::Result<String, CursorError> {
        let bytes = self.blob()?;
        core::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| CursorError::Malformed)
    }

    /// The next `count` bytes.
    fn take(&mut self, count: usize) -> core::result::Result<&'a [u8], CursorError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(CursorError::Malformed)?;
        let slice = self
            .bytes
            .get(self.position..end)
            .ok_or(CursorError::Malformed)?;
        self.position = end;
        Ok(slice)
    }

    /// Whether every byte has been read.
    const fn is_exhausted(&self) -> bool {
        self.position == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codec() -> CursorCodec {
        CursorCodec::new("the application secret, which is long enough")
    }

    /// One value of every kind a sort key can hold.
    fn every_kind() -> Vec<Value> {
        vec![
            Value::Null(ValueKind::Text),
            Value::Null(ValueKind::Unknown),
            Value::Bool(true),
            Value::Bool(false),
            Value::I8(-8),
            Value::I16(-16),
            Value::I32(-32),
            Value::I64(i64::MIN),
            Value::U8(8),
            Value::U16(16),
            Value::U32(32),
            Value::U64(u64::MAX),
            Value::F32(-1.5),
            Value::F64(f64::MAX),
            Value::Decimal(Decimal::new(-123_456, 3).expect("a decimal")),
            Value::text("ada lovelace"),
            Value::text(""),
            Value::bytes(vec![0, 1, 2, 255]),
            Value::Uuid(Uuid::from_bytes([7; 16])),
            Value::json(r#"{"a":1}"#).expect("valid json"),
            Value::Timestamp(Timestamp::new(1_700_000_000, 123_456_789).expect("a timestamp")),
            Value::DateTime(DateTime::new(
                Date::new(2026, 7, 30).expect("a date"),
                Time::new(13, 45, 30, 500).expect("a time"),
            )),
            Value::Date(Date::new(1970, 1, 1).expect("a date")),
            Value::Time(Time::new(0, 0, 0, 0).expect("a time")),
            Value::Interval(Interval::new(1, 2, 3)),
        ]
    }

    #[test]
    fn every_sort_key_type_round_trips_through_a_payload() {
        for value in every_kind() {
            let cursor = PageCursor::new(9, [value.clone()]);
            let payload = cursor.to_payload().expect("encodable");
            let back = PageCursor::from_payload(&payload).expect("decodable");
            assert_eq!(back, cursor, "for {value:?}");
        }
    }

    /// `every_kind`, split into tuples the arity limit allows.
    fn key_tuples() -> Vec<Vec<Value>> {
        every_kind()
            .chunks(MAX_KEY_COLUMNS)
            .map(<[Value]>::to_vec)
            .collect()
    }

    #[test]
    fn a_whole_tuple_round_trips_through_a_signed_token() {
        for tuple in key_tuples() {
            let width = tuple.len();
            let cursor = PageCursor::new(0xdead_beef, tuple);
            let token = cursor.seal(&codec(), "Post").expect("signable");
            let back = PageCursor::open(&codec(), "Post", &token).expect("verifiable");
            assert_eq!(back, cursor);
            assert_eq!(back.ordering(), 0xdead_beef);
            assert_eq!(back.len(), width);
        }
    }

    #[test]
    fn a_tampered_cursor_is_refused() {
        let token = PageCursor::new(1, [Value::I64(42)])
            .seal(&codec(), "Post")
            .expect("signable");

        // Flip one bit of the payload; the tag no longer covers it.
        let mut bytes = token.into_bytes();
        bytes[3] ^= 0x01;
        let forged = Cursor::from_bytes(bytes);

        let error = PageCursor::open(&codec(), "Post", &forged).expect_err("edited");
        assert_eq!(error, CursorError::Tampered);
        assert!(error.to_string().contains("help:"));
    }

    #[test]
    fn a_truncated_cursor_is_refused() {
        let token = PageCursor::new(1, [Value::I64(42)])
            .seal(&codec(), "Post")
            .expect("signable");
        let bytes = token.into_bytes();
        let short = Cursor::from_bytes(&bytes[..bytes.len() - 1]);
        assert_eq!(
            PageCursor::open(&codec(), "Post", &short).expect_err("truncated"),
            CursorError::Tampered
        );
    }

    #[test]
    fn a_cursor_from_another_listing_is_refused() {
        let token = PageCursor::new(1, [Value::I64(42)])
            .seal(&codec(), "Post")
            .expect("signable");
        assert_eq!(
            PageCursor::open(&codec(), "Comment", &token).expect_err("wrong scope"),
            CursorError::Tampered
        );
    }

    #[test]
    fn a_cursor_from_another_deployment_is_refused() {
        let token = PageCursor::new(1, [Value::I64(42)])
            .seal(&codec(), "Post")
            .expect("signable");
        let theirs = CursorCodec::new("a completely different application secret");
        assert_eq!(
            PageCursor::open(&theirs, "Post", &token).expect_err("wrong key"),
            CursorError::Tampered
        );
    }

    #[test]
    fn a_payload_of_the_wrong_format_is_malformed() {
        let mut payload = PageCursor::new(1, [Value::I64(42)])
            .to_payload()
            .expect("encodable");
        payload[0] = FORMAT.wrapping_add(1);
        assert_eq!(
            PageCursor::from_payload(&payload).expect_err("a future format"),
            CursorError::Malformed
        );
    }

    #[test]
    fn a_payload_with_trailing_bytes_is_malformed() {
        let mut payload = PageCursor::new(1, [Value::I64(42)])
            .to_payload()
            .expect("encodable");
        payload.push(0);
        assert_eq!(
            PageCursor::from_payload(&payload).expect_err("trailing"),
            CursorError::Malformed
        );
    }

    #[test]
    fn every_truncation_of_a_payload_is_refused_rather_than_panicking() {
        for tuple in key_tuples() {
            let payload = PageCursor::new(1, tuple).to_payload().expect("encodable");
            for end in 0..payload.len() {
                assert!(
                    PageCursor::from_payload(&payload[..end]).is_err(),
                    "a payload cut at {end} must be refused"
                );
            }
        }
    }

    #[test]
    fn an_unknown_value_tag_is_refused() {
        let mut payload = PageCursor::new(1, [Value::I64(42)])
            .to_payload()
            .expect("encodable");
        payload[10] = 200;
        assert_eq!(
            PageCursor::from_payload(&payload).expect_err("an unknown tag"),
            CursorError::Malformed
        );
    }

    #[test]
    fn a_sort_key_that_is_not_a_sort_key_is_refused_with_a_fix() {
        let array = Value::Array(moso_sql::Array::of([1_i32, 2, 3]));
        let error = PageCursor::new(1, [array])
            .to_payload()
            .expect_err("an array is not a sort key");
        let text = error.to_string();
        assert!(text.contains("help:"), "{text}");
        assert!(text.contains("paginate_offset"), "{text}");
        assert!(error.is_programmer_error());
    }

    #[test]
    fn a_key_wider_than_the_limit_is_refused() {
        let wide: Vec<Value> = (0..=MAX_KEY_COLUMNS as i64).map(Value::I64).collect();
        assert!(PageCursor::new(1, wide).to_payload().is_err());
    }

    #[test]
    fn a_key_too_long_for_a_url_is_refused_before_it_is_signed() {
        // One long text column: well inside `MAX_KEY_COLUMNS`, well past the
        // number of bytes that survives base64 into a 2 KiB query parameter.
        let long = Value::text("x".repeat(CursorCodec::MAX_PAYLOAD + 1));
        assert!(PageCursor::new(1, [long]).seal(&codec(), "Post").is_err());
    }

    #[test]
    fn the_fingerprint_separates_orderings_and_is_stable() {
        let by_date = fingerprint([
            "posts.created_at desc nulls first",
            "posts.id desc nulls first",
        ]);
        assert_eq!(
            by_date,
            fingerprint([
                "posts.created_at desc nulls first",
                "posts.id desc nulls first"
            ])
        );
        assert_ne!(by_date, fingerprint(["posts.created_at asc nulls last"]));
        // Direction is part of the identity: resuming a reversed sort would
        // skip and repeat rows in equal measure.
        assert_ne!(
            fingerprint(["posts.id asc nulls last"]),
            fingerprint(["posts.id desc nulls first"])
        );
        // And the separator makes concatenation unambiguous.
        assert_ne!(fingerprint(["ab", "c"]), fingerprint(["a", "bc"]));
        assert_ne!(fingerprint::<&str>([]), fingerprint(["a"]));
    }

    #[test]
    fn matches_checks_both_the_ordering_and_the_arity() {
        let cursor = PageCursor::new(5, [Value::I64(1), Value::I64(2)]);
        assert!(cursor.matches(5, 2));
        assert!(!cursor.matches(5, 1));
        assert!(!cursor.matches(4, 2));
        assert!(!cursor.is_empty());
        assert!(PageCursor::new(0, []).is_empty());
    }

    #[test]
    fn a_null_of_an_unencodable_kind_is_refused() {
        let error = PageCursor::new(1, [Value::Null(ValueKind::Array)])
            .to_payload()
            .expect_err("an array column is not a sort key");
        assert!(error.to_string().contains("help:"));
    }
}
