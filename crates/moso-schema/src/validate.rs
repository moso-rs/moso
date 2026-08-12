//! Runtime validation: the [`Validate`] trait and the error model it produces.
//!
//! Validation in Moso never returns "the first thing that was wrong". It walks
//! the whole value, collecting a [`FieldError`] per failing constraint, each
//! addressed by an RFC 6901 JSON Pointer so a client can attach the message to
//! the right form control without string matching.
//!
//! ```text
//! CreateUser { username: "ab", tags: ["", "ok"] }
//!   → /username  code=len       params={min:3,max:32}
//!   → /tags/0    code=len       params={min:1}
//! ```
//!
//! The [`codes`] module is the closed set of machine-readable codes. Clients
//! branch on the code; the human [`FieldError::message`] is localisable and is
//! never part of the contract.

use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Serialize, Serializer};
use serde_json::Value;
use smallvec::SmallVec;

use crate::message::{Locale, MessageProvider};

/// Default cap on the number of [`FieldError`]s collected for one value.
///
/// A malicious client can send a 10 000-element array; reporting 10 000 errors
/// is a cheap amplification attack, so collection stops here.
pub const DEFAULT_MAX_ERRORS: usize = 50;

/// The closed set of machine-readable validation codes.
///
/// Adding a constant here is a minor version change. Changing the string value
/// of an existing constant is a breaking change, because clients branch on it.
///
/// Anything outside this set must use [`ErrorCode::Custom`], whose values are
/// conventionally prefixed with [`codes::CUSTOM_PREFIX`].
pub mod codes {
    /// A required field was absent or null.
    pub const REQUIRED: &str = "required";
    /// The value had the wrong JSON type, or could not be parsed into the
    /// target type at all. Also used for rejected unknown fields.
    pub const TYPE: &str = "type";
    /// A length constraint failed: `minLength`/`maxLength` for strings
    /// (counted in characters) or `minItems`/`maxItems` for collections.
    pub const LEN: &str = "len";
    /// A numeric bound failed: `minimum`, `maximum`, `exclusiveMinimum` or
    /// `exclusiveMaximum`.
    pub const RANGE: &str = "range";
    /// A regular-expression constraint failed.
    pub const PATTERN: &str = "pattern";
    /// A named string format failed: `email`, `uri`, `uuid`, `hostname`, …
    pub const FORMAT: &str = "format";
    /// The value was not one of the permitted variants.
    pub const ENUM: &str = "enum";
    /// A collection required to hold distinct elements had duplicates.
    pub const UNIQUE: &str = "unique";
    /// A `multipleOf` constraint failed.
    pub const MULTIPLE_OF: &str = "multiple_of";
    /// A user-supplied check failed. Always used with a suffix, e.g.
    /// `custom:passwords_match`; see [`CUSTOM_PREFIX`].
    pub const CUSTOM: &str = "custom";
    /// Prefix for user-defined codes, so they can never collide with a code
    /// Moso adds in a future minor version.
    pub const CUSTOM_PREFIX: &str = "custom:";

    /// Every built-in code, in documentation order. Used by the CLI to render
    /// the error-code reference and by tests to assert the set has not drifted.
    pub const ALL: &[&str] = &[
        REQUIRED,
        TYPE,
        LEN,
        RANGE,
        PATTERN,
        FORMAT,
        ENUM,
        UNIQUE,
        MULTIPLE_OF,
        CUSTOM,
    ];
}

/// A typed handle on one of the [`codes`].
///
/// Using the enum rather than a bare string means a typo in a generated
/// validation body is a compile error rather than an error code no client
/// recognises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    /// [`codes::REQUIRED`]
    Required,
    /// [`codes::TYPE`]
    Type,
    /// [`codes::LEN`]
    Len,
    /// [`codes::RANGE`]
    Range,
    /// [`codes::PATTERN`]
    Pattern,
    /// [`codes::FORMAT`]
    Format,
    /// [`codes::ENUM`]
    Enum,
    /// [`codes::UNIQUE`]
    Unique,
    /// [`codes::MULTIPLE_OF`]
    MultipleOf,
    /// A user-defined code. The payload is the *complete* code string and
    /// should start with [`codes::CUSTOM_PREFIX`], e.g. `"custom:match"`.
    Custom(&'static str),
}

impl ErrorCode {
    /// The wire representation of this code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Required => codes::REQUIRED,
            Self::Type => codes::TYPE,
            Self::Len => codes::LEN,
            Self::Range => codes::RANGE,
            Self::Pattern => codes::PATTERN,
            Self::Format => codes::FORMAT,
            Self::Enum => codes::ENUM,
            Self::Unique => codes::UNIQUE,
            Self::MultipleOf => codes::MULTIPLE_OF,
            Self::Custom(s) => s,
        }
    }

    /// Parse a wire code back into the enum. Anything unrecognised becomes
    /// [`ErrorCode::Custom`] only if it is `'static`; otherwise `None`.
    #[must_use]
    pub fn from_static(s: &'static str) -> Self {
        match s {
            codes::REQUIRED => Self::Required,
            codes::TYPE => Self::Type,
            codes::LEN => Self::Len,
            codes::RANGE => Self::Range,
            codes::PATTERN => Self::Pattern,
            codes::FORMAT => Self::Format,
            codes::ENUM => Self::Enum,
            codes::UNIQUE => Self::Unique,
            codes::MULTIPLE_OF => Self::MultipleOf,
            other => Self::Custom(other),
        }
    }

    /// True if this is a user-defined code rather than one of Moso's.
    #[must_use]
    pub const fn is_custom(self) -> bool {
        matches!(self, Self::Custom(_))
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<ErrorCode> for Cow<'static, str> {
    fn from(c: ErrorCode) -> Self {
        Cow::Borrowed(c.as_str())
    }
}

