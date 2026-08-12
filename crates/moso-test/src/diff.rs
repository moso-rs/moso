//! A structural JSON diff, reported by RFC 6901 pointer.
//!
//! `assert_eq!` on two `serde_json::Value`s prints both documents and leaves the
//! reader to find the one character that differs. This module finds it for them:
//! the output names the pointer, the expected value and the actual one, and
//! nothing else.
//!
//! Two comparison modes, because tests want both:
//!
//! - [`exact`] — the documents must be identical; a member present only in the
//!   response is a difference.
//! - [`subset`] — every member of the expected document must appear in the
//!   response with the same value; extra members in the response are fine. This
//!   is what [`assert_json_matches`](crate::TestResponse::assert_json_matches)
//!   uses, and it is what makes an assertion survive the addition of a field.

use std::fmt::Write as _;

use serde_json::Value;

/// What kind of disagreement was found at a pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    /// The expected document has a value here and the actual one does not.
    Missing,
    /// The actual document has a value here and the expected one does not.
    Unexpected,
    /// Both have a value and they differ.
    Changed,
}

impl DiffKind {
    /// The marker the report prints in front of the line.
    #[must_use]
    pub const fn marker(self) -> char {
        match self {
            DiffKind::Missing => '-',
            DiffKind::Unexpected => '+',
            DiffKind::Changed => '~',
        }
    }
}

/// One disagreement between two JSON documents.
#[derive(Clone, Debug, PartialEq)]
pub struct Difference {
    /// RFC 6901 JSON Pointer to the disagreeing member. `""` is the root.
    pub pointer: String,
    /// Which of the three shapes of disagreement this is.
    pub kind: DiffKind,
    /// What the expected document has there.
    pub expected: Option<Value>,
    /// What the actual document has there.
    pub actual: Option<Value>,
}

/// Every difference between `expected` and `actual`, both directions.
#[must_use]
pub fn exact(expected: &Value, actual: &Value) -> Vec<Difference> {
    let mut out = Vec::new();
    compare(expected, actual, String::new(), false, &mut out);
    out
}

/// Every way `actual` fails to contain `expected`.
///
/// Members of `actual` that `expected` says nothing about are not differences.
#[must_use]
pub fn subset(expected: &Value, actual: &Value) -> Vec<Difference> {
    let mut out = Vec::new();
    compare(expected, actual, String::new(), true, &mut out);
    out
}

/// Walk both documents in step.
///
/// Arrays are compared positionally in both modes: a subset match on an array
/// would have to decide whether `[1]` matches `[1, 2]` at index 0 or means "some
/// element is 1", and a rule that needs a paragraph to explain is a rule that
/// will be misread in a test.
fn compare(
    expected: &Value,
    actual: &Value,
    pointer: String,
    lenient: bool,
    out: &mut Vec<Difference>,
) {
    match (expected, actual) {
        (Value::Object(expected_map), Value::Object(actual_map)) => {
            for (key, expected_value) in expected_map {
                let child = push(&pointer, key);
                match actual_map.get(key) {
                    Some(actual_value) => {
                        compare(expected_value, actual_value, child, lenient, out)
                    }
                    None => out.push(Difference {
                        pointer: child,
                        kind: DiffKind::Missing,
                        expected: Some(expected_value.clone()),
                        actual: None,
                    }),
                }
            }
            if !lenient {
                for (key, actual_value) in actual_map {
                    if !expected_map.contains_key(key) {
                        out.push(Difference {
                            pointer: push(&pointer, key),
                            kind: DiffKind::Unexpected,
                            expected: None,
                            actual: Some(actual_value.clone()),
                        });
                    }
                }
            }
        }
        (Value::Array(expected_items), Value::Array(actual_items)) => {
            for (index, expected_item) in expected_items.iter().enumerate() {
                let child = push(&pointer, &index.to_string());
                match actual_items.get(index) {
                    Some(actual_item) => compare(expected_item, actual_item, child, lenient, out),
                    None => out.push(Difference {
                        pointer: child,
                        kind: DiffKind::Missing,
                        expected: Some(expected_item.clone()),
                        actual: None,
                    }),
                }
            }
            for (index, actual_item) in actual_items.iter().enumerate().skip(expected_items.len()) {
                out.push(Difference {
                    pointer: push(&pointer, &index.to_string()),
                    kind: DiffKind::Unexpected,
                    expected: None,
                    actual: Some(actual_item.clone()),
                });
            }
        }
        _ if expected == actual => {}
        _ => out.push(Difference {
            pointer,
            kind: DiffKind::Changed,
            expected: Some(expected.clone()),
            actual: Some(actual.clone()),
        }),
    }
}

