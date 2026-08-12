//! The JSON Schema 2020-12 document model, its builders, and the generator
//! that de-duplicates named schemas.
//!
//! This lives in `moso-schema` rather than `moso-openapi` on purpose: the
//! schema model is what `#[derive(Schema)]` emits, and a model crate must not
//! have to depend on an HTTP documentation crate to describe itself.
//! `moso-openapi` depends on *this*, embedding [`SchemaGenerator`]'s output
//! into `components/schemas`.
//!
//! # Determinism
//!
//! Every map here is an [`IndexMap`], and [`SchemaGenerator`] emits definitions
//! in a stable order, so a committed `openapi.json` diffs cleanly and drift
//! detection is meaningful.
//!
//! # Recursion
//!
//! [`SchemaGenerator::define`] reserves a schema's name *before* generating its
//! body, so `struct Category { children: Vec<Category> }` terminates: the inner
//! `Category` sees the reservation and emits a `$ref`.

mod impls;

use std::borrow::Cow;
use std::collections::HashMap;

use indexmap::IndexMap;
use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Number, Value};
use smallvec::SmallVec;

use crate::schema::{Schema, generic_schema_name};

/// Where `$ref`s point by default: the OpenAPI 3.1 component section.
pub const DEFAULT_REF_PREFIX: &str = "#/components/schemas/";

/// The JSON Schema dialect every schema Moso emits conforms to.
pub const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// One of the seven JSON Schema primitive types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JsonType {
    /// `null`
    Null,
    /// `true` / `false`
    Boolean,
    /// A JSON object.
    Object,
    /// A JSON array.
    Array,
    /// Any JSON number.
    Number,
    /// A JSON number with no fractional part.
    Integer,
    /// A JSON string.
    String,
}

impl JsonType {
    /// The keyword spelling used on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Object => "object",
            Self::Array => "array",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::String => "string",
        }
    }

    /// Parse a wire spelling.
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "null" => Some(Self::Null),
            "boolean" => Some(Self::Boolean),
            "object" => Some(Self::Object),
            "array" => Some(Self::Array),
            "number" => Some(Self::Number),
            "integer" => Some(Self::Integer),
            "string" => Some(Self::String),
            _ => None,
        }
    }
}

/// The value of the `type` keyword.
///
/// JSON Schema allows either a single type or an array of them; Moso uses the
/// array form only to express nullability (`["string", "null"]`), which is why
/// the inline capacity is two.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeSet(SmallVec<[JsonType; 2]>);

impl TypeSet {
    /// The empty set — the `type` keyword is omitted entirely, meaning "any".
    #[must_use]
    pub fn new() -> Self {
        Self(SmallVec::new())
    }

    /// A single type.
    #[must_use]
    pub fn of(ty: JsonType) -> Self {
        let mut s = Self::new();
        s.0.push(ty);
        s
    }

    /// `[ty, "null"]`.
    #[must_use]
    pub fn nullable(ty: JsonType) -> Self {
        let mut s = Self::of(ty);
        s.0.push(JsonType::Null);
        s
    }

    /// Add a type, ignoring duplicates.
    pub fn insert(&mut self, ty: JsonType) {
        if !self.0.contains(&ty) {
            self.0.push(ty);
        }
    }

    /// Remove a type if present.
    pub fn remove(&mut self, ty: JsonType) {
        self.0.retain(|t| *t != ty);
    }

    /// True when the `type` keyword should be omitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many types are in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when `ty` is a member.
    #[must_use]
    pub fn contains(&self, ty: JsonType) -> bool {
        self.0.contains(&ty)
    }

    /// The first non-`null` type, which is the one carrying the constraints.
    #[must_use]
    pub fn primary(&self) -> Option<JsonType> {
        self.0.iter().copied().find(|t| *t != JsonType::Null)
    }

    /// True when `null` is permitted.
    #[must_use]
    pub fn is_nullable(&self) -> bool {
        self.contains(JsonType::Null)
    }

    /// Borrow as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[JsonType] {
        &self.0
    }

    /// Iterate the members in insertion order.
    pub fn iter(&self) -> std::slice::Iter<'_, JsonType> {
        self.0.iter()
    }
}

impl From<JsonType> for TypeSet {
    fn from(t: JsonType) -> Self {
        Self::of(t)
    }
}

impl Serialize for TypeSet {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.0.len() == 1 {
            s.serialize_str(self.0[0].as_str())
        } else {
            s.collect_seq(self.0.iter())
        }
    }
}

impl<'de> Deserialize<'de> for TypeSet {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de> Visitor<'de> for V {
            type Value = TypeSet;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON Schema type name or an array of them")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<TypeSet, E> {
                JsonType::from_str_opt(v)
                    .map(TypeSet::of)
                    .ok_or_else(|| E::custom(format!("unknown JSON Schema type `{v}`")))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<TypeSet, A::Error> {
                let mut set = TypeSet::new();
                while let Some(t) = seq.next_element::<JsonType>()? {
                    set.insert(t);
                }
                Ok(set)
            }
        }

        d.deserialize_any(V)
    }
}

/// The value of `additionalProperties`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AdditionalProperties {
    /// `true` permits anything; `false` forbids unknown properties, which is
    /// what `#[schema(deny_unknown)]` emits.
    Any(bool),
    /// Unknown properties must match this schema — the map case, e.g.
    /// `HashMap<String, T>`.
    Schema(Box<SchemaNode>),
}

/// OpenAPI's `discriminator`, carried through the JSON Schema model.
///
/// Strictly this is an OpenAPI annotation rather than a JSON Schema keyword,
/// but it is the thing that makes a generated TypeScript client produce a real
/// discriminated union instead of `any`, so Moso emits it for every internally
/// tagged enum and carries it here so `moso-openapi` does not have to
/// reconstruct it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Discriminator {
    /// The property holding the tag, e.g. `"kind"`.
    #[serde(rename = "propertyName")]
    pub property_name: String,
    /// Tag value → schema reference. Empty when the mapping is implicit.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub mapping: IndexMap<String, String>,
}

impl Discriminator {
    /// A discriminator on `property_name` with an implicit mapping.
    pub fn new(property_name: impl Into<String>) -> Self {
        Self {
            property_name: property_name.into(),
            mapping: IndexMap::new(),
        }
    }

    /// Add one explicit tag → `$ref` mapping.
    #[must_use]
    pub fn with_mapping(mut self, tag: impl Into<String>, reference: impl Into<String>) -> Self {
        self.mapping.insert(tag.into(), reference.into());
        self
    }
}