/// One failed constraint, addressed by a JSON Pointer.
///
/// ```
/// use moso::prelude::*;
/// use moso::schema::{Validate, ValidationCtx, codes};
///
/// /// A user, as the API accepts one.
/// #[derive(Schema)]
/// pub struct CreateUser {
///     /// Public handle.
///     #[schema(len = 3..=32)]
///     pub username: String,
/// }
///
/// # fn main() {
/// let errors = CreateUser { username: "ab".to_owned() }
///     .validate(&mut ValidationCtx::new())
///     .unwrap_err();
/// let error = errors.iter().next().unwrap();
///
/// // An RFC 6901 JSON Pointer, so a client can attach the message to the field …
/// assert_eq!(error.pointer, "/username");
///
/// // … a stable machine code from the closed `codes` set …
/// assert_eq!(error.code, codes::LEN);
///
/// // … and the constraint's parameters, so a client can word its own message.
/// assert_eq!(error.params["min"], serde_json::json!(3));
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FieldError {
    /// RFC 6901 JSON Pointer into the request body: `/address/postcode`,
    /// `/tags/2`. Non-body sources use a synthetic root: `/query/limit`,
    /// `/path/id`, `/header/x-tenant`.
    pub pointer: String,
    /// A value from [`codes`], or a `custom:`-prefixed user code.
    pub code: Cow<'static, str>,
    /// Human-readable message. Localisable; never part of the API contract.
    pub message: Cow<'static, str>,
    /// The constraint's parameters, so clients can render their own message.
    /// Keys are stable and documented per code (`min`, `max`, `pattern`, …).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<&'static str, Value>,
}

impl FieldError {
    /// Build an error with no parameters.
    pub fn new(
        pointer: impl Into<String>,
        code: impl Into<Cow<'static, str>>,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            pointer: pointer.into(),
            code: code.into(),
            message: message.into(),
            params: BTreeMap::new(),
        }
    }

    /// Attach one constraint parameter, builder-style.
    #[must_use]
    pub fn with_param(mut self, key: &'static str, value: impl Into<Value>) -> Self {
        self.params.insert(key, value.into());
        self
    }

    /// Attach several constraint parameters at once.
    #[must_use]
    pub fn with_params(mut self, params: impl IntoIterator<Item = (&'static str, Value)>) -> Self {
        self.params.extend(params);
        self
    }

    /// Prepend `prefix` to this error's pointer.
    ///
    /// Used when a nested value's errors are lifted into the parent's
    /// namespace: `/postcode` inside `address` becomes `/address/postcode`.
    pub fn prefix(&mut self, prefix: &str) {
        if prefix.is_empty() {
            return;
        }
        self.pointer.insert_str(0, prefix);
    }
}

/// Escape one JSON Pointer reference token per RFC 6901: `~` → `~0`,
/// `/` → `~1`.
///
/// Returns the input unchanged (borrowed) when no escaping is needed, which is
/// the case for every ordinary Rust field name.
#[must_use]
pub fn escape_token(token: &str) -> Cow<'_, str> {
    if !token.contains(['~', '/']) {
        return Cow::Borrowed(token);
    }
    Cow::Owned(token.replace('~', "~0").replace('/', "~1"))
}

/// Append `/`-separated, escaped `token` to `pointer` in place.
pub fn push_token(pointer: &mut String, token: &str) {
    pointer.push('/');
    match escape_token(token) {
        Cow::Borrowed(s) => pointer.push_str(s),
        Cow::Owned(s) => pointer.push_str(&s),
    }
}

/// A set of [`FieldError`]s.
///
/// # Why the inline capacity is one
///
/// `Result<(), ValidationErrors>` is returned by *every* `validate` call,
/// overwhelmingly with `Ok`, and a `Result` is as large as its largest variant.
/// A `FieldError` is about 96 bytes, so an inline capacity of four would make
/// every successful validation move 400 bytes — paying on the hot path to save
/// an allocation on the cold one, which is backwards. One inline slot covers
/// the single-bad-field case, by far the most common failure, and anything
/// larger allocates once.
///
/// # The cap
///
/// Collection stops at [`ValidationErrors::max_errors`], which defaults to
/// [`DEFAULT_MAX_ERRORS`]. Errors pushed past the cap are counted
/// ([`ValidationErrors::dropped`]) and discarded, and
/// [`ValidationErrors::truncated`] becomes true. The cap lives here as well as
/// on [`ValidationCtx`] on purpose: a hand-written `validate` that forgets to
/// consult [`ValidationCtx::is_full`] still cannot turn a 10 000-element array
/// into a 10 000-entry response body.
///
/// ```
/// use moso::prelude::*;
/// use moso::schema::{Validate, ValidationCtx};
///
/// /// A user, as the API accepts one.
/// #[derive(Schema)]
/// pub struct CreateUser {
///     /// Public handle.
///     #[schema(len = 3..=32)]
///     pub username: String,
///     /// Contact address.
///     #[schema(len = 1..=200)]
///     pub bio: String,
/// }
///
/// # fn main() {
/// let bad = CreateUser { username: "ab".to_owned(), bio: String::new() };
/// let errors = bad.validate(&mut ValidationCtx::new()).unwrap_err();
///
/// // Every failure is collected, not just the first — the whole point.
/// assert_eq!(errors.len(), 2);
///
/// let pointers: Vec<&str> = errors.iter().map(|e| e.pointer.as_str()).collect();
/// assert_eq!(pointers, ["/username", "/bio"]);
/// # }
/// ```
///
/// This is what becomes the `errors` array of a `422` problem document.
#[derive(Clone, Debug)]
pub struct ValidationErrors {
    errors: SmallVec<[FieldError; 1]>,
    // Both counters are `u32` rather than `usize` for one measured reason:
    // `Result<(), ValidationErrors>` is the return type of *every* `validate`
    // call, and two `usize`s here take it to exactly 128 bytes, which is
    // clippy's `result_large_err` threshold. At `u32` it is 120. A cap above
    // four billion is not a cap, so nothing is lost.
    max: u32,
    dropped: u32,
}

