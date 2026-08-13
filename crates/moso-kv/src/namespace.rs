//! Typed namespaces: [`Namespace`], [`FailureMode`], and the
//! [`namespace!`](macro@crate::namespace) macro that writes them.
//!
//! Raw byte APIs are not what handler code should touch. A namespace binds four
//! things together so that they cannot drift apart:
//!
//! | | |
//! | --- | --- |
//! | a **key type** | `Id<User>` — not `String`, so a post id cannot be passed |
//! | a **value type** | `UserProfile` — one namespace's value is never another's |
//! | a **key prefix and version** | `profile:1` — bump the version to invalidate |
//! | a **failure mode** | degrade, or fail the request |
//!
//! # Writing one
//!
//! ```
//! use moso_kv::{minutes, Namespace};
//!
//! /// A user profile, as it is cached.
//! #[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
//! pub struct UserProfile {
//!     /// The display name.
//!     pub name: String,
//! }
//!
//! moso_kv::namespace! {
//!     /// Cached user profile, refreshed on write.
//!     pub Profile: u64 => Option<UserProfile>, ttl = minutes(15), codec = Json;
//!
//!     /// One-time login codes. Losing one silently is worse than a 503.
//!     pub LoginCode: String => String,
//!         ttl = minutes(10), on_failure = fail, version = 2;
//! }
//!
//! assert_eq!(Profile::PREFIX, "profile");
//! assert_eq!(LoginCode::PREFIX, "login_code");
//! assert_eq!(LoginCode::VERSION, 2);
//! ```
//!
//! # Negative caching without a trait bound
//!
//! A namespace whose `Value` is an `Option<T>` gets negative caching for free:
//! the generated [`Namespace::is_negative`] returns `value.is_none()`, and
//! `Kv` then stores it under [`NEGATIVE_TTL`](Namespace::NEGATIVE_TTL) rather
//! than [`TTL`](Namespace::TTL). Every other value type gets the default,
//! which is `false`.
//!
//! That dispatch happens through [`NegativeProbe`], an inherent method that
//! wins over a trait method for `Option<T>` and loses for everything else. It
//! is the standard autoref trick, it needs no `specialization`, and it is why
//! negative caching costs a user nothing to opt into beyond writing the return
//! type they were going to write anyway.

use std::time::Duration;

use crate::codec::{Codec, Encodable};
use crate::key::KeyPart;

// ---------------------------------------------------------------------------
// FailureMode
// ---------------------------------------------------------------------------

/// What happens when the store is unreachable.
///
/// A cache is not a database. The default is [`Degrade`](Self::Degrade): a
/// Redis outage turns a `get` into a miss and a `set` into a no-op, and the
/// request proceeds to the source of truth. Sessions and locks declare
/// [`Fail`](Self::Fail), because silently losing one of those is worse than a
/// 503.
///
/// ```
/// use moso_kv::FailureMode;
///
/// assert_eq!(FailureMode::default(), FailureMode::Degrade);
/// assert!(FailureMode::Degrade.degrades());
/// assert_eq!(FailureMode::Fail.as_str(), "fail");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FailureMode {
    /// Swallow a transient failure and behave as if the key were absent.
    #[default]
    Degrade,
    /// Propagate a transient failure, which becomes a 503.
    Fail,
}

impl FailureMode {
    /// Whether a transient failure is swallowed.
    ///
    /// ```
    /// use moso_kv::FailureMode;
    ///
    /// assert!(FailureMode::Degrade.degrades());
    /// assert!(!FailureMode::Fail.degrades());
    /// ```
    #[must_use]
    pub const fn degrades(self) -> bool {
        matches!(self, FailureMode::Degrade)
    }

    /// The name in a log field or a metric label.
    ///
    /// ```
    /// use moso_kv::FailureMode;
    ///
    /// assert_eq!(FailureMode::Degrade.as_str(), "degrade");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureMode::Degrade => "degrade",
            FailureMode::Fail => "fail",
        }
    }
}

impl std::fmt::Display for FailureMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Namespace
// ---------------------------------------------------------------------------

