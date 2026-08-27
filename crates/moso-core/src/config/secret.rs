//! Secrets that cannot be logged by accident.
//!
//! A `String` holding a database password is indistinguishable from any other
//! `String`, so it ends up in a `Debug` line, a `tracing` field, a serialised
//! error, or a crash dump. [`SecretString`] makes each of those a compile error
//! or a redaction, and makes reading the value a call that says what it is
//! doing.
//!
//! ```
//! use moso::config::prelude::*;
//!
//! /// Everything this application reads from its environment.
//! #[derive(moso::Config, Debug)]
//! pub struct AppConfig {
//!     /// Signing key; never logged.
//!     #[config(secret)]
//!     pub secret_key: SecretString,
//! }
//!
//! # fn main() {
//! let cfg = AppConfig { secret_key: SecretString::from("hunter2") };
//!
//! let key = cfg.secret_key.expose();          // deliberately verbose
//! assert_eq!(key, "hunter2");
//!
//! // Every rendering redacts, including the one a `tracing` field would take.
//! assert_eq!(format!("{:?}", cfg.secret_key), "SecretString(***)");
//! assert!(!format!("{cfg:?}").contains("hunter2"));
//! # }
//! ```
//!
//! `#[config(secret)]` on a `String` field is a compile error suggesting
//! `SecretString`, because the attribute without the type is a comment. The
//! derive proves it by asserting [`SecretValue`], which only [`SecretString`]
//! and [`SecretBytes`] implement.
//!
//! # What this does and does not promise
//!
//! It promises: no `Debug`, no `Display`, no `Serialize`, no `tracing` field
//! ever renders the value; the buffer is zeroed on drop.
//!
//! It does not promise the value never reaches swap or a core dump. Memory
//! locking is an operating-system-level control and pretending a type provides
//! it would be dishonest. Nor does it promise that a *copy* you made with
//! [`expose`](SecretString::expose) is zeroed — that copy is yours.
//!
//! # The redaction canary
//!
//! `secret_never_leaks_through_any_rendering` in this module's tests formats a
//! secret through every path a value can escape by — `Debug`, `Display`,
//! `serde_json`, a `tracing` field — and greps for the plaintext. Adding a
//! rendering without adding it to that test is the mistake the test exists to
//! catch.

use std::fmt;

use zeroize::Zeroize;

use crate::BoxFuture;
use crate::error::{Error, Result};

/// What a redacted secret renders as, everywhere.
pub const REDACTED: &str = "***";

// ---------------------------------------------------------------------------
// SecretValue
// ---------------------------------------------------------------------------

/// The marker `#[config(secret)]` asserts.
///
/// Sealed: implemented for [`SecretString`] and [`SecretBytes`] and nothing
/// else, so a field marked secret provably *is* one. The derive emits
///
/// ```text
/// const _: fn() = || {
///     fn assert_secret<T: ::moso::__private::SecretValue>() {}
///     assert_secret::<String>();   // ← the user's field type
/// };
/// ```
///
/// and this trait's `on_unimplemented` note is what the user reads. The two
/// types that satisfy it:
///
/// ```
/// use moso::config::{SecretBytes, SecretString};
/// use moso_core::config::secret::SecretValue;
///
/// fn assert_secret<T: SecretValue>() {}
///
/// assert_secret::<SecretString>();
/// assert_secret::<SecretBytes>();
/// // `assert_secret::<String>()` does not compile.
/// ```
#[diagnostic::on_unimplemented(
    message = "`#[config(secret)]` needs a secret type, but this field is `{Self}`",
    label = "not a secret type",
    note = "help: change the field's type to `SecretString`
    #[config(secret)]
    pub secret_key: SecretString,",
    note = "a `String` holding a password is indistinguishable from any other `String`: it reaches \
            a `Debug` line, a tracing field, a serialised error and a core dump, and the attribute \
            alone cannot stop it",
    note = "`SecretBytes` is the same thing for binary key material"
)]
pub trait SecretValue: sealed::Sealed + Sized {
    /// The redacted rendering, so a generic printer never has to special-case.
    fn redacted(&self) -> &'static str {
        REDACTED
    }

    /// The length of the secret, which is safe to know.
    fn secret_len(&self) -> usize;
}

