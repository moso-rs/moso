//! Validated identifiers, and the table/column/type references built from them.
//!
//! [`Ident`] is the only way a string becomes a table name, a column name or an
//! alias. It validates on construction and every dialect emits it quoted, so a
//! value that reached the program from a request body cannot become SQL syntax
//! even if someone passes it straight through. That is a structural property of
//! the type, not a rule reviewers have to remember.

use core::fmt;
use core::str::FromStr;
use std::borrow::Cow;

/// A validated SQL identifier: a table name, a column name, an alias, an index
/// name, a type name, or a text-search configuration name.
///
/// # Why this type exists
///
/// Bound parameters protect *values*. Nothing in the SQL protocol protects
/// *identifiers*, because an identifier is syntax. `moso-sql` closes that hole
/// by making [`Ident`] the only path from a `String` to an identifier position
/// in any statement, and by always emitting it quoted.
///
/// # What is accepted
///
/// One to [`Ident::MAX_LEN`] bytes, none of which may be an ASCII control
/// character, a double quote, a backtick or a backslash. Everything else —
/// including spaces, punctuation and non-ASCII letters — is accepted, because
/// the output is always quoted and those characters cannot end the quoted
/// region. The length limit is PostgreSQL's `NAMEDATALEN - 1`: a longer name is
/// rejected here rather than silently truncated by the server, where two
/// distinct names would collide.
///
/// ```
/// use moso_sql::Ident;
///
/// // The common case is a compile-time constant, checked at compile time.
/// const EMAIL: Ident = Ident::from_static("email");
/// assert_eq!(EMAIL.as_str(), "email");
///
/// // A runtime string has to be validated, and the error says what is wrong.
/// let sneaky = Ident::new(r#"users" ; drop table users --"#);
/// assert!(sneaky.is_err());
/// # Ok::<(), moso_sql::IdentError>(())
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ident(Cow<'static, str>);

impl Ident {
    /// The longest identifier that is accepted, in bytes.
    ///
    /// PostgreSQL truncates identifiers to `NAMEDATALEN - 1` = 63 bytes. Two
    /// generated names that differ only after byte 63 would become the same
    /// object, so `moso-sql` refuses the input instead.
    ///
    /// ```
    /// assert_eq!(moso_sql::Ident::MAX_LEN, 63);
    /// ```
    pub const MAX_LEN: usize = 63;

    /// Builds an identifier from a `'static` string, validating at compile time
    /// when the call is in a `const` position.
    ///
    /// This is the constructor derives and hand-written entity definitions use:
    /// the column name is a literal, so the check costs nothing at runtime.
    ///
    /// # Panics
    ///
    /// If `raw` is not a valid identifier. In a `const` item that is a
    /// compile error rather than a panic.
    ///
    /// ```
    /// use moso_sql::Ident;
    ///
    /// const ID: Ident = Ident::from_static("id");
    /// assert_eq!(ID.as_str(), "id");
    /// ```
    #[must_use]
    pub const fn from_static(raw: &'static str) -> Self {
        assert!(
            Self::is_valid(raw),
            "moso-sql: invalid SQL identifier. An identifier must be 1..=63 bytes and must not \
             contain a control character, a double quote, a backtick or a backslash."
        );
        Self(Cow::Borrowed(raw))
    }

    /// Builds an identifier from a runtime string.
    ///
    /// # Errors
    ///
    /// [`IdentError`] naming the exact problem — empty, too long, or the byte
    /// and offset that is not allowed.
    ///
    /// ```
    /// use moso_sql::Ident;
    ///
    /// let column = Ident::new(format!("col_{}", 7))?;
    /// assert_eq!(column.as_str(), "col_7");
    /// # Ok::<(), moso_sql::IdentError>(())
    /// ```
    pub fn new(raw: impl Into<String>) -> Result<Self, IdentError> {
        let raw = raw.into();
        Self::validate(&raw)?;
        Ok(Self(Cow::Owned(raw)))
    }

    /// Whether `raw` would be accepted by [`Ident::new`].
    ///
    /// Usable in `const` context, which is what makes [`Ident::from_static`]
    /// a compile-time check.
    ///
    /// ```
    /// use moso_sql::Ident;
    ///
    /// assert!(Ident::is_valid("created_at"));
    /// assert!(!Ident::is_valid(""));
    /// assert!(!Ident::is_valid("a\"b"));
    /// ```
    #[must_use]
    pub const fn is_valid(raw: &str) -> bool {
        let bytes = raw.as_bytes();
        if bytes.is_empty() || bytes.len() > Self::MAX_LEN {
            return false;
        }
        let mut index = 0;
        while index < bytes.len() {
            if !byte_is_allowed(bytes[index]) {
                return false;
            }
            index += 1;
        }
        true
    }

    /// Validates `raw` and reports the first problem found.
    ///
    /// # Errors
    ///
    /// [`IdentError`] describing why `raw` is not usable as an identifier.
    ///
    /// ```
    /// use moso_sql::{Ident, IdentError};
    ///
    /// let error = Ident::validate("a\"b").expect_err("a quote is not allowed");
    /// assert!(matches!(error, IdentError::ForbiddenByte { .. }));
    /// ```
    pub fn validate(raw: &str) -> Result<(), IdentError> {
        if raw.is_empty() {
            return Err(IdentError::Empty);
        }
        if raw.len() > Self::MAX_LEN {
            return Err(IdentError::TooLong {
                identifier: raw.to_owned(),
                len: raw.len(),
                max: Self::MAX_LEN,
            });
        }
        for (position, byte) in raw.bytes().enumerate() {
            if !byte_is_allowed(byte) {
                return Err(IdentError::ForbiddenByte {
                    identifier: raw.to_owned(),
                    byte,
                    position,
                });
            }
        }
        Ok(())
    }

    /// The identifier as written, without quoting.
    ///
    /// ```
    /// assert_eq!(moso_sql::Ident::from_static("users").as_str(), "users");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The identifier's length in bytes. Never zero.
    ///
    /// ```
    /// assert_eq!(moso_sql::Ident::from_static("id").byte_len(), 2);
    /// ```
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.0.len()
    }

    /// Whether the identifier is `[A-Za-z_][A-Za-z0-9_]*` and therefore needs
    /// no quoting to keep its meaning.
    ///
    /// Dialects quote unconditionally; this is for diagnostics and for
    /// generated migration files, which are more readable unquoted.
    ///
    /// ```
    /// use moso_sql::Ident;
    ///
    /// assert!(Ident::from_static("created_at").is_simple());
    /// assert!(!Ident::from_static("créé").is_simple());
    /// assert!(!Ident::from_static("2fast").is_simple());
    /// ```
    #[must_use]
    pub fn is_simple(&self) -> bool {
        let mut bytes = self.0.bytes();
        let Some(first) = bytes.next() else {
            return false;
        };
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return false;
        }
        bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    }

    /// Consumes the identifier and returns the underlying string.
    ///
    /// ```
    /// assert_eq!(moso_sql::Ident::from_static("id").into_string(), "id");
    /// ```
    #[must_use]
    pub fn into_string(self) -> String {
        self.0.into_owned()
    }
}

