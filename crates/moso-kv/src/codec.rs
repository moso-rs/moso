//! Turning a value into bytes, and the envelope wrapped around it.
//!
//! # Two traits, not one
//!
//! [`Codec`] is a marker — `Json`, `Raw` — and [`Encodable<C>`] is "this type
//! can be coded with `C`". Splitting them is what lets `Raw` mean *the value's
//! own byte representation* (a `u64` is decimal ASCII, exactly like Redis'
//! `INCR`) while `Json` means *serde*. One trait with generic methods would
//! have forced `Raw` to serialise a counter as `"7"` with quotes, which no
//! `INCR` would then be able to touch.
//!
//! # The envelope
//!
//! A framed codec — `Json` — prefixes twelve bytes:
//!
//! ```text
//! │ 0    │ 1       │ 2     │ 3        │ 4 … 11              │ 12 …    │
//! │ 'M'  │ version │ flags │ reserved │ stored_at, ms, LE   │ payload │
//! ```
//!
//! Those twelve bytes buy three things that are otherwise impossible:
//!
//! * **stale-while-revalidate**, because "how old is this value" is a property
//!   of the value and not of the store's TTL, which is already spent on
//!   eviction;
//! * **negative caching**, because "absent, and we know it" has to be
//!   distinguishable from "absent";
//! * **a version byte**, so that changing the framing later is a decode error
//!   naming the namespace rather than a wrong value.
//!
//! `Raw` is unframed on purpose: its whole reason for existing is that the
//! backend's own operations — `INCR`, `APPEND` — can read what Moso wrote.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::BoxError;

/// How many bytes a framed codec prefixes.
///
/// ```
/// use moso_kv::codec::FRAME_HEADER_LEN;
///
/// assert_eq!(FRAME_HEADER_LEN, 12);
/// ```
pub const FRAME_HEADER_LEN: usize = 12;

/// The first byte of a framed value.
const FRAME_MAGIC: u8 = b'M';

/// The framing version. Bumping it invalidates every framed value.
const FRAME_VERSION: u8 = 1;

/// `flags` bit 0: the payload is a deliberate negative, cached to stop a
/// stampede against a value that is not there.
const FLAG_NEGATIVE: u8 = 0b0000_0001;

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

/// How a namespace's values are represented in the store.
///
/// Two ship: [`Json`] and [`Raw`]. Implement it for a third — MessagePack,
/// Bincode, protobuf — by pairing a marker type with an [`Encodable`] impl.
///
/// ```
/// use bytes::Bytes;
/// use moso_kv::codec::{Codec, Encodable};
/// use moso_kv::BoxError;
///
/// /// Big-endian `u32`, for a namespace shared with a C service.
/// #[derive(Debug, Clone, Copy)]
/// pub struct BigEndian;
///
/// impl Codec for BigEndian {
///     const NAME: &'static str = "be32";
///     const FRAMED: bool = false;
/// }
///
/// impl Encodable<BigEndian> for u32 {
///     fn encode_value(&self) -> Result<Bytes, BoxError> {
///         Ok(Bytes::copy_from_slice(&self.to_be_bytes()))
///     }
///     fn decode_value(bytes: &[u8]) -> Result<Self, BoxError> {
///         let array: [u8; 4] = bytes.try_into().map_err(|_| "expected 4 bytes")?;
///         Ok(u32::from_be_bytes(array))
///     }
/// }
///
/// assert_eq!(
///     <u32 as Encodable<BigEndian>>::encode_value(&1).unwrap().as_ref(),
///     &[0, 0, 0, 1],
/// );
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a codec",
    label = "this type has no `Codec` impl",
    note = "a codec is a marker: `const NAME` and `const FRAMED`, plus one `Encodable<{Self}>` \
            impl per value type",
    note = "help: the two that ship are `moso_kv::codec::Json` and `moso_kv::codec::Raw`"
)]
pub trait Codec: Send + Sync + 'static {
    /// The name in a decode error and in the `codec = …` a `namespace!` writes.
    const NAME: &'static str;

    /// Whether values carry the twelve-byte envelope.
    ///
    /// `false` means no stale-while-revalidate and no negative caching for
    /// namespaces using this codec, and it means the backend's own operations
    /// can read the bytes.
    const FRAMED: bool;
}