/// Clamp a count into the `u32` the error set stores it in.
fn clamp(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl ValidationErrors {
    /// An empty error set with the default cap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            errors: SmallVec::new(),
            max: clamp(DEFAULT_MAX_ERRORS),
            dropped: 0,
        }
    }

    /// An empty error set that collects at most `max` errors.
    ///
    /// `max == 0` means "collect nothing", which is only useful for a
    /// presence-only check. A `max` above `u32::MAX` saturates there.
    #[must_use]
    pub fn with_max_errors(mut self, max: usize) -> Self {
        self.max = clamp(max);
        self
    }

    /// Change the cap in place, truncating if the set already exceeds it.
    pub fn set_max_errors(&mut self, max: usize) {
        self.max = clamp(max);
        if self.errors.len() > max {
            self.truncate(max);
        }
    }

    /// The maximum number of errors this set will retain.
    #[must_use]
    pub fn max_errors(&self) -> usize {
        self.max as usize
    }

    /// True once the cap has been reached and further pushes will be dropped.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.errors.len() >= self.max as usize
    }

    /// How many errors were discarded because the cap was reached.
    #[must_use]
    pub fn dropped(&self) -> usize {
        self.dropped as usize
    }

    /// True when at least one error was discarded — the summary flag a caller
    /// renders as "… and 12 more".
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.dropped > 0
    }

    /// Shorthand for the single-error case, as used by `#[schema(check = …)]`
    /// functions.
    #[must_use]
    pub fn one(
        pointer: impl Into<String>,
        code: impl Into<Cow<'static, str>>,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        let mut e = Self::new();
        e.push(FieldError::new(pointer, code, message));
        e
    }

    /// Append one error, unless the cap has been reached.
    ///
    /// A dropped error still increments [`ValidationErrors::dropped`], so the
    /// count reported to the client is honest even when the list is not
    /// complete. Use [`ValidationErrors::try_push`] when the caller wants to
    /// know whether the error survived.
    pub fn push(&mut self, error: FieldError) {
        let _ = self.try_push(error);
    }

    /// [`ValidationErrors::push`], reporting whether the error was retained.
    pub fn try_push(&mut self, error: FieldError) -> bool {
        if self.is_full() {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.errors.push(error);
        true
    }

    /// Append one error built from its parts.
    pub fn add(
        &mut self,
        pointer: impl Into<String>,
        code: impl Into<Cow<'static, str>>,
        message: impl Into<Cow<'static, str>>,
    ) {
        self.push(FieldError::new(pointer, code, message));
    }

    /// Move every error out of `other` into `self`.
    ///
    /// `self`'s cap applies: anything past it is counted as dropped rather
    /// than retained. `other`'s own dropped count is carried over, so a cap hit
    /// deep inside a nested value is still visible at the top.
    pub fn merge(&mut self, other: ValidationErrors) {
        self.dropped = self.dropped.saturating_add(other.dropped);
        for e in other.errors {
            self.push(e);
        }
    }

    /// Move every error out of `other` into `self`, prefixing each pointer.
    ///
    /// This is how `nested` and `each` compose pointers: the inner type is
    /// validated as if it were the root, then lifted.
    pub fn merge_prefixed(&mut self, prefix: &str, mut other: ValidationErrors) {
        for e in &mut other.errors {
            e.prefix(prefix);
        }
        self.merge(other);
    }

    /// Prefix every pointer in this set.
    pub fn prefix_all(&mut self, prefix: &str) {
        for e in &mut self.errors {
            e.prefix(prefix);
        }
    }

    /// Number of *retained* errors. See [`ValidationErrors::dropped`] for the
    /// ones the cap discarded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// True when nothing failed.
    ///
    /// A set whose cap is zero is empty even after pushes, which is why this
    /// asks about retained errors *and* dropped ones.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty() && self.dropped == 0
    }

    /// Borrow the errors as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[FieldError] {
        &self.errors
    }

    /// Iterate the errors in collection order.
    pub fn iter(&self) -> std::slice::Iter<'_, FieldError> {
        self.errors.iter()
    }

    /// Consume into a plain `Vec`, for callers that do not want `SmallVec` in
    /// their public API.
    #[must_use]
    pub fn into_vec(self) -> Vec<FieldError> {
        self.errors.into_vec()
    }

    /// Drop everything past `max` errors, counting the removed ones as
    /// dropped.
    pub fn truncate(&mut self, max: usize) {
        if self.errors.len() > max {
            self.dropped = self.dropped.saturating_add(clamp(self.errors.len() - max));
            self.errors.truncate(max);
        }
    }

    /// `Ok(())` when empty, `Err(self)` otherwise. The tail of every generated
    /// `Validate::validate` body.
    ///
    /// # Errors
    /// Returns `self` when at least one constraint failed.
    pub fn into_result(self) -> Result<(), ValidationErrors> {
        if self.is_empty() { Ok(()) } else { Err(self) }
    }

    /// Rewrite every message using `provider`, falling back to the existing
    /// message when the provider has no translation for a code.
    pub fn localise(&mut self, provider: &dyn MessageProvider, locale: &Locale) {
        for e in &mut self.errors {
            if let Some(m) = provider.message(&e.code, &e.params, locale) {
                e.message = Cow::Owned(m);
            }
        }
    }
}

