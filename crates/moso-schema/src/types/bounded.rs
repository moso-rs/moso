//! Const-generic wrappers that move a bound from an attribute into the type.
//!
//! `#[schema(len = 3..=32)] name: String` protects one field. `Length<String,
//! 3, 32>` protects every value of that type, everywhere, including the ones
//! constructed in application code that never went through a request.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::json_schema::{SchemaGenerator, SchemaNode, SchemaRef};
use crate::schema::{Schema, generic_schema_name};
use crate::types::ConstraintError;
use crate::validate::{ErrorCode, Validate, ValidationCtx, ValidationErrors};

/// A type with a countable size.
///
/// [`Measured::UNIT`] decides both the wording of the error message and which
/// JSON Schema keyword pair the bound becomes — `minLength`/`maxLength` for
/// characters, `minItems`/`maxItems` for items.
///
/// Implemented for `String`, `Vec<T>`, `VecDeque<T>`, `HashSet<T>`,
/// `BTreeSet<T>`, `HashMap<K, V>` and `BTreeMap<K, V>`; implement it for a
/// collection of your own to use it inside [`NonEmpty`] or [`Length`].
///
/// ```
/// use moso_schema::types::Measured;
///
/// // Text is measured in characters, not bytes …
/// assert_eq!(String::from("héllo").measure(), 5);
/// assert_eq!(<String as Measured>::UNIT, "characters");
///
/// // … and a collection in items.
/// assert_eq!(vec![1, 2, 3].measure(), 3);
/// assert_eq!(<Vec<i32> as Measured>::UNIT, "items");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no length Moso can check",
    label = "does not implement `Measured`",
    note = "`NonEmpty<T>` and `Length<T, MIN, MAX>` need to know how to count `{Self}`",
    note = "implemented for `String`, `Vec<T>`, `VecDeque<T>`, `HashSet<T>`, `BTreeSet<T>`, \
            `HashMap<K, V>` and `BTreeMap<K, V>`",
    note = "help: implement it:\n    impl moso::Measured for {Self} {{\n        \
            const UNIT: &'static str = \"items\";\n        \
            fn measure(&self) -> usize {{ self.0.len() }}\n    }}"
)]
pub trait Measured {
    /// `"characters"` for text, `"items"` for collections. Appears in the
    /// validation message and selects the JSON Schema keyword pair.
    const UNIT: &'static str;

    /// The size: characters for text, elements for collections.
    fn measure(&self) -> usize;
}

impl Measured for String {
    const UNIT: &'static str = "characters";

    fn measure(&self) -> usize {
        self.chars().count()
    }
}

impl Measured for str {
    const UNIT: &'static str = "characters";

    fn measure(&self) -> usize {
        self.chars().count()
    }
}

macro_rules! measured_collection {
    ($($t:ty),* $(,)?) => {$(
        impl<T> Measured for $t {
            const UNIT: &'static str = "items";

            fn measure(&self) -> usize {
                self.len()
            }
        }
    )*};
}

measured_collection!(Vec<T>, VecDeque<T>, HashSet<T>, BTreeSet<T>);

impl<K, V> Measured for HashMap<K, V> {
    const UNIT: &'static str = "items";

    fn measure(&self) -> usize {
        self.len()
    }
}

impl<K, V> Measured for BTreeMap<K, V> {
    const UNIT: &'static str = "items";

    fn measure(&self) -> usize {
        self.len()
    }
}

/// An integer type a [`Bounded`] bound can be expressed in.
///
/// Bounds are `i64` because const generic parameters must have a single type;
/// `u64` values above `i64::MAX` saturate, which is harmless because such a
/// value cannot satisfy any `i64` upper bound anyway.
///
/// ```
/// use moso_schema::types::IntegerValue;
///
/// assert_eq!(42_u8.as_i64(), 42);
/// assert_eq!(u8::from_i64(42), Some(42));
///
/// // A bound the narrow type cannot hold is rejected rather than truncated.
/// assert_eq!(u8::from_i64(300), None);
///
/// // And an unrepresentable `u64` saturates, which no `i64` bound can accept.
/// assert_eq!(u64::MAX.as_i64(), i64::MAX);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot carry a `Bounded` range",
    label = "does not implement `IntegerValue`",
    note = "`Bounded<T, MIN, MAX>` requires an integer type: `i8`…`i64`, `u8`…`u64`, \
            `isize`, `usize`",
    note = "help: for a float bound use `#[schema(range = 0.0..1.0)]` on the field instead"
)]
pub trait IntegerValue: Copy + Sized {
    /// Widen to the bound's type, saturating.
    fn as_i64(self) -> i64;

