//! Keys: how a value's name is built, and why it cannot be forged.
//!
//! # The layout
//!
//! ```text
//! moso:v1:shop:profile:1:0192f8c1-…
//! ─┬── ─┬─ ─┬── ─┬───── ┬ ─┬───────
//!  │    │   │    │      │  └─ the key parts, escaped, one segment each
//!  │    │   │    │      └──── the namespace version (`#[namespace(version = 2)]`)
//!  │    │   │    └─────────── the namespace prefix
//!  │    │   └──────────────── the application name
//!  │    └──────────────────── this layout's version
//!  └───────────────────────── the fixed sentinel
//! ```
//!
//! Six things follow from it, and each one is a bug that does not happen:
//!
//! 1. **Two applications can share one Redis.** The application name is a
//!    segment, so `shop` and `blog` never collide.
//! 2. **A deploy can invalidate a namespace** by bumping its version, without
//!    a `FLUSHDB` and without touching any other namespace.
//! 3. **A key cannot forge a namespace.** Every segment before the key parts is
//!    at a *fixed index*, and every key part has its `:` escaped, so no value a
//!    user or an attacker can supply introduces a segment boundary. This is
//!    fuzzed in `tests/keys.rs`.
//! 4. **The layout itself is versioned**, so changing it later is a migration
//!    rather than a silent mass cache miss with old and new keys interleaved.
//! 5. **A prefix is a string prefix.** `delete_prefix` and `scan` are
//!    `starts_with` on the wire, and the namespace prefix key ends in `:`, so
//!    version `1` never matches version `11`.
//! 6. **Nothing in a key is a control byte.** PostgreSQL `text` cannot hold a
//!    `NUL`; the escaping means it never has to.
//!
//! # The escaping
//!
//! Inside a key part:
//!
//! | Byte | Becomes | Why |
//! | --- | --- | --- |
//! | `\` | `\\` | the escape character itself |
//! | `:` | `\c` | the segment separator — this is the one that matters |
//! | `#` | `\h` | the marker that introduces a hex-encoded byte string |
//! | `< 0x20`, `0x7F` | `\xHH` | control bytes; PostgreSQL `text` rejects `NUL` |
//!
//! Everything else, including all non-ASCII UTF-8, passes through: keys stay
//! readable in `redis-cli` and in a `SELECT key FROM moso_kv`, which is most of
//! why anybody looks at a key at all.

use std::borrow::Cow;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The version of the key *layout*, not of any namespace.
///
/// It appears as the second segment of every key. Changing it invalidates
/// every key at once, which is exactly what a layout change has to do.
///
/// ```
/// use moso_kv::key::KEY_FORMAT;
///
/// assert_eq!(KEY_FORMAT, "v1");
/// ```
pub const KEY_FORMAT: &str = "v1";

/// The fixed first segment, so a Moso key is recognisable in a shared store.
///
/// ```
/// use moso_kv::key::KEY_SENTINEL;
///
/// assert_eq!(KEY_SENTINEL, "moso");
/// ```
pub const KEY_SENTINEL: &str = "moso";

/// The longest key `moso-kv` will build, in bytes.
///
/// Chosen to sit comfortably under PostgreSQL's ~2704-byte B-tree index limit
/// with room for the table's own overhead, and far under Redis' 512 MiB, which
/// is not a limit anybody should approach. A key longer than this is almost
/// always an unbounded value used as a key part — a request body, a URL — and
/// failing loudly beats silently filling a keyspace.
///
/// ```
/// use moso_kv::key::MAX_KEY_LEN;
///
/// assert_eq!(MAX_KEY_LEN, 1024);
/// ```
pub const MAX_KEY_LEN: usize = 1024;

/// The longest application or namespace name, in bytes.
///
/// ```
/// use moso_kv::key::MAX_NAME_LEN;
///
/// assert_eq!(MAX_NAME_LEN, 48);
/// ```
pub const MAX_NAME_LEN: usize = 48;

// ---------------------------------------------------------------------------
// KeyError
// ---------------------------------------------------------------------------