impl Default for ValidationErrors {
    fn default() -> Self {
        Self::new()
    }
}

/// Compares the *contents*: the retained errors and how many were dropped.
///
/// The cap is a policy knob, not part of the value, so two sets that collected
/// the same failures compare equal even if one was configured to allow more.
impl PartialEq for ValidationErrors {
    fn eq(&self, other: &Self) -> bool {
        self.errors == other.errors && self.dropped == other.dropped
    }
}

impl Serialize for ValidationErrors {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.errors.iter())
    }
}

impl From<FieldError> for ValidationErrors {
    fn from(e: FieldError) -> Self {
        let mut v = Self::new();
        v.push(e);
        v
    }
}

impl FromIterator<FieldError> for ValidationErrors {
    fn from_iter<I: IntoIterator<Item = FieldError>>(iter: I) -> Self {
        let mut v = Self::new();
        v.extend(iter);
        v
    }
}

impl Extend<FieldError> for ValidationErrors {
    fn extend<I: IntoIterator<Item = FieldError>>(&mut self, iter: I) {
        for e in iter {
            self.push(e);
        }
    }
}

impl IntoIterator for ValidationErrors {
    type Item = FieldError;
    type IntoIter = smallvec::IntoIter<[FieldError; 1]>;

    fn into_iter(self) -> Self::IntoIter {
        self.errors.into_iter()
    }
}

impl<'a> IntoIterator for &'a ValidationErrors {
    type Item = &'a FieldError;
    type IntoIter = std::slice::Iter<'a, FieldError>;

    fn into_iter(self) -> Self::IntoIter {
        self.errors.iter()
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.errors.len(), self.dropped()) {
            (0, 0) => f.write_str("no validation errors"),
            (1, 0) => f.write_str("1 field is invalid"),
            (n, 0) => write!(f, "{n} fields are invalid"),
            (n, d) => write!(f, "{} fields are invalid ({d} not reported)", n + d),
        }
    }
}

impl std::error::Error for ValidationErrors {}

/// Ambient state threaded through a validation walk.
///
/// It carries three things generated code cannot carry itself:
///
/// * a **pointer stack**, so `nested` and `each` produce `/address/postcode`
///   and `/tags/2` without every check needing to know its own depth;
/// * an optional **locale** and [`MessageProvider`], so messages can be
///   translated at the point they are produced;
/// * a **max-error cap**, so a hostile payload cannot force an unbounded
///   response.
///
/// It also has a small typed side-channel ([`ValidationCtx::insert`] /
/// [`ValidationCtx::get`]) for `#[schema(check = …)]` functions that need
/// request-scoped context.
///
/// ```
/// use moso::prelude::*;
/// use moso::schema::{Validate, ValidationCtx};
///
/// /// A user, as the API accepts one.
/// #[derive(Schema)]
/// pub struct CreateUser {
///     /// Public handle.
///     #[schema(len = 3..=32)]
///     pub username: String,
/// }
///
/// # fn main() {
/// // Validating from the body root gives `/username`.
/// let errors = CreateUser { username: "ab".to_owned() }
///     .validate(&mut ValidationCtx::new())
///     .unwrap_err();
/// assert_eq!(errors.iter().next().unwrap().pointer, "/username");
///
/// // A hand-written `Validate` addresses its fields through the context, so a
/// // nested value reports where it actually is.
/// let mut ctx = ValidationCtx::new();
/// ctx.push_field("address");
/// assert_eq!(ctx.field_pointer("postcode"), "/address/postcode");
/// ctx.pop();
/// assert_eq!(ctx.field_pointer("postcode"), "/postcode");
/// # }
/// ```
///
/// Use [`ValidationCtx::new`], not `Default::default()`: the derived default has
/// an error cap of zero, which silently discards every failure.
pub struct ValidationCtx {
    pointer: String,
    frames: SmallVec<[usize; 8]>,
    locale: Option<Locale>,
    messages: Option<Arc<dyn MessageProvider>>,
    max_errors: usize,
    extensions: Vec<(TypeId, Box<dyn Any + Send + Sync>)>,
}

impl Default for ValidationCtx {
    /// Exactly [`ValidationCtx::new`].
    ///
    /// Written out rather than derived: `#[derive(Default)]` would give
    /// `max_errors` the `usize` default of **zero**, which means "collect
    /// nothing" — every validation would then report an empty error set and a
    /// caller reading `errors.len()` would see no failures at all. A default
    /// that silently disables the feature is worse than no default.
    fn default() -> Self {
        Self::new()
    }
}