/// Whether one byte may appear in an identifier.
///
/// Control characters cannot survive a round trip through a quoted identifier
/// intact; the double quote is the delimiter for PostgreSQL and SQLite; the
/// backtick is MySQL's; the backslash is MySQL's escape character. Rejecting
/// all four means one `Ident` is safe in every dialect this crate will ever
/// grow, not only the two it has today.
const fn byte_is_allowed(byte: u8) -> bool {
    !(byte < 0x20 || byte == 0x7f || byte == b'"' || byte == b'`' || byte == b'\\')
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ident({:?})", &*self.0)
    }
}

impl AsRef<str> for Ident {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for Ident {
    type Err = IdentError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::new(raw)
    }
}

impl TryFrom<String> for Ident {
    type Error = IdentError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

impl TryFrom<&str> for Ident {
    type Error = IdentError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

/// Why a string could not become an [`Ident`].
///
/// ```
/// use moso_sql::{Ident, IdentError};
///
/// let error = Ident::new("").expect_err("empty");
/// assert!(matches!(error, IdentError::Empty));
/// assert!(error.to_string().contains("empty"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdentError {
    /// The string was empty.
    #[error(
        "a SQL identifier cannot be empty\n\
         help: pass the column or table name, for example `Ident::from_static(\"email\")`"
    )]
    Empty,

    /// The string was longer than [`Ident::MAX_LEN`] bytes.
    #[error(
        "the identifier `{identifier}` is {len} bytes, and the limit is {max}\n\
         help: PostgreSQL truncates longer names, so two generated names could collide; \
         shorten it, or hash the tail — `idx_{identifier:.20}_<hash>`"
    )]
    TooLong {
        /// The rejected string.
        identifier: String,
        /// Its length in bytes.
        len: usize,
        /// The accepted maximum, [`Ident::MAX_LEN`].
        max: usize,
    },

    /// The string contained a byte that may not appear in an identifier.
    #[error(
        "the identifier `{identifier}` contains a byte that is not allowed \
         (0x{byte:02x} at offset {position})\n\
         help: identifiers may not contain control characters, `\"`, a backtick or `\\`; \
         if this value came from a request, it is data — bind it as a parameter instead"
    )]
    ForbiddenByte {
        /// The rejected string.
        identifier: String,
        /// The offending byte.
        byte: u8,
        /// Its zero-based offset in the string.
        position: usize,
    },
}

