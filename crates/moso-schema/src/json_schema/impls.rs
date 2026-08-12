//! [`Schema`] and [`Validate`] implementations for standard-library and
//! common third-party types.
//!
//! Every type here is **anonymous**: it overrides [`Schema::schema_ref`] to
//! return an inline node rather than a `$ref`, so `components/schemas` holds
//! the application's models and nothing else. Their [`Schema::schema_name`] is
//! still meaningful, because it is what generic name mangling uses —
//! `Page<String>` becomes `Page_String`.
//!
//! There is no blanket `impl<T> Validate for T`. It would conflict with every
//! derived impl, so primitives get explicit no-op impls instead.
//!
//! # Pointer convention
//!
//! A [`Validate`] impl reports pointers **relative to the value it was handed**,
//! and the container lifts them: [`Vec<T>`] prefixes its elements' errors with
//! `/<index>`, a map with `/<key>`. It is the same convention
//! [`crate::checks::check_nested`] uses and the one `#[derive(Schema)]` emits
//! literal `"/field"` pointers for, and it is what makes pointers compose —
//! `Vec<Vec<T>>` yields `/0/1/field` without any level knowing its own depth.
//!
//! # Two types that cannot be here
//!
//! * `&'static str` — [`Schema`] requires `DeserializeOwned`, i.e.
//!   `for<'de> Deserialize<'de>`, and `&'a str` deserialises only from a
//!   `'de: 'a` input. It gets a [`Validate`] impl (via the blanket one for
//!   references) and nothing more; use [`Cow<'static, str>`] in a model.
//! * `Rc<T>` — [`Schema`] requires `Send + Sync`.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::{
    NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64, NonZeroI128, NonZeroIsize, NonZeroU8,
    NonZeroU16, NonZeroU32, NonZeroU64, NonZeroU128, NonZeroUsize,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::json_schema::{
    ArrayBuilder, NumberBuilder, ObjectBuilder, SchemaGenerator, SchemaNode, SchemaRef,
    StringBuilder,
};
use crate::schema::{Schema, generic_schema_name, inline_schema_ref};
use crate::validate::{Validate, ValidationCtx, ValidationErrors, push_token};

/// Types whose values cannot violate a constraint: validation is a no-op.
macro_rules! trivial_validate {
    ($($t:ty),* $(,)?) => {$(
        impl Validate for $t {
            fn validate(&self, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
                Ok(())
            }
        }
    )*};
}

trivial_validate!(
    str,
    bool,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    f32,
    f64,
    char,
    String,
    Cow<'static, str>,
    (),
    Value,
    Uuid,
    DateTime<Utc>,
    NaiveDate,
    NaiveTime,
    NaiveDateTime,
    IpAddr,
    Ipv4Addr,
    Ipv6Addr,
    SocketAddr,
    PathBuf,
    url::Url,
    Duration,
);

/// A reference validates exactly as its referent does.
///
/// This is what gives `&'static str` — and every `&T` a generated body takes by
/// reference — a [`Validate`] impl without a second, divergent definition of
/// what validating a `str` means.
#[diagnostic::do_not_recommend]
impl<T: Validate + ?Sized> Validate for &T {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        (**self).validate(ctx)
    }
}

/// An anonymous `Schema` impl with a fixed node.
macro_rules! anonymous_schema {
    ($t:ty, $name:literal, $has_constraints:expr, |$g:pat_param| $node:expr) => {
        impl Schema for $t {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed($name)
            }

            fn json_schema($g: &mut SchemaGenerator) -> SchemaNode {
                $node
            }

            fn schema_ref() -> SchemaRef {
                inline_schema_ref::<Self>()
            }

            const HAS_CONSTRAINTS: bool = $has_constraints;
        }
    };
}

// ── scalars ─────────────────────────────────────────────────────────────

anonymous_schema!(bool, "Boolean", false, |_g| SchemaNode::boolean());
anonymous_schema!((), "Null", false, |_g| SchemaNode::null());
anonymous_schema!(String, "String", false, |_g| StringBuilder::new().build());
anonymous_schema!(Cow<'static, str>, "String", false, |_g| StringBuilder::new(
)
.build());
anonymous_schema!(char, "Char", true, |_g| StringBuilder::new()
    .min_length(1)
    .max_length(1)
    .description("A single Unicode character.")
    .build());