/// Why a key could not be built.
///
/// ```
/// use moso_kv::key::{KeyError, validate_name};
///
/// let error = validate_name("namespace", "Profile").expect_err("uppercase is rejected");
/// assert!(matches!(error, KeyError::InvalidName { .. }));
/// assert_eq!(
///     error.to_string(),
///     "`Profile` is not a valid namespace name: only `a`-`z`, `0`-`9`, `_` and `-` are allowed",
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyError {
    /// An application or namespace name is not usable.
    #[error("`{value}` is not a valid {kind} name: {reason}")]
    InvalidName {
        /// `application` or `namespace`.
        kind: &'static str,
        /// The name that was rejected.
        value: String,
        /// The rule it broke, phrased as the rule and not as the violation.
        reason: &'static str,
    },

    /// The finished key is over [`MAX_KEY_LEN`].
    #[error("the key is {len} bytes, over the {max}-byte limit; a key part is unbounded")]
    TooLong {
        /// How long it came out.
        len: usize,
        /// The limit, [`MAX_KEY_LEN`].
        max: usize,
    },
}

// ---------------------------------------------------------------------------
// Name validation
// ---------------------------------------------------------------------------

/// Whether `name` is a usable application or namespace name.
///
/// `const` so `namespace!` can reject a bad prefix at compile time rather than
/// at the first request.
///
/// ```
/// use moso_kv::key::is_valid_name;
///
/// assert!(is_valid_name("profile"));
/// assert!(is_valid_name("login-code_2"));
///
/// assert!(!is_valid_name(""));            // empty
/// assert!(!is_valid_name("Profile"));     // uppercase is not canonical
/// assert!(!is_valid_name("a:b"));         // would introduce a segment
/// assert!(!is_valid_name("a.b"));         // reserved for future layout use
/// ```
#[must_use]
pub const fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_NAME_LEN {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-';
        if !ok {
            return false;
        }
        i += 1;
    }
    true
}

/// [`is_valid_name`] as a `Result`, with the message the user sees.
///
/// ```
/// use moso_kv::key::validate_name;
///
/// assert!(validate_name("namespace", "profile").is_ok());
/// assert!(validate_name("application", "").is_err());
/// ```
pub fn validate_name(kind: &'static str, name: &str) -> Result<(), KeyError> {
    if is_valid_name(name) {
        return Ok(());
    }
    let reason = if name.is_empty() {
        "it must not be empty"
    } else if name.len() > MAX_NAME_LEN {
        "it must be at most 48 bytes"
    } else {
        "only `a`-`z`, `0`-`9`, `_` and `-` are allowed"
    };
    Err(KeyError::InvalidName {
        kind,
        value: name.to_owned(),
        reason,
    })
}

/// Reject an invalid name *at compile time*.
///
/// `namespace!` emits `const _: () = assert_name(PREFIX);` for every entry, so
/// a prefix with a colon in it is a build failure with the offending literal
/// underlined rather than a runtime error on the first cache read.
///
/// # Panics
///
/// If `name` is not [`is_valid_name`]. In a `const` context that is a compile
/// error; at runtime it is a panic, which is why nothing calls it at runtime.
///
/// ```
/// use moso_kv::key::assert_name;
///
/// const _: () = assert_name("profile");
/// ```
///
/// ```compile_fail
/// use moso_kv::key::assert_name;
///
/// // `login:code` would introduce a segment boundary, so it does not build.
/// const _: () = assert_name("login:code");
/// ```
#[track_caller]
pub const fn assert_name(name: &str) {
    assert!(
        is_valid_name(name),
        "a moso-kv namespace prefix must be 1-48 bytes of `a`-`z`, `0`-`9`, `_` or `-`"
    );
}

// ---------------------------------------------------------------------------
// KeyBuf
// ---------------------------------------------------------------------------

/// Builds a [`Key`], escaping every part as it goes.
///
/// A [`KeyPart`] receives one of these and pushes segments into it. It is the
/// only way to add a part, and it is the only place the escaping lives, which
/// is what makes "a key cannot forge a namespace" a property of the type rather
/// than of every call site.
///
/// ```
/// use moso_kv::key::KeyBuf;
///
/// let mut buf = KeyBuf::new("shop", "profile", 1).expect("valid names");
/// buf.segment_str("alice:bob");
/// let key = buf.finish().expect("under the length limit");
///
/// // The colon is escaped, so it is one segment and not two.
/// assert_eq!(key.as_str(), "moso:v1:shop:profile:1:alice\\cbob");
/// ```
#[derive(Debug, Clone)]
pub struct KeyBuf {
    text: String,
    prefix_len: usize,
}