mod sealed {
    /// Prevents an application from claiming a type is a secret when nothing
    /// redacts it.
    pub trait Sealed {}
    impl Sealed for super::SecretString {}
    impl Sealed for super::SecretBytes {}
}

/// Compare two byte slices without returning early on the first difference.
///
/// Lengths still differ observably, which is not a secret worth protecting
/// here: an attacker who can measure that can also count the characters you
/// typed.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in a.iter().zip(b) {
        difference |= left ^ right;
    }
    difference == 0
}

// ---------------------------------------------------------------------------
// SecretString
// ---------------------------------------------------------------------------

/// A string that will not be printed.
///
/// Zeroed on drop by `zeroize`, whose write is volatile and fenced so the
/// optimiser cannot remove it as dead.
///
/// ```
/// use moso::config::SecretString;
///
/// let key = SecretString::from("hunter2");
///
/// // Reading it is a call that says what it is doing …
/// assert_eq!(key.expose(), "hunter2");
///
/// // … and every rendering redacts, including the one a `tracing` field takes.
/// assert_eq!(format!("{key:?}"), "SecretString(***)");
/// assert_eq!(key.to_string(), "***");
///
/// // Serialising is refused outright, so a secret cannot leave in a response
/// // body even by accident.
/// assert!(serde_json::to_string(&key).is_err());
/// ```
///
/// Comparison is constant-time, so `==` against a shared token does not leak the
/// prefix length through timing.
#[derive(Clone, Default)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value.
    ///
    /// Verbose on purpose: `expose()` at a call site is greppable, and a review
    /// can ask why each one is there.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The value's bytes.
    pub fn expose_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// The length, which is safe to know and occasionally useful for a
    /// "your key is too short" boot check.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the secret is empty. An empty secret is almost always an
    /// unset environment variable rather than an intentional value.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Redact this secret inside a larger string.
    ///
    /// What renders `postgres://user:***@host/db`: the connection string is not
    /// itself secret, one substring of it is.
    ///
    /// A secret shorter than four bytes is not replaced — at that length the
    /// substring appears in ordinary text and redacting it would corrupt the
    /// output while protecting nothing.
    pub fn redact_within(&self, text: &str) -> String {
        if self.0.len() < 4 {
            return text.to_owned();
        }
        text.replace(&self.0, REDACTED)
    }
}

impl SecretValue for SecretString {
    fn secret_len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(***)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        SecretString(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        SecretString(value.to_owned())
    }
}

impl PartialEq for SecretString {
    /// Constant-time within a length class: the comparison does not return
    /// early on the first differing byte. Lengths still differ observably,
    /// which is not a secret worth protecting here.
    fn eq(&self, other: &Self) -> bool {
        constant_time_eq(self.0.as_bytes(), other.0.as_bytes())
    }
}

impl Eq for SecretString {}

impl Drop for SecretString {
    /// Overwrite the buffer with a volatile, fenced write.
    ///
    /// Best effort, and no `unsafe` in this crate; see the module header for
    /// what that does and does not buy.
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl serde::Serialize for SecretString {
    /// Always fails.
    ///
    /// A configuration struct that derives `Serialize` and contains a secret
    /// would otherwise write it into a debug endpoint or a config dump. Failing
    /// loudly at the one call site that wanted it beats leaking quietly at all
    /// of them.
    ///
    /// The fix at the call site is `#[serde(skip)]`, or `#[serde(serialize_with
    /// = "…")]` pointing at something that writes `"***"`.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let _ = serializer;
        Err(serde::ser::Error::custom(
            "a SecretString cannot be serialised; mark the field `#[serde(skip)]`",
        ))
    }
}

impl<'de> serde::Deserialize<'de> for SecretString {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <String as serde::Deserialize>::deserialize(deserializer).map(SecretString)
    }
}

// ---------------------------------------------------------------------------
// SecretBytes
// ---------------------------------------------------------------------------