anonymous_schema!(Value, "Any", false, |_g| SchemaNode::any()
    .with_description("Any JSON value. Intentionally unmodelled."));

/// Integers, bounded by the range their Rust type can hold — which is how an
/// unsigned type comes to assert `minimum: 0` for free.
macro_rules! integer_schema {
    ($($t:ty => ($name:literal, $format:literal)),* $(,)?) => {$(
        anonymous_schema!($t, $name, true, |_g| NumberBuilder::integer()
            .format($format)
            .minimum(<$t>::MIN)
            .maximum(<$t>::MAX)
            .build());
    )*};
}

integer_schema!(
    i8 => ("Int8", "int8"),
    i16 => ("Int16", "int16"),
    i32 => ("Int32", "int32"),
    i64 => ("Int64", "int64"),
    isize => ("Int64", "int64"),
    u8 => ("UInt8", "uint8"),
    u16 => ("UInt16", "uint16"),
    u32 => ("UInt32", "uint32"),
    u64 => ("UInt64", "uint64"),
    usize => ("UInt64", "uint64"),
);

// 128-bit integers carry no bounds: JSON numbers cannot represent them
// losslessly and emitting a bound wider than the format is worse than
// emitting none. Clients should treat them as opaque.
anonymous_schema!(i128, "Int128", false, |_g| NumberBuilder::integer()
    .format("int128")
    .build());
anonymous_schema!(u128, "UInt128", true, |_g| NumberBuilder::integer()
    .format("uint128")
    .minimum(0u64)
    .build());

anonymous_schema!(f32, "Float", false, |_g| NumberBuilder::number()
    .format("float")
    .build());
anonymous_schema!(f64, "Double", false, |_g| NumberBuilder::number()
    .format("double")
    .build());

/// Non-zero integers.
///
/// `minimum: 1` covers the unsigned case exactly. The signed case cannot be
/// expressed with bounds at all — the hole is in the middle of the range — so
/// it carries `not: { "const": 0 }`, which is exact rather than approximate.
/// Getting this wrong would mean a schema that accepts `0` and a deserialiser
/// that rejects it, which is the precise failure mode this crate exists to
/// prevent.
macro_rules! nonzero_schema {
    ($($t:ty => ($name:literal, $format:literal, $node:expr)),* $(,)?) => {$(
        trivial_validate!($t);

        anonymous_schema!($t, $name, true, |_g| $node
            .format($format)
            .description("A non-zero integer.")
            .build());
    )*};
}

nonzero_schema!(
    NonZeroU8 => ("NonZeroUInt8", "uint8",
        NumberBuilder::integer().minimum(1u8).maximum(u8::MAX)),
    NonZeroU16 => ("NonZeroUInt16", "uint16",
        NumberBuilder::integer().minimum(1u8).maximum(u16::MAX)),
    NonZeroU32 => ("NonZeroUInt32", "uint32",
        NumberBuilder::integer().minimum(1u8).maximum(u32::MAX)),
    NonZeroU64 => ("NonZeroUInt64", "uint64",
        NumberBuilder::integer().minimum(1u8).maximum(u64::MAX)),
    NonZeroUsize => ("NonZeroUInt64", "uint64",
        NumberBuilder::integer().minimum(1u8).maximum(usize::MAX)),
    // 128-bit integers carry no upper bound, for the reason given above `i128`.
    NonZeroU128 => ("NonZeroUInt128", "uint128", NumberBuilder::integer().minimum(1u8)),
    NonZeroI8 => ("NonZeroInt8", "int8",
        NumberBuilder::integer().minimum(i8::MIN).maximum(i8::MAX).not(SchemaNode::constant(0))),
    NonZeroI16 => ("NonZeroInt16", "int16",
        NumberBuilder::integer().minimum(i16::MIN).maximum(i16::MAX).not(SchemaNode::constant(0))),
    NonZeroI32 => ("NonZeroInt32", "int32",
        NumberBuilder::integer().minimum(i32::MIN).maximum(i32::MAX).not(SchemaNode::constant(0))),
    NonZeroI64 => ("NonZeroInt64", "int64",
        NumberBuilder::integer().minimum(i64::MIN).maximum(i64::MAX).not(SchemaNode::constant(0))),
    NonZeroIsize => ("NonZeroInt64", "int64",
        NumberBuilder::integer().minimum(isize::MIN).maximum(isize::MAX)
            .not(SchemaNode::constant(0))),
    NonZeroI128 => ("NonZeroInt128", "int128",
        NumberBuilder::integer().not(SchemaNode::constant(0))),
);