    /// Narrow from the bound's type, or `None` if out of range.
    fn from_i64(value: i64) -> Option<Self>;
}

macro_rules! integer_value {
    ($($t:ty),* $(,)?) => {$(
        impl IntegerValue for $t {
            fn as_i64(self) -> i64 {
                i64::try_from(self).unwrap_or(i64::MAX)
            }

            fn from_i64(value: i64) -> Option<Self> {
                <$t>::try_from(value).ok()
            }
        }
    )*};
}

integer_value!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

/// A collection or string guaranteed to hold at least one element.
///
/// The value `Some(vec![])` and the value `None` mean different things to a
/// client and the same thing to most code; `Option<NonEmpty<Vec<T>>>` makes
/// that distinction impossible to forget.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NonEmpty<T>(T);

impl<T: Measured> NonEmpty<T> {
    /// Wrap a value, rejecting empties.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `len` and `min: 1`.
    pub fn new(value: T) -> Result<Self, ConstraintError> {
        if value.measure() == 0 {
            return Err(ConstraintError::new(
                ErrorCode::Len,
                format!("must not be empty (at least 1 of {})", T::UNIT),
            )
            .with_param("min", 1)
            .with_param("unit", T::UNIT));
        }
        Ok(Self(value))
    }
}

impl<T> NonEmpty<T> {
    /// Wrap a value without measuring it.
    ///
    /// **Escape hatch.** A `NonEmpty` built from an empty value will still
    /// claim, in its type, that it holds something.
    #[must_use]
    pub const fn new_unchecked(value: T) -> Self {
        Self(value)
    }

    /// Borrow the inner value.
    pub fn get(&self) -> &T {
        &self.0
    }

    /// Consume into the inner value.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> std::ops::Deref for NonEmpty<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> AsRef<T> for NonEmpty<T> {
    fn as_ref(&self) -> &T {
        &self.0
    }
}

impl<T: fmt::Display> fmt::Display for NonEmpty<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Serialize> Serialize for NonEmpty<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de, T: Deserialize<'de> + Measured> Deserialize<'de> for NonEmpty<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let inner = T::deserialize(d)?;
        Self::new(inner).map_err(ConstraintError::into_serde_error)
    }
}

impl<T: Validate> Validate for NonEmpty<T> {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        self.0.validate(ctx)
    }
}

impl<T: Schema + Measured> Schema for NonEmpty<T> {
    fn schema_name() -> Cow<'static, str> {
        generic_schema_name("NonEmpty", &[T::schema_name()])
    }

    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
        let mut node = generator.subschema_for::<T>();
        node.apply_len(Some(1), None);
        node
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }

    const HAS_CONSTRAINTS: bool = true;
}

/// An integer constrained to `MIN..=MAX` by its type.
///
/// `Bounded<u16, 1, 100>` is a page size that cannot be zero and cannot be
/// 10 000, checked once at the boundary and then never again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bounded<T, const MIN: i64, const MAX: i64>(T);

impl<T: IntegerValue, const MIN: i64, const MAX: i64> Bounded<T, MIN, MAX> {
    /// The inclusive lower bound.
    pub const MIN: i64 = MIN;

    /// The inclusive upper bound.
    pub const MAX: i64 = MAX;

    /// Wrap a value, rejecting anything outside `MIN..=MAX`.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `range`, carrying `min` and `max` so the
    /// message names the bounds this type was declared with.
    pub fn new(value: T) -> Result<Self, ConstraintError> {
        let widened = value.as_i64();
        if widened < MIN || widened > MAX {
            return Err(ConstraintError::new(
                ErrorCode::Range,
                format!("must be between {MIN} and {MAX} (got {widened})"),
            )
            .with_param("min", MIN)
            .with_param("max", MAX));
        }
        Ok(Self(value))
    }

    /// Wrap a value without checking the bounds.
    ///
    /// **Escape hatch.** The type will assert a range the value does not
    /// satisfy, and every consumer that trusted `MIN..=MAX` — an array index,
    /// a page size, a percentage — is then wrong.
    #[must_use]
    pub const fn new_unchecked(value: T) -> Self {
        Self(value)
    }