/// Append one unescaped token to a JSON Pointer, escaping it as RFC 6901 asks.
fn push(pointer: &str, token: &str) -> String {
    format!("{pointer}/{}", token.replace('~', "~0").replace('/', "~1"))
}

/// Render a difference list for the failure report.
#[must_use]
pub fn render(differences: &[Difference]) -> String {
    if differences.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for difference in differences {
        let at = if difference.pointer.is_empty() {
            "(root)"
        } else {
            &difference.pointer
        };
        let _ = writeln!(out, "{} {at}", difference.kind.marker());
        if let Some(expected) = &difference.expected {
            let _ = writeln!(out, "    expected: {}", compact(expected));
        }
        if let Some(actual) = &difference.actual {
            let _ = writeln!(out, "    actual:   {}", compact(actual));
        }
        if difference.kind == DiffKind::Unexpected {
            out.push_str("    (present in the response, absent from the expectation)\n");
        }
    }
    out.trim_end().to_owned()
}

/// One-line rendering of a value, truncated so a diff line stays a line.
fn compact(value: &Value) -> String {
    let rendered = serde_json::to_string(value).unwrap_or_else(|_| "<unserialisable>".to_owned());
    if rendered.chars().count() <= 160 {
        return rendered;
    }
    let cut: String = rendered.chars().take(157).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identical_documents_have_no_differences() {
        let value = json!({"a": [1, 2, {"b": null}]});
        assert!(exact(&value, &value).is_empty());
        assert!(subset(&value, &value).is_empty());
    }

    #[test]
    fn a_changed_leaf_is_reported_at_its_pointer() {
        let differences = exact(&json!({"a": {"b": 1}}), &json!({"a": {"b": 2}}));
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].pointer, "/a/b");
        assert_eq!(differences[0].kind, DiffKind::Changed);
    }

    #[test]
    fn an_extra_member_is_a_difference_only_in_exact_mode() {
        let expected = json!({"a": 1});
        let actual = json!({"a": 1, "b": 2});
        let strict = exact(&expected, &actual);
        assert_eq!(strict.len(), 1);
        assert_eq!(strict[0].kind, DiffKind::Unexpected);
        assert_eq!(strict[0].pointer, "/b");
        assert!(subset(&expected, &actual).is_empty());
    }

    #[test]
    fn a_missing_member_is_a_difference_in_both_modes() {
        let expected = json!({"a": 1, "b": 2});
        let actual = json!({"a": 1});
        assert_eq!(exact(&expected, &actual).len(), 1);
        let lenient = subset(&expected, &actual);
        assert_eq!(lenient.len(), 1);
        assert_eq!(lenient[0].kind, DiffKind::Missing);
        assert_eq!(lenient[0].pointer, "/b");
    }

    #[test]
    fn arrays_are_compared_positionally_and_report_extra_elements() {
        let differences = exact(&json!([1]), &json!([1, 2]));
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].pointer, "/1");
        assert_eq!(differences[0].kind, DiffKind::Unexpected);
    }

    #[test]
    fn a_shorter_actual_array_reports_the_missing_tail() {
        let differences = subset(&json!([1, 2]), &json!([1]));
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].pointer, "/1");
        assert_eq!(differences[0].kind, DiffKind::Missing);
    }

    #[test]
    fn a_type_change_is_one_difference_and_not_a_recursive_storm() {
        let differences = exact(&json!({"a": {"b": 1}}), &json!({"a": 7}));
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].pointer, "/a");
    }

    #[test]
    fn pointer_tokens_are_escaped() {
        let differences = exact(&json!({"a/b": 1}), &json!({"a/b": 2}));
        assert_eq!(differences[0].pointer, "/a~1b");
        let differences = exact(&json!({"a~b": 1}), &json!({"a~b": 2}));
        assert_eq!(differences[0].pointer, "/a~0b");
    }

    #[test]
    fn a_root_level_difference_renders_as_root() {
        let rendered = render(&exact(&json!(1), &json!(2)));
        assert!(rendered.contains("(root)"), "{rendered}");
        assert!(rendered.contains("expected: 1"), "{rendered}");
        assert!(rendered.contains("actual:   2"), "{rendered}");
    }

    #[test]
    fn rendering_nothing_produces_nothing() {
        assert_eq!(render(&[]), "");
    }

    #[test]
    fn a_very_long_value_is_truncated_on_one_line() {
        let long = json!("x".repeat(500));
        let rendered = compact(&long);
        assert!(rendered.ends_with('…'));
        assert!(rendered.chars().count() <= 158);
    }
}