/// Bytes that will not be printed. [`SecretString`] for binary material.
///
/// ```
/// use moso::config::SecretBytes;
///
/// let key = SecretBytes::from(vec![0xde, 0xad, 0xbe, 0xef]);
///
/// assert_eq!(key.expose(), &[0xde, 0xad, 0xbe, 0xef]);
/// assert_eq!(format!("{key:?}"), "SecretBytes(***)");
/// ```
///
/// [`SecretString`] for text, this for key material — an HMAC key, a private key,
/// an encryption key. Both satisfy `#[config(secret)]`; a `String` or a `Vec<u8>`
/// does not, and the compile error says why.
#[derive(Clone, Default)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    /// Wrap a buffer.
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(value.into())
    }

    /// Decode hex, for a key written as text.
    ///
    /// # Errors
    /// A 400-class [`Error`] when the input has an odd length or a non-hex
    /// character. The message never quotes the input, because the input is the
    /// secret.
    pub fn from_hex(hex: &str) -> Result<Self> {
        let hex = hex.trim();
        if !hex.len().is_multiple_of(2) {
            return Err(Error::bad_request(
                "a hex secret must have an even number of characters",
            ));
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        for pair in hex.as_bytes().as_chunks::<2>().0 {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            bytes.push((high << 4) | low);
        }
        Ok(Self(bytes))
    }

    /// Decode standard base64, with or without `=` padding.
    ///
    /// # Errors
    /// A 400-class [`Error`] when the input is not standard base64. The message
    /// never quotes the input.
    pub fn from_base64(encoded: &str) -> Result<Self> {
        decode_base64(encoded.trim())
            .map(Self)
            .ok_or_else(|| Error::bad_request("a base64 secret was not valid base64"))
    }

    /// The bytes.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The length.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl SecretValue for SecretBytes {
    fn secret_len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBytes(***)")
    }
}

impl fmt::Display for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl From<Vec<u8>> for SecretBytes {
    fn from(value: Vec<u8>) -> Self {
        SecretBytes(value)
    }
}

impl PartialEq for SecretBytes {
    /// As [`SecretString::eq`]: no early return on the first differing byte.
    fn eq(&self, other: &Self) -> bool {
        constant_time_eq(&self.0, &other.0)
    }
}

impl Eq for SecretBytes {}

impl Drop for SecretBytes {
    /// As [`SecretString::drop`].
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl serde::Serialize for SecretBytes {
    /// Always fails, for the reason `Serialize for SecretString` gives: a
    /// redaction would round-trip into `"***"` and be written back somewhere as
    /// if it were the value.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let _ = serializer;
        Err(serde::ser::Error::custom(
            "a SecretBytes cannot be serialised; mark the field `#[serde(skip)]`",
        ))
    }
}

/// One hex digit's value.
fn hex_digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::bad_request(
            "a hex secret contained a character that is not 0-9, a-f or A-F",
        )),
    }
}

/// The standard base64 alphabet, in index order.
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// One base64 character's value.
fn base64_value(byte: u8) -> Option<u32> {
    BASE64
        .iter()
        .position(|candidate| *candidate == byte)
        .map(|index| index as u32)
}

/// Decode standard base64, tolerating missing padding but not other slop.
///
/// Hand-written rather than a dependency: it is thirty lines, it is exercised
/// by the tests below, and a crate in the dependency graph of every Moso
/// application needs a reason stronger than "this saved thirty lines".
fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
    let trimmed = encoded.trim_end_matches('=');
    if trimmed.len() % 4 == 1 {
        return None;
    }

    let mut output = Vec::with_capacity(trimmed.len() * 3 / 4);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;

    for byte in trimmed.bytes() {
        let value = base64_value(byte)?;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((accumulator >> bits) & 0xff) as u8);
        }
    }

    // Whatever is left over must be padding bits, and padding bits are zero.
    // Rejecting non-zero leftovers is what stops two spellings of one key.
    if bits > 0 && (accumulator & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(output)
}

// ---------------------------------------------------------------------------
// SecretProvider
// ---------------------------------------------------------------------------