    /// Clamp into range instead of failing.
    ///
    /// For values Moso itself produces — a default page size read from
    /// configuration — where failing is not a useful response. `None` only
    /// when `MIN`/`MAX` are not representable in `T`, which is a static
    /// mistake such as `Bounded<u8, 0, 300>`.
    #[must_use]
    pub fn clamped(value: T) -> Option<Self> {
        // `i64::clamp` panics when the bounds are inverted; a `Bounded<u8, 10,
        // 5>` is a static mistake, and `None` reports it without a panic.
        if MIN > MAX {
            return None;
        }
        // Both bounds must be representable in `T`, or the declaration is a
        // mistake such as `Bounded<u8, 0, 300>` — whose upper bound no value
        // can reach, so clamping into it would be a lie.
        let _ = T::from_i64(MIN)?;
        let _ = T::from_i64(MAX)?;
        let clamped = value.as_i64().clamp(MIN, MAX);
        T::from_i64(clamped).map(Self)
    }

    /// The wrapped value.
    pub fn get(&self) -> T {
        self.0
    }

    /// Consume into the wrapped value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: fmt::Display, const MIN: i64, const MAX: i64> fmt::Display for Bounded<T, MIN, MAX> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Serialize, const MIN: i64, const MAX: i64> Serialize for Bounded<T, MIN, MAX> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de, T, const MIN: i64, const MAX: i64> Deserialize<'de> for Bounded<T, MIN, MAX>
where
    T: Deserialize<'de> + IntegerValue,
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let inner = T::deserialize(d)?;
        Self::new(inner).map_err(ConstraintError::into_serde_error)
    }
}

impl<T, const MIN: i64, const MAX: i64> Validate for Bounded<T, MIN, MAX> {
    fn validate(&self, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        Ok(())
    }
}

impl<T, const MIN: i64, const MAX: i64> Schema for Bounded<T, MIN, MAX>
where
    T: Schema + IntegerValue,
{
    fn schema_name() -> Cow<'static, str> {
        generic_schema_name(
            "Bounded",
            &[
                T::schema_name(),
                Cow::Owned(MIN.to_string()),
                Cow::Owned(MAX.to_string()),
            ],
        )
    }

    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
        let mut node = generator.subschema_for::<T>();
        node.minimum = Some(MIN.into());
        node.maximum = Some(MAX.into());
        node
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }

    const HAS_CONSTRAINTS: bool = true;
}

/// A string or collection whose size is constrained by its type.
///
/// `Length<String, 3, 32>` is a username; `Length<Vec<Tag>, 0, 10>` is a
/// bounded tag list.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Length<T, const MIN: usize, const MAX: usize>(T);

impl<T: Measured, const MIN: usize, const MAX: usize> Length<T, MIN, MAX> {
    /// The inclusive lower bound, in [`Measured::UNIT`].
    pub const MIN: usize = MIN;

    /// The inclusive upper bound, in [`Measured::UNIT`].
    pub const MAX: usize = MAX;

    /// Wrap a value, rejecting anything outside `MIN..=MAX`.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `len`, carrying `min`, `max` and the
    /// `unit` — `characters` for text, `items` for collections — so the
    /// message reads correctly for either.
    pub fn new(value: T) -> Result<Self, ConstraintError> {
        let measured = value.measure();
        if measured < MIN || measured > MAX {
            return Err(ConstraintError::new(
                ErrorCode::Len,
                format!(
                    "must be between {MIN} and {MAX} {} (got {measured})",
                    T::UNIT
                ),
            )
            .with_param("min", MIN as u64)
            .with_param("max", MAX as u64)
            .with_param("unit", T::UNIT));
        }
        Ok(Self(value))
    }

    /// Borrow the inner value.
    pub fn get(&self) -> &T {
        &self.0
    }

    /// Consume into the inner value.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T, const MIN: usize, const MAX: usize> Length<T, MIN, MAX> {
    /// Wrap a value without measuring it.
    ///
    /// **Escape hatch.** The type will document `minLength`/`maxLength` that
    /// the value does not satisfy.
    #[must_use]
    pub const fn new_unchecked(value: T) -> Self {
        Self(value)
    }
}

impl<T, const MIN: usize, const MAX: usize> std::ops::Deref for Length<T, MIN, MAX> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: fmt::Display, const MIN: usize, const MAX: usize> fmt::Display for Length<T, MIN, MAX> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Serialize, const MIN: usize, const MAX: usize> Serialize for Length<T, MIN, MAX> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de, T, const MIN: usize, const MAX: usize> Deserialize<'de> for Length<T, MIN, MAX>
where
    T: Deserialize<'de> + Measured,
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let inner = T::deserialize(d)?;
        Self::new(inner).map_err(ConstraintError::into_serde_error)
    }
}

impl<T: Validate, const MIN: usize, const MAX: usize> Validate for Length<T, MIN, MAX> {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        self.0.validate(ctx)
    }
}