/// A [`Codec`] whose values carry the envelope.
///
/// The bound on [`Kv::get_swr`](crate::Kv::get_swr): stale-while-revalidate
/// needs to know how old a value is, and only a framed value knows.
///
/// ```
/// use moso_kv::codec::{Framed, Json, Raw};
///
/// fn needs_an_age<C: Framed>() {}
///
/// needs_an_age::<Json>();
/// // needs_an_age::<Raw>();  // `Raw` stores no timestamp — does not compile.
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not record how old a value is",
    label = "this codec is unframed",
    note = "stale-while-revalidate and negative caching need the 12-byte envelope, and `Raw` \
            deliberately has none so that `INCR` can read what Moso wrote",
    note = "help: declare the namespace with `codec = Json`"
)]
pub trait Framed: Codec {}

// ---------------------------------------------------------------------------
// Encodable
// ---------------------------------------------------------------------------

/// A value that codec `C` can turn into bytes and back.
///
/// ```
/// use moso_kv::codec::{Encodable, Raw};
///
/// let bytes = <u64 as Encodable<Raw>>::encode_value(&42).expect("encodes");
/// assert_eq!(bytes.as_ref(), b"42");
/// assert_eq!(<u64 as Encodable<Raw>>::decode_value(b"42").expect("decodes"), 42);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be stored with the `{C}` codec",
    label = "no `Encodable<{C}>` impl for this type",
    note = "`Json` accepts anything that is `Serialize + DeserializeOwned`; `Raw` accepts \
            `String`, `Vec<u8>`, `Bytes` and the integer types",
    note = "help: declare the namespace with `codec = Json` and derive \
            `#[derive(serde::Serialize, serde::Deserialize)]` on {Self}"
)]
pub trait Encodable<C: Codec>: Sized + Send + Sync + 'static {
    /// Turn `self` into the bytes the store holds — without the envelope,
    /// which the layer above adds.
    ///
    /// # Errors
    ///
    /// Whatever the serialiser says. The caller wraps it in
    /// [`Error::Codec`](crate::Error::Codec) with the namespace's name.
    fn encode_value(&self) -> Result<Bytes, BoxError>;

    /// Rebuild a value from the store's bytes.
    ///
    /// # Errors
    ///
    /// Whatever the deserialiser says.
    fn decode_value(bytes: &[u8]) -> Result<Self, BoxError>;
}

// ---------------------------------------------------------------------------
// Json
// ---------------------------------------------------------------------------

/// JSON, through `serde_json`, with the envelope.
///
/// The default for anything that is not a counter: it is readable in
/// `redis-cli`, it is what every other tool in a deployment already speaks, and
/// `null` gives negative caching for free.
///
/// ```
/// use moso_kv::codec::{Codec, Encodable, Json};
///
/// #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
/// struct Profile { name: String }
///
/// assert!(Json::FRAMED);
///
/// let bytes = <Profile as Encodable<Json>>::encode_value(&Profile { name: "a".into() })
///     .expect("encodes");
/// assert_eq!(bytes.as_ref(), br#"{"name":"a"}"#);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Json;

impl Codec for Json {
    const NAME: &'static str = "json";
    const FRAMED: bool = true;
}

impl Framed for Json {}