/// Where a secret comes from, when it does not come from the environment.
///
/// `#[config(secret_from = "file")]` reads `${KEY}_FILE`, which is how Docker
/// and Kubernetes mount secrets. Anything further — Vault, AWS Secrets Manager —
/// is a [`SecretProvider`], so Moso needs no dependency on any of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    /// The configuration key that wants a value.
    pub key: String,
    /// The provider-specific locator: a path, an ARN, a Vault path.
    pub locator: String,
}

impl SecretRef {
    /// A reference for `key`, resolved by `locator`.
    pub fn new(key: impl Into<String>, locator: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            locator: locator.into(),
        }
    }
}

/// Resolves a [`SecretRef`] into a value at boot.
///
/// Dyn-compatible, so an application registers one with
/// `AppBuilder::secret_provider` without Moso knowing what it talks to.
/// [`FileSecretProvider`] is the bundled implementation; write one to reach
/// Vault, AWS Secrets Manager or an internal service.
///
/// ```
/// use moso::config::{SecretProvider, SecretRef, SecretString};
/// use moso::{BoxFuture, Result};
///
/// /// Answers `vault://…` references.
/// pub struct Vault;
///
/// impl SecretProvider for Vault {
///     fn scheme(&self) -> &'static str {
///         "vault"
///     }
///
///     fn resolve<'a>(&'a self, reference: &'a SecretRef) -> BoxFuture<'a, Result<SecretString>> {
///         Box::pin(async move {
///             // A real provider would call out here.
///             Ok(SecretString::from(format!("secret-for-{}", reference.locator)))
///         })
///     }
/// }
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> Result<()> {
/// let reference = SecretRef::new("vault", "kv/data/api-key");
/// let secret = Vault.resolve(&reference).await?;
///
/// assert_eq!(secret.expose(), "secret-for-kv/data/api-key");
/// # Ok(())
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a secret provider",
    note = "implement `resolve(&self, r: &SecretRef) -> BoxFuture<'_, Result<SecretString>>`",
    note = "help: register it with `App::new(cfg).secret_provider(Arc::new({Self}::new(..)))`"
)]
pub trait SecretProvider: Send + Sync + 'static {
    /// The scheme this provider answers for: `file`, `vault`, `aws`.
    fn scheme(&self) -> &'static str;

    /// Fetch the secret.
    ///
    /// Boxed rather than an `async fn` so the trait is dyn-compatible.
    fn resolve<'a>(&'a self, reference: &'a SecretRef) -> BoxFuture<'a, Result<SecretString>>;
}

/// Reads `${KEY}_FILE`, the Docker and Kubernetes secret-mount convention.
///
/// Always available, because the convention is universal and the implementation
/// is a file read.
#[derive(Debug, Default, Clone, Copy)]
pub struct FileSecretProvider;

impl FileSecretProvider {
    /// Read `path`, trimming one trailing newline.
    ///
    /// The synchronous half, so the blocking configuration load can use it too:
    /// a secret file is a few bytes off a tmpfs, and spawning a task to read it
    /// during boot would cost more than the read.
    ///
    /// # Errors
    /// A 500-class [`Error`] naming the *path* and never the contents — a
    /// secret file with a stray byte must not print itself into a boot log.
    pub fn read(path: &str) -> Result<SecretString> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(SecretString::new(trim_one_newline(&contents).to_owned())),
            Err(error) => Err(Error::internal_msg(format!(
                "could not read the secret file `{path}`: {}",
                error.kind()
            ))),
        }
    }
}

impl SecretProvider for FileSecretProvider {
    fn scheme(&self) -> &'static str {
        "file"
    }

    fn resolve<'a>(&'a self, reference: &'a SecretRef) -> BoxFuture<'a, Result<SecretString>> {
        Box::pin(async move { FileSecretProvider::read(&reference.locator) })
    }
}