/// A JSON Schema 2020-12 node.
///
/// A struct of optional keywords rather than an enum: a schema is a *bag of
/// assertions*, not a tagged union, and modelling it as an enum forces every
/// producer to decide which single shape a node has before it has finished
/// describing it.
///
/// Unknown and vendor keywords land in [`SchemaNode::extensions`] and
/// round-trip verbatim, so `x-`-prefixed annotations survive a
/// deserialise/serialise cycle.
///
/// ```
/// use moso_schema::json_schema::{JsonType, SchemaGenerator, SchemaNode};
///
/// let generator = SchemaGenerator::default();
///
/// // Built with the builders rather than field by field …
/// let username = generator
///     .string()
///     .min_length(3)
///     .max_length(32)
///     .description("Public handle")
///     .build();
///
/// assert_eq!(username.types.iter().next(), Some(&JsonType::String));
/// assert_eq!(username.min_length, Some(3));
///
/// // … and serialised as JSON Schema 2020-12, omitting everything absent.
/// let json = serde_json::to_string(&username).unwrap();
/// assert!(json.contains(r#""minLength":3"#));
/// assert!(!json.contains("null"));
///
/// // `x-*` extensions round-trip verbatim.
/// let extended = SchemaNode::any().with_extension("x-internal", serde_json::Value::Bool(true));
/// let back: SchemaNode = serde_json::from_str(&serde_json::to_string(&extended).unwrap()).unwrap();
/// assert_eq!(back.extensions["x-internal"], serde_json::Value::Bool(true));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaNode {
    // ── reference ────────────────────────────────────────────────────────
    /// `$ref`. When set, every sibling keyword is an annotation only; Moso
    /// never emits constraint keywords alongside a `$ref`.
    #[serde(rename = "$ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,

    // ── core ─────────────────────────────────────────────────────────────
    /// `type`. Nullability is expressed as `["string", "null"]`, the 2020-12
    /// way, not OpenAPI 3.0's `nullable: true`.
    #[serde(rename = "type", default, skip_serializing_if = "TypeSet::is_empty")]
    pub types: TypeSet,
    /// `format` — `email`, `uri`, `uuid`, `date-time`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Cow<'static, str>>,
    /// `title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<Cow<'static, str>>,
    /// `description`. Sourced from the type's doc comment by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Cow<'static, str>>,
    /// `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// `examples`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<Value>,
    /// `deprecated`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deprecated: bool,
    /// `readOnly` — present in responses, rejected in requests.
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    /// `writeOnly` — accepted in requests, never present in responses. Every
    /// `#[schema(secret)]` field sets this.
    #[serde(default, skip_serializing_if = "is_false")]
    pub write_only: bool,
    /// `enum`.
    #[serde(rename = "enum", default, skip_serializing_if = "Vec::is_empty")]
    pub enumeration: Vec<Value>,
    /// `const`.
    #[serde(rename = "const", default, skip_serializing_if = "Option::is_none")]
    pub constant: Option<Value>,

    // ── string ───────────────────────────────────────────────────────────
    /// `minLength`, counted in Unicode code points as the specification
    /// requires — which is also what [`crate::checks::check_len_str`] counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u64>,
    /// `maxLength`, counted in Unicode code points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u64>,
    /// `pattern` — an ECMA-262 regular expression, unanchored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<Cow<'static, str>>,
    /// `contentEncoding`, e.g. `base64` for binary payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_encoding: Option<Cow<'static, str>>,
    /// `contentMediaType`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_media_type: Option<Cow<'static, str>>,

    // ── numeric ──────────────────────────────────────────────────────────
    /// `minimum` (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<Number>,
    /// `maximum` (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<Number>,
    /// `exclusiveMinimum` — a number in 2020-12, not a boolean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive_minimum: Option<Number>,
    /// `exclusiveMaximum` — a number in 2020-12, not a boolean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive_maximum: Option<Number>,
    /// `multipleOf`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple_of: Option<Number>,

    // ── array ────────────────────────────────────────────────────────────
    /// `items`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<SchemaNode>>,
    /// `prefixItems` — how tuples are described.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefix_items: Vec<SchemaNode>,
    /// `minItems`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_items: Option<u64>,
    /// `maxItems`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<u64>,
    /// `uniqueItems`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unique_items: bool,

    // ── object ───────────────────────────────────────────────────────────
    /// `properties`, in declaration order.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub properties: IndexMap<String, SchemaNode>,
    /// `required`, in declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
    /// `additionalProperties`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_properties: Option<AdditionalProperties>,
    /// `minProperties`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_properties: Option<u64>,
    /// `maxProperties`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_properties: Option<u64>,

    // ── composition ──────────────────────────────────────────────────────
    /// `oneOf` — externally/internally tagged enums.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub one_of: Vec<SchemaNode>,
    /// `anyOf` — also how a nullable `$ref` is expressed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any_of: Vec<SchemaNode>,
    /// `allOf` — `#[schema(flatten)]` composition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all_of: Vec<SchemaNode>,
    /// `not`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not: Option<Box<SchemaNode>>,
    /// OpenAPI `discriminator`; see [`Discriminator`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discriminator: Option<Discriminator>,

    // ── definitions & escape hatch ───────────────────────────────────────
    /// `$defs`. Normally empty because Moso hoists every named schema into
    /// `components/schemas`; populated when a schema is exported standalone.
    #[serde(rename = "$defs", default, skip_serializing_if = "IndexMap::is_empty")]
    pub defs: IndexMap<String, SchemaNode>,
    /// Every keyword Moso does not model, including `x-` vendor extensions.
    /// Round-trips verbatim.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

// `serde`'s `skip_serializing_if` hands the field by reference, so the signature
// is not ours to choose.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "the signature is dictated by serde's `skip_serializing_if`"
)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Raise a lower bound, never lower it. `None` leaves the slot untouched.
fn tighten_min(slot: &mut Option<u64>, candidate: Option<u64>) {
    if let Some(c) = candidate {
        *slot = Some(slot.map_or(c, |current| current.max(c)));
    }
}

/// Lower an upper bound, never raise it. `None` leaves the slot untouched.
fn tighten_max(slot: &mut Option<u64>, candidate: Option<u64>) {
    if let Some(c) = candidate {
        *slot = Some(slot.map_or(c, |current| current.min(c)));
    }
}

impl SchemaNode {
    /// The empty schema `{}` — accepts anything.
    #[must_use]
    pub fn any() -> Self {
        Self::default()
    }

    /// A schema asserting exactly one primitive type.
    #[must_use]
    pub fn of_type(ty: JsonType) -> Self {
        Self {
            types: TypeSet::of(ty),
            ..Self::default()
        }
    }

    /// `{"type": "null"}`.
    #[must_use]
    pub fn null() -> Self {
        Self::of_type(JsonType::Null)
    }

    /// `{"type": "boolean"}`.
    #[must_use]
    pub fn boolean() -> Self {
        Self::of_type(JsonType::Boolean)
    }

    /// `{"$ref": "…"}`.
    pub fn reference(target: impl Into<String>) -> Self {
        Self {
            reference: Some(target.into()),
            ..Self::default()
        }
    }

    /// `{"oneOf": [ … ]}`.
    #[must_use]
    pub fn one_of(variants: Vec<SchemaNode>) -> Self {
        Self {
            one_of: variants,
            ..Self::default()
        }
    }

    /// `{"anyOf": [ … ]}`.
    #[must_use]
    pub fn any_of(variants: Vec<SchemaNode>) -> Self {
        Self {
            any_of: variants,
            ..Self::default()
        }
    }

    /// `{"allOf": [ … ]}`.
    #[must_use]
    pub fn all_of(parts: Vec<SchemaNode>) -> Self {
        Self {
            all_of: parts,
            ..Self::default()
        }
    }

    /// `{"const": v}` — how unit enum variants and tag values are described.
    pub fn constant(value: impl Into<Value>) -> Self {
        Self {
            constant: Some(value.into()),
            ..Self::default()
        }
    }

    /// `{"enum": [ … ]}`.
    #[must_use]
    pub fn enumeration(values: Vec<Value>) -> Self {
        Self {
            enumeration: values,
            ..Self::default()
        }
    }

    /// True when this node is nothing but a `$ref`.
    #[must_use]
    pub fn is_reference(&self) -> bool {
        self.reference.is_some()
    }

    /// True when this node asserts nothing at all.
    #[must_use]
    pub fn is_any(&self) -> bool {
        *self == Self::default()
    }

    /// True when this node is a `$ref` and carries no other keyword, so it can
    /// be collapsed back into a [`SchemaRef::Ref`] without losing information.
    #[must_use]
    pub fn is_bare_reference(mut self) -> bool {
        if self.reference.take().is_none() {
            return false;
        }
        self.is_any()
    }

    /// Permit `null` in addition to whatever this node already allows.
    ///
    /// A plain typed node gains `"null"` in its `type`. A `$ref` cannot carry a
    /// sibling `type`, so it is rewrapped as
    /// `anyOf: [{$ref: …}, {type: "null"}]` — the only correct 2020-12
    /// spelling.
    ///
    /// Three cases are easy to get wrong and are handled here:
    ///
    /// * a node with **no** `type` keyword already admits `null`, and inserting
    ///   one would *narrow* it to `{"type": "null"}`, so it is left alone;
    /// * an `enum` is an assertion of its own — widening `type` is not enough,
    ///   `null` has to become a member;
    /// * a `const` or a composition (`$ref`, `oneOf`, `allOf`, `not`) still
    ///   rejects `null` however wide the `type` is, so it is wrapped.
    pub fn make_nullable(&mut self) {
        if self.types.is_nullable() {
            return;
        }

        let must_wrap = self.reference.is_some()
            || self.constant.is_some()
            || !self.one_of.is_empty()
            || !self.all_of.is_empty()
            || self.not.is_some();

        if must_wrap {
            let inner = std::mem::take(self);
            self.any_of = vec![inner, Self::null()];
            return;
        }

        // A node that is only an `anyOf` gains a branch rather than a wrapper —
        // which is also what makes this idempotent for a node *this* method
        // already wrapped once.
        if !self.any_of.is_empty() && self.types.is_empty() {
            if !self.any_of.contains(&Self::null()) {
                self.any_of.push(Self::null());
            }
            return;
        }

        if !self.enumeration.is_empty() && !self.enumeration.iter().any(Value::is_null) {
            self.enumeration.push(Value::Null);
        }

        if self.types.is_empty() {
            return;
        }
        self.types.insert(JsonType::Null);
    }