impl ValidationCtx {
    /// A context rooted at the document root with the default error cap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pointer: String::new(),
            frames: SmallVec::new(),
            locale: None,
            messages: None,
            max_errors: DEFAULT_MAX_ERRORS,
            extensions: Vec::new(),
        }
    }

    /// A context whose pointers are rooted at `pointer` instead of `""`.
    ///
    /// Used for non-body sources so a query-parameter failure reports
    /// `/query/limit`.
    #[must_use]
    pub fn rooted_at(pointer: impl Into<String>) -> Self {
        let mut c = Self::new();
        c.pointer = pointer.into();
        c
    }

    /// Set the locale used by [`ValidationCtx::message`].
    #[must_use]
    pub fn with_locale(mut self, locale: Locale) -> Self {
        self.locale = Some(locale);
        self
    }

    /// Set the message provider used by [`ValidationCtx::message`].
    #[must_use]
    pub fn with_messages(mut self, provider: Arc<dyn MessageProvider>) -> Self {
        self.messages = Some(provider);
        self
    }

    /// Override the maximum number of errors collected for one value.
    #[must_use]
    pub fn with_max_errors(mut self, max: usize) -> Self {
        self.max_errors = max;
        self
    }

    /// The locale in force, if any.
    #[must_use]
    pub fn locale(&self) -> Option<&Locale> {
        self.locale.as_ref()
    }

    /// The message provider in force, if any.
    #[must_use]
    pub fn messages(&self) -> Option<&dyn MessageProvider> {
        self.messages.as_deref()
    }

    /// The error cap.
    #[must_use]
    pub fn max_errors(&self) -> usize {
        self.max_errors
    }

    /// True when `errors` has reached the cap and further checks may be
    /// skipped.
    #[must_use]
    pub fn is_full(&self, errors: &ValidationErrors) -> bool {
        errors.len() >= self.max_errors
    }

    /// An empty error set carrying this context's cap.
    ///
    /// Generated `validate` bodies open with `let mut errors = ctx.errors();`
    /// so that a per-request cap set by the extractor is honoured even by the
    /// checks that never consult [`ValidationCtx::is_full`].
    #[must_use]
    pub fn errors(&self) -> ValidationErrors {
        ValidationErrors::new().with_max_errors(self.max_errors)
    }

    /// The pointer of the value currently being validated.
    #[must_use]
    pub fn pointer(&self) -> &str {
        &self.pointer
    }

    /// Descend into a named field.
    pub fn push_field(&mut self, name: &str) {
        self.frames.push(self.pointer.len());
        push_token(&mut self.pointer, name);
    }

    /// Descend into an array element.
    pub fn push_index(&mut self, index: usize) {
        self.frames.push(self.pointer.len());
        self.pointer.push('/');
        self.pointer.push_str(itoa(index).as_str());
    }

    /// Leave the innermost frame. A no-op at the root.
    pub fn pop(&mut self) {
        if let Some(len) = self.frames.pop() {
            self.pointer.truncate(len);
        }
    }

    /// Run `f` with the pointer extended by `name`, restoring it afterwards.
    pub fn with_field<R>(&mut self, name: &str, f: impl FnOnce(&mut Self) -> R) -> R {
        self.push_field(name);
        let r = f(self);
        self.pop();
        r
    }

    /// Run `f` with the pointer extended by `index`, restoring it afterwards.
    pub fn with_index<R>(&mut self, index: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        self.push_index(index);
        let r = f(self);
        self.pop();
        r
    }

    /// Descend into `name`, run `f`, and pop again — the form hand-written
    /// nested validation should use, because it cannot forget the [`pop`].
    ///
    /// ```
    /// # use moso_schema::ValidationCtx;
    /// let mut ctx = ValidationCtx::new();
    /// let p = ctx.scope("address", |ctx| ctx.scope("postcode", |ctx| ctx.pointer().to_owned()));
    /// assert_eq!(p, "/address/postcode");
    /// assert_eq!(ctx.pointer(), "");
    /// ```
    ///
    /// [`pop`]: ValidationCtx::pop
    pub fn scope<R>(&mut self, name: &str, f: impl FnOnce(&mut Self) -> R) -> R {
        self.with_field(name, f)
    }

    /// [`ValidationCtx::scope`] for an array element.
    pub fn scope_index<R>(&mut self, index: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        self.with_index(index, f)
    }

    /// The pointer `name` would have, without leaving it on the stack.
    ///
    /// The `check_*` helpers take a `&str` pointer rather than pushing onto the
    /// context, so this is how a generated body addresses one field:
    /// `check_len_str(&self.name, Some(3), None, &ctx.field_pointer("name"), &mut errors)`.
    #[must_use]
    pub fn field_pointer(&self, name: &str) -> String {
        let mut p = String::with_capacity(self.pointer.len() + name.len() + 1);
        p.push_str(&self.pointer);
        push_token(&mut p, name);
        p
    }

    /// The pointer `index` would have, without leaving it on the stack.
    #[must_use]
    pub fn index_pointer(&self, index: usize) -> String {
        let mut p = String::with_capacity(self.pointer.len() + 4);
        p.push_str(&self.pointer);
        p.push('/');
        p.push_str(itoa(index).as_str());
        p
    }

    /// Store request-scoped context for `#[schema(check = …)]` functions.
    ///
    /// Replaces any previous value of the same type.
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) {
        let id = TypeId::of::<T>();
        self.extensions.retain(|(k, _)| *k != id);
        self.extensions.push((id, Box::new(value)));
    }

    /// Retrieve context previously stored with [`ValidationCtx::insert`].
    #[must_use]
    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        let id = TypeId::of::<T>();
        self.extensions
            .iter()
            .find(|(k, _)| *k == id)
            .and_then(|(_, v)| v.downcast_ref::<T>())
    }

    /// Resolve a human message for `code`, consulting the [`MessageProvider`]
    /// first and falling back to the bundled English default.
    ///
    /// A provider with no locale set is consulted for [`Locale::EN`] rather
    /// than skipped: an application that registers a provider to *reword*
    /// messages has no reason to also have to set a locale.
    #[must_use]
    pub fn message(&self, code: &str, params: &BTreeMap<&'static str, Value>) -> Cow<'static, str> {
        if let Some(p) = self.messages.as_deref() {
            let locale = self.locale.clone().unwrap_or(Locale::EN);
            if let Some(m) = p.message(code, params, &locale) {
                return Cow::Owned(m);
            }
        }
        crate::message::default_message(code, params)
    }
}