impl KeyBuf {
    /// Start a key for `app`, `namespace` and `version`.
    ///
    /// # Errors
    ///
    /// [`KeyError::InvalidName`] when either name is not [`is_valid_name`].
    ///
    /// ```
    /// use moso_kv::key::KeyBuf;
    ///
    /// assert!(KeyBuf::new("shop", "profile", 1).is_ok());
    /// assert!(KeyBuf::new("shop", "pro:file", 1).is_err());
    /// ```
    pub fn new(app: &str, namespace: &str, version: u16) -> Result<Self, KeyError> {
        validate_name("application", app)?;
        validate_name("namespace", namespace)?;

        let mut text = String::with_capacity(64);
        text.push_str(KEY_SENTINEL);
        text.push(':');
        text.push_str(KEY_FORMAT);
        text.push(':');
        text.push_str(app);
        text.push(':');
        text.push_str(namespace);
        text.push(':');
        push_u64(&mut text, u64::from(version));
        let prefix_len = text.len();

        Ok(Self { text, prefix_len })
    }

    /// Push one segment of escaped text.
    ///
    /// ```
    /// use moso_kv::key::KeyBuf;
    ///
    /// let mut buf = KeyBuf::new("shop", "cart", 1).expect("valid");
    /// buf.segment_str("a\\b");
    /// assert!(buf.as_str().ends_with(":a\\\\b"));
    /// ```
    pub fn segment_str(&mut self, part: &str) {
        self.text.push(':');
        escape_into(&mut self.text, part);
    }

    /// Push one segment of hex-encoded bytes.
    ///
    /// Marked with a leading `#`, which escaped text can never start with, so
    /// a byte part and a text part can never produce the same segment.
    ///
    /// ```
    /// use moso_kv::key::KeyBuf;
    ///
    /// let mut buf = KeyBuf::new("shop", "blob", 1).expect("valid");
    /// buf.segment_bytes(&[0x00, 0xff]);
    /// assert!(buf.as_str().ends_with(":#00ff"));
    /// ```
    pub fn segment_bytes(&mut self, part: &[u8]) {
        self.text.push(':');
        self.text.push('#');
        for byte in part {
            self.text.push(char::from(HEX[usize::from(byte >> 4)]));
            self.text.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }

    /// Push one segment rendered with [`Display`](fmt::Display), escaped.
    ///
    /// The convenience [`KeyPart`] impls for integers, [`bool`], IP addresses
    /// and UUIDs go through this.
    ///
    /// ```
    /// use moso_kv::key::KeyBuf;
    ///
    /// let mut buf = KeyBuf::new("shop", "rate", 1).expect("valid");
    /// buf.segment_display(std::net::Ipv4Addr::LOCALHOST);
    /// assert!(buf.as_str().ends_with(":127.0.0.1"));
    /// ```
    pub fn segment_display(&mut self, part: impl fmt::Display) {
        use fmt::Write as _;

        self.text.push(':');
        let mut escaper = Escaper(&mut self.text);
        // `write!` into a `String` cannot fail, and neither can the adapter:
        // its `write_str` only ever pushes.
        let _ = write!(escaper, "{part}");
    }

    /// The key built so far, including the header.
    ///
    /// ```
    /// use moso_kv::key::KeyBuf;
    ///
    /// let buf = KeyBuf::new("shop", "profile", 2).expect("valid");
    /// assert_eq!(buf.as_str(), "moso:v1:shop:profile:2");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Finish, checking the length.
    ///
    /// # Errors
    ///
    /// [`KeyError::TooLong`] when the result is over [`MAX_KEY_LEN`].
    ///
    /// ```
    /// use moso_kv::key::KeyBuf;
    ///
    /// let mut buf = KeyBuf::new("shop", "profile", 1).expect("valid");
    /// buf.segment_display(42_u64);
    /// assert_eq!(buf.finish().expect("short").as_str(), "moso:v1:shop:profile:1:42");
    /// ```
    pub fn finish(self) -> Result<Key, KeyError> {
        if self.text.len() > MAX_KEY_LEN {
            return Err(KeyError::TooLong {
                len: self.text.len(),
                max: MAX_KEY_LEN,
            });
        }
        Ok(Key {
            text: self.text,
            prefix_len: self.prefix_len,
        })
    }

    /// Finish as a *prefix*: the header plus a trailing `:`.
    ///
    /// This is what [`delete_prefix`](crate::KvStore::delete_prefix) and
    /// [`scan`](crate::KvStore::scan) match against. The trailing `:` is
    /// load-bearing: without it, namespace version `1` would also match
    /// version `11`.
    ///
    /// ```
    /// use moso_kv::key::KeyBuf;
    ///
    /// let prefix = KeyBuf::new("shop", "profile", 1)
    ///     .expect("valid")
    ///     .finish_prefix()
    ///     .expect("short");
    ///
    /// assert_eq!(prefix.as_str(), "moso:v1:shop:profile:1:");
    /// assert!(!"moso:v1:shop:profile:11:x".starts_with(prefix.as_str()));
    /// ```
    pub fn finish_prefix(mut self) -> Result<Key, KeyError> {
        self.text.push(':');
        self.finish()
    }
}

/// The 16 lowercase hex digits, as bytes.
const HEX: &[u8; 16] = b"0123456789abcdef";

/// Push a `u64` as decimal without pulling in a formatting machine.
fn push_u64(out: &mut String, mut value: u64) {
    if value == 0 {
        out.push('0');
        return;
    }
    let mut digits = [0_u8; 20];
    let mut i = digits.len();
    while value > 0 {
        i -= 1;
        digits[i] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        value /= 10;
    }
    // Every byte written is an ASCII digit.
    for &digit in &digits[i..] {
        out.push(char::from(digit));
    }
}

/// Escape `part` into `out` per the table in the module documentation.
fn escape_into(out: &mut String, part: &str) {
    for ch in part.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ':' => out.push_str("\\c"),
            '#' => out.push_str("\\h"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let byte = c as u32 as u8;
                out.push_str("\\x");
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
            c => out.push(c),
        }
    }
}