    /// Builder-style [`SchemaNode::make_nullable`].
    #[must_use]
    pub fn nullable(mut self) -> Self {
        self.make_nullable();
        self
    }

    /// Apply a length constraint to whichever keyword pair fits this node's
    /// type: `minLength`/`maxLength` for strings, `minItems`/`maxItems` for
    /// arrays, `minProperties`/`maxProperties` for objects.
    ///
    /// This is what lets `Length<T, MIN, MAX>` describe itself without knowing
    /// statically whether `T` is a string or a collection.
    ///
    /// Existing bounds are **tightened**, never replaced: `NonEmpty<Length<…>>`
    /// stacks two wrappers over one node and neither may erase the other's
    /// work. A `None` argument therefore leaves that side untouched.
    ///
    /// A node with no `type` keyword — a `$ref`, a composition, the empty
    /// schema — gets all three pairs, which is sound because JSON Schema
    /// evaluates a size assertion only against the instance type it applies to
    /// and ignores it otherwise. A number or a boolean has no length, so
    /// nothing is emitted for them.
    pub fn apply_len(&mut self, min: Option<u64>, max: Option<u64>) {
        // Constraint keywords must never sit beside a `$ref`; the reference
        // moves into an `allOf` so the bounds have somewhere legal to live.
        if self.reference.is_some() {
            let inner = std::mem::take(self);
            self.all_of.push(inner);
        }

        match self.types.primary() {
            Some(JsonType::String) => {
                tighten_min(&mut self.min_length, min);
                tighten_max(&mut self.max_length, max);
            }
            Some(JsonType::Array) => {
                tighten_min(&mut self.min_items, min);
                tighten_max(&mut self.max_items, max);
            }
            Some(JsonType::Object) => {
                tighten_min(&mut self.min_properties, min);
                tighten_max(&mut self.max_properties, max);
            }
            Some(JsonType::Number | JsonType::Integer | JsonType::Boolean | JsonType::Null) => {}
            None => {
                tighten_min(&mut self.min_length, min);
                tighten_max(&mut self.max_length, max);
                tighten_min(&mut self.min_items, min);
                tighten_max(&mut self.max_items, max);
                tighten_min(&mut self.min_properties, min);
                tighten_max(&mut self.max_properties, max);
            }
        }
    }

    /// Set an inclusive numeric range.
    pub fn apply_range(&mut self, min: Option<Number>, max: Option<Number>) {
        self.minimum = min;
        self.maximum = max;
    }

    /// Attach a vendor extension. Keys are conventionally `x-` prefixed.
    #[must_use]
    pub fn with_extension(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extensions.insert(key.into(), value.into());
        self
    }

    /// Set the description, builder-style.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<Cow<'static, str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the format, builder-style.
    #[must_use]
    pub fn with_format(mut self, format: impl Into<Cow<'static, str>>) -> Self {
        self.format = Some(format.into());
        self
    }

    /// Set the default value, builder-style.
    #[must_use]
    pub fn with_default(mut self, value: impl Into<Value>) -> Self {
        self.default = Some(value.into());
        self
    }

    /// Add one example, builder-style.
    #[must_use]
    pub fn with_example(mut self, value: impl Into<Value>) -> Self {
        self.examples.push(value.into());
        self
    }
}

/// A reference to a schema: either the schema itself, or a pointer to a named
/// one.
///
/// Returned by [`Schema::schema_ref`], which is the *cheap* path used when the
/// referenced schema is already known to be registered. It performs no
/// registration of its own — use [`SchemaGenerator::subschema_for`] when you
/// are building a document.
///
/// ```
/// use moso_schema::json_schema::{SchemaNode, SchemaRef};
///
/// // A named type is published once and referenced everywhere.
/// let named = SchemaRef::inline_or_named("UserOut".into());
/// assert!(named.is_ref());
/// assert_eq!(named.as_ref_str(), Some("#/components/schemas/UserOut"));
/// assert_eq!(named.ref_name(), Some("UserOut"));
///
/// // An anonymous one is written out in place.
/// let inline = SchemaRef::inline(SchemaNode::any());
/// assert!(!inline.is_ref());
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SchemaRef {
    /// The schema, written out in place. Used by types with no stable name of
    /// their own: `Vec<T>`, `Option<T>`, tuples, primitives.
    Inline(Box<SchemaNode>),
    /// A JSON Pointer URI such as `#/components/schemas/CreateUser`.
    Ref(String),
}

impl SchemaRef {
    /// A `$ref` to `name` under [`DEFAULT_REF_PREFIX`].
    ///
    /// An empty `name` means "this type is anonymous": such types must override
    /// [`Schema::schema_ref`] to return [`SchemaRef::Inline`], and the empty
    /// schema returned here is the deliberately loud fallback if they do not.
    #[must_use]
    pub fn inline_or_named(name: Cow<'static, str>) -> Self {
        if name.is_empty() {
            Self::Inline(Box::new(SchemaNode::any()))
        } else {
            Self::Ref(format!("{DEFAULT_REF_PREFIX}{name}"))
        }
    }

    /// A `$ref` to `name` under an explicit prefix.
    pub fn named_with_prefix(prefix: &str, name: &str) -> Self {
        Self::Ref(format!("{prefix}{name}"))
    }

    /// Wrap a node as an inline reference.
    #[must_use]
    pub fn inline(node: SchemaNode) -> Self {
        Self::Inline(Box::new(node))
    }

    /// True for [`SchemaRef::Ref`].
    #[must_use]
    pub fn is_ref(&self) -> bool {
        matches!(self, Self::Ref(_))
    }

    /// The full `$ref` URI, if this is a reference.
    #[must_use]
    pub fn as_ref_str(&self) -> Option<&str> {
        match self {
            Self::Ref(r) => Some(r),
            Self::Inline(_) => None,
        }
    }

    /// The component name a reference points at, i.e. everything after the
    /// final `/`.
    #[must_use]
    pub fn ref_name(&self) -> Option<&str> {
        self.as_ref_str().map(|r| r.rsplit('/').next().unwrap_or(r))
    }

    /// The inline node, if this is not a reference.
    #[must_use]
    pub fn as_node(&self) -> Option<&SchemaNode> {
        match self {
            Self::Inline(n) => Some(n),
            Self::Ref(_) => None,
        }
    }

    /// Collapse into a node: an inline schema unwraps, a reference becomes
    /// `{"$ref": …}`.
    #[must_use]
    pub fn into_node(self) -> SchemaNode {
        match self {
            Self::Inline(n) => *n,
            Self::Ref(r) => SchemaNode::reference(r),
        }
    }
}

impl From<SchemaRef> for SchemaNode {
    fn from(r: SchemaRef) -> Self {
        r.into_node()
    }
}

impl From<SchemaNode> for SchemaRef {
    fn from(n: SchemaNode) -> Self {
        // A node that is *only* a `$ref` collapses back to the cheap form;
        // anything with sibling keywords must stay inline or they are lost.
        match &n.reference {
            Some(r) if n.clone().is_bare_reference() => Self::Ref(r.clone()),
            _ => Self::Inline(Box::new(n)),
        }
    }
}

/// Two distinct Rust types claimed the same [`Schema::schema_name`].
///
/// Collected rather than panicked on, so `App::build()` can report every
/// collision at once and name both offenders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaCollision {
    /// The contested schema name.
    pub name: String,
    /// `std::any::type_name` of the type that registered first.
    pub first: &'static str,
    /// `std::any::type_name` of the type that collided with it.
    pub second: &'static str,
}