/// A typed slice of the keyspace.
///
/// Almost always written by [`namespace!`](macro@crate::namespace) rather than by
/// hand; a hand-written
/// impl is for the case where the key type needs a bound the macro cannot
/// express.
///
/// ```
/// use moso_kv::codec::Json;
/// use moso_kv::{FailureMode, Namespace};
/// use std::time::Duration;
///
/// /// The session record, keyed by session id.
/// pub struct Session;
///
/// impl Namespace for Session {
///     type Key = uuid::Uuid;
///     type Value = String;
///     type Codec = Json;
///
///     const NAME: &'static str = "Session";
///     const PREFIX: &'static str = "session";
///     const TTL: Option<Duration> = Some(Duration::from_secs(8 * 3600));
///     // Losing a session silently logs everybody out with no error anywhere.
///     const FAILURE_MODE: FailureMode = FailureMode::Fail;
/// }
///
/// assert_eq!(Session::VERSION, 1);
/// assert!(!Session::FAILURE_MODE.degrades());
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a kv namespace",
    label = "this type has no `Namespace` impl",
    note = "a namespace binds a key type, a value type, a prefix and a failure mode together",
    note = "help: moso_kv::namespace! {{ pub {Self}: u64 => String, ttl = minutes(5); }}"
)]
pub trait Namespace: Send + Sync + 'static {
    /// What names one value.
    type Key: KeyPart + ?Sized;

    /// What is stored.
    type Value: Encodable<Self::Codec>;

    /// How the value becomes bytes.
    type Codec: Codec;

    /// The Rust name, for error messages and metric labels.
    ///
    /// `namespace!` fills this with `stringify!` of the type name, so a decode
    /// failure says `Profile` and not `profile:2`.
    const NAME: &'static str;

    /// The key segment, `[a-z0-9_-]{1,48}`.
    ///
    /// `namespace!` derives it from the type name in `snake_case` and checks it
    /// at compile time.
    const PREFIX: &'static str;

    /// Bump to invalidate every key in this namespace at once.
    ///
    /// A deploy that changes a cached value's shape bumps the version; the old
    /// keys are then unreachable and expire on their own TTL. This is the
    /// alternative to a `FLUSHDB` that also empties everybody else's namespaces.
    const VERSION: u16 = 1;

    /// How long a value lives. `None` means "until something deletes it".
    const TTL: Option<Duration> = None;

    /// How long a *negative* value lives, when the value type is an `Option`.
    ///
    /// Shorter than [`TTL`](Self::TTL) on purpose: a "this user does not exist"
    /// answer should stop being served soon after the user is created. `None`
    /// falls back to `TTL`.
    const NEGATIVE_TTL: Option<Duration> = None;

    /// What an unreachable store does to a request.
    const FAILURE_MODE: FailureMode = FailureMode::Degrade;

    /// Whether this value is a cached absence.
    ///
    /// The default is `false`. [`namespace!`](macro@crate::namespace) overrides it
    /// with
    /// [`NegativeProbe`], which answers `is_none()` for an `Option` value type
    /// and `false` for everything else.
    fn is_negative(value: &Self::Value) -> bool {
        let _ = value;
        false
    }

    /// The TTL to write this value under: [`NEGATIVE_TTL`](Self::NEGATIVE_TTL)
    /// for a cached absence, [`TTL`](Self::TTL) otherwise.
    ///
    /// ```
    /// use moso_kv::{minutes, seconds, Namespace};
    ///
    /// moso_kv::namespace! {
    ///     /// Whether a coupon code exists.
    ///     pub Coupon: String => Option<u32>, ttl = minutes(10), negative_ttl = seconds(30);
    /// }
    ///
    /// assert_eq!(Coupon::ttl_for(&Some(1)), Some(minutes(10)));
    /// assert_eq!(Coupon::ttl_for(&None), Some(seconds(30)));
    /// ```
    fn ttl_for(value: &Self::Value) -> Option<Duration> {
        if Self::is_negative(value) {
            Self::NEGATIVE_TTL.or(Self::TTL)
        } else {
            Self::TTL
        }
    }
}