impl fmt::Debug for ValidationCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidationCtx")
            .field("pointer", &self.pointer)
            .field("locale", &self.locale)
            .field("messages", &self.messages.is_some())
            .field("max_errors", &self.max_errors)
            .finish()
    }
}

/// Small allocation-free integer formatter for array indices.
///
/// `usize::to_string` would allocate a `String` for every element of every
/// sequence being validated, on a path that is otherwise allocation-free until
/// something actually fails.
pub(crate) fn itoa(mut n: usize) -> ArrayIndex {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + u8::try_from(n % 10).unwrap_or(0);
        n /= 10;
        if n == 0 {
            break;
        }
    }
    ArrayIndex { buf, start: i }
}

/// Stack buffer holding the decimal rendering of an array index.
pub(crate) struct ArrayIndex {
    buf: [u8; 20],
    start: usize,
}

impl ArrayIndex {
    /// The rendered digits.
    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[self.start..]).unwrap_or("0")
    }
}

/// Runtime validation of a value against the constraints declared on its type.
///
/// `#[derive(Schema)]` generates this impl from the same `#[schema(...)]`
/// attributes that generate the JSON Schema, so the documented constraint and
/// the enforced constraint cannot disagree.
///
/// Implement it by hand only for types that are not `#[derive(Schema)]`
/// structs; constrained newtypes such as [`Email`](crate::types::Email) enforce
/// their invariant on construction and so have a trivial impl.
///
/// The framework calls it inside body extraction: a `Json<T>` cannot be
/// obtained from a request whose payload failed this check, and the failures
/// become a `422` whose field pointers are RFC 6901 JSON Pointers.
///
/// ```
/// use moso::prelude::*;
/// use moso::schema::{Validate, ValidationCtx};
///
/// /// A user, as the API accepts one.
/// #[derive(Schema)]
/// pub struct CreateUser {
///     /// Public handle.
///     #[schema(len = 3..=32)]
///     pub username: String,
/// }
///
/// # fn main() {
/// let good = CreateUser { username: "ada".to_owned() };
/// assert!(good.validate(&mut ValidationCtx::new()).is_ok());
///
/// let bad = CreateUser { username: "ab".to_owned() };
/// let errors = bad.validate(&mut ValidationCtx::new()).unwrap_err();
///
/// // Every failure is reported, each pointing at the field that caused it.
/// assert_eq!(errors.len(), 1);
/// assert_eq!(errors.iter().next().unwrap().pointer, "/username");
/// # }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be validated",
    label = "does not implement `Validate`",
    note = "every Moso model type must implement `Validate`; `#[derive(Schema)]` generates it",
    note = "if `{Self}` has no constraints, an empty impl is correct",
    note = "help: add the derive:\n    #[derive(moso::Schema)]\n    pub struct {Self} {{ /* … */ }}\n\
            or write the trivial impl:\n    impl moso::Validate for {Self} {{\n        \
            fn validate(&self, _: &mut moso::ValidationCtx)\n            \
            -> Result<(), moso::ValidationErrors> {{ Ok(()) }}\n    }}"
)]
pub trait Validate {
    /// Check every constraint, collecting all failures.
    ///
    /// Implementations must not short-circuit on the first failure: reporting
    /// one field at a time is the single most common complaint about
    /// validation libraries. Stop only when `ctx.is_full(&errors)`.
    ///
    /// # Errors
    /// Returns the complete set of failed constraints.
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_pointer_tokens() {
        assert_eq!(escape_token("plain"), "plain");
        assert_eq!(escape_token("a/b"), "a~1b");
        assert_eq!(escape_token("a~b"), "a~0b");
        assert_eq!(escape_token("a~/b"), "a~0~1b");
    }

    #[test]
    fn pointer_stack_composes_and_unwinds() {
        let mut ctx = ValidationCtx::new();
        assert_eq!(ctx.pointer(), "");
        ctx.push_field("address");
        assert_eq!(ctx.pointer(), "/address");
        ctx.push_field("postcode");
        assert_eq!(ctx.pointer(), "/address/postcode");
        ctx.pop();
        ctx.pop();
        assert_eq!(ctx.pointer(), "");
        ctx.push_field("tags");
        ctx.push_index(2);
        assert_eq!(ctx.pointer(), "/tags/2");
        ctx.pop();
        ctx.push_index(0);
        assert_eq!(ctx.pointer(), "/tags/0");
    }