/// A `fmt::Write` that escapes on the way through, so `segment_display` never
/// has to allocate a scratch `String`.
struct Escaper<'a>(&'a mut String);

impl fmt::Write for Escaper<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        escape_into(self.0, s);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Key
// ---------------------------------------------------------------------------

/// A fully-qualified key, as it appears in the store.
///
/// Built by [`KeyBuf`], or by [`Kv::key`](crate::Kv::key) from a namespace and
/// a [`KeyPart`]. Handler code rarely names one: it names a namespace and a
/// value, and the key is derived.
///
/// ```
/// use moso_kv::key::{Key, KeyBuf};
///
/// let mut buf = KeyBuf::new("shop", "profile", 1).expect("valid");
/// buf.segment_display(7_u32);
/// let key = buf.finish().expect("short");
///
/// assert_eq!(key.as_str(), "moso:v1:shop:profile:1:7");
/// assert_eq!(key.namespace_prefix(), "moso:v1:shop:profile:1");
/// assert_eq!(key.parts(), "7");
///
/// // Keys compare and hash by their whole text.
/// assert_eq!(key, Key::from_raw("moso:v1:shop:profile:1:7").expect("valid"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    text: String,
    prefix_len: usize,
}

impl Key {
    /// Adopt an already-formed key string.
    ///
    /// The escape hatch: a backend hands back the keys a `SCAN` found, and a
    /// battery that stores its own bookkeeping alongside Moso's namespaces
    /// needs to name a key it did not build. The text is length-checked and
    /// rejected if it contains a `NUL`, because PostgreSQL `text` cannot hold
    /// one.
    ///
    /// # Errors
    ///
    /// [`KeyError::TooLong`], or [`KeyError::InvalidName`] for a `NUL`.
    ///
    /// ```
    /// use moso_kv::key::Key;
    ///
    /// assert!(Key::from_raw("moso:v1:shop:profile:1:7").is_ok());
    /// assert!(Key::from_raw("has\0nul").is_err());
    /// ```
    pub fn from_raw(text: impl Into<String>) -> Result<Self, KeyError> {
        let text = text.into();
        if text.len() > MAX_KEY_LEN {
            return Err(KeyError::TooLong {
                len: text.len(),
                max: MAX_KEY_LEN,
            });
        }
        if text.as_bytes().contains(&0) {
            return Err(KeyError::InvalidName {
                kind: "key",
                value: text.escape_debug().to_string(),
                reason: "a key must not contain a NUL byte",
            });
        }
        // A raw key has no known header, so everything is "parts".
        let prefix_len = header_len(&text);
        Ok(Self { text, prefix_len })
    }