impl std::fmt::Display for SchemaCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "two types both claim the schema name `{}`: `{}` and `{}`",
            self.name, self.first, self.second
        )
    }
}

impl std::error::Error for SchemaCollision {}

/// Collects every named schema reachable from a set of roots, de-duplicating
/// by name and returning `$ref`s.
///
/// The generator is the *only* thing that knows the `$ref` prefix, so the same
/// `Schema` impls serve an OpenAPI `components/schemas` document and a
/// standalone `$defs` document without change.
///
/// ```
/// use moso::prelude::*;
/// use moso::schema::SchemaGenerator;
///
/// /// An address, referenced from more than one place.
/// #[derive(Schema)]
/// pub struct Address {
///     /// The postcode.
///     pub postcode: String,
/// }
///
/// /// A user, as the API returns one.
/// #[derive(Schema)]
/// pub struct UserOut {
///     /// Where they live.
///     pub address: Address,
/// }
///
/// # fn main() {
/// let mut generator = SchemaGenerator::default();
/// let node = generator.subschema_for::<UserOut>();
///
/// // A named type becomes a `$ref`, and is registered exactly once.
/// assert!(node.is_reference());
/// assert!(generator.contains("UserOut"));
/// assert!(generator.contains("Address"));
///
/// // The definitions are what `components/schemas` is built from.
/// generator.sort_definitions();
/// assert_eq!(
///     generator.definitions().keys().map(String::as_str).collect::<Vec<_>>(),
///     ["Address", "UserOut"],
/// );
/// # }
/// ```
///
/// Always reach nested types through [`SchemaGenerator::subschema_for`]: calling
/// `T::json_schema` directly leaves `T` unregistered and the document with a
/// dangling `$ref`.
#[derive(Debug)]
pub struct SchemaGenerator {
    ref_prefix: &'static str,
    definitions: IndexMap<String, SchemaNode>,
    /// Names reserved by an in-flight [`SchemaGenerator::define`] call. The
    /// recursion guard: a type that refers to itself sees its own reservation.
    in_progress: Vec<String>,
    /// Schema name → `std::any::type_name` of the type that claimed it. A
    /// second, *different* type claiming a name is a [`SchemaCollision`].
    owners: HashMap<String, &'static str>,
    collisions: Vec<SchemaCollision>,
}

impl Default for SchemaGenerator {
    fn default() -> Self {
        Self::new(DEFAULT_REF_PREFIX)
    }
}

impl SchemaGenerator {
    /// A generator emitting `$ref`s under `ref_prefix`.
    ///
    /// Use [`DEFAULT_REF_PREFIX`] for OpenAPI documents and `"#/$defs/"` for
    /// standalone JSON Schema.
    #[must_use]
    pub fn new(ref_prefix: &'static str) -> Self {
        Self {
            ref_prefix,
            definitions: IndexMap::new(),
            in_progress: Vec::new(),
            owners: HashMap::new(),
            collisions: Vec::new(),
        }
    }