// ── common third-party scalars ──────────────────────────────────────────

anonymous_schema!(Uuid, "Uuid", true, |_g| StringBuilder::new()
    .format("uuid")
    .build());
anonymous_schema!(DateTime<Utc>, "DateTime", true, |_g| StringBuilder::new()
    .format("date-time")
    .description("An RFC 3339 timestamp in UTC.")
    .build());
anonymous_schema!(NaiveDate, "Date", true, |_g| StringBuilder::new()
    .format("date")
    .build());
anonymous_schema!(NaiveTime, "Time", true, |_g| StringBuilder::new()
    .format("time")
    .build());
anonymous_schema!(NaiveDateTime, "LocalDateTime", true, |_g| {
    StringBuilder::new()
        .format("date-time")
        .description("A timestamp with no timezone. Prefer `DateTime<Utc>`.")
        .build()
});
anonymous_schema!(IpAddr, "IpAddr", true, |_g| StringBuilder::new()
    .format("ip")
    .description("An IPv4 or IPv6 address.")
    .build());
anonymous_schema!(Ipv4Addr, "Ipv4Addr", true, |_g| StringBuilder::new()
    .format("ipv4")
    .build());
anonymous_schema!(Ipv6Addr, "Ipv6Addr", true, |_g| StringBuilder::new()
    .format("ipv6")
    .build());

// No `format` keyword: there is no registered format for a host:port pair, and
// documenting one Moso cannot also *enforce* (see `checks::is_valid_format`)
// is the exact drift this crate exists to prevent. `HAS_CONSTRAINTS` is false
// for the same reason — a malformed value fails to deserialise, which is a 400,
// not a 422.
anonymous_schema!(SocketAddr, "SocketAddr", false, |_g| StringBuilder::new()
    .description("An IP address and port, e.g. `127.0.0.1:8080` or `[::1]:8080`.")
    .example("127.0.0.1:8080")
    .build());

// `serde` renders a `PathBuf` as a string and *fails* on a non-UTF-8 path, so
// the schema is honest about the only shape that can cross the wire.
anonymous_schema!(PathBuf, "Path", false, |_g| StringBuilder::new()
    .description("A filesystem path. Must be valid UTF-8.")
    .build());

// `moso_schema::types::Url` is the type to prefer in a model: it rejects
// relative references, which is the difference between a URL you can fetch and
// an SSRF report. This impl exists so a foreign struct holding a bare
// `url::Url` can still be described.
anonymous_schema!(url::Url, "Url", true, |_g| StringBuilder::new()
    .format("uri")
    .description("An absolute URL.")
    .build());

// `serde` renders a `Duration` as `{ "secs": u64, "nanos": u32 }`; the schema
// has to say so rather than pretend it is an ISO 8601 duration string.
anonymous_schema!(Duration, "Duration", true, |g| ObjectBuilder::new()
    .property("secs", g.subschema_for::<u64>(), true)
    .property("nanos", g.subschema_for::<u32>(), true)
    .description("A duration as whole seconds plus nanoseconds.")
    .build());

// ── wrappers ────────────────────────────────────────────────────────────

impl<T: Validate> Validate for Option<T> {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        match self {
            Some(v) => v.validate(ctx),
            None => Ok(()),
        }
    }
}

impl<T: Schema> Schema for Option<T> {
    fn schema_name() -> Cow<'static, str> {
        generic_schema_name("Nullable", &[T::schema_name()])
    }

    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
        generator.subschema_for::<T>().nullable()
    }

    fn schema_ref() -> SchemaRef {
        SchemaRef::inline(T::schema_ref().into_node().nullable())
    }

    const HAS_CONSTRAINTS: bool = T::HAS_CONSTRAINTS;
}

impl<T: Validate> Validate for Box<T> {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        (**self).validate(ctx)
    }
}