// The blanket that makes `codec = Json` work for every model type without a
// line of glue. `do_not_recommend` so that a type which is missing
// `Serialize` is told *that*, rather than being told to implement
// `Encodable<Json>` by hand.
#[diagnostic::do_not_recommend]
impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> Encodable<Json> for T {
    fn encode_value(&self) -> Result<Bytes, BoxError> {
        Ok(Bytes::from(serde_json::to_vec(self)?))
    }

    fn decode_value(bytes: &[u8]) -> Result<Self, BoxError> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

// ---------------------------------------------------------------------------
// Raw
// ---------------------------------------------------------------------------

/// The value's own byte representation, with no envelope.
///
/// For the values the backend itself operates on: a counter the rate limiter
/// `INCR`s, a token a lock compares, a blob something else wrote. An integer is
/// decimal ASCII, which is exactly what Redis' `INCR` produces and consumes.
///
/// ```
/// use moso_kv::codec::{Codec, Encodable, Raw};
///
/// assert!(!Raw::FRAMED);
/// assert_eq!(<u64 as Encodable<Raw>>::encode_value(&7).unwrap().as_ref(), b"7");
/// assert_eq!(
///     <String as Encodable<Raw>>::encode_value(&"hi".to_owned()).unwrap().as_ref(),
///     b"hi",
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Raw;

impl Codec for Raw {
    const NAME: &'static str = "raw";
    const FRAMED: bool = false;
}

impl Encodable<Raw> for String {
    fn encode_value(&self) -> Result<Bytes, BoxError> {
        Ok(Bytes::copy_from_slice(self.as_bytes()))
    }

    fn decode_value(bytes: &[u8]) -> Result<Self, BoxError> {
        Ok(std::str::from_utf8(bytes)?.to_owned())
    }
}

impl Encodable<Raw> for Vec<u8> {
    fn encode_value(&self) -> Result<Bytes, BoxError> {
        Ok(Bytes::copy_from_slice(self))
    }

    fn decode_value(bytes: &[u8]) -> Result<Self, BoxError> {
        Ok(bytes.to_vec())
    }
}

impl Encodable<Raw> for Bytes {
    fn encode_value(&self) -> Result<Bytes, BoxError> {
        Ok(self.clone())
    }

    fn decode_value(bytes: &[u8]) -> Result<Self, BoxError> {
        Ok(Bytes::copy_from_slice(bytes))
    }
}

/// `Encodable<Raw>` for an integer, as decimal ASCII — `INCR`'s representation.
macro_rules! raw_integer {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Encodable<Raw> for $ty {
                fn encode_value(&self) -> Result<Bytes, BoxError> {
                    Ok(Bytes::from(self.to_string().into_bytes()))
                }

                fn decode_value(bytes: &[u8]) -> Result<Self, BoxError> {
                    Ok(std::str::from_utf8(bytes)?.trim().parse::<$ty>()?)
                }
            }
        )*
    };
}

raw_integer!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

// ---------------------------------------------------------------------------
// The envelope
// ---------------------------------------------------------------------------

/// A framed value, as it came out of the store.
///
/// ```
/// use moso_kv::codec::Envelope;
/// use bytes::Bytes;
/// use std::time::Duration;
///
/// let framed = Envelope::wrap(Bytes::from_static(b"{}"), false);
/// let opened = Envelope::open(&framed).expect("valid framing");
///
/// assert_eq!(opened.payload, b"{}");
/// assert!(!opened.negative);
/// assert!(opened.age() < Duration::from_secs(1));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope<'a> {
    /// The bytes the codec produced.
    pub payload: &'a [u8],
    /// Whether this is a cached "not there".
    pub negative: bool,
    /// When it was written, in milliseconds since the Unix epoch.
    pub stored_at_ms: u64,
}

impl<'a> Envelope<'a> {
    /// Frame `payload` with the current time.
    ///
    /// ```
    /// use moso_kv::codec::{Envelope, FRAME_HEADER_LEN};
    /// use bytes::Bytes;
    ///
    /// let framed = Envelope::wrap(Bytes::from_static(b"null"), true);
    /// assert_eq!(framed.len(), FRAME_HEADER_LEN + 4);
    /// assert!(Envelope::open(&framed).expect("valid").negative);
    /// ```
    #[must_use]
    pub fn wrap(payload: Bytes, negative: bool) -> Bytes {
        Self::wrap_at(payload, negative, now_ms())
    }