/// A table, optionally qualified by a schema.
///
/// ```
/// use moso_sql::{Ident, TableRef};
///
/// let users = TableRef::from_static("users");
/// assert_eq!(users.name().as_str(), "users");
/// assert!(users.schema().is_none());
///
/// let invoices = TableRef::qualified(Ident::from_static("billing"), Ident::from_static("invoices"));
/// assert_eq!(invoices.schema().map(Ident::as_str), Some("billing"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TableRef {
    schema: Option<Ident>,
    name: Ident,
}

impl TableRef {
    /// A table in the connection's default schema.
    ///
    /// ```
    /// use moso_sql::{Ident, TableRef};
    ///
    /// let posts = TableRef::new(Ident::from_static("posts"));
    /// assert_eq!(posts.name().as_str(), "posts");
    /// ```
    #[must_use]
    pub const fn new(name: Ident) -> Self {
        Self { schema: None, name }
    }

    /// A table in the default schema, named by a literal.
    ///
    /// ```
    /// assert_eq!(moso_sql::TableRef::from_static("posts").name().as_str(), "posts");
    /// ```
    #[must_use]
    pub const fn from_static(name: &'static str) -> Self {
        Self::new(Ident::from_static(name))
    }

    /// A table in a named schema.
    ///
    /// ```
    /// use moso_sql::{Ident, TableRef};
    ///
    /// let t = TableRef::qualified(Ident::from_static("audit"), Ident::from_static("events"));
    /// assert_eq!(t.schema().map(Ident::as_str), Some("audit"));
    /// ```
    #[must_use]
    pub const fn qualified(schema: Ident, name: Ident) -> Self {
        Self {
            schema: Some(schema),
            name,
        }
    }

    /// The schema, if the table is qualified.
    ///
    /// ```
    /// assert!(moso_sql::TableRef::from_static("posts").schema().is_none());
    /// ```
    #[must_use]
    pub const fn schema(&self) -> Option<&Ident> {
        self.schema.as_ref()
    }