impl<T: Schema> Schema for Box<T> {
    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
        T::json_schema(generator)
    }

    fn schema_ref() -> SchemaRef {
        T::schema_ref()
    }

    const HAS_CONSTRAINTS: bool = T::HAS_CONSTRAINTS;
}

impl<T: Validate + ?Sized> Validate for Arc<T> {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        (**self).validate(ctx)
    }
}

/// `Arc<T>` is invisible on the wire, so it is invisible in the schema too.
///
/// `Rc<T>` deliberately has no impl: [`Schema`] requires `Send + Sync`.
///
/// One caveat inherited from `serde`: deserialising two `Arc<T>` fields that
/// pointed at one allocation produces two allocations. Sharing is a property of
/// the process, not of the JSON.
impl<T: Schema> Schema for Arc<T> {
    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
        T::json_schema(generator)
    }

    fn schema_ref() -> SchemaRef {
        T::schema_ref()
    }

    const HAS_CONSTRAINTS: bool = T::HAS_CONSTRAINTS;
}

// ── sequences ───────────────────────────────────────────────────────────

/// Validates every element, addressing failures by index.
///
/// The prefix is the element's *own* segment — `/2`, not the whole path from
/// the document root — because the caller lifts it in turn. See the module
/// header on the pointer convention; using an absolute prefix here would
/// double-count every segment for a nested collection.
#[allow(
    clippy::result_large_err,
    reason = "the return type is `Validate::validate`'s; `ValidationErrors` is sized \
              deliberately (see its docs) and boxing it here would not change the trait"
)]
fn validate_each<'a, T: Validate + 'a>(
    items: impl IntoIterator<Item = &'a T>,
    ctx: &mut ValidationCtx,
) -> Result<(), ValidationErrors> {
    let mut errors = ValidationErrors::new();
    for (index, item) in items.into_iter().enumerate() {
        if ctx.is_full(&errors) {
            break;
        }
        if let Err(inner) = item.validate(ctx) {
            errors.merge_prefixed(&index_pointer(index), inner);
        }
    }
    errors.truncate(ctx.max_errors());
    errors.into_result()
}

/// `/7` — one JSON Pointer segment for an array index.
fn index_pointer(index: usize) -> String {
    format!("/{index}")
}

/// `/a~1b` — one JSON Pointer segment for a map key, RFC 6901 escaped.
fn key_pointer(key: &str) -> String {
    let mut pointer = String::with_capacity(key.len() + 1);
    push_token(&mut pointer, key);
    pointer
}

macro_rules! sequence_schema {
    ($t:ty, $name:literal, $unique:expr) => {
        impl<T: Validate> Validate for $t {
            fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
                validate_each(self.iter(), ctx)
            }
        }

        impl<T: Schema> Schema for $t {
            fn schema_name() -> Cow<'static, str> {
                generic_schema_name($name, &[T::schema_name()])
            }

            fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
                ArrayBuilder::new()
                    .items(generator.subschema_for::<T>())
                    .unique_items($unique)
                    .build()
            }

            fn schema_ref() -> SchemaRef {
                SchemaRef::inline(
                    ArrayBuilder::new()
                        .items(T::schema_ref())
                        .unique_items($unique)
                        .build(),
                )
            }

            const HAS_CONSTRAINTS: bool = $unique || T::HAS_CONSTRAINTS;
        }
    };
}

sequence_schema!(Vec<T>, "Array", false);
sequence_schema!(VecDeque<T>, "Array", false);

impl<T: Validate> Validate for HashSet<T> {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        validate_each(self.iter(), ctx)
    }
}

impl<T: Schema + Eq + std::hash::Hash> Schema for HashSet<T> {
    fn schema_name() -> Cow<'static, str> {
        generic_schema_name("Set", &[T::schema_name()])
    }

    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
        ArrayBuilder::new()
            .items(generator.subschema_for::<T>())
            .unique_items(true)
            .build()
    }

    fn schema_ref() -> SchemaRef {
        SchemaRef::inline(
            ArrayBuilder::new()
                .items(T::schema_ref())
                .unique_items(true)
                .build(),
        )
    }

    const HAS_CONSTRAINTS: bool = true;
}