    /// Frame `payload` with an explicit timestamp — what the tests use to make
    /// "stale" reproducible without sleeping.
    ///
    /// ```
    /// use moso_kv::codec::Envelope;
    /// use bytes::Bytes;
    ///
    /// let framed = Envelope::wrap_at(Bytes::from_static(b"1"), false, 1_700_000_000_000);
    /// assert_eq!(Envelope::open(&framed).expect("valid").stored_at_ms, 1_700_000_000_000);
    /// ```
    #[must_use]
    pub fn wrap_at(payload: Bytes, negative: bool, stored_at_ms: u64) -> Bytes {
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
        out.push(FRAME_MAGIC);
        out.push(FRAME_VERSION);
        out.push(if negative { FLAG_NEGATIVE } else { 0 });
        out.push(0);
        out.extend_from_slice(&stored_at_ms.to_le_bytes());
        out.extend_from_slice(&payload);
        Bytes::from(out)
    }

    /// Read a framed value.
    ///
    /// # Errors
    ///
    /// A message naming what is wrong, for
    /// [`Error::Codec`](crate::Error::Codec) to carry. The three ways it fails
    /// — too short, wrong magic, wrong version — are all "these bytes were not
    /// written by this version of Moso", which is a real thing that happens
    /// during a rolling deploy and must not be mistaken for a valid value.
    ///
    /// ```
    /// use moso_kv::codec::Envelope;
    ///
    /// assert!(Envelope::open(b"too short").is_err());
    /// assert!(Envelope::open(b"XXXXXXXXXXXXpayload").is_err());
    /// ```
    pub fn open(bytes: &'a [u8]) -> Result<Self, BoxError> {
        if bytes.len() < FRAME_HEADER_LEN {
            return Err(format!(
                "a framed value is at least {FRAME_HEADER_LEN} bytes, this one is {}",
                bytes.len()
            )
            .into());
        }
        if bytes[0] != FRAME_MAGIC {
            return Err("these bytes were not written by moso-kv (bad frame marker)".into());
        }
        if bytes[1] != FRAME_VERSION {
            return Err(format!(
                "frame version {} was written by a different moso-kv; this one reads version {FRAME_VERSION}",
                bytes[1]
            )
            .into());
        }
        let mut stamp = [0_u8; 8];
        stamp.copy_from_slice(&bytes[4..12]);
        Ok(Self {
            payload: &bytes[FRAME_HEADER_LEN..],
            negative: bytes[2] & FLAG_NEGATIVE != 0,
            stored_at_ms: u64::from_le_bytes(stamp),
        })
    }

    /// How long ago this was written, saturating at zero for a clock that went
    /// backwards.
    ///
    /// ```
    /// use moso_kv::codec::Envelope;
    /// use bytes::Bytes;
    /// use std::time::Duration;
    ///
    /// // Written a minute in the future by a machine with a fast clock.
    /// let framed = Envelope::wrap_at(Bytes::new(), false, u64::MAX);
    /// assert_eq!(Envelope::open(&framed).expect("valid").age(), Duration::ZERO);
    /// ```
    #[must_use]
    pub fn age(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.stored_at_ms))
    }

    /// Whether this value is older than `fresh_for`.
    ///
    /// ```
    /// use moso_kv::codec::Envelope;
    /// use bytes::Bytes;
    /// use std::time::Duration;
    ///
    /// let old = Envelope::wrap_at(Bytes::new(), false, 0);
    /// assert!(Envelope::open(&old).expect("valid").is_stale(Duration::from_secs(60)));
    /// ```
    #[must_use]
    pub fn is_stale(&self, fresh_for: Duration) -> bool {
        self.age() > fresh_for
    }
}

