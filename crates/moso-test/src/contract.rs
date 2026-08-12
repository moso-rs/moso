//! Contract assertions: does the response match the schema the document promises?
//!
//! This is the feature that turns a test suite into a contract suite. A handler
//! that quietly starts returning `created_at` as a Unix integer instead of an
//! RFC 3339 string breaks every client and no test — unless the test validates
//! the body against the schema the OpenAPI document publishes for that
//! operation. [`TestResponse::assert_matches_openapi`](crate::TestResponse::assert_matches_openapi)
//! does exactly that, and this module is the validator behind it.
//!
//! # Strictness
//!
//! JSON Schema says an object with `properties` and no `additionalProperties`
//! accepts *any* extra member. That is the right default for an input schema and
//! precisely the wrong one for a contract test: the drift a contract test exists
//! to catch is "the handler returns a field the document does not mention".
//!
//! So [`Options::strict`] — on by default — treats an undocumented property as a
//! violation whenever the schema declares `properties` at all. Turn it off with
//! [`Options::lax`] for literal JSON Schema semantics.
//!
//! # What is not checked
//!
//! `format` is an annotation, not a constraint, and JSON Schema says so. A
//! validator that rejected `"format": "email"` on a string it did not like would
//! fail tests for a reason the document does not actually assert. Formats are
//! checked where they *are* a constraint — on the way in, by
//! `moso_schema`'s `Validate` — and ignored here.

use std::fmt::Write as _;

use moso::openapi::Document;
use moso::schema::json_schema::{AdditionalProperties, JsonType, SchemaNode};
use moso::schema::regex::Regex;
use serde_json::Value;

/// How deep a chain of `$ref`s and nested schemas may go before the validator
/// gives up. Far beyond any real document; a guard against a cyclic `$ref`.
const MAX_DEPTH: usize = 128;

/// One way the body disagrees with its schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// RFC 6901 JSON Pointer into the response body.
    pub pointer: String,
    /// What is wrong there, in one sentence.
    pub message: String,
}

impl Violation {
    fn new(pointer: &str, message: impl Into<String>) -> Self {
        Self {
            pointer: if pointer.is_empty() {
                "(root)".to_owned()
            } else {
                pointer.to_owned()
            },
            message: message.into(),
        }
    }
}

/// How strictly to read the document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Treat a property the schema does not declare as a violation, even when
    /// `additionalProperties` is absent. On by default; see the module header.
    pub strict: bool,
}

impl Options {
    /// The contract-test reading: an undocumented field is drift.
    #[must_use]
    pub const fn strict() -> Self {
        Self { strict: true }
    }

    /// Literal JSON Schema semantics.
    #[must_use]
    pub const fn lax() -> Self {
        Self { strict: false }
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::strict()
    }
}

/// Validate `value` against `schema`, resolving `$ref` against `document`.
///
/// Returns every violation rather than the first, because a body with three
/// wrong fields should be one test run, not three.
#[must_use]
pub fn validate(
    document: &Document,
    schema: &SchemaNode,
    value: &Value,
    options: Options,
) -> Vec<Violation> {
    let mut validator = Validator {
        document,
        options,
        out: Vec::new(),
    };
    validator.check(schema, value, "", 0);
    validator.out
}

/// Render a violation list for the failure report.
#[must_use]
pub fn render(violations: &[Violation]) -> String {
    violations
        .iter()
        .fold(String::new(), |mut out, violation| {
            let _ = writeln!(out, "{}: {}", violation.pointer, violation.message);
            out
        })
        .trim_end()
        .to_owned()
}

struct Validator<'a> {
    document: &'a Document,
    options: Options,
    out: Vec<Violation>,
}