impl<T: Validate> Validate for BTreeSet<T> {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        validate_each(self.iter(), ctx)
    }
}

impl<T: Schema + Eq + Ord> Schema for BTreeSet<T> {
    fn schema_name() -> Cow<'static, str> {
        generic_schema_name("Set", &[T::schema_name()])
    }

    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
        ArrayBuilder::new()
            .items(generator.subschema_for::<T>())
            .unique_items(true)
            .build()
    }

    fn schema_ref() -> SchemaRef {
        SchemaRef::inline(
            ArrayBuilder::new()
                .items(T::schema_ref())
                .unique_items(true)
                .build(),
        )
    }

    const HAS_CONSTRAINTS: bool = true;
}

impl<T: Validate, const N: usize> Validate for [T; N] {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        validate_each(self.iter(), ctx)
    }
}

/// `serde` implements `Serialize`/`Deserialize` for `[T; N]` one `N` at a time,
/// for `N` in `0..=32`, so `Schema` follows exactly that range rather than being
/// const-generic and unsatisfiable.
macro_rules! fixed_array_schema {
    ($($n:literal),* $(,)?) => {$(
        impl<T: Schema> Schema for [T; $n] {
            fn schema_name() -> Cow<'static, str> {
                generic_schema_name(
                    "FixedArray",
                    &[T::schema_name(), Cow::Borrowed(stringify!($n))],
                )
            }

            fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
                ArrayBuilder::new()
                    .items(generator.subschema_for::<T>())
                    .min_items($n)
                    .max_items($n)
                    .build()
            }

            fn schema_ref() -> SchemaRef {
                inline_schema_ref::<Self>()
            }

            const HAS_CONSTRAINTS: bool = true;
        }
    )*};
}

fixed_array_schema!(
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31, 32,
);

// ── maps ────────────────────────────────────────────────────────────────

/// Validates every value, addressing failures by key.
///
/// Keys are escaped, so a map key containing `/` cannot forge a pointer into a
/// sibling field.
#[allow(
    clippy::result_large_err,
    reason = "see `validate_each`: the size is `Validate::validate`'s, not this helper's"
)]
fn validate_map<'a, V: Validate + 'a>(
    entries: impl IntoIterator<Item = (&'a String, &'a V)>,
    ctx: &mut ValidationCtx,
) -> Result<(), ValidationErrors> {
    let mut errors = ValidationErrors::new();
    for (key, value) in entries {
        if ctx.is_full(&errors) {
            break;
        }
        if let Err(inner) = value.validate(ctx) {
            errors.merge_prefixed(&key_pointer(key), inner);
        }
    }
    errors.truncate(ctx.max_errors());
    errors.into_result()
}

macro_rules! map_schema {
    ($t:ty) => {
        impl<V: Validate> Validate for $t {
            fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
                validate_map(self.iter(), ctx)
            }
        }

        impl<V: Schema> Schema for $t {
            fn schema_name() -> Cow<'static, str> {
                generic_schema_name("Map", &[V::schema_name()])
            }

            fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
                let values = generator.subschema_for::<V>();
                ObjectBuilder::new()
                    .additional_properties_schema(values)
                    .build()
            }

            fn schema_ref() -> SchemaRef {
                SchemaRef::inline(
                    ObjectBuilder::new()
                        .additional_properties_schema(V::schema_ref())
                        .build(),
                )
            }

            const HAS_CONSTRAINTS: bool = V::HAS_CONSTRAINTS;
        }
    };
}

map_schema!(HashMap<String, V>);
map_schema!(BTreeMap<String, V>);

// ── tuples ──────────────────────────────────────────────────────────────

macro_rules! tuple_schema {
    ($name:literal, $arity:literal, $($ty:ident . $idx:tt),+) => {
        impl<$($ty: Validate),+> Validate for ($($ty,)+) {
            fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
                let mut errors = ValidationErrors::new();
                $(
                    ctx.push_index($idx);
                    let pointer = ctx.pointer().to_owned();
                    ctx.pop();
                    if let Err(inner) = self.$idx.validate(ctx) {
                        errors.merge_prefixed(&pointer, inner);
                    }
                )+
                errors.into_result()
            }
        }

        impl<$($ty: Schema),+> Schema for ($($ty,)+) {
            fn schema_name() -> Cow<'static, str> {
                generic_schema_name($name, &[$($ty::schema_name()),+])
            }

            fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
                let mut b = ArrayBuilder::new();
                $( b = b.prefix_item(generator.subschema_for::<$ty>()); )+
                b.min_items($arity).max_items($arity).build()
            }

            fn schema_ref() -> SchemaRef {
                inline_schema_ref::<Self>()
            }

            const HAS_CONSTRAINTS: bool = true;
        }
    };
}

