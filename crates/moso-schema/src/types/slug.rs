//! [`Slug`] — a URL-safe identifier.

use std::borrow::Cow;

use crate::json_schema::{SchemaGenerator, SchemaNode, SchemaRef, StringBuilder};
use crate::schema::Schema;
use crate::types::ConstraintError;
use crate::validate::ErrorCode;

/// A lowercase, hyphen-separated, URL-safe identifier.
///
/// Matches `^[a-z0-9]+(-[a-z0-9]+)*$`: no leading, trailing or doubled
/// hyphens, no uppercase, no underscores. Being strict here is what makes a
/// slug safe to interpolate into a path segment without escaping.
///
/// ```text
/// JSON Schema: { "type": "string", "format": "slug",
///                "pattern": "^[a-z0-9]+(-[a-z0-9]+)*$",
///                "minLength": 1, "maxLength": 128 }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Slug(String);

impl Slug {
    /// The JSON Schema `format` this type emits.
    pub const FORMAT: &'static str = "slug";

    /// The pattern emitted into the schema and enforced on construction.
    pub const PATTERN: &'static str = "^[a-z0-9]+(-[a-z0-9]+)*$";

    /// A slug is never empty.
    pub const MIN_LENGTH: u64 = 1;

    /// Long enough for a sentence-length title, short enough to index.
    pub const MAX_LENGTH: u64 = 128;

    /// How many suffixes [`Slug::unique_from`] will try before giving up.
    pub const MAX_UNIQUE_ATTEMPTS: u32 = 10_000;

    /// Accept an already-valid slug.
    ///
    /// Nothing is normalised — not even surrounding whitespace. A slug is an
    /// identifier a client will round-trip through a URL, so `" post "` and
    /// `"post"` being *different inputs* that produce the *same* value is a
    /// worse surprise than a rejection. Use [`Slug::slugify`] to derive one
    /// from free text.
    ///
    /// # Errors
    /// [`ConstraintError`] with code `pattern` when the input does not match
    /// [`Slug::PATTERN`], or `len` when it is longer than
    /// [`Slug::MAX_LENGTH`].
    pub fn new(value: impl Into<String>) -> Result<Self, ConstraintError> {
        let value = value.into();

        // Length first: a 4 KiB body deserves the length message, not a
        // pattern message quoting a regex at it.
        let length = value.chars().count() as u64;
        if length > Self::MAX_LENGTH {
            return Err(ConstraintError::new(
                ErrorCode::Len,
                format!(
                    "must be at most {} characters (got {length})",
                    Self::MAX_LENGTH
                ),
            )
            .with_param("min", Self::MIN_LENGTH)
            .with_param("max", Self::MAX_LENGTH)
            .with_param("unit", "characters"));
        }

        if !is_slug(&value) {
            return Err(ConstraintError::new(
                ErrorCode::Pattern,
                "must be lowercase letters, digits and single hyphens, e.g. `my-first-post`",
            )
            .with_param("pattern", Self::PATTERN));
        }

        Ok(Self(value))
    }