// ---------------------------------------------------------------------------
// NegativeProbe
// ---------------------------------------------------------------------------

/// Answers "is this value a cached absence?" without `specialization`.
///
/// `NegativeProbe<'_, Option<T>>` has an **inherent** `is_negative`, and every
/// `NegativeProbe<'_, T>` gets a trait one through [`NotNegative`]. Rust
/// resolves an inherent method before a trait method, so the `Option` case
/// wins where it applies and the fallback covers everything else.
///
/// ```
/// use moso_kv::namespace::{NegativeProbe, NotNegative as _};
///
/// assert!(NegativeProbe(&None::<u8>).is_negative());
/// assert!(!NegativeProbe(&Some(1_u8)).is_negative());
/// assert!(!NegativeProbe(&"present").is_negative());
/// ```
#[derive(Debug, Clone, Copy)]
pub struct NegativeProbe<'a, T: ?Sized>(
    /// The value being probed.
    pub &'a T,
);

impl<T> NegativeProbe<'_, Option<T>> {
    /// `true` when the `Option` is `None`.
    ///
    /// ```
    /// use moso_kv::namespace::NegativeProbe;
    ///
    /// assert!(NegativeProbe(&None::<String>).is_negative());
    /// ```
    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.0.is_none()
    }
}

/// The fallback half of [`NegativeProbe`]: everything that is not an `Option`
/// is never a cached absence.
///
/// Bring it into scope to call `is_negative` on a probe over a non-`Option`
/// type. [`namespace!`](macro@crate::namespace) does that for you.
///
/// ```
/// use moso_kv::namespace::{NegativeProbe, NotNegative as _};
///
/// assert!(!NegativeProbe(&42_u8).is_negative());
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be probed for negativity",
    note = "this trait is implemented for every `NegativeProbe<'_, T>`; if you are seeing this, \
            the probe was built over something that is not a `NegativeProbe`"
)]
pub trait NotNegative {
    /// Always `false`.
    fn is_negative(&self) -> bool {
        false
    }
}

// The blanket half. `do_not_recommend` because a user never implements this —
// the useful message is always on `Namespace` or on `Encodable`.
#[diagnostic::do_not_recommend]
impl<T: ?Sized> NotNegative for NegativeProbe<'_, T> {}

// ---------------------------------------------------------------------------
// Duration helpers
// ---------------------------------------------------------------------------