/// Trim exactly one trailing newline, `\n` or `\r\n`.
///
/// One, not all: a secret may legitimately end in a newline, and `echo -n` is
/// not something a Kubernetes secret manifest can express. Trimming everything
/// would silently corrupt such a value.
fn trim_one_newline(text: &str) -> &str {
    text.strip_suffix('\n')
        .map_or(text, |rest| rest.strip_suffix('\r').unwrap_or(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_are_redacted() {
        let secret = SecretString::new("hunter2");
        assert_eq!(format!("{secret:?}"), "SecretString(***)");
        assert_eq!(format!("{secret}"), "***");
        assert!(!format!("{secret:?}{secret}").contains("hunter2"));
    }

    #[test]
    fn secret_bytes_debug_is_redacted() {
        let secret = SecretBytes::new(vec![1u8, 2, 3]);
        assert_eq!(format!("{secret:?}"), "SecretBytes(***)");
        assert_eq!(format!("{secret}"), "***");
        assert_eq!(secret.len(), 3);
    }

    /// The canary the acceptance criteria name.
    ///
    /// Every rendering a value can escape through, one plaintext to grep for.
    #[test]
    fn secret_never_leaks_through_any_rendering() {
        const CANARY: &str = "correct-horse-battery-staple";

        let secret = SecretString::new(CANARY);
        let bytes = SecretBytes::new(CANARY.as_bytes().to_vec());

        let renderings = [
            format!("{secret:?}"),
            format!("{secret}"),
            format!("{secret:#?}"),
            format!("{bytes:?}"),
            format!("{bytes}"),
            // A tracing field renders through `Debug`/`Display`, so these are
            // the same two paths a `tracing::info!(?secret)` would take, and
            // the wrappers are the shapes a struct field is printed inside.
            format!("{:?}", Some(&secret)),
            format!("{:?}", vec![&secret]),
            // serde must fail rather than emit anything at all.
            serde_json::to_string(&secret).unwrap_err().to_string(),
            serde_json::to_string(&bytes).unwrap_err().to_string(),
        ];

        for rendering in &renderings {
            assert!(
                !rendering.contains(CANARY),
                "a secret leaked through: {rendering}"
            );
        }
    }

    #[test]
    fn serialising_a_secret_is_an_error_not_a_redaction() {
        // A redaction would round-trip into `"***"` and be written to a
        // database by the next person who deserialised it.
        let error = serde_json::to_string(&SecretString::new("x")).unwrap_err();
        assert!(
            error.to_string().contains("cannot be serialised"),
            "{error}"
        );
    }

    #[test]
    fn secrets_deserialise_from_a_plain_string() {
        let secret: SecretString = serde_json::from_str("\"hunter2\"").unwrap();
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn equality_is_length_then_constant_time() {
        assert_eq!(SecretString::new("abc"), SecretString::new("abc"));
        assert_ne!(SecretString::new("abc"), SecretString::new("abd"));
        assert_ne!(SecretString::new("abc"), SecretString::new("abcd"));
        assert_eq!(SecretBytes::new(vec![1, 2]), SecretBytes::new(vec![1, 2]));
        assert_ne!(SecretBytes::new(vec![1, 2]), SecretBytes::new(vec![1, 3]));
    }

    #[test]
    fn constant_time_eq_agrees_with_the_obvious_implementation() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"", b""),
            (b"a", b"a"),
            (b"a", b"b"),
            (b"abc", b"abd"),
            (b"abc", b"abcd"),
            (b"\0\0", b"\0\0"),
        ];
        for (left, right) in cases {
            assert_eq!(constant_time_eq(left, right), left == right);
        }
    }

    #[test]
    fn secrets_redact_themselves_inside_a_connection_string() {
        let password = SecretString::new("s3cret-pw");
        assert_eq!(
            password.redact_within("postgres://user:s3cret-pw@db/shop"),
            "postgres://user:***@db/shop"
        );
    }

    #[test]
    fn a_very_short_secret_is_not_substituted_into_unrelated_text() {
        // Redacting "a" would rewrite every `a` in the string.
        let short = SecretString::new("a");
        assert_eq!(short.redact_within("banana"), "banana");
    }

    #[test]
    fn secret_values_report_their_length_but_nothing_else() {
        assert_eq!(SecretString::new("hunter2").secret_len(), 7);
        assert_eq!(SecretBytes::new(vec![0u8; 32]).secret_len(), 32);
        assert_eq!(SecretString::new("x").redacted(), "***");
    }

    #[test]
    fn an_empty_secret_is_detectable() {
        assert!(SecretString::default().is_empty());
        assert!(SecretBytes::default().is_empty());
        assert!(!SecretString::new("x").is_empty());
    }

    // ── decoding ─────────────────────────────────────────────────────────

    #[test]
    fn hex_decodes_both_cases() {
        assert_eq!(SecretBytes::from_hex("00ff").unwrap().expose(), &[0, 255]);
        assert_eq!(SecretBytes::from_hex("00FF").unwrap().expose(), &[0, 255]);
        assert!(SecretBytes::from_hex("").unwrap().is_empty());
    }

    #[test]
    fn hex_rejects_odd_lengths_and_non_hex_without_quoting_the_input() {
        let odd = SecretBytes::from_hex("abc").unwrap_err();
        assert!(odd.to_string().contains("even number"));
        assert!(!odd.to_string().contains("abc"));

        let bad = SecretBytes::from_hex("zz").unwrap_err();
        assert!(!bad.to_string().contains("zz"));
    }

    #[test]
    fn base64_decodes_the_documented_vectors() {
        // RFC 4648 §10.
        let vectors: &[(&str, &[u8])] = &[
            ("", b""),
            ("Zg==", b"f"),
            ("Zm8=", b"fo"),
            ("Zm9v", b"foo"),
            ("Zm9vYg==", b"foob"),
            ("Zm9vYmE=", b"fooba"),
            ("Zm9vYmFy", b"foobar"),
        ];
        for (encoded, expected) in vectors {
            assert_eq!(
                SecretBytes::from_base64(encoded).unwrap().expose(),
                *expected,
                "{encoded}"
            );
        }
    }

    #[test]
    fn base64_tolerates_missing_padding_but_not_slop() {
        assert_eq!(SecretBytes::from_base64("Zm8").unwrap().expose(), b"fo");
        assert!(SecretBytes::from_base64("Z").is_err());
        assert!(SecretBytes::from_base64("Zm9v!!").is_err());
        // Non-zero padding bits are a second spelling of one key.
        assert!(SecretBytes::from_base64("Zn==").is_err());
    }

    #[test]
    fn base64_never_quotes_the_input_in_its_error() {
        let error = SecretBytes::from_base64("not-base64-@@@").unwrap_err();
        assert!(!error.to_string().contains("@@@"), "{error}");
    }

    // ── the file provider ────────────────────────────────────────────────

    #[test]
    fn one_trailing_newline_is_trimmed_and_no_more() {
        assert_eq!(trim_one_newline("secret\n"), "secret");
        assert_eq!(trim_one_newline("secret\r\n"), "secret");
        assert_eq!(trim_one_newline("secret\n\n"), "secret\n");
        assert_eq!(trim_one_newline("secret"), "secret");
        assert_eq!(trim_one_newline(""), "");
    }

    #[test]
    fn a_missing_secret_file_names_the_path_and_not_the_contents() {
        let error = FileSecretProvider::read("/nonexistent/moso/secret").unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("/nonexistent/moso/secret"), "{rendered}");
    }

    #[test]
    fn the_file_provider_reads_a_mounted_secret() {
        // Unique per process: a fixed path under the system temp directory is
        // shared with every other `cargo test` on the machine, and this test
        // both writes and deletes it. That is a flake, and a flake in a test
        // about secrets is one nobody will investigate twice.
        let dir = std::env::temp_dir().join(format!("moso-secret-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp directory");
        let path = dir.join("api_key");
        std::fs::write(&path, "s3cret\n").expect("the file is written");

        let secret = FileSecretProvider::read(path.to_str().expect("utf-8 path"))
            .expect("the file is readable");
        assert_eq!(secret.expose(), "s3cret");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_file_provider_answers_for_the_file_scheme() {
        assert_eq!(FileSecretProvider.scheme(), "file");
        assert_eq!(
            SecretRef::new("secret_key", "/run/secrets/key").locator,
            "/run/secrets/key"
        );
    }
}