    #[test]
    fn rooted_context_prefixes_everything() {
        let mut ctx = ValidationCtx::rooted_at("/query");
        ctx.push_field("limit");
        assert_eq!(ctx.pointer(), "/query/limit");
    }

    #[test]
    fn index_formatting_handles_large_values() {
        assert_eq!(itoa(0).as_str(), "0");
        assert_eq!(itoa(7).as_str(), "7");
        assert_eq!(itoa(1234).as_str(), "1234");
        assert_eq!(itoa(usize::MAX).as_str(), usize::MAX.to_string());
    }

    #[test]
    fn merge_prefixed_lifts_nested_pointers() {
        let inner = ValidationErrors::one("/postcode", codes::PATTERN, "bad");
        let mut outer = ValidationErrors::new();
        outer.merge_prefixed("/address", inner);
        assert_eq!(outer.as_slice()[0].pointer, "/address/postcode");
    }

    #[test]
    fn error_codes_round_trip() {
        for c in codes::ALL {
            assert_eq!(ErrorCode::from_static(c).as_str(), *c);
        }
        assert!(ErrorCode::from_static("custom:match").is_custom());
    }

    #[test]
    fn context_extensions_are_typed() {
        let mut ctx = ValidationCtx::new();
        ctx.insert(7u32);
        ctx.insert(String::from("hi"));
        assert_eq!(ctx.get::<u32>(), Some(&7));
        assert_eq!(ctx.get::<String>().map(String::as_str), Some("hi"));
        assert_eq!(ctx.get::<i8>(), None);
    }

    fn err(pointer: &str) -> FieldError {
        FieldError::new(pointer, codes::LEN, "too short")
    }

    #[test]
    fn pointer_stack_escapes_hostile_field_names() {
        let mut ctx = ValidationCtx::new();
        ctx.push_field("a/b");
        assert_eq!(ctx.pointer(), "/a~1b");
        ctx.push_field("c~d");
        assert_eq!(ctx.pointer(), "/a~1b/c~0d");
        ctx.pop();
        ctx.pop();
        assert_eq!(ctx.pointer(), "");
    }

    #[test]
    fn pop_at_the_root_is_a_no_op() {
        let mut ctx = ValidationCtx::new();
        ctx.pop();
        ctx.pop();
        assert_eq!(ctx.pointer(), "");
        ctx.push_field("a");
        assert_eq!(ctx.pointer(), "/a");
    }

    #[test]
    fn scope_restores_the_pointer_even_when_nested() {
        let mut ctx = ValidationCtx::rooted_at("/query");
        let inner = ctx.scope("filter", |c| {
            c.scope_index(3, |c| c.scope("name", |c| c.pointer().to_owned()))
        });
        assert_eq!(inner, "/query/filter/3/name");
        assert_eq!(ctx.pointer(), "/query");
    }

    #[test]
    fn pointer_helpers_do_not_mutate_the_stack() {
        let mut ctx = ValidationCtx::new();
        ctx.push_field("address");
        assert_eq!(ctx.field_pointer("postcode"), "/address/postcode");
        assert_eq!(ctx.field_pointer("a/b"), "/address/a~1b");
        assert_eq!(ctx.index_pointer(12), "/address/12");
        assert_eq!(ctx.pointer(), "/address");
    }

    #[test]
    fn errors_from_context_inherit_the_cap() {
        let ctx = ValidationCtx::new().with_max_errors(2);
        let mut errors = ctx.errors();
        assert_eq!(errors.max_errors(), 2);
        for _ in 0..5 {
            errors.push(err("/a"));
        }
        assert_eq!(errors.len(), 2);
        assert_eq!(errors.dropped(), 3);
    }

    #[test]
    fn the_cap_drops_and_flags() {
        let mut errors = ValidationErrors::new();
        assert_eq!(errors.max_errors(), DEFAULT_MAX_ERRORS);
        for i in 0..DEFAULT_MAX_ERRORS + 7 {
            errors.push(err(&format!("/f{i}")));
        }
        assert_eq!(errors.len(), DEFAULT_MAX_ERRORS);
        assert_eq!(errors.dropped(), 7);
        assert!(errors.truncated());
        assert!(errors.is_full());
        assert_eq!(
            errors.to_string(),
            format!(
                "{} fields are invalid (7 not reported)",
                DEFAULT_MAX_ERRORS + 7
            )
        );
        // The retained errors are the *first* ones, not the last.
        assert_eq!(errors.as_slice()[0].pointer, "/f0");
    }

