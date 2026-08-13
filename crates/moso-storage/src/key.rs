//! [`StorageKey`] — the one way to name an object.
//!
//! A storage key is not a path and must not be treated as one. The local
//! backend joins it onto a directory, so a key containing `..` is a path
//! traversal; S3 accepts almost any byte sequence, so a key containing a
//! newline is a name nobody can ever type again in a console. Validation
//! happens once, on construction, and the type is the proof it happened.

use serde::{Deserialize, Serialize};

use crate::Result;

/// The separator between key segments. Not a path separator.
///
/// ```
/// assert_eq!(moso_storage::key::SEPARATOR, '/');
/// ```
pub const SEPARATOR: char = '/';

/// The longest key any supported backend accepts, in bytes.
///
/// 1024 is S3's limit and the smallest of the four; using the smallest means a
/// key that works in development works in production.
///
/// ```
/// assert_eq!(moso_storage::key::MAX_LENGTH, 1024);
/// ```
pub const MAX_LENGTH: usize = 1024;

/// A validated object key.
///
/// ```no_run
/// use moso_storage::StorageKey;
///
/// let key = StorageKey::new("uploads/2026/07/logo.png")?;
/// assert_eq!(key.extension(), Some("png"));
/// assert_eq!(key.prefix(), "uploads/2026/07");
/// # Ok::<(), moso_storage::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StorageKey(String);

impl StorageKey {
    /// Validate and wrap a key.
    ///
    /// # Errors
    ///
    /// [`Error::Key`](crate::Error::Key) when the key is empty, longer than
    /// [`MAX_LENGTH`], starts or ends with [`SEPARATOR`], contains an empty
    /// segment, contains `.` or `..` as a segment, or contains a control
    /// character.
    ///
    /// ```
    /// use moso_storage::StorageKey;
    ///
    /// assert!(StorageKey::new("../secrets").is_err());
    /// assert!(StorageKey::new("/leading").is_err());
    /// assert!(StorageKey::new("ok/enough.txt").is_ok());
    /// ```
    pub fn new(key: impl Into<String>) -> Result<Self> {
        let key = key.into();
        let reject = |detail: &'static str| Err(crate::Error::key(key.clone(), detail));

        if key.is_empty() {
            return reject("a key must not be empty");
        }
        if key.len() > MAX_LENGTH {
            return reject("a key must be at most 1024 bytes, which is S3's limit");
        }
        if key.starts_with(SEPARATOR) {
            return reject("a key must not start with `/` — it is not a path");
        }
        if key.ends_with(SEPARATOR) {
            return reject("a key must not end with `/` — that names a prefix, not an object");
        }
        // A backslash is a separator on the platform the local backend may be
        // running on, so it is rejected rather than escaped.
        if key.contains('\\') {
            return reject("a key must not contain `\\`");
        }
        for segment in key.split(SEPARATOR) {
            if segment.is_empty() {
                return reject("a key must not contain an empty segment (`//`)");
            }
            if segment == "." || segment == ".." {
                return reject("a key must not contain `.` or `..` as a segment");
            }
        }
        if key.chars().any(char::is_control) {
            return reject("a key must not contain a control character");
        }

        Ok(Self(key))
    }

    /// Build a key from segments, escaping nothing and rejecting anything
    /// that would need escaping.
    ///
    /// The safe way to build a key from user input: a segment containing a
    /// separator is an error rather than a silent extra level.
    ///
    /// # Errors
    ///
    /// [`Error::Key`](crate::Error::Key) when any segment is empty or contains
    /// [`SEPARATOR`].
    ///
    /// ```
    /// use moso_storage::StorageKey;
    ///
    /// let key = StorageKey::from_segments(["avatars", "usr_123", "original.jpg"])?;
    /// assert_eq!(key.as_str(), "avatars/usr_123/original.jpg");
    ///
    /// // A segment that would add a level is refused, not silently accepted.
    /// assert!(StorageKey::from_segments(["a", "b/c"]).is_err());
    /// # Ok::<(), moso_storage::Error>(())
    /// ```
    pub fn from_segments<S: AsRef<str>>(segments: impl IntoIterator<Item = S>) -> Result<Self> {
        let mut key = String::new();
        for segment in segments {
            let segment = segment.as_ref();
            if segment.is_empty() {
                return Err(crate::Error::key(key, "a segment must not be empty"));
            }
            if segment.contains(SEPARATOR) {
                return Err(crate::Error::key(
                    segment.to_owned(),
                    "a segment must not contain `/` — pass it as two segments, or sanitise it \
                     first; a key built from user input that silently gains a level is a way to \
                     write outside the prefix you meant",
                ));
            }
            if !key.is_empty() {
                key.push(SEPARATOR);
            }
            key.push_str(segment);
        }
        Self::new(key)
    }

