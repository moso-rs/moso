//! Failure output.
//!
//! The harness's actual product. `assertion failed: left == right` is a
//! sentence that costs a quarter of an hour; this module prints the request, the
//! response, a JSON diff and the server-side log lines for that request id, so
//! the same failure costs five seconds.
//!
//! Everything here is plain text with no colour. Test output is read in CI logs,
//! in terminals with every possible background, and in editors that strip ANSI —
//! a report that is only legible in one of them is a report that is not read.

use std::fmt::Write as _;

use bytes::Bytes;

/// The width of the horizontal rules the report draws.
const WIDTH: usize = 74;

/// How much of a body to print before eliding the rest.
const BODY_BUDGET: usize = 8 * 1024;

// ---------------------------------------------------------------------------
// Structure
// ---------------------------------------------------------------------------

/// The opening rule, with the headline in it.
#[must_use]
pub fn rule(title: &str) -> String {
    let dashes = WIDTH.saturating_sub(title.len() + 6);
    format!("\n── {title} {}\n", "─".repeat(dashes.max(3)))
}

/// The closing rule.
#[must_use]
pub fn rule_end() -> String {
    format!("{}\n", "─".repeat(WIDTH))
}

/// One titled, indented block.
///
/// Returns the empty string for empty content, so a report never grows a
/// heading with nothing underneath it.
#[must_use]
pub fn section(title: &str, body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    let mut out = format!("  {title}:\n");
    for line in body.lines() {
        let _ = writeln!(out, "    {line}");
    }
    out.push('\n');
    out
}

/// A name/value table, one pair per line.
#[must_use]
pub fn pairs(entries: &[(String, String)]) -> String {
    entries
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

/// Render a body for the report: pretty JSON when it is JSON, the text when it
/// is text, and a byte summary when it is neither.
#[must_use]
pub fn body(bytes: &[u8], content_type: Option<&str>) -> String {
    if bytes.is_empty() {
        return "(empty)".to_owned();
    }
    let Ok(text) = core::str::from_utf8(bytes) else {
        return format!("({} bytes of binary data)", bytes.len());
    };

    let looks_json = content_type.is_some_and(is_json_like)
        || text.trim_start().starts_with('{')
        || text.trim_start().starts_with('[');
    if looks_json
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
        && let Ok(pretty) = serde_json::to_string_pretty(&value)
    {
        return elide(&pretty);
    }
    elide(text)
}

/// Whether a `Content-Type` names a JSON media type, `+json` suffixes included.
#[must_use]
pub fn is_json_like(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "application/json" || essence.ends_with("+json")
}

/// Cut a rendered body down to [`BODY_BUDGET`], saying how much was dropped.
fn elide(text: &str) -> String {
    if text.len() <= BODY_BUDGET {
        return text.to_owned();
    }
    // Never split a UTF-8 code point.
    let mut cut = BODY_BUDGET;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… {} more bytes elided", &text[..cut], text.len() - cut)
}

// ---------------------------------------------------------------------------
// RequestRecord
// ---------------------------------------------------------------------------

/// Everything the harness knows about the request it sent.
///
/// Kept alongside the response so that a failed assertion can print what
/// produced it without the test having to repeat itself.
#[derive(Clone, Debug)]
pub struct RequestRecord {
    /// The HTTP method, uppercase.
    pub method: String,
    /// The absolute URL the request was addressed to.
    pub url: String,
    /// The headers actually sent, in insertion order.
    pub headers: Vec<(String, String)>,
    /// The request body, if there was one.
    pub body: Option<Bytes>,
    /// Which transport carried it, for the report's first line.
    pub transport: &'static str,
}

impl RequestRecord {
    /// The `POST http://…/users` line.
    #[must_use]
    pub fn line(&self) -> String {
        format!("{} {}", self.method, self.url)
    }

    /// The `Content-Type` that was sent, if any.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str())
    }

    /// The request block of the report.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&section("request", &self.line()));
        out.push_str(&section("request headers", &pairs(&self.headers)));
        if let Some(bytes) = &self.body {
            out.push_str(&section("request body", &body(bytes, self.content_type())));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rule_never_collapses_below_three_dashes() {
        let long = "x".repeat(200);
        assert!(rule(&long).contains("───"));
    }

    #[test]
    fn an_empty_section_renders_as_nothing() {
        assert_eq!(section("headers", ""), "");
    }

    #[test]
    fn a_section_indents_every_line() {
        let rendered = section("body", "a\nb");
        assert_eq!(rendered, "  body:\n    a\n    b\n\n");
    }

    #[test]
    fn json_bodies_are_pretty_printed() {
        let rendered = body(br#"{"a":1}"#, Some("application/json"));
        assert_eq!(rendered, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn a_problem_json_body_is_recognised_as_json() {
        assert!(is_json_like("application/problem+json"));
        assert!(is_json_like("application/json; charset=utf-8"));
        assert!(!is_json_like("text/plain"));
    }

    #[test]
    fn a_non_utf8_body_is_summarised_rather_than_dumped() {
        let rendered = body(&[0xff, 0xfe, 0x00], Some("application/octet-stream"));
        assert_eq!(rendered, "(3 bytes of binary data)");
    }

    #[test]
    fn an_empty_body_says_so() {
        assert_eq!(body(b"", None), "(empty)");
    }

    #[test]
    fn a_huge_body_is_elided_on_a_character_boundary() {
        let text = "é".repeat(BODY_BUDGET);
        let rendered = body(text.as_bytes(), Some("text/plain"));
        assert!(rendered.contains("more bytes elided"));
        // The prefix must still be valid UTF-8, which it is by construction:
        // rendering it at all proves the slice did not split a code point.
        assert!(rendered.starts_with('é'));
    }

    #[test]
    fn text_that_merely_starts_with_a_brace_is_not_forced_through_serde() {
        let rendered = body(b"{not json", Some("text/plain"));
        assert_eq!(rendered, "{not json");
    }

    #[test]
    fn a_request_record_finds_its_content_type_case_insensitively() {
        let record = RequestRecord {
            method: "POST".to_owned(),
            url: "http://localhost/users".to_owned(),
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: Some(Bytes::from_static(b"{}")),
            transport: "in-process",
        };
        assert_eq!(record.content_type(), Some("application/json"));
        assert!(record.render().contains("POST http://localhost/users"));
    }
}