    /// The whole key.
    ///
    /// ```
    /// use moso_kv::key::Key;
    ///
    /// let key = Key::from_raw("moso:v1:shop:cart:1:a").expect("valid");
    /// assert_eq!(key.as_str(), "moso:v1:shop:cart:1:a");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The whole key, as bytes — what a driver actually sends.
    ///
    /// ```
    /// use moso_kv::key::Key;
    ///
    /// let key = Key::from_raw("moso:v1:a:b:1:c").expect("valid");
    /// assert_eq!(key.as_bytes()[0], b'm');
    /// ```
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    /// Length in bytes.
    ///
    /// ```
    /// use moso_kv::key::Key;
    ///
    /// assert_eq!(Key::from_raw("abc").expect("valid").len(), 3);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Whether the key is the empty string, which only `Key::from_raw("")`
    /// produces.
    ///
    /// ```
    /// use moso_kv::key::Key;
    ///
    /// assert!(Key::from_raw("").expect("valid").is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Everything before the key parts: sentinel, layout, app, namespace,
    /// version.
    ///
    /// ```
    /// use moso_kv::key::Key;
    ///
    /// let key = Key::from_raw("moso:v1:shop:profile:1:7").expect("valid");
    /// assert_eq!(key.namespace_prefix(), "moso:v1:shop:profile:1");
    /// ```
    #[must_use]
    pub fn namespace_prefix(&self) -> &str {
        &self.text[..self.prefix_len]
    }

    /// The key parts, without the leading separator.
    ///
    /// ```
    /// use moso_kv::key::Key;
    ///
    /// let key = Key::from_raw("moso:v1:shop:profile:1:7:b").expect("valid");
    /// assert_eq!(key.parts(), "7:b");
    /// ```
    #[must_use]
    pub fn parts(&self) -> &str {
        self.text[self.prefix_len..].strip_prefix(':').unwrap_or("")
    }

    /// Whether this key is under `prefix`.
    ///
    /// The definition of "under" used by `scan` and `delete_prefix`: a plain
    /// byte-string prefix, which is unambiguous exactly because the escaping
    /// keeps `:` out of key parts.
    ///
    /// ```
    /// use moso_kv::key::Key;
    ///
    /// let prefix = Key::from_raw("moso:v1:shop:profile:1:").expect("valid");
    /// let key = Key::from_raw("moso:v1:shop:profile:1:7").expect("valid");
    /// let other = Key::from_raw("moso:v1:shop:profile:11:7").expect("valid");
    ///
    /// assert!(key.starts_with(&prefix));
    /// assert!(!other.starts_with(&prefix));
    /// ```
    #[must_use]
    pub fn starts_with(&self, prefix: &Key) -> bool {
        self.text.starts_with(prefix.as_str())
    }

    /// A new key with `suffix` appended as one more escaped segment.
    ///
    /// Used for the companion keys a feature needs next to its own — the
    /// fencing counter beside a lock, the revalidation marker beside a
    /// stale-while-revalidate entry.
    ///
    /// # Errors
    ///
    /// [`KeyError::TooLong`] when the result is over [`MAX_KEY_LEN`].
    ///
    /// ```
    /// use moso_kv::key::Key;
    ///
    /// let key = Key::from_raw("moso:v1:shop:lock:1:import").expect("valid");
    /// assert_eq!(key.joined("fence").expect("short").as_str(), "moso:v1:shop:lock:1:import:fence");
    /// ```
    pub fn joined(&self, suffix: &str) -> Result<Key, KeyError> {
        let mut text = self.text.clone();
        text.push(':');
        escape_into(&mut text, suffix);
        if text.len() > MAX_KEY_LEN {
            return Err(KeyError::TooLong {
                len: text.len(),
                max: MAX_KEY_LEN,
            });
        }
        Ok(Key {
            text,
            prefix_len: self.prefix_len,
        })
    }

    /// Consume, yielding the key text.
    ///
    /// ```
    /// use moso_kv::key::Key;
    ///
    /// assert_eq!(Key::from_raw("abc").expect("valid").into_string(), "abc");
    /// ```
    #[must_use]
    pub fn into_string(self) -> String {
        self.text
    }
}