    /// The key as stored.
    ///
    /// ```
    /// # use moso_storage::StorageKey;
    /// assert_eq!(StorageKey::new("a/b")?.as_str(), "a/b");
    /// # Ok::<(), moso_storage::Error>(())
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Everything before the last separator, or `""` for a top-level key.
    ///
    /// ```
    /// # use moso_storage::StorageKey;
    /// assert_eq!(StorageKey::new("a/b/c.txt")?.prefix(), "a/b");
    /// assert_eq!(StorageKey::new("c.txt")?.prefix(), "");
    /// # Ok::<(), moso_storage::Error>(())
    /// ```
    #[must_use]
    pub fn prefix(&self) -> &str {
        self.0.rfind(SEPARATOR).map_or("", |at| &self.0[..at])
    }

    /// The last segment.
    ///
    /// ```
    /// # use moso_storage::StorageKey;
    /// assert_eq!(StorageKey::new("a/b/c.txt")?.name(), "c.txt");
    /// # Ok::<(), moso_storage::Error>(())
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        self.0
            .rfind(SEPARATOR)
            .map_or(self.0.as_str(), |at| &self.0[at + 1..])
    }

    /// The extension of the last segment, when there is one.
    ///
    /// Returned verbatim rather than lowercased, because the key is the
    /// object's real name and this borrows from it. Compare it with
    /// [`eq_ignore_ascii_case`](str::eq_ignore_ascii_case).
    ///
    /// Used for a `Content-Type` *hint* only. It is never trusted: the type is
    /// decided by [`sniff`](crate::sniff), which reads the bytes.
    ///
    /// ```
    /// # use moso_storage::StorageKey;
    /// let key = StorageKey::new("a/LOGO.PNG")?;
    /// assert!(key.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("png")));
    /// assert_eq!(StorageKey::new("a/logo.png")?.extension(), Some("png"));
    /// assert_eq!(StorageKey::new("a/README")?.extension(), None);
    /// // A leading dot is a hidden file, not an extension.
    /// assert_eq!(StorageKey::new("a/.gitignore")?.extension(), None);
    /// # Ok::<(), moso_storage::Error>(())
    /// ```
    #[must_use]
    pub fn extension(&self) -> Option<&str> {
        let name = self.name();
        let at = name.rfind('.')?;
        (at > 0 && at + 1 < name.len()).then(|| &name[at + 1..])
    }

    /// The key's segments.
    ///
    /// ```
    /// # use moso_storage::StorageKey;
    /// assert_eq!(StorageKey::new("a/b")?.segments().count(), 2);
    /// # Ok::<(), moso_storage::Error>(())
    /// ```
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split(SEPARATOR)
    }

    /// A key with `suffix` appended to the last segment's stem.
    ///
    /// How a variant's key is derived: `photo.jpg` and `Variant::Thumb` give
    /// `photo.thumb.jpg`, so an object and its variants sort together and a
    /// prefix listing finds them in one call.
    ///
    /// # Errors
    ///
    /// [`Error::Key`](crate::Error::Key) when the result would exceed
    /// [`MAX_LENGTH`].
    ///
    /// ```
    /// # use moso_storage::StorageKey;
    /// let key = StorageKey::new("a/photo.jpg")?;
    /// assert_eq!(key.with_suffix("thumb")?.as_str(), "a/photo.thumb.jpg");
    /// // With no extension the suffix simply appends.
    /// assert_eq!(StorageKey::new("a/photo")?.with_suffix("thumb")?.as_str(), "a/photo.thumb");
    /// # Ok::<(), moso_storage::Error>(())
    /// ```
    pub fn with_suffix(&self, suffix: &str) -> Result<Self> {
        let prefix = self.prefix();
        let name = self.name();
        let renamed = match self.extension() {
            Some(extension) => {
                let stem = &name[..name.len() - extension.len() - 1];
                format!("{stem}.{suffix}.{extension}")
            }
            None => format!("{name}.{suffix}"),
        };
        if prefix.is_empty() {
            Self::new(renamed)
        } else {
            Self::new(format!("{prefix}{SEPARATOR}{renamed}"))
        }
    }

    /// A key with a different last segment, keeping the prefix.
    ///
    /// # Errors
    ///
    /// [`Error::Key`](crate::Error::Key) when the result is not a valid key.
    ///
    /// ```
    /// # use moso_storage::StorageKey;
    /// let key = StorageKey::new("uploads/2026/photo.jpg")?;
    /// assert_eq!(key.with_name("thumb.webp")?.as_str(), "uploads/2026/thumb.webp");
    /// # Ok::<(), moso_storage::Error>(())
    /// ```
    pub fn with_name(&self, name: &str) -> Result<Self> {
        let prefix = self.prefix();
        if prefix.is_empty() {
            Self::new(name.to_owned())
        } else {
            Self::new(format!("{prefix}{SEPARATOR}{name}"))
        }
    }

    /// Whether this key is inside `prefix`.
    ///
    /// Segment-aware: `a/bc` is **not** under prefix `a/b`.
    ///
    /// ```
    /// # use moso_storage::StorageKey;
    /// let key = StorageKey::new("uploads/2026/logo.png")?;
    /// assert!(key.is_under("uploads"));
    /// assert!(key.is_under("uploads/2026"));
    /// assert!(!key.is_under("upload"));
    /// assert!(key.is_under(""), "everything is under the empty prefix");
    /// # Ok::<(), moso_storage::Error>(())
    /// ```
    #[must_use]
    pub fn is_under(&self, prefix: &str) -> bool {
        let prefix = prefix.trim_end_matches(SEPARATOR);
        if prefix.is_empty() {
            return true;
        }
        self.0
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(SEPARATOR))
    }
}