impl Validator<'_> {
    fn push(&mut self, pointer: &str, message: impl Into<String>) {
        self.out.push(Violation::new(pointer, message));
    }

    /// Run a sub-schema in isolation, so `anyOf` can ask "did this branch pass"
    /// without its failures reaching the report.
    fn probe(
        &self,
        schema: &SchemaNode,
        value: &Value,
        pointer: &str,
        depth: usize,
    ) -> Vec<Violation> {
        let mut nested = Validator {
            document: self.document,
            options: self.options,
            out: Vec::new(),
        };
        nested.check(schema, value, pointer, depth);
        nested.out
    }

    fn check(&mut self, schema: &SchemaNode, value: &Value, pointer: &str, depth: usize) {
        if depth > MAX_DEPTH {
            self.push(
                pointer,
                "the schema nests deeper than the validator will follow",
            );
            return;
        }

        if let Some(reference) = &schema.reference {
            match resolve(self.document, reference) {
                Some(target) => self.check(target, value, pointer, depth + 1),
                None => self.push(
                    pointer,
                    format!("the document's `$ref` {reference} does not resolve"),
                ),
            }
            // OpenAPI 3.1 permits keywords beside `$ref`; a bare reference has
            // none, which is what Moso generates, so this is usually a no-op.
            // `SchemaNode::is_bare_reference` takes `self` by value, hence the
            // clone — a test harness can afford one, and the alternative is
            // silently ignoring a sibling keyword.
            if schema.clone().is_bare_reference() {
                return;
            }
        }

        self.check_type(schema, value, pointer);
        self.check_values(schema, value, pointer);
        self.check_string(schema, value, pointer);
        self.check_number(schema, value, pointer);
        self.check_array(schema, value, pointer, depth);
        self.check_object(schema, value, pointer, depth);
        self.check_combinators(schema, value, pointer, depth);
    }

    fn check_type(&mut self, schema: &SchemaNode, value: &Value, pointer: &str) {
        if schema.types.is_empty() {
            return;
        }
        if schema.types.iter().any(|ty| matches_type(*ty, value)) {
            return;
        }
        let allowed: Vec<&str> = schema.types.iter().map(|ty| ty.as_str()).collect();
        self.push(
            pointer,
            format!(
                "expected type {}, found {}",
                allowed.join(" or "),
                type_name(value)
            ),
        );
    }

    fn check_values(&mut self, schema: &SchemaNode, value: &Value, pointer: &str) {
        if let Some(constant) = &schema.constant
            && value != constant
        {
            self.push(
                pointer,
                format!("expected the constant {constant}, found {value}"),
            );
        }
        if !schema.enumeration.is_empty() && !schema.enumeration.contains(value) {
            let allowed: Vec<String> = schema.enumeration.iter().map(ToString::to_string).collect();
            self.push(
                pointer,
                format!("{value} is not one of {}", allowed.join(", ")),
            );
        }
    }

    fn check_string(&mut self, schema: &SchemaNode, value: &Value, pointer: &str) {
        let Some(text) = value.as_str() else { return };
        // JSON Schema counts code points, not bytes.
        let length = text.chars().count() as u64;
        if let Some(min) = schema.min_length
            && length < min
        {
            self.push(
                pointer,
                format!("expected at least {min} characters, found {length}"),
            );
        }
        if let Some(max) = schema.max_length
            && length > max
        {
            self.push(
                pointer,
                format!("expected at most {max} characters, found {length}"),
            );
        }
        if let Some(pattern) = &schema.pattern {
            match Regex::new(pattern) {
                Ok(regex) => {
                    if !regex.is_match(text) {
                        self.push(pointer, format!("{text:?} does not match /{pattern}/"));
                    }
                }
                Err(_) => self.push(
                    pointer,
                    format!("the document's pattern /{pattern}/ is not a valid regular expression"),
                ),
            }
        }
    }

    fn check_number(&mut self, schema: &SchemaNode, value: &Value, pointer: &str) {
        let Some(number) = value.as_f64() else { return };
        if let Some(min) = schema.minimum.as_ref().and_then(serde_json::Number::as_f64)
            && number < min
        {
            self.push(pointer, format!("expected at least {min}, found {number}"));
        }
        if let Some(max) = schema.maximum.as_ref().and_then(serde_json::Number::as_f64)
            && number > max
        {
            self.push(pointer, format!("expected at most {max}, found {number}"));
        }
        if let Some(min) = schema
            .exclusive_minimum
            .as_ref()
            .and_then(serde_json::Number::as_f64)
            && number <= min
        {
            self.push(pointer, format!("expected more than {min}, found {number}"));
        }
        if let Some(max) = schema
            .exclusive_maximum
            .as_ref()
            .and_then(serde_json::Number::as_f64)
            && number >= max
        {
            self.push(pointer, format!("expected less than {max}, found {number}"));
        }
        if let Some(step) = schema
            .multiple_of
            .as_ref()
            .and_then(serde_json::Number::as_f64)
            && step > 0.0
        {
            let quotient = number / step;
            if (quotient - quotient.round()).abs() > 1e-9 {
                self.push(pointer, format!("{number} is not a multiple of {step}"));
            }
        }
    }

    fn check_array(&mut self, schema: &SchemaNode, value: &Value, pointer: &str, depth: usize) {
        let Some(items) = value.as_array() else {
            return;
        };
        let length = items.len() as u64;
        if let Some(min) = schema.min_items
            && length < min
        {
            self.push(
                pointer,
                format!("expected at least {min} items, found {length}"),
            );
        }
        if let Some(max) = schema.max_items
            && length > max
        {
            self.push(
                pointer,
                format!("expected at most {max} items, found {length}"),
            );
        }
        if schema.unique_items {
            for (index, item) in items.iter().enumerate() {
                if items[..index].contains(item) {
                    self.push(
                        &child(pointer, &index.to_string()),
                        "duplicate item in a uniqueItems array",
                    );
                }
            }
        }
        for (index, prefix) in schema.prefix_items.iter().enumerate() {
            if let Some(item) = items.get(index) {
                self.check(prefix, item, &child(pointer, &index.to_string()), depth + 1);
            }
        }
        if let Some(item_schema) = &schema.items {
            for (index, item) in items.iter().enumerate().skip(schema.prefix_items.len()) {
                self.check(
                    item_schema,
                    item,
                    &child(pointer, &index.to_string()),
                    depth + 1,
                );
            }
        }
    }

    fn check_object(&mut self, schema: &SchemaNode, value: &Value, pointer: &str, depth: usize) {
        let Some(members) = value.as_object() else {
            return;
        };
        for required in &schema.required {
            if !members.contains_key(required) {
                self.push(
                    &child(pointer, required),
                    "the document requires this property and the response omits it",
                );
            }
        }
        let count = members.len() as u64;
        if let Some(min) = schema.min_properties
            && count < min
        {
            self.push(
                pointer,
                format!("expected at least {min} properties, found {count}"),
            );
        }
        if let Some(max) = schema.max_properties
            && count > max
        {
            self.push(
                pointer,
                format!("expected at most {max} properties, found {count}"),
            );
        }

        for (name, member) in members {
            let at = child(pointer, name);
            if let Some(property) = schema.properties.get(name) {
                self.check(property, member, &at, depth + 1);
                continue;
            }
            match &schema.additional_properties {
                // `additionalProperties: true` is the document saying "anything
                // may appear here", so strictness has nothing to add.
                Some(AdditionalProperties::Any(true)) => {}
                None => {
                    if self.options.strict && !schema.properties.is_empty() {
                        self.push(
                            &at,
                            "the response carries a property the document does not describe",
                        );
                    }
                }
                Some(AdditionalProperties::Any(false)) => self.push(
                    &at,
                    "the document forbids additional properties and the response has one",
                ),
                Some(AdditionalProperties::Schema(node)) => {
                    self.check(node, member, &at, depth + 1);
                }
            }
        }
    }

    fn check_combinators(
        &mut self,
        schema: &SchemaNode,
        value: &Value,
        pointer: &str,
        depth: usize,
    ) {
        for branch in &schema.all_of {
            self.check(branch, value, pointer, depth + 1);
        }
        if !schema.any_of.is_empty() {
            let passed = schema
                .any_of
                .iter()
                .any(|branch| self.probe(branch, value, pointer, depth + 1).is_empty());
            if !passed {
                self.push(
                    pointer,
                    format!(
                        "matches none of the {} `anyOf` branches",
                        schema.any_of.len()
                    ),
                );
            }
        }
        if !schema.one_of.is_empty() {
            let matched = schema
                .one_of
                .iter()
                .filter(|branch| self.probe(branch, value, pointer, depth + 1).is_empty())
                .count();
            if matched != 1 {
                self.push(
                    pointer,
                    format!(
                        "matches {matched} of the {} `oneOf` branches; exactly one must match",
                        schema.one_of.len()
                    ),
                );
            }
        }
        if let Some(forbidden) = &schema.not
            && self.probe(forbidden, value, pointer, depth + 1).is_empty()
        {
            self.push(pointer, "matches a schema the document forbids with `not`");
        }
    }
}