tuple_schema!("Tuple1", 1, A.0);
tuple_schema!("Tuple2", 2, A.0, B.1);
tuple_schema!("Tuple3", 3, A.0, B.1, C.2);
tuple_schema!("Tuple4", 4, A.0, B.1, C.2, D.3);
tuple_schema!("Tuple5", 5, A.0, B.1, C.2, D.3, E.4);
tuple_schema!("Tuple6", 6, A.0, B.1, C.2, D.3, E.4, F.5);
tuple_schema!("Tuple7", 7, A.0, B.1, C.2, D.3, E.4, F.5, G.6);
tuple_schema!("Tuple8", 8, A.0, B.1, C.2, D.3, E.4, F.5, G.6, H.7);
tuple_schema!("Tuple9", 9, A.0, B.1, C.2, D.3, E.4, F.5, G.6, H.7, I.8);
tuple_schema!(
    "Tuple10", 10, A.0, B.1, C.2, D.3, E.4, F.5, G.6, H.7, I.8, J.9
);
tuple_schema!(
    "Tuple11", 11, A.0, B.1, C.2, D.3, E.4, F.5, G.6, H.7, I.8, J.9, K.10
);
tuple_schema!(
    "Tuple12", 12, A.0, B.1, C.2, D.3, E.4, F.5, G.6, H.7, I.8, J.9, K.10, L.11
);

#[cfg(test)]
mod tests {
    use serde_json::{Number, json};

    use super::*;
    use crate::json_schema::JsonType;
    use crate::validate::codes;

    /// Fails once, at `/field`, so a test can watch the pointer be lifted.
    struct AlwaysInvalid;

    impl Validate for AlwaysInvalid {
        fn validate(&self, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
            Err(ValidationErrors::one("/field", codes::PATTERN, "nope"))
        }
    }

    fn pointers<T: Validate>(value: &T) -> Vec<String> {
        let mut ctx = ValidationCtx::new();
        match value.validate(&mut ctx) {
            Ok(()) => Vec::new(),
            Err(e) => e.iter().map(|f| f.pointer.clone()).collect(),
        }
    }

    fn node_of<T: Schema>() -> SchemaNode {
        T::json_schema(&mut SchemaGenerator::default())
    }

    #[test]
    fn sequence_errors_are_addressed_by_index() {
        assert_eq!(
            pointers(&vec![AlwaysInvalid, AlwaysInvalid]),
            vec!["/0/field", "/1/field"]
        );
    }

    #[test]
    fn nested_sequences_compose_pointers() {
        // The bug this guards: prefixing with the *absolute* pointer instead of
        // the element's own segment yields `/0/0/field` only by luck at depth
        // one and garbage below it.
        assert_eq!(
            pointers(&vec![vec![AlwaysInvalid], vec![AlwaysInvalid]]),
            vec!["/0/0/field", "/1/0/field"]
        );
    }

    #[test]
    fn map_errors_are_addressed_by_escaped_key() {
        let mut map = BTreeMap::new();
        map.insert(String::from("a/b"), AlwaysInvalid);
        map.insert(String::from("plain"), AlwaysInvalid);
        assert_eq!(pointers(&map), vec!["/a~1b/field", "/plain/field"]);
    }

    #[test]
    fn options_do_not_add_a_segment_and_none_is_valid() {
        assert_eq!(pointers(&Some(AlwaysInvalid)), vec!["/field"]);
        assert!(pointers(&Option::<AlwaysInvalid>::None).is_empty());
    }

    #[test]
    fn wrappers_are_transparent_to_validation() {
        assert_eq!(pointers(&Box::new(AlwaysInvalid)), vec!["/field"]);
        assert_eq!(pointers(&Arc::new(AlwaysInvalid)), vec!["/field"]);
        assert_eq!(pointers(&&AlwaysInvalid), vec!["/field"]);
    }