    /// The prefix every `$ref` this generator produces is built from.
    #[must_use]
    pub fn ref_prefix(&self) -> &'static str {
        self.ref_prefix
    }

    /// The `$ref` URI for a schema name, whether or not it is registered.
    #[must_use]
    pub fn ref_for(&self, name: &str) -> String {
        format!("{}{}", self.ref_prefix, name)
    }

    /// Register `T` (and everything it references) and return a node
    /// describing it.
    ///
    /// Named types yield `{"$ref": …}`; anonymous ones (`Vec<T>`, `Option<T>`,
    /// primitives) are written out inline, with their named children still
    /// registered. This is the method every `json_schema` implementation should
    /// call for its fields.
    ///
    /// "Named" means [`Schema::schema_ref`] yields a [`SchemaRef::Ref`], which
    /// is exactly the contract anonymous types opt out of by overriding it —
    /// see the [`Schema`] trait docs.
    pub fn subschema_for<T: Schema>(&mut self) -> SchemaNode {
        if T::schema_ref().is_ref() {
            self.define::<T>().into_node()
        } else {
            // Anonymous: written out in place. Its *children* still come back
            // through here, so any named type it mentions is still registered.
            T::json_schema(self)
        }
    }

    /// Register `T` and return a reference to it, reserving its name before
    /// generating its body so recursive types terminate.
    ///
    /// Calling this for an anonymous type registers it under its
    /// [`Schema::schema_name`] anyway, which is occasionally what you want for
    /// a hand-tuned document; prefer [`SchemaGenerator::subschema_for`].
    ///
    /// A type whose `schema_name` is `""` has nothing to key a definition on
    /// and is written out inline instead.
    ///
    /// Definitions land in *completion* order, so a child appears before the
    /// parent that referenced it. Call [`SchemaGenerator::sort_definitions`]
    /// before serialising if you want alphabetical order instead; either way
    /// the result is a function of the type graph alone, never of hash
    /// iteration.
    pub fn define<T: Schema>(&mut self) -> SchemaRef {
        let name = T::schema_name();
        if name.is_empty() {
            return SchemaRef::inline(T::json_schema(self));
        }
        let name = name.into_owned();
        self.record_owner(&name, std::any::type_name::<T>());

        // Registered, or reserved by an enclosing `define` for this same type:
        // the second case is the recursion guard, and returning the `$ref`
        // here is what makes `Category { children: Vec<Category> }` terminate.
        if self.contains(&name) {
            return SchemaRef::Ref(self.ref_for(&name));
        }

        self.in_progress.push(name.clone());
        let node = T::json_schema(self);
        let reserved = self.in_progress.pop();
        debug_assert_eq!(
            reserved.as_deref(),
            Some(name.as_str()),
            "`define` reservations must nest"
        );

        self.definitions.insert(name.clone(), node);
        SchemaRef::Ref(self.ref_for(&name))
    }

    /// Record which Rust type owns a schema name, collecting a
    /// [`SchemaCollision`] when a second, different type claims the same one.
    fn record_owner(&mut self, name: &str, type_name: &'static str) {
        match self.owners.get(name) {
            Some(owner) if *owner == type_name => {}
            Some(owner) => {
                let collision = SchemaCollision {
                    name: name.to_owned(),
                    first: owner,
                    second: type_name,
                };
                if !self.collisions.contains(&collision) {
                    self.collisions.push(collision);
                }
            }
            None => {
                self.owners.insert(name.to_owned(), type_name);
            }
        }
    }

    /// Register a pre-built node under `name`, returning a reference to it.
    ///
    /// Re-registering the same name is a no-op; use
    /// [`SchemaGenerator::collisions`] to find out whether it was a *different*
    /// type that did so.
    pub fn insert(&mut self, name: impl Into<String>, node: SchemaNode) -> SchemaRef {
        let name = name.into();
        self.definitions.entry(name.clone()).or_insert(node);
        SchemaRef::Ref(self.ref_for(&name))
    }

    /// True when `name` is registered or reserved.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.definitions.contains_key(name) || self.in_progress.iter().any(|n| n == name)
    }

    /// Every registered schema, in registration order.
    #[must_use]
    pub fn definitions(&self) -> &IndexMap<String, SchemaNode> {
        &self.definitions
    }

    /// Sort the registered schemas by name.
    ///
    /// Called once before serialisation so the committed document is stable
    /// regardless of the order routes were registered in.
    pub fn sort_definitions(&mut self) {
        self.definitions.sort_unstable_keys();
    }

    /// Consume the generator, yielding its definitions.
    #[must_use]
    pub fn into_definitions(self) -> IndexMap<String, SchemaNode> {
        self.definitions
    }

    /// Take the definitions, leaving the generator empty but reusable.
    pub fn take_definitions(&mut self) -> IndexMap<String, SchemaNode> {
        std::mem::take(&mut self.definitions)
    }

    /// Every schema-name collision seen so far. `App::build()` turns a
    /// non-empty slice into a boot error naming both types and suggesting
    /// `#[schema(rename = "…")]`.
    #[must_use]
    pub fn collisions(&self) -> &[SchemaCollision] {
        &self.collisions
    }

    /// The schema name a generic type takes: `Page<UserOut>` → `Page_UserOut`.
    ///
    /// The associated-function spelling of [`generic_schema_name`], provided
    /// because generated code already has a `SchemaGenerator` in scope. The
    /// mangling is documented and stable — generated client type names depend
    /// on it — and is defined once, in [`generic_schema_name`].
    ///
    /// ```
    /// # use std::borrow::Cow;
    /// # use moso_schema::json_schema::SchemaGenerator;
    /// assert_eq!(
    ///     SchemaGenerator::name_for_generic("Page", &[Cow::Borrowed("UserOut")]),
    ///     "Page_UserOut",
    /// );
    /// ```
    #[must_use]
    pub fn name_for_generic(base: &str, arguments: &[Cow<'static, str>]) -> Cow<'static, str> {
        generic_schema_name(base, arguments)
    }

    // ── builder shortcuts ────────────────────────────────────────────────
    // Each takes `&self` so a builder can be constructed inside an argument
    // position while another borrow of the generator is live:
    //     g.object("User").property("id", g.subschema_for::<Uuid>(), true)

    /// Start an object schema. `name` is bookkeeping only and is not emitted;
    /// call [`ObjectBuilder::title`] if you want a `title` in the output.
    #[must_use]
    pub fn object(&self, name: impl Into<Cow<'static, str>>) -> ObjectBuilder {
        ObjectBuilder::named(name)
    }

    /// Start a string schema.
    #[must_use]
    pub fn string(&self) -> StringBuilder {
        StringBuilder::new()
    }

    /// Start a `number` schema.
    #[must_use]
    pub fn number(&self) -> NumberBuilder {
        NumberBuilder::number()
    }

    /// Start an `integer` schema.
    #[must_use]
    pub fn integer(&self) -> NumberBuilder {
        NumberBuilder::integer()
    }

    /// Start an array schema.
    #[must_use]
    pub fn array(&self) -> ArrayBuilder {
        ArrayBuilder::new()
    }

    /// Start an array schema with `items` already set.
    #[must_use]
    pub fn array_of(&self, items: impl Into<SchemaNode>) -> ArrayBuilder {
        ArrayBuilder::new().items(items)
    }

    /// `{"oneOf": [ … ]}` — how an externally tagged enum is described.
    #[must_use]
    pub fn one_of(&self, variants: impl IntoIterator<Item = SchemaNode>) -> SchemaNode {
        SchemaNode::one_of(variants.into_iter().collect())
    }

    /// `{"anyOf": [ … ]}` — how an untagged enum is described.
    #[must_use]
    pub fn any_of(&self, variants: impl IntoIterator<Item = SchemaNode>) -> SchemaNode {
        SchemaNode::any_of(variants.into_iter().collect())
    }

    /// `{"const": v}` — how a unit enum variant and an internal tag value are
    /// described.
    #[must_use]
    pub fn const_value(&self, value: impl Into<Value>) -> SchemaNode {
        SchemaNode::constant(value)
    }

    /// `{"enum": [ … ]}`.
    ///
    /// The node carries no `type`: the members already pin the instance down,
    /// and asserting a type as well is redundant in every case and wrong for a
    /// mixed-type enumeration.
    #[must_use]
    pub fn enum_values<V: Into<Value>>(&self, values: impl IntoIterator<Item = V>) -> SchemaNode {
        SchemaNode::enumeration(values.into_iter().map(Into::into).collect())
    }

    /// `{"type": "boolean"}`.
    #[must_use]
    pub fn boolean(&self) -> SchemaNode {
        SchemaNode::boolean()
    }

    /// `{"type": "null"}`.
    #[must_use]
    pub fn null(&self) -> SchemaNode {
        SchemaNode::null()
    }

    /// `{}` — accepts anything.
    #[must_use]
    pub fn any(&self) -> SchemaNode {
        SchemaNode::any()
    }
}

/// Conversion into a JSON number, used by the numeric builders.
///
/// Non-finite floats convert to `None`, which omits the keyword: an infinite
/// bound is not a bound, and emitting `null` would produce an invalid schema.
///
/// Implemented for every integer and float type, so
/// [`NumberBuilder::minimum`](crate::json_schema::NumberBuilder::minimum) and
/// its siblings accept whichever one the caller happens to have.
///
/// ```
/// use moso_schema::json_schema::{IntoNumber, SchemaGenerator};
///
/// let generator = SchemaGenerator::default();
/// let node = generator.integer().minimum(1_u8).maximum(100_i64).build();
///
/// assert_eq!(node.minimum, 1_u8.into_number());
/// assert_eq!(node.maximum, 100_i64.into_number());
///
/// // An infinite bound is not a bound, so the keyword is simply absent.
/// assert_eq!(f64::INFINITY.into_number(), None);
/// assert_eq!(generator.number().minimum(f64::NAN).build().minimum, None);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be a JSON Schema number",
    label = "not a number",
    note = "a numeric bound has to be an integer (`i8`–`i64`, `u8`–`u64`, `isize`, `usize`), an \
            `f32`, an `f64`, or a `serde_json::Number`",
    note = "help: write the bound as a number, not a string: `#[schema(range = 1..=100)]`",
    note = "help: a length or item-count bound belongs on `len` or `items`, not on `range` — \
            `#[schema(len = 3..=32)]` for a `String`"
)]
pub trait IntoNumber {
    /// Convert, or `None` if the value has no JSON representation.
    fn into_number(self) -> Option<Number>;
}

macro_rules! into_number_int {
    ($($t:ty),* $(,)?) => {$(
        impl IntoNumber for $t {
            fn into_number(self) -> Option<Number> {
                Some(Number::from(self))
            }
        }
    )*};
}

into_number_int!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

impl IntoNumber for f32 {
    fn into_number(self) -> Option<Number> {
        Number::from_f64(f64::from(self))
    }
}

impl IntoNumber for f64 {
    fn into_number(self) -> Option<Number> {
        Number::from_f64(self)
    }
}

impl IntoNumber for Number {
    fn into_number(self) -> Option<Number> {
        Some(self)
    }
}

impl<T: IntoNumber> IntoNumber for Option<T> {
    fn into_number(self) -> Option<Number> {
        self.and_then(IntoNumber::into_number)
    }
}

/// Generates the annotation keywords shared by every builder.
macro_rules! annotations {
    ($builder:ident) => {
        impl $builder {
            /// Set `title`.
            #[must_use]
            pub fn title(mut self, title: impl Into<Cow<'static, str>>) -> Self {
                self.node.title = Some(title.into());
                self
            }

            /// Set `description`.
            #[must_use]
            pub fn description(mut self, description: impl Into<Cow<'static, str>>) -> Self {
                self.node.description = Some(description.into());
                self
            }

            /// Set `description` from an optional value.
            ///
            /// The form generated code uses, since a doc comment may be absent.
            #[must_use]
            pub fn description_opt(mut self, description: Option<Cow<'static, str>>) -> Self {
                self.node.description = description;
                self
            }

            /// Set `format`.
            #[must_use]
            pub fn format(mut self, format: impl Into<Cow<'static, str>>) -> Self {
                self.node.format = Some(format.into());
                self
            }

            /// Set `default`.
            #[must_use]
            pub fn default_value(mut self, value: impl Into<Value>) -> Self {
                self.node.default = Some(value.into());
                self
            }

            /// Append to `examples`.
            #[must_use]
            pub fn example(mut self, value: impl Into<Value>) -> Self {
                self.node.examples.push(value.into());
                self
            }

            /// Set `enum`.
            #[must_use]
            pub fn enumeration(mut self, values: Vec<Value>) -> Self {
                self.node.enumeration = values;
                self
            }

            /// Set `const`.
            #[must_use]
            pub fn constant(mut self, value: impl Into<Value>) -> Self {
                self.node.constant = Some(value.into());
                self
            }

            /// Set `deprecated`.
            #[must_use]
            pub fn deprecated(mut self, yes: bool) -> Self {
                self.node.deprecated = yes;
                self
            }

            /// Set `readOnly`.
            #[must_use]
            pub fn read_only(mut self, yes: bool) -> Self {
                self.node.read_only = yes;
                self
            }

            /// Set `writeOnly`.
            #[must_use]
            pub fn write_only(mut self, yes: bool) -> Self {
                self.node.write_only = yes;
                self
            }

            /// Attach a vendor extension.
            #[must_use]
            pub fn extension(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
                self.node.extensions.insert(key.into(), value.into());
                self
            }

            /// Also permit `null`.
            #[must_use]
            pub fn nullable(mut self) -> Self {
                self.node.make_nullable();
                self
            }

            /// Finish, returning the node.
            #[must_use]
            pub fn build(self) -> SchemaNode {
                self.node
            }
        }

        impl From<$builder> for SchemaNode {
            fn from(b: $builder) -> SchemaNode {
                b.build()
            }
        }
    };
}