/// `n` seconds, as a `const` expression a `namespace!` can use.
///
/// ```
/// use moso_kv::seconds;
/// use std::time::Duration;
///
/// const TTL: Duration = seconds(30);
/// assert_eq!(TTL, Duration::from_secs(30));
/// ```
#[must_use]
pub const fn seconds(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// `n` minutes.
///
/// ```
/// use moso_kv::minutes;
/// use std::time::Duration;
///
/// assert_eq!(minutes(15), Duration::from_secs(900));
/// ```
#[must_use]
pub const fn minutes(n: u64) -> Duration {
    Duration::from_secs(n * 60)
}

/// `n` hours.
///
/// ```
/// use moso_kv::hours;
/// use std::time::Duration;
///
/// assert_eq!(hours(2), Duration::from_secs(7_200));
/// ```
#[must_use]
pub const fn hours(n: u64) -> Duration {
    Duration::from_secs(n * 3_600)
}

/// `n` days.
///
/// ```
/// use moso_kv::days;
/// use std::time::Duration;
///
/// assert_eq!(days(1), Duration::from_secs(86_400));
/// ```
#[must_use]
pub const fn days(n: u64) -> Duration {
    Duration::from_secs(n * 86_400)
}

// ---------------------------------------------------------------------------
// snake_case, at compile time
// ---------------------------------------------------------------------------

/// How long `name` becomes in `snake_case`.
///
/// The first half of the compile-time prefix derivation: an array's length has
/// to be a `const`, so the length is computed first and the bytes second.
///
/// ```
/// use moso_kv::namespace::snake_len;
///
/// const _: () = assert!(snake_len("LoginCode") == "login_code".len());
/// assert_eq!(snake_len("Profile"), 7);
/// assert_eq!(snake_len("IpRate"), 7);
/// ```
#[must_use]
pub const fn snake_len(name: &str) -> usize {
    let bytes = name.as_bytes();
    let mut index = 0;
    let mut len = 0;
    while index < bytes.len() {
        if needs_underscore(bytes, index) {
            len += 1;
        }
        len += 1;
        index += 1;
    }
    len
}

/// `name` in `snake_case`, as exactly `N` bytes.
///
/// `N` must be [`snake_len`] of the same string; a mismatch is a compile error
/// on the array length rather than a silently truncated prefix.
///
/// ```
/// use moso_kv::namespace::{snake_bytes, snake_len};
///
/// const NAME: &str = "LoginCode";
/// const N: usize = snake_len(NAME);
/// const BUF: [u8; N] = snake_bytes(NAME);
/// assert_eq!(&BUF, b"login_code");
/// ```
#[must_use]
pub const fn snake_bytes<const N: usize>(name: &str) -> [u8; N] {
    let bytes = name.as_bytes();
    let mut out = [0_u8; N];
    let mut index = 0;
    let mut written = 0;
    while index < bytes.len() {
        if needs_underscore(bytes, index) {
            out[written] = b'_';
            written += 1;
        }
        out[written] = bytes[index].to_ascii_lowercase();
        written += 1;
        index += 1;
    }
    out
}

/// Whether a `_` goes before `bytes[index]`: an upper-case letter that follows
/// a lower-case letter or a digit starts a new word.
const fn needs_underscore(bytes: &[u8], index: usize) -> bool {
    if index == 0 || !bytes[index].is_ascii_uppercase() {
        return false;
    }
    let previous = bytes[index - 1];
    previous.is_ascii_lowercase() || previous.is_ascii_digit()
}

/// A `&'static [u8]` as a `&'static str`, in a `const`.
///
/// # Panics
///
/// If the bytes are not UTF-8. [`snake_bytes`] only ever writes ASCII, so this
/// fires only when a type name contains a non-ASCII letter — in which case the
/// compile error is the right outcome, and
/// [`assert_name`](crate::key::assert_name) would have rejected the prefix
/// immediately afterwards anyway.
///
/// ```
/// use moso_kv::namespace::buf_as_str;
///
/// const BUF: [u8; 3] = *b"abc";
/// const NAME: &str = buf_as_str(&BUF);
/// assert_eq!(NAME, "abc");
/// ```
#[must_use]
pub const fn buf_as_str(bytes: &'static [u8]) -> &'static str {
    match core::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => panic!("a namespace name must be ASCII"),
    }
}

// ---------------------------------------------------------------------------
// namespace!
// ---------------------------------------------------------------------------

/// Declare typed namespaces.
///
/// ```
/// use moso_kv::{minutes, seconds, Namespace};
///
/// moso_kv::namespace! {
///     /// Cached user profile, refreshed on write.
///     pub Profile: u64 => Option<String>, ttl = minutes(15), negative_ttl = seconds(30);
///
///     /// Per-IP request counter for the rate limiter.
///     pub IpRate: std::net::IpAddr => u64, ttl = minutes(1), codec = Raw;
///
///     /// The session record. Losing one logs everybody out.
///     pub Session: String => String, ttl = minutes(480), on_failure = fail, version = 3;
/// }
///
/// assert_eq!(Profile::PREFIX, "profile");
/// assert_eq!(IpRate::PREFIX, "ip_rate");
/// assert_eq!(Session::VERSION, 3);
/// assert!(!Session::FAILURE_MODE.degrades());
/// ```
///
/// # Grammar
///
/// ```text
/// [attributes] [vis] Name : KeyType => ValueType [, option]* ;
/// ```
///
/// | Option | Default | Meaning |
/// | --- | --- | --- |
/// | `ttl = <Duration>` | none | how long a value lives |
/// | `negative_ttl = <Duration>` | `ttl` | how long a cached `None` lives |
/// | `codec = Json` or `Raw` or a type | `Json` | how a value becomes bytes |
/// | `version = <u16>` | `1` | bump to invalidate the namespace |
/// | `prefix = "<literal>"` | `snake_case` of the name | the key segment |
/// | `on_failure = degrade` or `fail` | `degrade` | what an outage does |
///
/// Every option is optional, they may appear in any order, and an unknown one
/// is a compile error naming the six that exist.
///
/// # What it generates
///
/// For each entry: a zero-sized `pub struct`, its [`Namespace`] impl, and a
/// `const _: () = assert_name(PREFIX);` that rejects an unusable prefix at
/// compile time.
///
/// ```compile_fail
/// moso_kv::namespace! {
///     /// A prefix with a colon would introduce a key segment.
///     pub Bad: u64 => u64, prefix = "a:b";
/// }
/// ```
#[macro_export]
macro_rules! namespace {
    () => {};

    // An entry with no options.
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident : $key:ty => $value:ty ; $($rest:tt)*
    ) => {
        $crate::__ns_emit! {
            [$(#[$meta])*] [$vis $name] [$key] [$value]
            [$crate::__ns_default_prefix!($name)]
            [1_u16]
            [::core::option::Option::None]
            [::core::option::Option::None]
            [$crate::codec::Json]
            [$crate::FailureMode::Degrade]
        }
        $crate::namespace! { $($rest)* }
    };

    // An entry with options: peel the option list off at the `;`, appending a
    // trailing comma so that every option is comma-terminated.
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident : $key:ty => $value:ty , $($tail:tt)*
    ) => {
        $crate::__ns_split! {
            [$(#[$meta])*] [$vis $name] [$key] [$value]
            []
            $($tail)*
        }
    };
}

/// The `snake_case` of a namespace's type name, as a `&'static str`.
///
/// Not callable as a function: the array length has to be a `const` derived
/// from the same string, which only a block of `const` items can express.
#[doc(hidden)]
#[macro_export]
macro_rules! __ns_default_prefix {
    ($name:ident) => {{
        const __MOSO_KV_NAME: &str = ::core::stringify!($name);
        const __MOSO_KV_LEN: usize = $crate::namespace::snake_len(__MOSO_KV_NAME);
        const __MOSO_KV_BUF: [u8; __MOSO_KV_LEN] = $crate::namespace::snake_bytes(__MOSO_KV_NAME);
        $crate::namespace::buf_as_str(&__MOSO_KV_BUF)
    }};
}

/// Split an entry's option list from the entries that follow it.
#[doc(hidden)]
#[macro_export]
macro_rules! __ns_split {
    // The `;` ends this entry: parse the options, then carry on.
    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$($opts:tt)*]
        ; $($rest:tt)*
    ) => {
        $crate::__ns_opts! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$crate::__ns_default_prefix!($name)]
            [1_u16]
            [::core::option::Option::None]
            [::core::option::Option::None]
            [$crate::codec::Json]
            [$crate::FailureMode::Degrade]
            $($opts)* ,
        }
        $crate::namespace! { $($rest)* }
    };

    // Anything else belongs to the option list.
    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$($opts:tt)*]
        $next:tt $($tail:tt)*
    ) => {
        $crate::__ns_split! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$($opts)* $next]
            $($tail)*
        }
    };
}