impl core::fmt::Display for StorageKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for StorageKey {
    type Error = crate::Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<StorageKey> for String {
    fn from(key: StorageKey) -> Self {
        key.0
    }
}

impl AsRef<str> for StorageKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The local backend joins a key onto a directory, so a key that can
    /// escape it is a file-read primitive.
    #[test]
    fn a_key_cannot_traverse_out_of_its_prefix() {
        for hostile in [
            "../etc/passwd",
            "a/../../etc/passwd",
            "a/./b",
            "/etc/passwd",
            "a//b",
            "a/",
            "",
            "a\\b",
        ] {
            assert!(
                StorageKey::new(hostile).is_err(),
                "`{hostile}` must be refused",
            );
        }
    }

    /// A key with a newline in it is a name nobody can ever type again in a
    /// console, and a NUL truncates on several backends.
    #[test]
    fn a_key_cannot_contain_a_control_character() {
        assert!(StorageKey::new("a\nb").is_err());
        assert!(StorageKey::new("a\u{0}b").is_err());
        assert!(StorageKey::new("a\tb").is_err());
    }

    /// The limit is the smallest of the four backends', so a key that works in
    /// development works in production.
    #[test]
    fn a_key_is_bounded_by_the_smallest_backends_limit() {
        assert!(StorageKey::new("a".repeat(MAX_LENGTH)).is_ok());
        assert!(StorageKey::new("a".repeat(MAX_LENGTH + 1)).is_err());
    }

    /// A user-supplied segment that would add a level is the whole reason
    /// `from_segments` exists.
    #[test]
    fn a_segment_that_would_add_a_level_is_refused() {
        assert!(StorageKey::from_segments(["avatars", "usr/../../etc"]).is_err());
        assert!(StorageKey::from_segments(["avatars", ""]).is_err());
        assert_eq!(
            StorageKey::from_segments(["a", "b", "c.txt"])
                .expect("valid")
                .as_str(),
            "a/b/c.txt",
        );
    }

    /// A variant key sorts next to its original, which is what makes one
    /// prefix listing find both.
    #[test]
    fn a_variant_key_sorts_beside_its_original() {
        let original = StorageKey::new("uploads/photo.jpg").expect("valid");
        let thumb = original.with_suffix("thumb").expect("valid");
        assert_eq!(thumb.as_str(), "uploads/photo.thumb.jpg");
        assert_eq!(thumb.prefix(), original.prefix());
        assert!(thumb.is_under("uploads"));
    }

    /// A suffix that would push the key past the limit fails rather than
    /// producing a key the backend will reject later.
    #[test]
    fn a_suffix_past_the_limit_is_refused() {
        let key = StorageKey::new("a".repeat(MAX_LENGTH - 2)).expect("valid");
        assert!(key.with_suffix("thumbnail").is_err());
    }

    /// `a/bc` is not under `a/b`, and a prefix check that got this wrong would
    /// let one tenant list another's objects.
    #[test]
    fn a_prefix_check_is_segment_aware() {
        let key = StorageKey::new("tenants/acme2/file.txt").expect("valid");
        assert!(key.is_under("tenants"));
        assert!(key.is_under("tenants/acme2"));
        assert!(!key.is_under("tenants/acme"));
        assert!(key.is_under("tenants/"), "a trailing slash is tolerated");
    }

    /// The key is the wire form: it round-trips through JSON and revalidates
    /// on the way back, so a hostile key cannot arrive by deserialisation.
    #[test]
    fn a_key_revalidates_when_it_is_deserialised() {
        let key = StorageKey::new("a/b.txt").expect("valid");
        let json = serde_json::to_string(&key).expect("serialises");
        assert_eq!(json, "\"a/b.txt\"");
        assert_eq!(
            serde_json::from_str::<StorageKey>(&json).expect("deserialises"),
            key,
        );
        assert!(serde_json::from_str::<StorageKey>("\"../etc/passwd\"").is_err());
    }
}