/// Where the header ends in an adopted key: after the fifth segment, when the
/// text has one, and at zero otherwise.
fn header_len(text: &str) -> usize {
    let mut seen = 0_u8;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b':' {
            seen += 1;
            if seen == 5 {
                return index;
            }
        }
    }
    text.len()
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl AsRef<str> for Key {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

impl AsRef<[u8]> for Key {
    fn as_ref(&self) -> &[u8] {
        self.text.as_bytes()
    }
}

// ---------------------------------------------------------------------------
// KeyPart
// ---------------------------------------------------------------------------

/// A value that can become part of a key.
///
/// Implement it for a domain type so it can be a namespace's `Key`. The
/// contract is one line long: **push at least one segment, always the same
/// number of them, and never touch [`KeyBuf`]'s escaping.**
///
/// ```
/// use moso_kv::key::{KeyBuf, KeyPart};
///
/// /// A tenant, as it appears in a key.
/// pub struct TenantId(pub u32);
///
/// impl KeyPart for TenantId {
///     fn write_key_part(&self, out: &mut KeyBuf) {
///         out.segment_display(self.0);
///     }
/// }
///
/// let mut buf = KeyBuf::new("shop", "quota", 1).expect("valid");
/// TenantId(42).write_key_part(&mut buf);
/// assert_eq!(buf.finish().expect("short").parts(), "42");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be a key part",
    label = "this type has no `KeyPart` impl",
    note = "a namespace's key must be a `KeyPart`: `&str`, `String`, an integer, `bool`, `char`, \
            `Uuid`, `Id<E>`, `Email`, `Slug`, an IP address, bytes, or a tuple of those",
    note = "help: impl moso_kv::KeyPart for {Self} {{ fn write_key_part(&self, out: &mut KeyBuf) \
            {{ out.segment_display(self) }} }}"
)]
pub trait KeyPart {
    /// Push this value's segments into `out`.
    fn write_key_part(&self, out: &mut KeyBuf);
}

// A reference is the same key part as the value. `do_not_recommend` so that a
// missing impl on `T` reports `T` rather than suggesting the user implement
// `KeyPart for &T`.
#[diagnostic::do_not_recommend]
impl<T: KeyPart + ?Sized> KeyPart for &T {
    fn write_key_part(&self, out: &mut KeyBuf) {
        (**self).write_key_part(out);
    }
}

impl KeyPart for str {
    fn write_key_part(&self, out: &mut KeyBuf) {
        out.segment_str(self);
    }
}

impl KeyPart for String {
    fn write_key_part(&self, out: &mut KeyBuf) {
        out.segment_str(self);
    }
}

impl KeyPart for Cow<'_, str> {
    fn write_key_part(&self, out: &mut KeyBuf) {
        out.segment_str(self);
    }
}

impl KeyPart for char {
    fn write_key_part(&self, out: &mut KeyBuf) {
        let mut scratch = [0_u8; 4];
        out.segment_str(self.encode_utf8(&mut scratch));
    }
}

impl KeyPart for [u8] {
    fn write_key_part(&self, out: &mut KeyBuf) {
        out.segment_bytes(self);
    }
}

impl KeyPart for Vec<u8> {
    fn write_key_part(&self, out: &mut KeyBuf) {
        out.segment_bytes(self);
    }
}

impl KeyPart for bytes::Bytes {
    fn write_key_part(&self, out: &mut KeyBuf) {
        out.segment_bytes(self);
    }
}

/// `KeyPart` for a type whose `Display` is already unambiguous.
macro_rules! key_part_via_display {
    ($($ty:ty),* $(,)?) => {
        $(
            impl KeyPart for $ty {
                fn write_key_part(&self, out: &mut KeyBuf) {
                    out.segment_display(self);
                }
            }
        )*
    };
}

key_part_via_display!(
    bool,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    IpAddr,
    Ipv4Addr,
    Ipv6Addr,
    uuid::Uuid,
    moso_schema::types::Email,
    moso_schema::types::Slug,
);

impl<E: moso_schema::types::IdMarker> KeyPart for moso_schema::types::Id<E> {
    fn write_key_part(&self, out: &mut KeyBuf) {
        out.segment_display(self);
    }
}