/// Builds `{"type": "object", …}`.
#[derive(Clone, Debug)]
pub struct ObjectBuilder {
    /// Bookkeeping name; never serialised.
    name: Option<Cow<'static, str>>,
    node: SchemaNode,
}

impl ObjectBuilder {
    /// An anonymous object schema.
    #[must_use]
    pub fn new() -> Self {
        Self {
            name: None,
            node: SchemaNode::of_type(JsonType::Object),
        }
    }

    /// An object schema tagged with the schema name it will be registered
    /// under. The name is not emitted.
    #[must_use]
    pub fn named(name: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: Some(name.into()),
            ..Self::new()
        }
    }

    /// The bookkeeping name, if any.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Add a property, recording it in `required` when `required` is true.
    #[must_use]
    pub fn property(
        mut self,
        name: impl Into<String>,
        schema: impl Into<SchemaNode>,
        required: bool,
    ) -> Self {
        let name = name.into();
        if required {
            self.node.required.push(name.clone());
        }
        self.node.properties.insert(name, schema.into());
        self
    }

    /// Set `additionalProperties` to `true` or `false`.
    #[must_use]
    pub fn additional_properties(mut self, allowed: bool) -> Self {
        self.node.additional_properties = Some(AdditionalProperties::Any(allowed));
        self
    }

    /// Constrain unknown properties to a schema — the map case.
    #[must_use]
    pub fn additional_properties_schema(mut self, schema: impl Into<SchemaNode>) -> Self {
        self.node.additional_properties =
            Some(AdditionalProperties::Schema(Box::new(schema.into())));
        self
    }

    /// Set `minProperties`.
    #[must_use]
    pub fn min_properties(mut self, n: u64) -> Self {
        self.node.min_properties = Some(n);
        self
    }

    /// Set `maxProperties`.
    #[must_use]
    pub fn max_properties(mut self, n: u64) -> Self {
        self.node.max_properties = Some(n);
        self
    }

    /// Compose with another schema via `allOf` — how `#[schema(flatten)]` is
    /// described.
    #[must_use]
    pub fn all_of(mut self, schema: impl Into<SchemaNode>) -> Self {
        self.node.all_of.push(schema.into());
        self
    }

    /// Set the OpenAPI `discriminator`.
    #[must_use]
    pub fn discriminator(mut self, d: Discriminator) -> Self {
        self.node.discriminator = Some(d);
        self
    }
}

impl Default for ObjectBuilder {
    fn default() -> Self {
        Self::new()
    }
}

annotations!(ObjectBuilder);

/// Builds `{"type": "string", …}`.
#[derive(Clone, Debug)]
pub struct StringBuilder {
    node: SchemaNode,
}

impl StringBuilder {
    /// A bare string schema.
    #[must_use]
    pub fn new() -> Self {
        Self {
            node: SchemaNode::of_type(JsonType::String),
        }
    }

    /// Set `minLength` (Unicode code points).
    #[must_use]
    pub fn min_length(mut self, n: u64) -> Self {
        self.node.min_length = Some(n);
        self
    }

    /// Set `maxLength` (Unicode code points).
    #[must_use]
    pub fn max_length(mut self, n: u64) -> Self {
        self.node.max_length = Some(n);
        self
    }

    /// Set `pattern`.
    #[must_use]
    pub fn pattern(mut self, pattern: impl Into<Cow<'static, str>>) -> Self {
        self.node.pattern = Some(pattern.into());
        self
    }

    /// Set `contentEncoding`.
    #[must_use]
    pub fn content_encoding(mut self, encoding: impl Into<Cow<'static, str>>) -> Self {
        self.node.content_encoding = Some(encoding.into());
        self
    }

    /// Set `contentMediaType`.
    #[must_use]
    pub fn content_media_type(mut self, media_type: impl Into<Cow<'static, str>>) -> Self {
        self.node.content_media_type = Some(media_type.into());
        self
    }
}

impl Default for StringBuilder {
    fn default() -> Self {
        Self::new()
    }
}

annotations!(StringBuilder);

/// Builds `{"type": "number"}` or `{"type": "integer"}`.
#[derive(Clone, Debug)]
pub struct NumberBuilder {
    node: SchemaNode,
}

impl NumberBuilder {
    /// `{"type": "number"}`.
    #[must_use]
    pub fn number() -> Self {
        Self {
            node: SchemaNode::of_type(JsonType::Number),
        }
    }

    /// `{"type": "integer"}`.
    #[must_use]
    pub fn integer() -> Self {
        Self {
            node: SchemaNode::of_type(JsonType::Integer),
        }
    }

    /// Set `minimum` (inclusive). A non-finite bound is dropped.
    #[must_use]
    pub fn minimum(mut self, n: impl IntoNumber) -> Self {
        self.node.minimum = n.into_number();
        self
    }

    /// Set `maximum` (inclusive). A non-finite bound is dropped.
    #[must_use]
    pub fn maximum(mut self, n: impl IntoNumber) -> Self {
        self.node.maximum = n.into_number();
        self
    }

    /// Set `exclusiveMinimum`.
    #[must_use]
    pub fn exclusive_minimum(mut self, n: impl IntoNumber) -> Self {
        self.node.exclusive_minimum = n.into_number();
        self
    }

    /// Set `exclusiveMaximum`.
    #[must_use]
    pub fn exclusive_maximum(mut self, n: impl IntoNumber) -> Self {
        self.node.exclusive_maximum = n.into_number();
        self
    }

    /// Set `multipleOf`.
    #[must_use]
    pub fn multiple_of(mut self, n: impl IntoNumber) -> Self {
        self.node.multiple_of = n.into_number();
        self
    }

    /// Set `not`.
    ///
    /// Present on the numeric builder specifically because it is the only
    /// exact way to say "any integer *except* this one" — which is what the
    /// `NonZero*` types need and what `minimum`/`maximum` cannot express.
    #[must_use]
    pub fn not(mut self, schema: impl Into<SchemaNode>) -> Self {
        self.node.not = Some(Box::new(schema.into()));
        self
    }
}

annotations!(NumberBuilder);

/// Builds `{"type": "array", …}`.
#[derive(Clone, Debug)]
pub struct ArrayBuilder {
    node: SchemaNode,
}

impl ArrayBuilder {
    /// A bare array schema.
    #[must_use]
    pub fn new() -> Self {
        Self {
            node: SchemaNode::of_type(JsonType::Array),
        }
    }

    /// Set `items`.
    #[must_use]
    pub fn items(mut self, schema: impl Into<SchemaNode>) -> Self {
        self.node.items = Some(Box::new(schema.into()));
        self
    }

    /// Append to `prefixItems` — how tuples are described.
    #[must_use]
    pub fn prefix_item(mut self, schema: impl Into<SchemaNode>) -> Self {
        self.node.prefix_items.push(schema.into());
        self
    }

    /// Set `minItems`.
    #[must_use]
    pub fn min_items(mut self, n: u64) -> Self {
        self.node.min_items = Some(n);
        self
    }

    /// Set `maxItems`.
    #[must_use]
    pub fn max_items(mut self, n: u64) -> Self {
        self.node.max_items = Some(n);
        self
    }

    /// Set `uniqueItems`.
    #[must_use]
    pub fn unique_items(mut self, yes: bool) -> Self {
        self.node.unique_items = yes;
        self
    }
}

impl Default for ArrayBuilder {
    fn default() -> Self {
        Self::new()
    }
}