impl<T, const MIN: usize, const MAX: usize> Schema for Length<T, MIN, MAX>
where
    T: Schema + Measured,
{
    fn schema_name() -> Cow<'static, str> {
        generic_schema_name(
            "Length",
            &[
                T::schema_name(),
                Cow::Owned(MIN.to_string()),
                Cow::Owned(MAX.to_string()),
            ],
        )
    }

    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
        let mut node = generator.subschema_for::<T>();
        node.apply_len(Some(MIN as u64), Some(MAX as u64));
        node
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }

    const HAS_CONSTRAINTS: bool = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_measured_in_characters() {
        assert_eq!(String::from("héllo").measure(), 5);
        assert_eq!(String::from("héllo").len(), 6, "bytes differ from chars");
        assert_eq!(String::UNIT, "characters");
    }

    #[test]
    fn collections_are_measured_in_items() {
        assert_eq!(vec![1, 2, 3].measure(), 3);
        assert_eq!(<Vec<u8> as Measured>::UNIT, "items");
    }

    #[test]
    fn integer_conversions_saturate_rather_than_wrap() {
        assert_eq!(7u8.as_i64(), 7);
        assert_eq!(u64::MAX.as_i64(), i64::MAX);
        assert_eq!(u8::from_i64(300), None);
        assert_eq!(u8::from_i64(200), Some(200u8));
    }

    // ── NonEmpty ─────────────────────────────────────────────────────────

    #[test]
    fn non_empty_rejects_empty_values_with_a_len_code() {
        let e = NonEmpty::new(String::new()).expect_err("empty string");
        assert_eq!(e.code().as_str(), crate::validate::codes::LEN);
        assert_eq!(e.params().get("min"), Some(&serde_json::json!(1)));
        assert_eq!(
            e.params().get("unit"),
            Some(&serde_json::json!("characters")),
            "a string is measured in characters"
        );

        let e = NonEmpty::new(Vec::<u8>::new()).expect_err("empty vec");
        assert_eq!(e.params().get("unit"), Some(&serde_json::json!("items")));
        assert!(e.message().contains("items"), "{}", e.message());

        // Whitespace is a character: `NonEmpty` counts, it does not judge.
        assert!(NonEmpty::new(String::from(" ")).is_ok());
    }

    #[test]
    fn non_empty_accepts_and_derefs() {
        let v = NonEmpty::new(vec![1, 2, 3]).unwrap();
        assert_eq!(v.get(), &[1, 2, 3]);
        assert_eq!(v.len(), 3, "Deref reaches the inner value");
        assert_eq!(v.as_ref(), &vec![1, 2, 3]);
        assert_eq!(v.clone().into_inner(), vec![1, 2, 3]);
        assert_eq!(NonEmpty::new(String::from("hi")).unwrap().to_string(), "hi");
        assert!(NonEmpty::new_unchecked(Vec::<u8>::new()).get().is_empty());
    }

    #[test]
    fn non_empty_enforces_the_invariant_on_deserialise() {
        assert!(serde_json::from_str::<NonEmpty<Vec<u8>>>("[]").is_err());
        assert_eq!(
            serde_json::from_str::<NonEmpty<Vec<u8>>>("[1]")
                .unwrap()
                .into_inner(),
            vec![1]
        );
        assert_eq!(
            serde_json::to_value(NonEmpty::new(vec![1, 2]).unwrap()).unwrap(),
            serde_json::json!([1, 2]),
            "the wrapper is transparent on the wire"
        );

        let err = serde_json::from_str::<NonEmpty<String>>("\"\"").unwrap_err();
        assert_eq!(
            crate::types::parse_serde_message(&err.to_string()).map(|(c, _)| c),
            Some(crate::validate::codes::LEN)
        );
    }

    // ── Bounded ──────────────────────────────────────────────────────────

    type PageSize = Bounded<u16, 1, 100>;

    #[test]
    fn bounded_accepts_inside_the_range_and_rejects_outside() {
        assert_eq!(PageSize::new(1).unwrap().get(), 1);
        assert_eq!(PageSize::new(100).unwrap().get(), 100);
        assert_eq!(PageSize::new(25).unwrap().into_inner(), 25);
        assert!(PageSize::new(0).is_err());
        assert!(PageSize::new(101).is_err());
        assert_eq!(PageSize::MIN, 1);
        assert_eq!(PageSize::MAX, 100);
    }

    #[test]
    fn bounded_error_names_the_actual_bounds() {
        let e = PageSize::new(0).expect_err("below the range");
        assert_eq!(e.code().as_str(), crate::validate::codes::RANGE);
        assert_eq!(e.message(), "must be between 1 and 100 (got 0)");
        assert_eq!(e.params().get("min"), Some(&serde_json::json!(1)));
        assert_eq!(e.params().get("max"), Some(&serde_json::json!(100)));

        // A different instantiation must report *its* bounds, not these.
        let e = Bounded::<i8, -5, 5>::new(9).expect_err("above the range");
        assert_eq!(e.message(), "must be between -5 and 5 (got 9)");
        assert_eq!(e.params().get("min"), Some(&serde_json::json!(-5)));
    }

    #[test]
    fn bounded_clamps_instead_of_failing_when_asked() {
        assert_eq!(PageSize::clamped(0).unwrap().get(), 1);
        assert_eq!(PageSize::clamped(10_000).unwrap().get(), 100);
        assert_eq!(PageSize::clamped(50).unwrap().get(), 50);
        // `u8` cannot hold 300, so the bound is unusable: `None`, not a panic.
        assert_eq!(Bounded::<u8, 0, 300>::clamped(5), None);
        // Inverted bounds are a static mistake and must not panic either.
        assert_eq!(Bounded::<u8, 10, 5>::clamped(7), None);
    }

    #[test]
    fn bounded_enforces_the_invariant_on_deserialise() {
        assert!(serde_json::from_str::<PageSize>("0").is_err());
        assert_eq!(serde_json::from_str::<PageSize>("50").unwrap().get(), 50);
        assert_eq!(
            serde_json::to_value(PageSize::new(50).unwrap()).unwrap(),
            serde_json::json!(50)
        );
        let err = serde_json::from_str::<PageSize>("0").unwrap_err();
        assert_eq!(
            crate::types::parse_serde_message(&err.to_string()).map(|(c, _)| c),
            Some(crate::validate::codes::RANGE)
        );
        assert_eq!(PageSize::new(7).unwrap().to_string(), "7");
    }

    // ── Length ───────────────────────────────────────────────────────────

    type Username = Length<String, 3, 32>;

    #[test]
    fn length_accepts_inside_the_range_and_rejects_outside() {
        assert_eq!(Username::new("ada".into()).unwrap().get(), "ada");
        assert!(Username::new("ab".into()).is_err());
        assert!(Username::new("a".repeat(33)).is_err());
        assert!(Username::new("a".repeat(32)).is_ok());
        assert_eq!(Username::MIN, 3);
        assert_eq!(Username::MAX, 32);
        assert_eq!(Username::new("ada".into()).unwrap().len(), 3, "Deref works");
    }

    #[test]
    fn length_error_names_the_actual_bounds_and_unit() {
        let e = Username::new("ab".into()).expect_err("too short");
        assert_eq!(e.code().as_str(), crate::validate::codes::LEN);
        assert_eq!(e.message(), "must be between 3 and 32 characters (got 2)");
        assert_eq!(e.params().get("min"), Some(&serde_json::json!(3)));
        assert_eq!(e.params().get("max"), Some(&serde_json::json!(32)));
        assert_eq!(
            e.params().get("unit"),
            Some(&serde_json::json!("characters"))
        );

        let e = Length::<Vec<u8>, 1, 2>::new(vec![1, 2, 3]).expect_err("too many");
        assert_eq!(e.message(), "must be between 1 and 2 items (got 3)");
    }

    #[test]
    fn length_counts_characters_not_bytes() {
        // Three characters, six bytes: a byte-counting check would reject it.
        let name = String::from("héé");
        assert_eq!(name.len(), 5);
        assert!(Username::new(name).is_ok());
    }

    #[test]
    fn length_enforces_the_invariant_on_deserialise() {
        assert!(serde_json::from_str::<Username>("\"ab\"").is_err());
        assert_eq!(
            serde_json::from_str::<Username>("\"ada\"").unwrap().get(),
            "ada"
        );
        assert_eq!(
            serde_json::to_value(Username::new("ada".into()).unwrap()).unwrap(),
            serde_json::json!("ada")
        );
        let err = serde_json::from_str::<Username>("\"ab\"").unwrap_err();
        assert_eq!(
            crate::types::parse_serde_message(&err.to_string()).map(|(c, _)| c),
            Some(crate::validate::codes::LEN)
        );
        assert_eq!(Username::new_unchecked("x".into()).get(), "x");
    }

    #[test]
    fn generic_schema_names_are_stable_and_carry_the_bounds() {
        assert_eq!(<Username as Schema>::schema_name(), "Length_String_3_32");
        assert_eq!(<PageSize as Schema>::schema_name(), "Bounded_UInt16_1_100");
        assert_eq!(
            <NonEmpty<String> as Schema>::schema_name(),
            "NonEmpty_String"
        );
    }
}