/// Fold one `name = value` option into the accumulator, then emit.
#[doc(hidden)]
#[macro_export]
macro_rules! __ns_opts {
    // Nothing left: emit.
    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
        $(,)?
    ) => {
        $crate::__ns_emit! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$prefix] [$version] [$ttl] [$nttl] [$codec] [$failure]
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
        ttl = $new:expr, $($tail:tt)*
    ) => {
        $crate::__ns_opts! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$prefix] [$version] [::core::option::Option::Some($new)] [$nttl] [$codec] [$failure]
            $($tail)*
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
        negative_ttl = $new:expr, $($tail:tt)*
    ) => {
        $crate::__ns_opts! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$prefix] [$version] [$ttl] [::core::option::Option::Some($new)] [$codec] [$failure]
            $($tail)*
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
        codec = Json, $($tail:tt)*
    ) => {
        $crate::__ns_opts! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$prefix] [$version] [$ttl] [$nttl] [$crate::codec::Json] [$failure]
            $($tail)*
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
        codec = Raw, $($tail:tt)*
    ) => {
        $crate::__ns_opts! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$prefix] [$version] [$ttl] [$nttl] [$crate::codec::Raw] [$failure]
            $($tail)*
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
        codec = $new:ty, $($tail:tt)*
    ) => {
        $crate::__ns_opts! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$prefix] [$version] [$ttl] [$nttl] [$new] [$failure]
            $($tail)*
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
        version = $new:expr, $($tail:tt)*
    ) => {
        $crate::__ns_opts! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$prefix] [$new] [$ttl] [$nttl] [$codec] [$failure]
            $($tail)*
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
        prefix = $new:literal, $($tail:tt)*
    ) => {
        $crate::__ns_opts! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$new] [$version] [$ttl] [$nttl] [$codec] [$failure]
            $($tail)*
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
        on_failure = degrade, $($tail:tt)*
    ) => {
        $crate::__ns_opts! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$prefix] [$version] [$ttl] [$nttl] [$codec] [$crate::FailureMode::Degrade]
            $($tail)*
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
        on_failure = fail, $($tail:tt)*
    ) => {
        $crate::__ns_opts! {
            [$($meta)*] [$vis $name] [$key] [$value]
            [$prefix] [$version] [$ttl] [$nttl] [$codec] [$crate::FailureMode::Fail]
            $($tail)*
        }
    };

    // Anything else: one error naming the six options, on the offending token.
    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
        $bad:ident = $($tail:tt)*
    ) => {
        ::core::compile_error!(::core::concat!(
            "`",
            ::core::stringify!($bad),
            "` is not a namespace option. The options are `ttl`, `negative_ttl`, `codec`, \
             `version`, `prefix` and `on_failure`. help: namespace! { pub ",
            ::core::stringify!($name),
            ": Key => Value, ttl = minutes(5), on_failure = fail; }"
        ));
    };
}