annotations!(ArrayBuilder);

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::*;
    use crate::validate::{Validate, ValidationCtx, ValidationErrors};

    /// A directly recursive type: the canonical generator stress test.
    #[derive(Serialize, Deserialize)]
    struct Category {
        name: String,
        children: Vec<Category>,
    }

    /// Mutual recursion, which a naive "am I already generating *this* type"
    /// guard passes and a name-reservation guard is needed for.
    #[derive(Serialize, Deserialize)]
    struct Node {
        edges: Vec<Edge>,
    }

    #[derive(Serialize, Deserialize)]
    struct Edge {
        to: Box<Node>,
    }

    /// Two distinct types deliberately claiming one schema name.
    #[derive(Serialize, Deserialize)]
    struct UserV1 {
        id: u32,
    }

    #[derive(Serialize, Deserialize)]
    struct UserV2 {
        id: u64,
    }

    macro_rules! trivial_validate_for_test {
        ($($t:ty),* $(,)?) => {$(
            impl Validate for $t {
                fn validate(&self, _ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
                    Ok(())
                }
            }
        )*};
    }

    trivial_validate_for_test!(Category, Node, Edge, UserV1, UserV2);

    impl Schema for Category {
        fn schema_name() -> Cow<'static, str> {
            Cow::Borrowed("Category")
        }

        fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
            let name = generator.subschema_for::<String>();
            let children = generator.subschema_for::<Vec<Category>>();
            generator
                .object("Category")
                .property("name", name, true)
                .property("children", children, true)
                .build()
        }
    }

    impl Schema for Node {
        fn schema_name() -> Cow<'static, str> {
            Cow::Borrowed("Node")
        }

        fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
            let edges = generator.subschema_for::<Vec<Edge>>();
            generator
                .object("Node")
                .property("edges", edges, true)
                .build()
        }
    }

    impl Schema for Edge {
        fn schema_name() -> Cow<'static, str> {
            Cow::Borrowed("Edge")
        }

        fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
            let to = generator.subschema_for::<Box<Node>>();
            generator.object("Edge").property("to", to, true).build()
        }
    }

    impl Schema for UserV1 {
        fn schema_name() -> Cow<'static, str> {
            Cow::Borrowed("User")
        }

        fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
            let id = generator.subschema_for::<u32>();
            generator.object("User").property("id", id, true).build()
        }
    }

    impl Schema for UserV2 {
        fn schema_name() -> Cow<'static, str> {
            Cow::Borrowed("User")
        }

        fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
            let id = generator.subschema_for::<u64>();
            generator.object("User").property("id", id, true).build()
        }
    }

    /// Assert the structural rules Moso relies on across a whole document:
    /// every `$ref` resolves, and no `$ref` carries a constraint keyword
    /// beside it.
    fn assert_document_is_well_formed(generator: &SchemaGenerator) {
        fn walk(node: &SchemaNode, prefix: &str, known: &[&String], path: &str) {
            if let Some(reference) = &node.reference {
                let name = reference
                    .strip_prefix(prefix)
                    .unwrap_or_else(|| panic!("{path}: `{reference}` is not under `{prefix}`"));
                assert!(
                    known.iter().any(|k| k.as_str() == name),
                    "{path}: dangling $ref to `{name}`"
                );
                let mut bare = node.clone();
                bare.reference = None;
                bare.description = None;
                assert!(
                    bare.is_any(),
                    "{path}: `$ref` must not carry sibling keywords"
                );
            }
            for (key, child) in &node.properties {
                walk(child, prefix, known, &format!("{path}/properties/{key}"));
            }
            if let Some(items) = &node.items {
                walk(items, prefix, known, &format!("{path}/items"));
            }
            for (i, child) in node.prefix_items.iter().enumerate() {
                walk(child, prefix, known, &format!("{path}/prefixItems/{i}"));
            }
            for (name, list) in [
                ("oneOf", &node.one_of),
                ("anyOf", &node.any_of),
                ("allOf", &node.all_of),
            ] {
                for (i, child) in list.iter().enumerate() {
                    walk(child, prefix, known, &format!("{path}/{name}/{i}"));
                }
            }
            if let Some(AdditionalProperties::Schema(child)) = &node.additional_properties {
                walk(
                    child,
                    prefix,
                    known,
                    &format!("{path}/additionalProperties"),
                );
            }
            if let Some(child) = &node.not {
                walk(child, prefix, known, &format!("{path}/not"));
            }
        }

        let known: Vec<&String> = generator.definitions().keys().collect();
        for (name, node) in generator.definitions() {
            walk(node, generator.ref_prefix(), &known, name);
            // Every definition must survive a serialise/deserialise cycle, which
            // is the cheapest available proof that the emitted keywords are the
            // ones JSON Schema actually defines.
            let json = serde_json::to_string(node).expect("definition serialises");
            let back: SchemaNode = serde_json::from_str(&json).expect("definition parses");
            assert_eq!(&back, node, "{name} does not round-trip");
        }
    }

    #[test]
    fn recursive_types_terminate_with_a_ref() {
        let mut g = SchemaGenerator::default();
        let root = g.subschema_for::<Category>();

        assert_eq!(
            root.reference.as_deref(),
            Some("#/components/schemas/Category")
        );
        assert_eq!(g.definitions().len(), 1);

        let category = &g.definitions()["Category"];
        let children = &category.properties["children"];
        let items = children.items.as_deref().expect("array items");
        assert_eq!(
            items.reference.as_deref(),
            Some("#/components/schemas/Category"),
            "the inner occurrence must be a $ref, not an expansion"
        );
        assert_document_is_well_formed(&g);
    }

    #[test]
    fn mutually_recursive_types_terminate() {
        let mut g = SchemaGenerator::default();
        let _ = g.subschema_for::<Node>();

        assert!(g.contains("Node"));
        assert!(g.contains("Edge"));
        assert_eq!(g.definitions().len(), 2);
        // Completion order: `Edge` finishes before the `Node` that opened it.
        assert_eq!(
            g.definitions()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["Edge", "Node"]
        );
        assert_document_is_well_formed(&g);
    }

    #[test]
    fn definitions_are_generated_once_and_reused() {
        let mut g = SchemaGenerator::default();
        let a = g.subschema_for::<Category>();
        let b = g.subschema_for::<Category>();
        assert_eq!(a, b);
        assert_eq!(g.definitions().len(), 1);
        assert!(g.collisions().is_empty());
    }

    #[test]
    fn colliding_names_are_collected_not_panicked_on() {
        let mut g = SchemaGenerator::default();
        let _ = g.subschema_for::<UserV1>();
        let _ = g.subschema_for::<UserV2>();

        assert_eq!(g.collisions().len(), 1);
        let c = &g.collisions()[0];
        assert_eq!(c.name, "User");
        assert!(c.first.ends_with("UserV1"), "got {}", c.first);
        assert!(c.second.ends_with("UserV2"), "got {}", c.second);
        // The first type keeps the slot; the document stays well-formed.
        assert_eq!(g.definitions().len(), 1);
        assert_eq!(
            g.definitions()["User"].properties["id"].maximum,
            Some(Number::from(u32::MAX))
        );
        // A repeat does not duplicate the report.
        let _ = g.subschema_for::<UserV2>();
        assert_eq!(g.collisions().len(), 1);
    }

    #[test]
    fn ref_prefix_is_configurable() {
        let mut g = SchemaGenerator::new("#/$defs/");
        let root = g.subschema_for::<Category>();
        assert_eq!(root.reference.as_deref(), Some("#/$defs/Category"));
        assert_document_is_well_formed(&g);
    }

    #[test]
    fn anonymous_types_are_inlined_and_still_register_children() {
        let mut g = SchemaGenerator::default();
        let node = g.subschema_for::<Vec<Category>>();
        assert!(node.reference.is_none(), "Vec has no component identity");
        assert_eq!(node.types.primary(), Some(JsonType::Array));
        assert!(
            g.contains("Category"),
            "the element type is still registered"
        );
    }

    #[test]
    fn definitions_snapshot_is_stable() {
        let mut g = SchemaGenerator::default();
        let _ = g.subschema_for::<Category>();
        g.sort_definitions();

        assert_eq!(
            serde_json::to_value(g.definitions()).unwrap(),
            json!({
                "Category": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "children": {
                            "type": "array",
                            "items": { "$ref": "#/components/schemas/Category" }
                        }
                    },
                    "required": ["name", "children"]
                }
            })
        );
    }

    #[test]
    fn sorting_definitions_is_alphabetical() {
        let mut g = SchemaGenerator::default();
        let _ = g.subschema_for::<Node>();
        let _ = g.subschema_for::<Category>();
        g.sort_definitions();
        assert_eq!(
            g.definitions()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["Category", "Edge", "Node"]
        );
    }

    #[test]
    fn nullable_widens_the_type_keyword() {
        let node = StringBuilder::new().build().nullable();
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            json!({ "type": ["string", "null"] }),
            "2020-12 spells nullability in `type`, never as `nullable: true`"
        );
        // Idempotent.
        assert_eq!(node.clone().nullable(), node);
    }

    #[test]
    fn nullable_ref_becomes_any_of() {
        let node = SchemaNode::reference("#/components/schemas/User").nullable();
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            json!({
                "anyOf": [
                    { "$ref": "#/components/schemas/User" },
                    { "type": "null" }
                ]
            })
        );
        // `Option<Option<T>>` collapses on the wire, so it must collapse here.
        assert_eq!(node.clone().nullable(), node);
    }

    #[test]
    fn nullable_enum_gains_a_null_member() {
        let node = StringBuilder::new()
            .enumeration(vec![json!("draft"), json!("live")])
            .build()
            .nullable();
        assert_eq!(node.types, TypeSet::nullable(JsonType::String));
        assert_eq!(
            node.enumeration,
            vec![json!("draft"), json!("live"), Value::Null]
        );
    }

    #[test]
    fn nullable_const_and_untyped_nodes() {
        // `const` still rejects null however wide `type` is, so it is wrapped.
        let node = SchemaNode::constant(1).nullable();
        assert_eq!(node.any_of.len(), 2);
        assert_eq!(node.any_of[1], SchemaNode::null());

        // The empty schema already admits null; adding `type` would narrow it.
        assert_eq!(SchemaNode::any().nullable(), SchemaNode::any());

        // An `anyOf` gains a branch rather than another layer of wrapping.
        let node = SchemaNode::any_of(vec![SchemaNode::boolean()]).nullable();
        assert_eq!(node.any_of.len(), 2);
    }

    #[test]
    fn apply_len_dispatches_on_the_instance_type() {
        let mut s = StringBuilder::new().build();
        s.apply_len(Some(3), Some(32));
        assert_eq!((s.min_length, s.max_length), (Some(3), Some(32)));
        assert_eq!((s.min_items, s.max_items), (None, None));

        let mut a = ArrayBuilder::new().build();
        a.apply_len(Some(1), None);
        assert_eq!((a.min_items, a.max_items), (Some(1), None));

        let mut o = ObjectBuilder::new().build();
        o.apply_len(None, Some(4));
        assert_eq!((o.min_properties, o.max_properties), (None, Some(4)));

        // A number has no length: nothing is emitted rather than something wrong.
        let mut n = NumberBuilder::integer().build();
        n.apply_len(Some(1), Some(2));
        assert_eq!(
            (n.min_length, n.min_items, n.min_properties),
            (None, None, None)
        );
    }

    #[test]
    fn apply_len_tightens_and_never_widens() {
        let mut s = StringBuilder::new().min_length(3).max_length(32).build();
        s.apply_len(Some(1), None);
        assert_eq!(
            (s.min_length, s.max_length),
            (Some(3), Some(32)),
            "a looser bound must not erase a tighter one"
        );
        s.apply_len(Some(8), Some(16));
        assert_eq!((s.min_length, s.max_length), (Some(8), Some(16)));
    }

    #[test]
    fn apply_len_on_a_ref_moves_it_into_all_of() {
        let mut node = SchemaNode::reference("#/components/schemas/Tags");
        node.apply_len(Some(1), Some(10));
        assert!(node.reference.is_none(), "no constraints beside a $ref");
        assert_eq!(node.all_of.len(), 1);
        assert_eq!(
            node.all_of[0].reference.as_deref(),
            Some("#/components/schemas/Tags")
        );
        // The instance type is unknown, so every size pair is emitted; JSON
        // Schema ignores the ones that do not apply.
        assert_eq!((node.min_length, node.max_length), (Some(1), Some(10)));
        assert_eq!((node.min_items, node.max_items), (Some(1), Some(10)));
    }

    #[test]
    fn generator_shortcuts_build_the_expected_nodes() {
        let g = SchemaGenerator::default();
        assert_eq!(
            serde_json::to_value(g.enum_values(["a", "b"])).unwrap(),
            json!({ "enum": ["a", "b"] })
        );
        assert_eq!(
            serde_json::to_value(g.const_value("created")).unwrap(),
            json!({ "const": "created" })
        );
        assert_eq!(
            serde_json::to_value(g.one_of([g.boolean(), g.null()])).unwrap(),
            json!({ "oneOf": [{ "type": "boolean" }, { "type": "null" }] })
        );
        assert_eq!(
            serde_json::to_value(g.array_of(g.boolean()).min_items(1).build()).unwrap(),
            json!({ "type": "array", "items": { "type": "boolean" }, "minItems": 1 })
        );
    }

    #[test]
    fn insert_registers_a_prebuilt_node_once() {
        let mut g = SchemaGenerator::default();
        let first = g.insert("Widget", SchemaNode::boolean());
        let second = g.insert("Widget", StringBuilder::new().build());
        assert_eq!(first, second);
        assert_eq!(g.definitions()["Widget"], SchemaNode::boolean());
        assert!(g.contains("Widget"));
    }

    #[test]
    fn type_set_serialises_singly_and_as_array() {
        let one = serde_json::to_string(&TypeSet::of(JsonType::String)).unwrap();
        assert_eq!(one, "\"string\"");
        let two = serde_json::to_string(&TypeSet::nullable(JsonType::String)).unwrap();
        assert_eq!(two, "[\"string\",\"null\"]");
    }

    #[test]
    fn type_set_round_trips() {
        for s in [
            TypeSet::of(JsonType::Integer),
            TypeSet::nullable(JsonType::Array),
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: TypeSet = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn empty_node_serialises_to_empty_object() {
        assert_eq!(serde_json::to_string(&SchemaNode::any()).unwrap(), "{}");
    }

    #[test]
    fn unknown_keywords_round_trip_as_extensions() {
        let json = r#"{"type":"string","x-moso-secret":true}"#;
        let node: SchemaNode = serde_json::from_str(json).unwrap();
        assert_eq!(node.types, TypeSet::of(JsonType::String));
        assert_eq!(
            node.extensions.get("x-moso-secret"),
            Some(&Value::Bool(true))
        );
        assert_eq!(serde_json::to_string(&node).unwrap(), json);
    }

    #[test]
    fn schema_ref_names_are_extracted() {
        let r = SchemaRef::inline_or_named(Cow::Borrowed("CreateUser"));
        assert_eq!(r.as_ref_str(), Some("#/components/schemas/CreateUser"));
        assert_eq!(r.ref_name(), Some("CreateUser"));
        assert!(
            SchemaRef::inline_or_named(Cow::Borrowed(""))
                .as_node()
                .is_some()
        );
    }

    #[test]
    fn generator_ref_prefix_is_honoured() {
        let g = SchemaGenerator::new("#/$defs/");
        assert_eq!(g.ref_for("Category"), "#/$defs/Category");
        assert_eq!(SchemaGenerator::default().ref_prefix(), DEFAULT_REF_PREFIX);
    }

    #[test]
    fn non_finite_bounds_are_dropped() {
        let n = NumberBuilder::number().minimum(f64::NEG_INFINITY).build();
        assert!(n.minimum.is_none());
        let n = NumberBuilder::number().minimum(1.5_f64).build();
        assert_eq!(n.minimum, Number::from_f64(1.5));
    }

    #[test]
    fn object_builder_tracks_required() {
        let node = ObjectBuilder::named("User")
            .property("id", StringBuilder::new().format("uuid"), true)
            .property("nickname", StringBuilder::new(), false)
            .build();
        assert_eq!(node.required, vec!["id".to_string()]);
        assert_eq!(node.properties.len(), 2);
    }
}