    #[test]
    fn tuple_errors_are_addressed_by_position() {
        assert_eq!(
            pointers(&(AlwaysInvalid, AlwaysInvalid)),
            vec!["/0/field", "/1/field"]
        );
    }

    #[test]
    fn collection_validation_respects_the_error_cap() {
        let items: Vec<AlwaysInvalid> = (0..10).map(|_| AlwaysInvalid).collect();
        let mut ctx = ValidationCtx::new().with_max_errors(3);
        let errors = items.validate(&mut ctx).unwrap_err();
        assert_eq!(errors.len(), 3);
    }

    #[test]
    fn primitives_are_inline_not_referenced() {
        assert!(<String as Schema>::schema_ref().as_node().is_some());
        assert!(<i32 as Schema>::schema_ref().as_node().is_some());
        assert!(<Uuid as Schema>::schema_ref().as_node().is_some());
    }

    #[test]
    fn integer_bounds_come_from_the_rust_type() {
        let node = <u8 as Schema>::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            node.types.primary(),
            Some(crate::json_schema::JsonType::Integer)
        );
        assert_eq!(node.minimum, Some(serde_json::Number::from(0)));
        assert_eq!(node.maximum, Some(serde_json::Number::from(255)));
    }

    #[test]
    fn generic_names_compose() {
        assert_eq!(<Vec<String> as Schema>::schema_name(), "Array_String");
        assert_eq!(
            <Option<Vec<i32>> as Schema>::schema_name(),
            "Nullable_Array_Int32"
        );
        assert_eq!(
            <HashMap<String, bool> as Schema>::schema_name(),
            "Map_Boolean"
        );
        assert_eq!(
            <(i32, String) as Schema>::schema_name(),
            "Tuple2_Int32_String"
        );
    }

    #[test]
    fn sets_assert_uniqueness() {
        let node = <BTreeSet<String> as Schema>::schema_ref().into_node();
        assert!(node.unique_items);
        assert!(node_of::<HashSet<String>>().unique_items);
        assert!(!node_of::<Vec<String>>().unique_items);
    }

    #[test]
    fn signed_integer_bounds_come_from_the_rust_type() {
        let node = node_of::<i64>();
        assert_eq!(node.types.primary(), Some(JsonType::Integer));
        assert_eq!(node.minimum, Some(Number::from(i64::MIN)));
        assert_eq!(node.maximum, Some(Number::from(i64::MAX)));
        assert_eq!(node.format.as_deref(), Some("int64"));

        // 128-bit integers deliberately carry no bound: a JSON number cannot
        // hold one losslessly, and a bound wider than the format is a lie.
        assert!(node_of::<i128>().minimum.is_none());
        assert!(node_of::<u128>().maximum.is_none());
        assert_eq!(node_of::<u128>().minimum, Some(Number::from(0)));
    }

    #[test]
    fn non_zero_unsigned_starts_at_one() {
        let node = node_of::<NonZeroU8>();
        assert_eq!(node.minimum, Some(Number::from(1)));
        assert_eq!(node.maximum, Some(Number::from(255)));
        assert!(node.not.is_none(), "`minimum: 1` already excludes zero");
    }

    #[test]
    fn non_zero_signed_excludes_zero_exactly() {
        let node = node_of::<NonZeroI8>();
        assert_eq!(node.minimum, Some(Number::from(-128)));
        assert_eq!(node.maximum, Some(Number::from(127)));
        assert_eq!(
            node.not.as_deref(),
            Some(&SchemaNode::constant(0)),
            "the hole is mid-range, so bounds alone cannot express it"
        );
        assert_eq!(
            serde_json::to_value(node_of::<NonZeroI128>()).unwrap(),
            json!({
                "type": "integer",
                "format": "int128",
                "description": "A non-zero integer.",
                "not": { "const": 0 }
            })
        );
    }

    #[test]
    fn foreign_scalars_have_the_documented_formats() {
        assert_eq!(node_of::<Uuid>().format.as_deref(), Some("uuid"));
        assert_eq!(node_of::<url::Url>().format.as_deref(), Some("uri"));
        assert_eq!(
            node_of::<DateTime<Utc>>().format.as_deref(),
            Some("date-time")
        );
        assert_eq!(node_of::<NaiveDate>().format.as_deref(), Some("date"));
        assert_eq!(node_of::<NaiveTime>().format.as_deref(), Some("time"));
        assert_eq!(node_of::<Ipv4Addr>().format.as_deref(), Some("ipv4"));

        for ty in [node_of::<SocketAddr>(), node_of::<PathBuf>()] {
            assert_eq!(ty.types.primary(), Some(JsonType::String));
            assert!(
                ty.format.is_none(),
                "Moso does not document a format it cannot enforce"
            );
        }
    }

    #[test]
    fn duration_is_the_shape_serde_actually_emits() {
        assert_eq!(
            serde_json::to_value(node_of::<Duration>()).unwrap(),
            json!({
                "type": "object",
                "description": "A duration as whole seconds plus nanoseconds.",
                "properties": {
                    "secs": {
                        "type": "integer", "format": "uint64",
                        "minimum": 0, "maximum": u64::MAX
                    },
                    "nanos": {
                        "type": "integer", "format": "uint32",
                        "minimum": 0, "maximum": u32::MAX
                    }
                },
                "required": ["secs", "nanos"]
            })
        );
    }

    #[test]
    fn optional_scalars_widen_the_type_keyword() {
        assert_eq!(
            serde_json::to_value(node_of::<Option<String>>()).unwrap(),
            json!({ "type": ["string", "null"] })
        );
        assert_eq!(
            serde_json::to_value(node_of::<Vec<Option<bool>>>()).unwrap(),
            json!({ "type": "array", "items": { "type": ["boolean", "null"] } })
        );
    }

    #[test]
    fn maps_describe_their_values_with_additional_properties() {
        assert_eq!(
            serde_json::to_value(node_of::<BTreeMap<String, bool>>()).unwrap(),
            json!({ "type": "object", "additionalProperties": { "type": "boolean" } })
        );
    }

    #[test]
    fn tuples_and_fixed_arrays_pin_their_length() {
        assert_eq!(
            serde_json::to_value(node_of::<(bool, String)>()).unwrap(),
            json!({
                "type": "array",
                "prefixItems": [{ "type": "boolean" }, { "type": "string" }],
                "minItems": 2,
                "maxItems": 2
            })
        );
        let node = node_of::<[bool; 3]>();
        assert_eq!((node.min_items, node.max_items), (Some(3), Some(3)));
        assert_eq!(<[bool; 0] as Schema>::schema_name(), "FixedArray_Boolean_0");
    }

    #[test]
    fn wrappers_are_transparent_to_the_schema() {
        assert_eq!(node_of::<Arc<String>>(), node_of::<String>());
        assert_eq!(node_of::<Box<String>>(), node_of::<String>());
        assert_eq!(<Arc<String> as Schema>::schema_name(), "String");
        assert_eq!(node_of::<Cow<'static, str>>(), node_of::<String>());
    }

    #[test]
    fn char_is_a_one_character_string() {
        let node = node_of::<char>();
        assert_eq!(node.types.primary(), Some(JsonType::String));
        assert_eq!((node.min_length, node.max_length), (Some(1), Some(1)));
    }

    #[test]
    fn any_json_value_asserts_nothing() {
        let mut node = node_of::<Value>();
        node.description = None;
        assert!(node.is_any());
    }

    #[test]
    fn has_constraints_propagates_through_containers() {
        // `const` blocks: `HAS_CONSTRAINTS` drives whether a `422` is
        // documented, and it is decided at compile time, so checking it at
        // compile time is both cheaper and stricter.
        const {
            assert!(!<String as Schema>::HAS_CONSTRAINTS);
            assert!(<Uuid as Schema>::HAS_CONSTRAINTS);
            assert!(!<Vec<String> as Schema>::HAS_CONSTRAINTS);
            assert!(<Vec<Uuid> as Schema>::HAS_CONSTRAINTS);
            assert!(<Option<Uuid> as Schema>::HAS_CONSTRAINTS);
            assert!(<BTreeSet<String> as Schema>::HAS_CONSTRAINTS, "uniqueItems");
        }
    }
}