/// Resolve a local `$ref` against the document's components.
///
/// Only local references are followed: a document that points at another file
/// is not something a test harness should be fetching.
#[must_use]
pub fn resolve<'a>(document: &'a Document, reference: &str) -> Option<&'a SchemaNode> {
    let name = reference.strip_prefix(moso::openapi::COMPONENTS_SCHEMAS_PREFIX)?;
    document.components.schemas.get(name)
}

/// Whether `value` is an instance of `ty`.
fn matches_type(ty: JsonType, value: &Value) -> bool {
    match ty {
        JsonType::Null => value.is_null(),
        JsonType::Boolean => value.is_boolean(),
        JsonType::Object => value.is_object(),
        JsonType::Array => value.is_array(),
        JsonType::Number => value.is_number(),
        // JSON Schema 2020-12: 1.0 is a valid integer.
        JsonType::Integer => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        JsonType::String => value.is_string(),
    }
}

/// The JSON type of a value, for an error message.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Append one token to a JSON Pointer, escaped as RFC 6901 asks.
fn child(pointer: &str, token: &str) -> String {
    format!("{pointer}/{}", token.replace('~', "~0").replace('/', "~1"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use moso::openapi::{Document, Info};
    use moso::schema::json_schema::SchemaNode;
    use serde_json::json;

    fn document() -> Document {
        Document::new(Info::new("Test", "1"))
    }

    fn object(properties: &[(&str, SchemaNode)], required: &[&str]) -> SchemaNode {
        let mut node = SchemaNode::of_type(JsonType::Object);
        for (name, schema) in properties {
            node.properties.insert((*name).to_owned(), schema.clone());
        }
        node.required = required.iter().map(|name| (*name).to_owned()).collect();
        node
    }

    #[test]
    fn a_matching_body_produces_no_violations() {
        let schema = object(
            &[
                ("id", SchemaNode::of_type(JsonType::Integer)),
                ("name", SchemaNode::of_type(JsonType::String)),
            ],
            &["id", "name"],
        );
        let body = json!({"id": 1, "name": "ada"});
        assert!(validate(&document(), &schema, &body, Options::strict()).is_empty());
    }

    #[test]
    fn a_wrong_type_is_reported_at_its_pointer() {
        let schema = object(&[("id", SchemaNode::of_type(JsonType::Integer))], &["id"]);
        let violations = validate(&document(), &schema, &json!({"id": "1"}), Options::strict());
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].pointer, "/id");
        assert!(violations[0].message.contains("expected type integer"));
    }

    #[test]
    fn a_missing_required_property_is_reported() {
        let schema = object(&[("id", SchemaNode::of_type(JsonType::Integer))], &["id"]);
        let violations = validate(&document(), &schema, &json!({}), Options::strict());
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].pointer, "/id");
    }

    /// Acceptance criterion 7 of `43-testing.md`.
    #[test]
    fn an_undocumented_field_is_drift_under_the_strict_default() {
        let schema = object(&[("id", SchemaNode::of_type(JsonType::Integer))], &["id"]);
        let body = json!({"id": 1, "secret": "oops"});
        let violations = validate(&document(), &schema, &body, Options::strict());
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].pointer, "/secret");
        assert!(validate(&document(), &schema, &body, Options::lax()).is_empty());
    }

    #[test]
    fn additional_properties_false_is_enforced_even_in_lax_mode() {
        let mut schema = object(&[("id", SchemaNode::of_type(JsonType::Integer))], &[]);
        schema.additional_properties = Some(AdditionalProperties::Any(false));
        let violations = validate(
            &document(),
            &schema,
            &json!({"id": 1, "extra": true}),
            Options::lax(),
        );
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].pointer, "/extra");
    }

    #[test]
    fn a_free_form_object_is_not_drift() {
        // No `properties` at all: a map, not a struct.
        let schema = SchemaNode::of_type(JsonType::Object);
        assert!(
            validate(
                &document(),
                &schema,
                &json!({"anything": 1}),
                Options::strict()
            )
            .is_empty()
        );
    }

    #[test]
    fn additional_properties_as_a_schema_is_applied_to_every_extra_member() {
        let mut schema = SchemaNode::of_type(JsonType::Object);
        schema.additional_properties = Some(AdditionalProperties::Schema(Box::new(
            SchemaNode::of_type(JsonType::String),
        )));
        let violations = validate(&document(), &schema, &json!({"a": 1}), Options::strict());
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].pointer, "/a");
    }

    #[test]
    fn string_constraints_count_characters_not_bytes() {
        let mut schema = SchemaNode::of_type(JsonType::String);
        schema.min_length = Some(3);
        let violations = validate(&document(), &schema, &json!("ééé"), Options::strict());
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn a_pattern_is_enforced() {
        let mut schema = SchemaNode::of_type(JsonType::String);
        schema.pattern = Some("^[a-z]+$".into());
        assert!(validate(&document(), &schema, &json!("ada"), Options::strict()).is_empty());
        assert_eq!(
            validate(&document(), &schema, &json!("Ada"), Options::strict()).len(),
            1
        );
    }

    #[test]
    fn an_invalid_documented_pattern_is_itself_a_violation() {
        let mut schema = SchemaNode::of_type(JsonType::String);
        schema.pattern = Some("([".into());
        let violations = validate(&document(), &schema, &json!("x"), Options::strict());
        assert_eq!(violations.len(), 1);
        assert!(
            violations[0]
                .message
                .contains("not a valid regular expression")
        );
    }

    #[test]
    fn numeric_bounds_are_enforced_in_both_directions() {
        let mut schema = SchemaNode::of_type(JsonType::Integer);
        schema.minimum = Some(serde_json::Number::from(1));
        schema.maximum = Some(serde_json::Number::from(10));
        assert!(validate(&document(), &schema, &json!(5), Options::strict()).is_empty());
        assert_eq!(
            validate(&document(), &schema, &json!(0), Options::strict()).len(),
            1
        );
        assert_eq!(
            validate(&document(), &schema, &json!(11), Options::strict()).len(),
            1
        );
    }

    #[test]
    fn multiple_of_tolerates_float_representation() {
        let mut schema = SchemaNode::of_type(JsonType::Number);
        schema.multiple_of = Some(serde_json::Number::from_f64(0.1).expect("finite"));
        assert!(validate(&document(), &schema, &json!(0.3), Options::strict()).is_empty());
        assert_eq!(
            validate(&document(), &schema, &json!(0.35), Options::strict()).len(),
            1
        );
    }

    #[test]
    fn array_items_are_validated_and_counted() {
        let mut schema = SchemaNode::of_type(JsonType::Array);
        schema.items = Some(Box::new(SchemaNode::of_type(JsonType::Integer)));
        schema.min_items = Some(2);
        let violations = validate(&document(), &schema, &json!(["a"]), Options::strict());
        assert_eq!(violations.len(), 2, "{violations:?}");
    }

    #[test]
    fn unique_items_reports_the_duplicate_position() {
        let mut schema = SchemaNode::of_type(JsonType::Array);
        schema.unique_items = true;
        let violations = validate(&document(), &schema, &json!([1, 2, 1]), Options::strict());
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].pointer, "/2");
    }

    #[test]
    fn a_ref_is_resolved_against_the_components() {
        let mut document = document();
        document.components.schemas.insert(
            "User".to_owned(),
            object(&[("id", SchemaNode::of_type(JsonType::Integer))], &["id"]),
        );
        let schema =
            SchemaNode::reference(format!("{}User", moso::openapi::COMPONENTS_SCHEMAS_PREFIX));
        assert!(validate(&document, &schema, &json!({"id": 1}), Options::strict()).is_empty());
        assert_eq!(
            validate(&document, &schema, &json!({}), Options::strict()).len(),
            1
        );
    }

    #[test]
    fn an_unresolvable_ref_is_reported_rather_than_ignored() {
        let schema = SchemaNode::reference("#/components/schemas/Missing");
        let violations = validate(&document(), &schema, &json!({}), Options::strict());
        assert_eq!(violations.len(), 1);
        assert!(violations[0].message.contains("does not resolve"));
    }

    #[test]
    fn a_nullable_type_set_accepts_null() {
        let mut schema = SchemaNode::of_type(JsonType::String);
        schema.types.insert(JsonType::Null);
        assert!(validate(&document(), &schema, &json!(null), Options::strict()).is_empty());
    }

    #[test]
    fn one_of_requires_exactly_one_branch() {
        let schema = SchemaNode {
            one_of: vec![
                SchemaNode::of_type(JsonType::String),
                SchemaNode::of_type(JsonType::Integer),
            ],
            ..SchemaNode::default()
        };
        assert!(validate(&document(), &schema, &json!("a"), Options::strict()).is_empty());
        assert_eq!(
            validate(&document(), &schema, &json!(true), Options::strict()).len(),
            1
        );
    }

    #[test]
    fn any_of_passes_when_one_branch_does() {
        let schema = SchemaNode {
            any_of: vec![
                SchemaNode::of_type(JsonType::String),
                SchemaNode::of_type(JsonType::Integer),
            ],
            ..SchemaNode::default()
        };
        assert!(validate(&document(), &schema, &json!(3), Options::strict()).is_empty());
        assert_eq!(
            validate(&document(), &schema, &json!(null), Options::strict()).len(),
            1
        );
    }

    #[test]
    fn an_empty_schema_accepts_anything() {
        let schema = SchemaNode::default();
        assert!(validate(&document(), &schema, &json!({"a": [1]}), Options::strict()).is_empty());
    }

    #[test]
    fn rendering_lists_one_violation_per_line() {
        let violations = vec![Violation::new("/a", "first"), Violation::new("", "second")];
        assert_eq!(render(&violations), "/a: first\n(root): second");
    }
}