    #[test]
    fn try_push_reports_whether_the_error_survived() {
        let mut errors = ValidationErrors::new().with_max_errors(1);
        assert!(errors.try_push(err("/a")));
        assert!(!errors.try_push(err("/b")));
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn merge_carries_the_dropped_count_upwards() {
        let mut inner = ValidationErrors::new().with_max_errors(1);
        inner.push(err("/x"));
        inner.push(err("/y"));
        assert_eq!(inner.dropped(), 1);

        let mut outer = ValidationErrors::new();
        outer.merge_prefixed("/child", inner);
        assert_eq!(outer.len(), 1);
        assert_eq!(outer.as_slice()[0].pointer, "/child/x");
        assert!(outer.truncated(), "an inner cap hit must remain visible");
    }

    #[test]
    fn merge_respects_the_outer_cap() {
        let mut outer = ValidationErrors::new().with_max_errors(2);
        let inner: ValidationErrors = (0..5).map(|i| err(&format!("/f{i}"))).collect();
        outer.merge(inner);
        assert_eq!(outer.len(), 2);
        assert_eq!(outer.dropped(), 3);
    }

    #[test]
    fn truncate_counts_what_it_removes() {
        let mut errors: ValidationErrors = (0..5).map(|i| err(&format!("/f{i}"))).collect();
        errors.truncate(2);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors.dropped(), 3);
        errors.truncate(9);
        assert_eq!(
            errors.dropped(),
            3,
            "growing the cap must not invent errors"
        );
    }

    #[test]
    fn set_max_errors_truncates_in_place() {
        let mut errors: ValidationErrors = (0..5).map(|i| err(&format!("/f{i}"))).collect();
        errors.set_max_errors(3);
        assert_eq!(errors.len(), 3);
        assert!(errors.is_full());
    }

    #[test]
    fn extend_respects_the_cap() {
        let mut errors = ValidationErrors::new().with_max_errors(2);
        errors.extend((0..4).map(|i| err(&format!("/f{i}"))));
        assert_eq!(errors.len(), 2);
        assert_eq!(errors.dropped(), 2);
    }

    #[test]
    fn into_result_is_ok_only_when_nothing_failed() {
        assert!(ValidationErrors::new().into_result().is_ok());
        assert!(ValidationErrors::from(err("/a")).into_result().is_err());

        // A cap of zero retains nothing, but the failure must not vanish.
        let mut none_kept = ValidationErrors::new().with_max_errors(0);
        none_kept.push(err("/a"));
        assert!(none_kept.as_slice().is_empty());
        assert!(none_kept.into_result().is_err());
    }

    #[test]
    fn equality_ignores_the_cap_but_not_the_content() {
        let a: ValidationErrors = std::iter::once(err("/a")).collect();
        let b = ValidationErrors::from(err("/a")).with_max_errors(7);
        assert_eq!(a, b);
        assert_ne!(a, ValidationErrors::from(err("/b")));
        assert_eq!(ValidationErrors::default(), ValidationErrors::new());
    }

    #[test]
    fn serialises_as_a_bare_array() {
        let errors: ValidationErrors = std::iter::once(
            FieldError::new(
                "/username",
                codes::LEN,
                "must be between 3 and 32 characters",
            )
            .with_param("min", 3)
            .with_param("max", 32),
        )
        .collect();
        let json = serde_json::to_value(&errors).expect("serialises");
        assert_eq!(
            json,
            serde_json::json!([{
                "pointer": "/username",
                "code": "len",
                "message": "must be between 3 and 32 characters",
                "params": { "min": 3, "max": 32 },
            }])
        );
    }

    #[test]
    fn localise_rewrites_only_known_codes() {
        struct OnlyLen;
        impl MessageProvider for OnlyLen {
            fn message(
                &self,
                code: &str,
                _params: &BTreeMap<&'static str, Value>,
                _locale: &Locale,
            ) -> Option<String> {
                (code == codes::LEN).then(|| "trop court".to_owned())
            }
        }

        let mut errors = ValidationErrors::new();
        errors.push(FieldError::new("/a", codes::LEN, "too short"));
        errors.push(FieldError::new(
            "/b",
            codes::REQUIRED,
            "this field is required",
        ));
        errors.localise(&OnlyLen, &Locale::EN);
        assert_eq!(errors.as_slice()[0].message, "trop court");
        assert_eq!(errors.as_slice()[1].message, "this field is required");
    }

    #[test]
    fn context_message_uses_the_provider_without_an_explicit_locale() {
        struct Shouty;
        impl MessageProvider for Shouty {
            fn message(
                &self,
                code: &str,
                _params: &BTreeMap<&'static str, Value>,
                locale: &Locale,
            ) -> Option<String> {
                Some(format!("{}/{locale}", code.to_uppercase()))
            }
        }

        let ctx = ValidationCtx::new().with_messages(Arc::new(Shouty));
        assert_eq!(ctx.message(codes::LEN, &BTreeMap::new()), "LEN/en");

        // With no provider at all the bundled English text is used.
        let ctx = ValidationCtx::new();
        assert_eq!(
            ctx.message(codes::REQUIRED, &BTreeMap::new()),
            "this field is required"
        );
    }

    #[test]
    fn field_error_prefixing_is_idempotent_for_the_empty_prefix() {
        let mut e = err("/a");
        e.prefix("");
        assert_eq!(e.pointer, "/a");
        e.prefix("/root");
        assert_eq!(e.pointer, "/root/a");
    }

    #[test]
    fn the_default_context_collects_errors_rather_than_discarding_them() {
        // `#[derive(Default)]` would set the cap to zero, which turns every
        // validation into a silent pass: `errors.len()` reads 0 and only
        // `truncated()` — which nobody checks — says otherwise.
        let mut errors = ValidationCtx::default().errors();
        errors.add("/username", codes::LEN, "too short");
        assert_eq!(errors.len(), 1);
        assert!(!errors.truncated());

        assert_eq!(
            ValidationCtx::default().max_errors(),
            ValidationCtx::new().max_errors()
        );
    }
}