    /// Wrap a string without checking it.
    ///
    /// **Escape hatch.** The invariant becomes your responsibility: a `Slug`
    /// built from `"../../etc/passwd"` is exactly as dangerous as it sounds,
    /// because the whole point of the type is that callers may interpolate it
    /// into a path without escaping. Use it only for values a database
    /// constraint already guarantees.
    #[must_use]
    pub fn new_unchecked(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Derive a slug from arbitrary text: lowercase, non-alphanumerics
    /// collapsed to single hyphens, ends trimmed.
    ///
    /// ASCII only, and deliberately so: there is no transliteration table here.
    /// `"Über"` becomes `"ber"`, not `"uber"`, because a half-complete
    /// transliteration that works for German and mangles Greek is worse than an
    /// obviously-lossy rule the caller can see. Applications that need
    /// transliteration should do it first and pass the result in.
    ///
    /// Returns `None` when nothing sluggable survives — an all-emoji title, for
    /// instance. Returning `None` rather than an empty slug forces the caller
    /// to decide, which is the right place for that decision.
    #[must_use]
    pub fn slugify(text: &str) -> Option<Self> {
        // Everything kept is ASCII, so byte length and character count agree.
        let max = Self::MAX_LENGTH as usize;
        let mut out = String::with_capacity(text.len().min(max));
        let mut pending_separator = false;

        for c in text.chars().flat_map(char::to_lowercase) {
            if c.is_ascii_alphanumeric() {
                if pending_separator && !out.is_empty() {
                    // Only emit the separator if a character can follow it;
                    // a trailing hyphen would not match `PATTERN`.
                    if out.len() + 2 > max {
                        break;
                    }
                    out.push('-');
                }
                pending_separator = false;
                if out.len() == max {
                    break;
                }
                out.push(c);
            } else {
                // Runs of anything else — spaces, punctuation, emoji, accented
                // letters — collapse into a single separator, emitted lazily so
                // leading and trailing runs disappear.
                pending_separator = true;
            }
        }

        if out.is_empty() {
            return None;
        }
        Some(Self(out))
    }

    /// Derive a slug from a title. An alias for [`Slug::slugify`], named for
    /// the call site that reads best: `Slug::from_title(&post.title)`.
    ///
    /// ```
    /// # use moso_schema::types::Slug;
    /// assert_eq!(Slug::from_title("Hello World!").unwrap().as_str(), "hello-world");
    /// ```
    #[must_use]
    pub fn from_title(title: &str) -> Option<Self> {
        Self::slugify(title)
    }

    /// Find the first free variant of `base`: `post`, then `post-2`,
    /// `post-3`, …
    ///
    /// `exists` is a **synchronous** predicate on purpose. The database check it
    /// usually wraps is asynchronous, and hiding an `await` inside a helper on a
    /// value type would make an innocuous-looking call do IO. Collect the taken
    /// slugs first, or drive the loop yourself:
    ///
    /// ```
    /// # use moso_schema::types::Slug;
    /// let taken = ["my-post", "my-post-2"];
    /// let base = Slug::from_title("My Post").unwrap();
    /// let free = Slug::unique_from(&base, |s| taken.contains(&s));
    /// assert_eq!(free.as_str(), "my-post-3");
    /// ```
    ///
    /// This is a *suggestion*, not a reservation: between the check and the
    /// insert, someone else may take it. The unique index is what actually
    /// enforces uniqueness, and a `409` is the correct answer when it fires.
    ///
    /// The suffix is appended within [`Slug::MAX_LENGTH`], truncating the base
    /// if it has to. Gives up after 10 000 attempts and returns the last
    /// candidate rather than looping forever on a predicate that always says
    /// yes.
    #[must_use]
    pub fn unique_from(base: &Slug, mut exists: impl FnMut(&str) -> bool) -> Slug {
        if !exists(base.as_str()) {
            return base.clone();
        }

        let mut candidate = Slug(String::new());
        for n in 2..=Self::MAX_UNIQUE_ATTEMPTS {
            let suffix = format!("-{n}");
            let room = (Self::MAX_LENGTH as usize).saturating_sub(suffix.chars().count());
            let stem: String = base.0.chars().take(room).collect();
            // Truncation can leave a trailing hyphen, which the pattern forbids.
            let stem = stem.trim_end_matches('-');
            candidate = Slug(format!("{stem}{suffix}"));
            if !exists(candidate.as_str()) {
                return candidate;
            }
        }
        candidate
    }

    /// The slug text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the underlying `String`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

/// `^[a-z0-9]+(-[a-z0-9]+)*$`, hand-written.
///
/// A regex would need a `OnceLock` and a scan; this is a single pass with no
/// allocation and no lazy initialisation. A test asserts it agrees with
/// [`Slug::PATTERN`] compiled by `regex`, so the documented constraint and the
/// enforced one cannot drift.
fn is_slug(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut previous_was_hyphen = true; // a leading hyphen is invalid
    for b in value.bytes() {
        match b {
            b'a'..=b'z' | b'0'..=b'9' => previous_was_hyphen = false,
            // Rejects a leading hyphen and a doubled hyphen in one condition.
            b'-' if !previous_was_hyphen => previous_was_hyphen = true,
            _ => return false,
        }
    }
    // A trailing hyphen is invalid.
    !previous_was_hyphen
}

string_newtype!(Slug);

impl Schema for Slug {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Slug")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> SchemaNode {
        StringBuilder::new()
            .format(Self::FORMAT)
            .pattern(Self::PATTERN)
            .min_length(Self::MIN_LENGTH)
            .max_length(Self::MAX_LENGTH)
            .description("A lowercase, hyphen-separated, URL-safe identifier.")
            .build()
    }

    fn schema_ref() -> SchemaRef {
        crate::schema::inline_schema_ref::<Self>()
    }

    const HAS_CONSTRAINTS: bool = true;
}

#[cfg(test)]
mod tests {
    use regex::Regex;
    use serde_json::json;

    use super::*;
    use crate::validate::codes;

    #[test]
    fn accepts_well_formed_slugs() {
        for input in ["a", "post", "my-first-post", "2024-in-review", "x9"] {
            assert_eq!(
                Slug::new(input)
                    .unwrap_or_else(|e| panic!("{input:?}: {e}"))
                    .as_str(),
                input
            );
        }
    }

    #[test]
    fn rejects_malformed_slugs_with_a_pattern_code() {
        for input in [
            "", "-post", "post-", "my--post", "My-Post", "my_post", "my post", "my.post", "café",
        ] {
            let e = Slug::new(input).expect_err(input);
            assert_eq!(e.code().as_str(), codes::PATTERN, "for {input:?}");
            assert_eq!(e.params().get("pattern"), Some(&json!(Slug::PATTERN)));
        }
    }

    #[test]
    fn rejects_over_long_slugs_with_a_len_code() {
        let long = "a".repeat(Slug::MAX_LENGTH as usize + 1);
        let e = Slug::new(&long).expect_err("too long");
        assert_eq!(e.code().as_str(), codes::LEN);
        assert_eq!(e.params().get("max"), Some(&json!(Slug::MAX_LENGTH)));
        assert!(Slug::new("a".repeat(Slug::MAX_LENGTH as usize)).is_ok());
    }