/// Write one namespace: the type, its impl, and the compile-time prefix check.
#[doc(hidden)]
#[macro_export]
macro_rules! __ns_emit {
    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$key:ty] [$value:ty]
        [$prefix:expr] [$version:expr] [$ttl:expr] [$nttl:expr] [$codec:ty] [$failure:expr]
    ) => {
        $($meta)*
        ///
        /// A zero-sized `moso_kv::Namespace`, written by `moso_kv::namespace!`.
        #[derive(::core::fmt::Debug, ::core::clone::Clone, ::core::marker::Copy)]
        #[derive(::core::default::Default)]
        $vis struct $name;

        impl $crate::Namespace for $name {
            type Key = $key;
            type Value = $value;
            type Codec = $codec;

            const NAME: &'static str = ::core::stringify!($name);
            const PREFIX: &'static str = $prefix;
            const VERSION: u16 = $version;
            const TTL: ::core::option::Option<::core::time::Duration> = $ttl;
            const NEGATIVE_TTL: ::core::option::Option<::core::time::Duration> = $nttl;
            const FAILURE_MODE: $crate::FailureMode = $failure;

            fn is_negative(value: &$value) -> bool {
                // The inherent method on `NegativeProbe<'_, Option<T>>` wins
                // here when `$value` is an `Option`; otherwise the trait's
                // `false` does. No `specialization`, no bound on the user.
                #[allow(unused_imports)]
                use $crate::namespace::NotNegative as _;
                $crate::namespace::NegativeProbe(value).is_negative()
            }
        }

        // The prefix has to be a legal key segment, and finding that out at
        // compile time is the difference between a build failure and a 500 on
        // the first cache read.
        const _: () = $crate::key::assert_name(<$name as $crate::Namespace>::PREFIX);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_is_computed_at_compile_time() {
        const _: () = assert!(snake_len("Profile") == 7);
        const _: () = assert!(snake_len("LoginCode") == 10);

        assert_eq!(snake_len("IpRate"), "ip_rate".len());
        assert_eq!(snake_len("HTTPCache"), "httpcache".len());
        assert_eq!(snake_len("a"), 1);
        assert_eq!(snake_len(""), 0);
    }

    #[test]
    fn snake_bytes_matches_snake_len() {
        const A: [u8; snake_len("LoginCode")] = snake_bytes("LoginCode");
        assert_eq!(&A, b"login_code");

        const B: [u8; snake_len("IpRate2")] = snake_bytes("IpRate2");
        assert_eq!(&B, b"ip_rate2");

        // A leading capital never gets a leading underscore.
        const C: [u8; snake_len("Profile")] = snake_bytes("Profile");
        assert_eq!(&C, b"profile");

        // A run of capitals is one word.
        const D: [u8; snake_len("HTTPCache")] = snake_bytes("HTTPCache");
        assert_eq!(&D, b"httpcache");

        // An underscore already there is left alone.
        const E: [u8; snake_len("login_code")] = snake_bytes("login_code");
        assert_eq!(&E, b"login_code");
    }

    #[test]
    fn a_failure_mode_names_itself() {
        assert_eq!(FailureMode::default(), FailureMode::Degrade);
        assert!(FailureMode::Degrade.degrades());
        assert!(!FailureMode::Fail.degrades());
        assert_eq!(FailureMode::Fail.to_string(), "fail");
    }

    #[test]
    fn the_probe_distinguishes_option_from_everything_else() {
        use crate::namespace::NotNegative as _;

        assert!(NegativeProbe(&None::<u8>).is_negative());
        assert!(!NegativeProbe(&Some(1_u8)).is_negative());
        assert!(!NegativeProbe(&1_u8).is_negative());
        assert!(!NegativeProbe(&"x").is_negative());
    }

    crate::namespace! {
        /// The plainest possible namespace.
        pub Plain: u64 => String;

        /// Every option at once, in a deliberately jumbled order.
        pub(crate) Everything: (u32, String) => Option<Vec<u8>>,
            on_failure = fail,
            version = 7,
            negative_ttl = seconds(5),
            codec = Json,
            ttl = minutes(3),
            prefix = "custom-name";

        /// A counter, unframed so `INCR` can read it.
        pub IpRate: std::net::IpAddr => u64, ttl = minutes(1), codec = Raw;
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        assert_eq!(Plain::NAME, "Plain");
        assert_eq!(Plain::PREFIX, "plain");
        assert_eq!(Plain::VERSION, 1);
        assert_eq!(Plain::TTL, None);
        assert_eq!(Plain::NEGATIVE_TTL, None);
        assert_eq!(Plain::FAILURE_MODE, FailureMode::Degrade);
        assert!(!Plain::is_negative(&String::new()));
    }

    #[test]
    fn every_option_lands_where_it_should_whatever_the_order() {
        assert_eq!(Everything::NAME, "Everything");
        assert_eq!(Everything::PREFIX, "custom-name");
        assert_eq!(Everything::VERSION, 7);
        assert_eq!(Everything::TTL, Some(minutes(3)));
        assert_eq!(Everything::NEGATIVE_TTL, Some(seconds(5)));
        assert_eq!(Everything::FAILURE_MODE, FailureMode::Fail);
    }

    #[test]
    fn an_option_value_type_gets_negative_caching() {
        assert!(Everything::is_negative(&None));
        assert!(!Everything::is_negative(&Some(vec![1])));
        assert_eq!(Everything::ttl_for(&None), Some(seconds(5)));
        assert_eq!(Everything::ttl_for(&Some(vec![1])), Some(minutes(3)));
    }

    #[test]
    fn a_non_option_value_type_is_never_negative() {
        assert!(!IpRate::is_negative(&7));
        assert_eq!(IpRate::ttl_for(&7), Some(minutes(1)));
        assert_eq!(IpRate::PREFIX, "ip_rate");
    }

    #[test]
    fn a_namespace_type_is_zero_sized_and_default() {
        assert_eq!(std::mem::size_of::<Plain>(), 0);
        let _ = Plain;
        let _: Plain = Default::default();
    }
}