    /// The table name.
    ///
    /// ```
    /// assert_eq!(moso_sql::TableRef::from_static("posts").name().as_str(), "posts");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// A column of this table, qualified by the table's own name.
    ///
    /// When the table has an alias in the query, qualify with the alias
    /// instead: [`ColumnRef::qualified`].
    ///
    /// ```
    /// use moso_sql::{Ident, TableRef};
    ///
    /// let column = TableRef::from_static("users").column(Ident::from_static("email"));
    /// assert_eq!(column.qualifier().map(Ident::as_str), Some("users"));
    /// ```
    #[must_use]
    pub fn column(&self, name: Ident) -> ColumnRef {
        ColumnRef::qualified(self.name.clone(), name)
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.schema {
            Some(schema) => write!(f, "{schema}.{}", self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

/// A column, optionally qualified by a table name or an alias.
///
/// ```
/// use moso_sql::{ColumnRef, Ident};
///
/// let bare = ColumnRef::from_static("email");
/// assert!(bare.qualifier().is_none());
///
/// let qualified = ColumnRef::qualified(Ident::from_static("u"), Ident::from_static("email"));
/// assert_eq!(qualified.to_string(), "u.email");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnRef {
    qualifier: Option<Ident>,
    name: Ident,
}

impl ColumnRef {
    /// An unqualified column.
    ///
    /// ```
    /// use moso_sql::{ColumnRef, Ident};
    ///
    /// let c = ColumnRef::new(Ident::from_static("id"));
    /// assert_eq!(c.name().as_str(), "id");
    /// ```
    #[must_use]
    pub const fn new(name: Ident) -> Self {
        Self {
            qualifier: None,
            name,
        }
    }

    /// An unqualified column, named by a literal.
    ///
    /// ```
    /// assert_eq!(moso_sql::ColumnRef::from_static("id").name().as_str(), "id");
    /// ```
    #[must_use]
    pub const fn from_static(name: &'static str) -> Self {
        Self::new(Ident::from_static(name))
    }

    /// A column qualified by a table name or an alias.
    ///
    /// ```
    /// use moso_sql::{ColumnRef, Ident};
    ///
    /// let c = ColumnRef::qualified(Ident::from_static("p"), Ident::from_static("title"));
    /// assert_eq!(c.to_string(), "p.title");
    /// ```
    #[must_use]
    pub const fn qualified(qualifier: Ident, name: Ident) -> Self {
        Self {
            qualifier: Some(qualifier),
            name,
        }
    }

    /// The `excluded` pseudo-table's version of a column, for the `SET` list of
    /// an `ON CONFLICT DO UPDATE`.
    ///
    /// ```
    /// use moso_sql::{ColumnRef, Ident};
    ///
    /// assert_eq!(ColumnRef::excluded(Ident::from_static("name")).to_string(), "excluded.name");
    /// ```
    #[must_use]
    pub const fn excluded(name: Ident) -> Self {
        Self::qualified(Ident::from_static("excluded"), name)
    }

    /// The qualifier, if there is one.
    ///
    /// ```
    /// assert!(moso_sql::ColumnRef::from_static("id").qualifier().is_none());
    /// ```
    #[must_use]
    pub const fn qualifier(&self) -> Option<&Ident> {
        self.qualifier.as_ref()
    }

    /// The column name.
    ///
    /// ```
    /// assert_eq!(moso_sql::ColumnRef::from_static("id").name().as_str(), "id");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// Replaces the qualifier, which is how a query that aliases a table
    /// rewrites the entity's constant columns.
    ///
    /// ```
    /// use moso_sql::{ColumnRef, Ident};
    ///
    /// let c = ColumnRef::from_static("id").with_qualifier(Ident::from_static("u"));
    /// assert_eq!(c.to_string(), "u.id");
    /// ```
    #[must_use]
    pub fn with_qualifier(mut self, qualifier: Ident) -> Self {
        self.qualifier = Some(qualifier);
        self
    }

    /// Drops the qualifier.
    ///
    /// `INSERT`, `UPDATE ... SET` and `ON CONFLICT` targets are unqualified in
    /// standard SQL, so a qualified entity column has to be stripped there.
    ///
    /// ```
    /// use moso_sql::{ColumnRef, Ident};
    ///
    /// let c = ColumnRef::qualified(Ident::from_static("u"), Ident::from_static("id"));
    /// assert_eq!(c.unqualified().to_string(), "id");
    /// ```
    #[must_use]
    pub fn unqualified(mut self) -> Self {
        self.qualifier = None;
        self
    }
}

impl fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.qualifier {
            Some(qualifier) => write!(f, "{qualifier}.{}", self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

/// A user-defined type name — a PostgreSQL `enum`, a domain, or a composite —
/// optionally schema-qualified.
///
/// ```
/// use moso_sql::{Ident, TypeRef};
///
/// let status = TypeRef::from_static("order_status");
/// assert_eq!(status.name().as_str(), "order_status");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeRef {
    schema: Option<Ident>,
    name: Ident,
}

impl TypeRef {
    /// A type in the default schema.
    ///
    /// ```
    /// use moso_sql::{Ident, TypeRef};
    ///
    /// assert_eq!(TypeRef::new(Ident::from_static("mood")).name().as_str(), "mood");
    /// ```
    #[must_use]
    pub const fn new(name: Ident) -> Self {
        Self { schema: None, name }
    }

    /// A type in the default schema, named by a literal.
    ///
    /// ```
    /// assert_eq!(moso_sql::TypeRef::from_static("mood").name().as_str(), "mood");
    /// ```
    #[must_use]
    pub const fn from_static(name: &'static str) -> Self {
        Self::new(Ident::from_static(name))
    }

    /// A type in a named schema.
    ///
    /// ```
    /// use moso_sql::{Ident, TypeRef};
    ///
    /// let t = TypeRef::qualified(Ident::from_static("shop"), Ident::from_static("mood"));
    /// assert_eq!(t.schema().map(Ident::as_str), Some("shop"));
    /// ```
    #[must_use]
    pub const fn qualified(schema: Ident, name: Ident) -> Self {
        Self {
            schema: Some(schema),
            name,
        }
    }

    /// The schema, if the type is qualified.
    ///
    /// ```
    /// assert!(moso_sql::TypeRef::from_static("mood").schema().is_none());
    /// ```
    #[must_use]
    pub const fn schema(&self) -> Option<&Ident> {
        self.schema.as_ref()
    }

    /// The type name.
    ///
    /// ```
    /// assert_eq!(moso_sql::TypeRef::from_static("mood").name().as_str(), "mood");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }
}

impl fmt::Display for TypeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.schema {
            Some(schema) => write!(f, "{schema}.{}", self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_cannot_reach_an_identifier() {
        for attempt in [
            r#"users" ; drop table users --"#,
            "a\u{0}b",
            "tab\there",
            "back`tick",
            "back\\slash",
        ] {
            assert!(
                Ident::new(attempt).is_err(),
                "{attempt:?} must be rejected: quoting is the only thing between an identifier \
                 and syntax"
            );
        }
    }

    #[test]
    fn unicode_and_spaces_are_accepted_because_the_output_is_quoted() {
        for attempt in ["créé", "a b", "über-lang", "имя"] {
            assert!(Ident::new(attempt).is_ok(), "{attempt:?}");
        }
    }

    #[test]
    fn the_length_limit_is_postgres_namedatalen() {
        let at_limit = "a".repeat(Ident::MAX_LEN);
        assert!(Ident::new(at_limit).is_ok());
        let over = "a".repeat(Ident::MAX_LEN + 1);
        let error = Ident::new(over).expect_err("one byte over");
        assert!(matches!(error, IdentError::TooLong { .. }));
    }

    #[test]
    fn validate_reports_the_offending_offset() {
        let error = Ident::validate("ab\"cd").expect_err("a quote");
        match error {
            IdentError::ForbiddenByte { byte, position, .. } => {
                assert_eq!(byte, b'"');
                assert_eq!(position, 2);
            }
            other => panic!("expected a forbidden byte, got {other:?}"),
        }
    }

    #[test]
    fn is_simple_matches_the_unquoted_grammar() {
        assert!(Ident::from_static("_x9").is_simple());
        assert!(!Ident::from_static("9x").is_simple());
        assert!(!Ident::from_static("a b").is_simple());
    }

    /// A deterministic xorshift64\* generator.
    ///
    /// The fuzzing below has to be reproducible — a test that fails on one CI
    /// run in a hundred with no way to reproduce it is worse than no test — so
    /// the seed is fixed and the sequence is the same on every machine.
    struct Rng(u64);

    impl Rng {
        const fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next(&mut self) -> u64 {
            let mut state = self.0;
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            self.0 = state;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, limit: usize) -> usize {
            usize::try_from(self.next() % limit as u64).unwrap_or(0)
        }
    }

    /// The characters the fuzzer draws from: the ones a legitimate name uses,
    /// the ones an attacker would reach for, and a few that break a naive
    /// byte-oriented validator.
    const ALPHABET: &[char] = &[
        'a', 'z', 'A', 'Z', '0', '9', '_', '-', ' ', '.', ',', ';', ':', '\'', '"', '`', '\\', '/',
        '(', ')', '[', ']', '{', '}', '*', '%', '$', '?', '!', '@', '#', '&', '|', '+', '=', '<',
        '>', '\n', '\r', '\t', '\0', '\u{7f}', '\u{1}', '\u{1f}', 'é', 'ü', 'я', '中', '𝔘',
        '\u{200b}',
    ];

    /// The invariant `Ident` exists to hold, stated once so both the fuzzer and
    /// the reader can check it: a string is a valid identifier exactly when it
    /// is one to sixty-three bytes long and contains none of the four byte
    /// classes that could end a quoted region.
    fn should_be_accepted(candidate: &str) -> bool {
        !candidate.is_empty()
            && candidate.len() <= Ident::MAX_LEN
            && candidate
                .bytes()
                .all(|byte| !(byte < 0x20 || byte == 0x7f || matches!(byte, b'"' | b'`' | b'\\')))
    }

    #[test]
    fn fuzzing_never_finds_an_accepted_identifier_that_could_escape_its_quotes() {
        let mut rng = Rng::new(0x5EED_1234_ABCD_0001);
        let mut accepted = 0_usize;
        let mut rejected = 0_usize;

        for _ in 0..200_000 {
            let length = rng.below(70);
            let mut candidate = String::with_capacity(length);
            for _ in 0..length {
                candidate.push(ALPHABET[rng.below(ALPHABET.len())]);
            }

            let expected = should_be_accepted(&candidate);
            let result = Ident::new(candidate.clone());
            assert_eq!(
                result.is_ok(),
                expected,
                "the accept/reject decision drifted from the documented rule for {candidate:?}"
            );

            let Ok(ident) = result else {
                rejected += 1;
                // A rejection always says which rule was broken, and never
                // silently truncates.
                let error = Ident::validate(&candidate).expect_err("validate must agree with new");
                assert!(!error.to_string().is_empty());
                continue;
            };
            accepted += 1;

            // The structural property: an accepted identifier round-trips
            // through the quoted form byte for byte, which is only possible if
            // it contains no quote of its own.
            assert_eq!(ident.as_str(), candidate);
            let quoted = format!("\"{}\"", ident.as_str());
            assert_eq!(
                quoted.matches('"').count(),
                2,
                "an accepted identifier must not contain the delimiter: {candidate:?}"
            );
            assert_eq!(
                &quoted[1..quoted.len() - 1],
                candidate,
                "the quoted region must be exactly the identifier"
            );
            // And a backslash cannot smuggle the delimiter past a MySQL-style
            // escape reader either.
            assert!(!candidate.contains('\\'));
        }

        // A fuzz run that only ever produced rejections would pass vacuously.
        assert!(accepted > 1_000, "only {accepted} candidates were accepted");
        assert!(rejected > 1_000, "only {rejected} candidates were rejected");
    }

    #[test]
    fn fuzzing_the_known_injection_shapes_finds_nothing() {
        // The payloads a scanner would try, each of which must be rejected
        // because it carries a delimiter, a control character or a backslash.
        let payloads = [
            r#"users" ; drop table users --"#,
            r#"" or 1=1 --"#,
            r#"a"" b"#,
            "a`b",
            "a\\\"b",
            "a\u{0}b",
            "a\nb",
            "a\rb",
            "a\tb",
            "a\u{7f}b",
            "\u{1b}[31m",
        ];
        for payload in payloads {
            assert!(
                Ident::new(payload).is_err(),
                "{payload:?} must be rejected: quoting is the only thing between an identifier \
                 and syntax"
            );
        }

        // And the shapes that look dangerous but are not, because the output is
        // always quoted: they must be *accepted*, or a legitimate column called
        // `order` or `full name` would be unusable.
        for benign in [
            "order",
            "select",
            "full name",
            "a'b",
            "a;b",
            "a--b",
            "%",
            "*",
            "é",
            "中文",
        ] {
            assert!(Ident::new(benign).is_ok(), "{benign:?} must be accepted");
        }
    }

    #[test]
    fn the_length_limit_is_measured_in_bytes_not_characters() {
        // 21 three-byte characters is 63 bytes: at the limit.
        let at_limit = "中".repeat(21);
        assert_eq!(at_limit.len(), Ident::MAX_LEN);
        assert!(Ident::new(at_limit).is_ok());
        // 22 is 66 bytes, which PostgreSQL would truncate mid-character.
        let over = "中".repeat(22);
        assert!(matches!(
            Ident::new(over).expect_err("over the limit"),
            IdentError::TooLong { .. }
        ));
    }

    #[test]
    fn references_render_for_diagnostics() {
        assert_eq!(
            TableRef::qualified(Ident::from_static("s"), Ident::from_static("t")).to_string(),
            "s.t"
        );
        assert_eq!(ColumnRef::from_static("c").to_string(), "c");
        assert_eq!(
            TableRef::from_static("t")
                .column(Ident::from_static("c"))
                .to_string(),
            "t.c"
        );
    }
}