    /// The premise of the crate: the enforced rule and the documented one are
    /// the same rule. `is_slug` is hand-written for speed, so prove it agrees
    /// with the pattern that ships in the schema.
    #[test]
    fn the_hand_written_check_agrees_with_the_published_pattern() {
        let re = Regex::new(Slug::PATTERN).expect("PATTERN must be a valid regex");
        let cases = [
            "", "a", "9", "-", "--", "a-", "-a", "a--b", "a-b", "a-b-c", "A", "a_b", "a b", "ab9",
            "9-a", "a-9", "aa--bb", "a-b-", "ä", "a\nb", "a-b\n",
        ];
        for case in cases {
            assert_eq!(
                is_slug(case),
                re.is_match(case),
                "disagreement on {case:?}: hand-written says {}",
                is_slug(case)
            );
        }
    }

    #[test]
    fn slugify_derives_a_slug_from_free_text() {
        assert_eq!(
            Slug::from_title("Hello World!").unwrap().as_str(),
            "hello-world"
        );
        assert_eq!(
            Slug::slugify("  Hello,   World  ").unwrap().as_str(),
            "hello-world"
        );
        assert_eq!(
            Slug::slugify("Rust 1.97 Released").unwrap().as_str(),
            "rust-1-97-released"
        );
        assert_eq!(
            Slug::slugify("--already-a-slug--").unwrap().as_str(),
            "already-a-slug"
        );
        assert_eq!(
            Slug::slugify("Über").unwrap().as_str(),
            "ber",
            "no transliteration"
        );
        assert_eq!(Slug::slugify("🎉🎉🎉"), None);
        assert_eq!(Slug::slugify(""), None);
    }

    #[test]
    fn slugify_always_produces_something_new_accepts() {
        let cases = [
            "Hello World!",
            "a",
            "  spaced  out  ",
            "MIXED Case 123",
            "punctuation!!!everywhere???",
            &"long ".repeat(200),
            &"x".repeat(500),
        ];
        for case in cases {
            if let Some(slug) = Slug::slugify(case) {
                assert!(
                    Slug::new(slug.as_str()).is_ok(),
                    "slugify produced an invalid slug for {case:?}: {slug:?}"
                );
                assert!(slug.as_str().chars().count() as u64 <= Slug::MAX_LENGTH);
            }
        }
    }

    #[test]
    fn unique_from_appends_the_first_free_suffix() {
        let taken = ["my-post", "my-post-2", "my-post-3"];
        let base = Slug::from_title("My Post").unwrap();
        assert_eq!(
            Slug::unique_from(&base, |s| taken.contains(&s)).as_str(),
            "my-post-4"
        );
        assert_eq!(
            Slug::unique_from(&base, |_| false).as_str(),
            "my-post",
            "an unused base is returned unchanged"
        );
    }

    #[test]
    fn unique_from_keeps_the_result_within_the_length_limit() {
        let base = Slug::new("a".repeat(Slug::MAX_LENGTH as usize)).unwrap();
        let unique = Slug::unique_from(&base, |s| s.ends_with('a'));
        assert!(unique.as_str().chars().count() as u64 <= Slug::MAX_LENGTH);
        assert!(
            Slug::new(unique.as_str()).is_ok(),
            "{unique:?} must be valid"
        );
        assert!(unique.as_str().ends_with("-2"));
    }

    #[test]
    fn unique_from_terminates_on_a_predicate_that_never_yields() {
        // Pathological, but a caller with a broken predicate should get a value
        // back rather than a hung request.
        let base = Slug::new("post").unwrap();
        let unique = Slug::unique_from(&base, |_| true);
        assert_eq!(unique.as_str(), "post-10000");
    }

    #[test]
    fn every_constructor_enforces_the_invariant() {
        assert!("Bad Slug".parse::<Slug>().is_err());
        assert!(Slug::try_from(String::from("good-slug")).is_ok());
        assert!(serde_json::from_str::<Slug>("\"Bad Slug\"").is_err());
        assert_eq!(
            serde_json::from_str::<Slug>("\"good-slug\"").unwrap(),
            Slug::new("good-slug").unwrap()
        );
        let err = serde_json::from_str::<Slug>("\"Bad Slug\"").unwrap_err();
        assert_eq!(
            crate::types::parse_serde_message(&err.to_string()).map(|(c, _)| c),
            Some(codes::PATTERN)
        );
    }

    #[test]
    fn json_schema_documents_what_is_enforced() {
        let node = Slug::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            serde_json::to_value(&node).unwrap(),
            json!({
                "type": "string",
                "format": "slug",
                "pattern": "^[a-z0-9]+(-[a-z0-9]+)*$",
                "minLength": 1,
                "maxLength": 128,
                "description": "A lowercase, hyphen-separated, URL-safe identifier.",
            })
        );
    }

    #[test]
    fn unchecked_construction_skips_the_check() {
        assert_eq!(Slug::new_unchecked("Not A Slug").as_str(), "Not A Slug");
    }
}