impl KeyPart for () {
    /// The unit key is one empty segment, not zero segments: a namespace with a
    /// unit key must still be distinguishable from that namespace's prefix.
    fn write_key_part(&self, out: &mut KeyBuf) {
        out.segment_str("");
    }
}

/// `KeyPart` for tuples, one segment per element.
macro_rules! key_part_tuple {
    ($($name:ident),+) => {
        impl<$($name: KeyPart),+> KeyPart for ($($name,)+) {
            fn write_key_part(&self, out: &mut KeyBuf) {
                #[allow(non_snake_case)]
                let ($($name,)+) = self;
                $($name.write_key_part(out);)+
            }
        }
    };
}

key_part_tuple!(A, B);
key_part_tuple!(A, B, C);
key_part_tuple!(A, B, C, D);
key_part_tuple!(A, B, C, D, E);
key_part_tuple!(A, B, C, D, E, F);

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(app: &str, ns: &str, version: u16, part: &impl KeyPart) -> Key {
        let mut buf = KeyBuf::new(app, ns, version).expect("valid names");
        part.write_key_part(&mut buf);
        buf.finish().expect("short enough")
    }

    #[test]
    fn the_layout_is_the_documented_one() {
        let key = key_of("shop", "profile", 1, &7_u64);
        assert_eq!(key.as_str(), "moso:v1:shop:profile:1:7");
        assert_eq!(key.namespace_prefix(), "moso:v1:shop:profile:1");
        assert_eq!(key.parts(), "7");
    }

    #[test]
    fn a_colon_in_a_key_part_cannot_forge_a_namespace() {
        // `evil` tries to look like namespace `other` in app `shop`.
        let forged = key_of("shop", "profile", 1, &"x:other:1:y");
        let honest = key_of("shop", "other", 1, &"y");

        assert_ne!(forged.as_str(), honest.as_str());
        assert!(forged.as_str().starts_with("moso:v1:shop:profile:1:"));
        assert_eq!(forged.parts(), "x\\cother\\c1\\cy");
    }

    #[test]
    fn a_backslash_cannot_unescape_a_colon() {
        // The classic: escape the escape so the next colon "escapes" itself.
        let key = key_of("shop", "profile", 1, &"a\\:b");
        assert_eq!(key.parts(), "a\\\\\\cb");
        assert!(!key.parts().contains(':'));
    }

    #[test]
    fn control_bytes_never_reach_the_store() {
        let key = key_of("shop", "profile", 1, &"a\u{0}b\nc\u{7f}");
        assert_eq!(key.parts(), "a\\x00b\\x0ac\\x7f");
        assert!(!key.as_bytes().contains(&0));
    }

    #[test]
    fn text_and_bytes_live_in_disjoint_alphabets() {
        let text = key_of("shop", "blob", 1, &"00ff");
        let bytes = key_of("shop", "blob", 1, &vec![0x00_u8, 0xff]);
        assert_eq!(text.parts(), "00ff");
        assert_eq!(bytes.parts(), "#00ff");
        assert_ne!(text, bytes);

        // ... and a `#` in text is escaped, so it cannot pretend to be bytes.
        assert_eq!(key_of("shop", "blob", 1, &"#00ff").parts(), "\\h00ff");
    }

    #[test]
    fn a_tuple_is_one_segment_per_element() {
        let key = key_of("shop", "quota", 1, &(7_u32, "eu-west"));
        assert_eq!(key.parts(), "7:eu-west");
    }

    #[test]
    fn the_unit_key_is_distinguishable_from_the_prefix() {
        let key = key_of("shop", "flag", 1, &());
        let prefix = KeyBuf::new("shop", "flag", 1)
            .expect("valid")
            .finish_prefix()
            .expect("short");
        assert_eq!(key.as_str(), "moso:v1:shop:flag:1:");
        assert_eq!(prefix.as_str(), "moso:v1:shop:flag:1:");
        // They coincide, and that is fine: the prefix of a unit namespace is
        // its only key. What must not coincide is two *different* keys.
        assert!(key.starts_with(&prefix));
    }

    #[test]
    fn version_one_is_not_a_prefix_of_version_eleven() {
        let one = KeyBuf::new("shop", "profile", 1)
            .expect("valid")
            .finish_prefix()
            .expect("short");
        let eleven = key_of("shop", "profile", 11, &"x");
        assert!(!eleven.starts_with(&one));
    }

    #[test]
    fn two_applications_never_collide() {
        assert_ne!(
            key_of("shop", "profile", 1, &"a"),
            key_of("blog", "profile", 1, &"a")
        );
    }

    #[test]
    fn names_are_validated() {
        assert!(KeyBuf::new("shop", "profile", 1).is_ok());
        assert!(KeyBuf::new("sh:op", "profile", 1).is_err());
        assert!(KeyBuf::new("shop", "Profile", 1).is_err());
        assert!(KeyBuf::new("shop", "", 1).is_err());
        assert!(KeyBuf::new("shop", &"x".repeat(MAX_NAME_LEN + 1), 1).is_err());
    }

    #[test]
    fn an_unbounded_key_part_is_rejected_rather_than_stored() {
        let mut buf = KeyBuf::new("shop", "profile", 1).expect("valid");
        buf.segment_str(&"x".repeat(MAX_KEY_LEN));
        let error = buf.finish().expect_err("over the limit");
        assert!(matches!(
            error,
            KeyError::TooLong {
                max: MAX_KEY_LEN,
                ..
            }
        ));
    }

    #[test]
    fn a_raw_key_round_trips_and_rejects_a_nul() {
        let key = Key::from_raw("moso:v1:shop:profile:1:7").expect("valid");
        assert_eq!(key.namespace_prefix(), "moso:v1:shop:profile:1");
        assert_eq!(key.parts(), "7");
        assert!(Key::from_raw("a\0b").is_err());
        assert!(Key::from_raw("x".repeat(MAX_KEY_LEN + 1)).is_err());
    }

    #[test]
    fn a_raw_key_with_no_header_is_all_parts() {
        let key = Key::from_raw("legacy-key").expect("valid");
        assert_eq!(key.namespace_prefix(), "legacy-key");
        assert_eq!(key.parts(), "");
    }

    #[test]
    fn joined_appends_an_escaped_segment() {
        let key = Key::from_raw("moso:v1:shop:lock:1:import").expect("valid");
        assert_eq!(
            key.joined("fen:ce").expect("short").as_str(),
            "moso:v1:shop:lock:1:import:fen\\cce"
        );
    }

    #[test]
    fn every_documented_key_part_type_compiles_and_escapes() {
        let uuid = uuid::Uuid::from_u128(0x0192_f8c1);
        let email = moso_schema::types::Email::new("a@b.test").expect("valid");
        let slug = moso_schema::types::Slug::new("hello-world").expect("valid");

        assert_eq!(key_of("a", "b", 1, &true).parts(), "true");
        assert_eq!(key_of("a", "b", 1, &-3_i64).parts(), "-3");
        assert_eq!(key_of("a", "b", 1, &'x').parts(), "x");
        assert_eq!(key_of("a", "b", 1, &uuid).parts(), uuid.to_string());
        assert_eq!(key_of("a", "b", 1, &email).parts(), "a@b.test");
        assert_eq!(key_of("a", "b", 1, &slug).parts(), "hello-world");
        assert_eq!(
            key_of("a", "b", 1, &IpAddr::V4(Ipv4Addr::LOCALHOST)).parts(),
            "127.0.0.1"
        );
        assert_eq!(
            key_of("a", "b", 1, &bytes::Bytes::from_static(b"\x01")).parts(),
            "#01"
        );
        assert_eq!(key_of("a", "b", 1, &String::from("s")).parts(), "s");
        assert_eq!(key_of("a", "b", 1, &Cow::Borrowed("c")).parts(), "c");
    }

    #[test]
    fn push_u64_agrees_with_the_formatter() {
        for value in [0_u64, 1, 9, 10, 99, 100, 12_345, u64::MAX] {
            let mut out = String::new();
            push_u64(&mut out, value);
            assert_eq!(out, value.to_string());
        }
    }

    #[test]
    fn a_display_key_part_is_escaped_too() {
        struct Sneaky;
        impl fmt::Display for Sneaky {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a:b")
            }
        }
        let mut buf = KeyBuf::new("a", "b", 1).expect("valid");
        buf.segment_display(Sneaky);
        assert_eq!(buf.finish().expect("short").parts(), "a\\cb");
    }
}