/// Now, in milliseconds since the Unix epoch.
///
/// Saturating rather than panicking: a machine whose clock is before 1970 gets
/// values that are always stale, which is a degraded cache and not a crash.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trips_a_model_type() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Profile {
            name: String,
            age: u8,
        }

        let value = Profile {
            name: "alice".to_owned(),
            age: 30,
        };
        let bytes = <Profile as Encodable<Json>>::encode_value(&value).expect("encodes");
        let back = <Profile as Encodable<Json>>::decode_value(&bytes).expect("decodes");
        assert_eq!(back, value);
    }

    #[test]
    fn json_encodes_none_as_null_which_is_what_makes_negative_caching_work() {
        let bytes = <Option<u8> as Encodable<Json>>::encode_value(&None).expect("encodes");
        assert_eq!(bytes.as_ref(), b"null");
        assert_eq!(
            <Option<u8> as Encodable<Json>>::decode_value(b"null").expect("decodes"),
            None
        );
    }

    #[test]
    fn raw_integers_are_what_incr_produces() {
        assert_eq!(
            <i64 as Encodable<Raw>>::encode_value(&-7)
                .expect("encodes")
                .as_ref(),
            b"-7"
        );
        assert_eq!(
            <u64 as Encodable<Raw>>::decode_value(b"9007199254740993").expect("decodes"),
            9_007_199_254_740_993
        );
        // Redis pads nothing, but a hand-written value might have whitespace.
        assert_eq!(
            <u32 as Encodable<Raw>>::decode_value(b" 4 ").expect("decodes"),
            4
        );
        assert!(<u32 as Encodable<Raw>>::decode_value(b"nope").is_err());
    }

    #[test]
    fn raw_bytes_are_the_bytes() {
        let value = vec![0_u8, 255, 12];
        let bytes = <Vec<u8> as Encodable<Raw>>::encode_value(&value).expect("encodes");
        assert_eq!(bytes.as_ref(), value.as_slice());
        assert_eq!(
            <Bytes as Encodable<Raw>>::decode_value(&value).expect("decodes"),
            Bytes::from(value.clone())
        );
        assert!(<String as Encodable<Raw>>::decode_value(&[0xff]).is_err());
    }

    #[test]
    fn the_envelope_round_trips() {
        let framed = Envelope::wrap(Bytes::from_static(b"{\"a\":1}"), false);
        let opened = Envelope::open(&framed).expect("valid");
        assert_eq!(opened.payload, b"{\"a\":1}");
        assert!(!opened.negative);
        assert!(opened.stored_at_ms > 1_700_000_000_000);
    }

    #[test]
    fn the_negative_flag_survives() {
        let framed = Envelope::wrap(Bytes::from_static(b"null"), true);
        assert!(Envelope::open(&framed).expect("valid").negative);
    }

    #[test]
    fn an_empty_payload_is_legal() {
        let framed = Envelope::wrap(Bytes::new(), false);
        assert_eq!(framed.len(), FRAME_HEADER_LEN);
        assert_eq!(Envelope::open(&framed).expect("valid").payload, b"");
    }

    #[test]
    fn foreign_bytes_are_rejected_rather_than_misread() {
        assert!(Envelope::open(b"").is_err());
        assert!(Envelope::open(b"01234567890").is_err());

        let mut wrong_magic = Envelope::wrap(Bytes::from_static(b"x"), false).to_vec();
        wrong_magic[0] = b'Z';
        assert!(Envelope::open(&wrong_magic).is_err());

        let mut wrong_version = Envelope::wrap(Bytes::from_static(b"x"), false).to_vec();
        wrong_version[1] = 99;
        let error = Envelope::open(&wrong_version).expect_err("rejected");
        assert!(error.to_string().contains("version 99"), "{error}");
    }

    #[test]
    fn age_and_staleness_use_the_stored_timestamp() {
        let old = Envelope::wrap_at(Bytes::new(), false, 1_000);
        let opened = Envelope::open(&old).expect("valid");
        assert!(opened.age() > Duration::from_secs(60 * 60 * 24));
        assert!(opened.is_stale(Duration::from_secs(60)));

        let fresh = Envelope::wrap(Bytes::new(), false);
        assert!(
            !Envelope::open(&fresh)
                .expect("valid")
                .is_stale(Duration::from_secs(60))
        );
    }

    #[test]
    fn a_future_timestamp_is_not_negative_age() {
        let framed = Envelope::wrap_at(Bytes::new(), false, u64::MAX);
        assert_eq!(
            Envelope::open(&framed).expect("valid").age(),
            Duration::ZERO
        );
    }

    #[test]
    fn the_codec_markers_say_which_are_framed() {
        const { assert!(Json::FRAMED) };
        const { assert!(!Raw::FRAMED) };
        assert_eq!(Json::NAME, "json");
        assert_eq!(Raw::NAME, "raw");

        fn framed_only<C: Framed>() -> &'static str {
            C::NAME
        }
        assert_eq!(framed_only::<Json>(), "json");
    }
}
