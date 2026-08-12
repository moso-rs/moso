//! Fixtures: seeded fake data, factories, and the fast test password hash.
//!
//! # The one idea
//!
//! **A fixture should say only what the test is about.** `43-testing.md` writes
//! it as
//!
//! ```text
//! let admin = User::factory().is_admin(true).create(&db).await?;
//! ```
//!
//! and every other column — the email, the name, the password hash, and the
//! organisation row `users.org_id` insists on — is invented. This module is the
//! runtime that does the inventing: a deterministic [`Faker`], an
//! [`EntityFactory`] that reads an entity's own
//! [`EntityDescriptor`] to decide what to fill in,
//! and a [`RelationPlan`] that creates the parent rows a `NOT NULL` foreign key
//! requires.
//!
//! ```
//! use moso_test::factory::{Faker, Seed};
//!
//! // Seeded from the test's own name, so a failure reproduces exactly.
//! let mut faker = Faker::for_test("users::create_returns_201");
//! let email = faker.email();
//! assert!(email.contains("@example."), "{email}");
//!
//! // The same seed always produces the same data.
//! let mut again = Faker::for_test("users::create_returns_201");
//! assert_eq!(again.email(), email);
//! ```
//!
//! # Determinism
//!
//! Every value comes from a [`Seed`], and the seed comes from the test's name.
//! A test that fails on a fake email address fails on *that* email address every
//! time, which is the difference between a bug and a haunting. Nothing here
//! reads the clock or the operating system's entropy.
//!
//! # Where `#[derive(Factory)]` fits
//!
//! `43-testing.md` gives factories typed setters:
//!
//! ```text
//! #[derive(Entity, Factory)]
//! #[factory(email = "faker::internet::Email", password = "PasswordHash::test()")]
//! pub struct User { … }
//! ```
//!
//! The derive lives in `moso-orm-macros`; **this module is the runtime it
//! targets**, and it is usable without it. `#[derive(Factory)]` generates an
//! `impl Factory for User` whose [`Factory::defaults`] applies the `#[factory]`
//! attributes, plus a `UserFactory` newtype whose typed setters call
//! [`EntityFactory::set`]. Everything else — the faker, the descriptor walk, the
//! relation plan, the insert — is here and is already tested.
//!
//! # Passwords
//!
//! [`PasswordHash::test`] exists because Argon2 in a fixture makes a suite
//! unusable: a hundred users at 100 ms each is ten seconds of a test run spent
//! proving that a library everyone already trusts still works. It is **not** a
//! password hash, it is labelled as such in its own text, and
//! [`PasswordHash::is_test_hash`] lets an authentication backend refuse it
//! outside tests.

use core::marker::PhantomData;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use moso_orm::descriptor::{ColumnDescriptor, ForeignKeyDescriptor};
use moso_orm::{Db, Entity, EntityDescriptor, Executor};
use moso_sql::{
    DataType, Decimal, Expr, Ident, Insert, Returning, TableRef, Timestamp, Uuid, Value,
};

use crate::db::BoxFuture;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything a factory can fail at.
///
/// ```
/// use moso_test::factory::Error;
///
/// let error = Error::NoParentFactory {
///     table: "organisations".to_owned(),
///     column: "org_id".to_owned(),
///     entity: "User",
/// };
/// assert!(error.to_string().contains("organisations"));
/// assert!(error.to_string().contains("register"));
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A required parent row is needed and nothing knows how to make one.
    NoParentFactory {
        /// The table the foreign key points at.
        table: String,
        /// The local column that needs a value.
        column: String,
        /// The entity being built.
        entity: &'static str,
    },
    /// The foreign key cannot be satisfied by inventing a parent.
    Unsatisfiable {
        /// The local column.
        column: String,
        /// Why not.
        reason: String,
        /// The entity being built.
        entity: &'static str,
    },
    /// The database refused, or the row could not be decoded.
    Database {
        /// The entity being built.
        entity: &'static str,
        /// What the ORM said.
        message: String,
    },
    /// The insert returned no row, so there is nothing to hand back.
    NoRowReturned {
        /// The entity being built.
        entity: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoParentFactory {
                table,
                column,
                entity,
            } => write!(
                f,
                "`{entity}` needs a row in `{table}` for `{column}`, and no factory is \
                 registered for it\n  help: register one: \
                 `FactoryRegistry::global().register::<Organisation>()`\n  help: or supply the \
                 parent: `{entity}::factory().set(\"{column}\", parent.id)`"
            ),
            Self::Unsatisfiable {
                column,
                reason,
                entity,
            } => write!(
                f,
                "`{entity}` cannot invent a value for `{column}`: {reason}\n  help: supply it: \
                 `{entity}::factory().set(\"{column}\", ..)`"
            ),
            Self::Database { entity, message } => {
                write!(f, "creating a `{entity}` fixture: {message}")
            }
            Self::NoRowReturned { entity } => write!(
                f,
                "the insert for a `{entity}` fixture returned no row\n  help: the table may have \
                 a `BEFORE INSERT` trigger that suppresses it; use `EntityFactory::row` and \
                 insert it yourself"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// What every factory operation returns.
///
/// ```
/// fn ok() -> moso_test::factory::Result<u8> {
///     Ok(7)
/// }
/// assert_eq!(ok().unwrap(), 7);
/// ```
pub type Result<T, E = Error> = core::result::Result<T, E>;

// ---------------------------------------------------------------------------
// Seed
// ---------------------------------------------------------------------------

/// The number every fake value in a test comes from.
///
/// ```
/// use moso_test::factory::Seed;
///
/// // The same name is always the same seed, in this process and the next.
/// assert_eq!(Seed::of("users::create"), Seed::of("users::create"));
/// assert_ne!(Seed::of("users::create"), Seed::of("users::update"));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Seed(u64);

impl Seed {
    /// An explicit seed.
    ///
    /// ```
    /// assert_eq!(moso_test::factory::Seed::new(7).value(), 7);
    /// ```
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The seed a name hashes to.
    ///
    /// Pass the test's own name — `module_path!()` and the function name — so
    /// that two tests in one file do not share fixtures and one test is the same
    /// on every machine.
    ///
    /// ```
    /// use moso_test::factory::Seed;
    ///
    /// assert_eq!(Seed::of("a"), Seed::of("a"));
    /// assert_ne!(Seed::of("a"), Seed::of("b"));
    /// ```
    #[must_use]
    pub fn of(name: &str) -> Self {
        // FNV-1a: a name is short, this is not a security boundary, and a
        // dependency for eight lines is a dependency too many.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in name.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
        Self(hash)
    }

    /// The number.
    ///
    /// ```
    /// assert_eq!(moso_test::factory::Seed::new(3).value(), 3);
    /// ```
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    /// A derived seed, for one row of a `count(n)` batch.
    ///
    /// Derived rather than sequential so that row 4 of a run of ten is the same
    /// as row 4 of a run of a hundred.
    ///
    /// ```
    /// use moso_test::factory::Seed;
    ///
    /// let seed = Seed::of("posts::list");
    /// assert_eq!(seed.derive(4), seed.derive(4));
    /// assert_ne!(seed.derive(4), seed.derive(5));
    ///
    /// // Row zero is a derived seed like any other, not the seed itself.
    /// assert_ne!(seed.derive(0), seed);
    /// ```
    #[must_use]
    pub const fn derive(self, index: usize) -> Self {
        // `index + 1` and a final mixing round, so that `derive(0)` is not the
        // identity: row zero of a batch must not silently share a generator with
        // the batch itself.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "an index beyond 2^64 rows is not a thing"
        )]
        let step = (index as u64).wrapping_add(1);
        Self(mix64(self.0 ^ step.wrapping_mul(0x9e37_79b9_7f4a_7c15)))
    }
}

/// One round of SplitMix64's finaliser. Avalanches, which is all that is asked
/// of it.
const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl fmt::Display for Seed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Faker
// ---------------------------------------------------------------------------

/// Word lists. Small on purpose: a fixture wants *plausible*, not *varied*, and
/// a megabyte of names is a megabyte in everybody's binary.
const FIRST_NAMES: &[&str] = &[
    "ada",
    "alan",
    "barbara",
    "brian",
    "claude",
    "dennis",
    "edsger",
    "grace",
    "haskell",
    "john",
    "katherine",
    "ken",
    "linus",
    "margaret",
    "niklaus",
    "radia",
    "tony",
    "vint",
];

const LAST_NAMES: &[&str] = &[
    "lovelace",
    "turing",
    "liskov",
    "kernighan",
    "shannon",
    "ritchie",
    "dijkstra",
    "hopper",
    "curry",
    "backus",
    "johnson",
    "thompson",
    "torvalds",
    "hamilton",
    "wirth",
    "perlman",
    "hoare",
    "cerf",
];

const WORDS: &[&str] = &[
    "alpha", "amber", "anchor", "beacon", "bramble", "cedar", "cobalt", "compass", "delta",
    "ember", "falcon", "fathom", "garnet", "harbour", "indigo", "juniper", "kestrel", "lantern",
    "meadow", "nimbus", "onyx", "pebble", "quarry", "ripple", "saffron", "thistle", "umber",
    "verdant", "willow", "zephyr",
];

/// RFC 2606 reserves these, so a fixture can never send real mail to a real
/// person. This is not a detail: a test suite that mails strangers is a bug that
/// only shows up in someone else's inbox.
const DOMAINS: &[&str] = &["example.com", "example.org", "example.net"];

/// Deterministic fake data.
///
/// ```
/// use moso_test::factory::{Faker, Seed};
///
/// let mut faker = Faker::new(Seed::new(1));
/// let first = faker.email();
///
/// let mut same = Faker::new(Seed::new(1));
/// assert_eq!(same.email(), first);
///
/// // And it moves on, so a unique index is never violated by two fixtures.
/// assert_ne!(faker.email(), first);
/// ```
#[derive(Clone, Debug)]
pub struct Faker {
    seed: Seed,
    state: u64,
    sequence: u64,
    /// A short discriminator, folded into every value that has to be unique.
    ///
    /// Two generators with different seeds draw from the same eighteen first
    /// names and eighteen surnames, so within a batch of fifty rows a collision
    /// on `email` is not unlikely — it is *expected*, by the birthday bound —
    /// and a collision on a `unique` index is a test failure with nothing to do
    /// with the code under test. Tagging the unique part with the seed removes
    /// the possibility rather than making it rarer.
    tag: u32,
}

impl Faker {
    /// A generator for `seed`.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let faker = Faker::new(Seed::new(42));
    /// assert_eq!(faker.seed().value(), 42);
    /// ```
    #[must_use]
    pub const fn new(seed: Seed) -> Self {
        let scrambled = mix64(seed.0);
        Self {
            seed,
            // A zero seed would make SplitMix64 start at a fixed point, which is
            // fine but makes `Seed::new(0)` look broken in a debugger.
            state: seed.0 ^ 0x2545_f491_4f6c_dd1d,
            sequence: 0,
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the low 32 bits are the discriminator; the rest is not wanted"
            )]
            tag: (scrambled ^ (scrambled >> 32)) as u32,
        }
    }

    /// A generator seeded from a test's name.
    ///
    /// ```
    /// use moso_test::factory::Faker;
    ///
    /// let mut a = Faker::for_test("posts::list_is_paginated");
    /// let mut b = Faker::for_test("posts::list_is_paginated");
    /// assert_eq!(a.name(), b.name());
    /// ```
    #[must_use]
    pub fn for_test(name: &str) -> Self {
        Self::new(Seed::of(name))
    }

    /// The seed this generator started from.
    ///
    /// Print it in a failure message: it is everything needed to reproduce the
    /// data.
    ///
    /// ```
    /// # use moso_test::factory::{Faker, Seed};
    /// assert_eq!(Faker::new(Seed::new(9)).seed(), Seed::new(9));
    /// ```
    #[must_use]
    pub const fn seed(&self) -> Seed {
        self.seed
    }

    /// Rewinds to the beginning, so the next value is the first one again.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let mut faker = Faker::new(Seed::new(1));
    /// let first = faker.word();
    /// faker.reset();
    /// assert_eq!(faker.word(), first);
    /// ```
    pub const fn reset(&mut self) {
        self.state = self.seed.0 ^ 0x2545_f491_4f6c_dd1d;
        self.sequence = 0;
    }

    /// The next raw number. SplitMix64 — small, fast, and good enough for
    /// fixtures.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let mut faker = Faker::new(Seed::new(1));
    /// assert_ne!(faker.next_u64(), faker.next_u64());
    /// ```
    pub const fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A monotonically increasing number, for the parts of a value that have to
    /// be unique.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let mut faker = Faker::new(Seed::new(1));
    /// assert_eq!(faker.next_in_sequence(), 0);
    /// assert_eq!(faker.next_in_sequence(), 1);
    /// ```
    pub const fn next_in_sequence(&mut self) -> u64 {
        let value = self.sequence;
        self.sequence += 1;
        value
    }

    /// A boolean.
    ///
    /// ```
    /// # use moso_test::factory::{Faker, Seed};
    /// let _: bool = Faker::new(Seed::new(1)).bool();
    /// ```
    pub const fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    /// An integer in `low..=high`, inclusive at both ends.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let mut faker = Faker::new(Seed::new(1));
    /// for _ in 0..100 {
    ///     let value = faker.i64_in(10, 20);
    ///     assert!((10..=20).contains(&value));
    /// }
    /// ```
    pub const fn i64_in(&mut self, low: i64, high: i64) -> i64 {
        if low >= high {
            return low;
        }
        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_possible_wrap,
            reason = "the width is computed modulo 2^64 on purpose, so that the whole range works"
        )]
        let span = (high.wrapping_sub(low) as u64).wrapping_add(1);
        if span == 0 {
            // `low..=high` is the whole of `i64`, so every value is in range.
            #[allow(clippy::cast_possible_wrap, reason = "every bit pattern is in range")]
            return self.next_u64() as i64;
        }
        #[allow(
            clippy::cast_possible_wrap,
            reason = "the remainder is below the width"
        )]
        let offset = (self.next_u64() % span) as i64;
        low.wrapping_add(offset)
    }

    /// A float in `low..=high`.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let mut faker = Faker::new(Seed::new(1));
    /// let value = faker.f64_in(0.0, 1.0);
    /// assert!((0.0..=1.0).contains(&value));
    /// ```
    pub fn f64_in(&mut self, low: f64, high: f64) -> f64 {
        #[allow(
            clippy::cast_precision_loss,
            reason = "the fraction only needs 53 bits, and this is fixture data"
        )]
        let unit = (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64;
        low + unit * (high - low)
    }

    /// One of `choices`.
    ///
    /// # Panics
    ///
    /// If `choices` is empty.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let mut faker = Faker::new(Seed::new(1));
    /// assert!(["red", "green"].contains(faker.one_of(&["red", "green"])));
    /// ```
    pub fn one_of<'a, T>(&mut self, choices: &'a [T]) -> &'a T {
        assert!(
            !choices.is_empty(),
            "moso-test: `Faker::one_of` needs at least one choice"
        );
        let index = usize::try_from(self.next_u64() % choices.len() as u64).unwrap_or(0);
        &choices[index]
    }

    /// A first name, lower-case.
    ///
    /// ```
    /// # use moso_test::factory::{Faker, Seed};
    /// assert!(!Faker::new(Seed::new(1)).first_name().is_empty());
    /// ```
    pub fn first_name(&mut self) -> &'static str {
        self.one_of(FIRST_NAMES)
    }

    /// A surname, lower-case.
    ///
    /// ```
    /// # use moso_test::factory::{Faker, Seed};
    /// assert!(!Faker::new(Seed::new(1)).last_name().is_empty());
    /// ```
    pub fn last_name(&mut self) -> &'static str {
        self.one_of(LAST_NAMES)
    }

    /// A full name, capitalised.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let name = Faker::new(Seed::new(1)).name();
    /// assert!(name.contains(' '));
    /// assert!(name.starts_with(|c: char| c.is_uppercase()));
    /// ```
    pub fn name(&mut self) -> String {
        let first = capitalise(self.first_name());
        let last = capitalise(self.last_name());
        format!("{first} {last}")
    }

    /// A handle: `ada.lovelace3f2a1b0`.
    ///
    /// The trailing part is this generator's discriminator and its sequence
    /// number, so two users never collide on a unique index — neither within one
    /// generator nor between the fifty generators a `count(50)` batch uses.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let mut faker = Faker::new(Seed::new(1));
    /// assert_ne!(faker.username(), faker.username());
    /// ```
    pub fn username(&mut self) -> String {
        let first = self.first_name();
        let last = self.last_name();
        let tag = self.tag;
        let ordinal = self.next_in_sequence();
        format!("{first}.{last}{tag:x}{ordinal}")
    }

    /// An address at a domain RFC 2606 reserves, so it can never reach anyone.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let email = Faker::new(Seed::new(1)).email();
    /// assert!(email.contains('@'));
    /// assert!(email.ends_with(".com") || email.ends_with(".org") || email.ends_with(".net"));
    /// ```
    pub fn email(&mut self) -> String {
        let local = self.username();
        let domain = self.one_of(DOMAINS);
        format!("{local}@{domain}")
    }

    /// A reserved domain.
    ///
    /// ```
    /// # use moso_test::factory::{Faker, Seed};
    /// assert!(Faker::new(Seed::new(1)).domain().starts_with("example."));
    /// ```
    pub fn domain(&mut self) -> String {
        (*self.one_of(DOMAINS)).to_owned()
    }

    /// An `https://` URL on a reserved domain.
    ///
    /// ```
    /// # use moso_test::factory::{Faker, Seed};
    /// assert!(Faker::new(Seed::new(1)).url().starts_with("https://"));
    /// ```
    pub fn url(&mut self) -> String {
        let domain = self.domain();
        let path = self.slug();
        format!("https://{domain}/{path}")
    }

    /// One word.
    ///
    /// ```
    /// # use moso_test::factory::{Faker, Seed};
    /// assert!(!Faker::new(Seed::new(1)).word().is_empty());
    /// ```
    pub fn word(&mut self) -> &'static str {
        self.one_of(WORDS)
    }

    /// `count` words, space separated.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// assert_eq!(Faker::new(Seed::new(1)).words(3).split(' ').count(), 3);
    /// ```
    pub fn words(&mut self, count: usize) -> String {
        (0..count.max(1))
            .map(|_| self.word())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A URL-safe slug, unique within this generator.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let mut faker = Faker::new(Seed::new(1));
    /// let slug = faker.slug();
    /// assert!(slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    /// assert_ne!(faker.slug(), slug);
    /// ```
    pub fn slug(&mut self) -> String {
        let a = self.word();
        let b = self.word();
        let tag = self.tag;
        let ordinal = self.next_in_sequence();
        format!("{a}-{b}-{tag:x}{ordinal}")
    }

    /// A capitalised title of three or four words.
    ///
    /// ```
    /// # use moso_test::factory::{Faker, Seed};
    /// assert!(Faker::new(Seed::new(1)).title().starts_with(|c: char| c.is_uppercase()));
    /// ```
    pub fn title(&mut self) -> String {
        let count = usize::try_from(self.next_u64() % 2).unwrap_or(0) + 3;
        capitalise(&self.words(count))
    }

    /// A sentence, capitalised, with a full stop.
    ///
    /// ```
    /// # use moso_test::factory::{Faker, Seed};
    /// assert!(Faker::new(Seed::new(1)).sentence().ends_with('.'));
    /// ```
    pub fn sentence(&mut self) -> String {
        let count = usize::try_from(self.next_u64() % 8).unwrap_or(0) + 6;
        format!("{}.", capitalise(&self.words(count)))
    }

    /// A paragraph of three to five sentences.
    ///
    /// ```
    /// # use moso_test::factory::{Faker, Seed};
    /// assert!(Faker::new(Seed::new(1)).paragraph().len() > 40);
    /// ```
    pub fn paragraph(&mut self) -> String {
        let count = usize::try_from(self.next_u64() % 3).unwrap_or(0) + 3;
        (0..count)
            .map(|_| self.sentence())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A UUID. Deterministic, so it is not a v4 and does not pretend to be: the
    /// version and variant bits are set correctly, and the rest is the seed.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let mut a = Faker::new(Seed::new(1));
    /// let mut b = Faker::new(Seed::new(1));
    /// assert_eq!(a.uuid(), b.uuid());
    /// ```
    pub const fn uuid(&mut self) -> Uuid {
        let high = self.next_u64().to_be_bytes();
        let low = self.next_u64().to_be_bytes();
        let mut bytes = [0_u8; 16];
        let mut index = 0;
        while index < 8 {
            bytes[index] = high[index];
            bytes[index + 8] = low[index];
            index += 1;
        }
        // Version 4, variant RFC 4122 — so that a `uuid` column round-trips
        // through every driver that validates them.
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Uuid::from_bytes(bytes)
    }

    /// A timestamp in the last year, to the second.
    ///
    /// Anchored to a fixed epoch rather than "now", because a fixture that moves
    /// with the wall clock is a fixture that fails on new year's eve.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let mut a = Faker::new(Seed::new(1));
    /// let mut b = Faker::new(Seed::new(1));
    /// assert_eq!(a.timestamp().unix_seconds(), b.timestamp().unix_seconds());
    /// ```
    pub fn timestamp(&mut self) -> Timestamp {
        // 2024-01-01T00:00:00Z, plus up to a year.
        const ANCHOR: i64 = 1_704_067_200;
        let offset = self.i64_in(0, 365 * 24 * 60 * 60);
        Timestamp::new(ANCHOR + offset, 0).unwrap_or_else(|_| {
            Timestamp::new(ANCHOR, 0).expect("a fixed anchor is always a valid timestamp")
        })
    }

    /// A decimal with two places, between `low` and `high` units.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let value = Faker::new(Seed::new(1)).decimal(0, 100);
    /// assert_eq!(value.scale(), 2);
    /// ```
    pub fn decimal(&mut self, low: i64, high: i64) -> Decimal {
        let cents = self.i64_in(low.saturating_mul(100), high.saturating_mul(100));
        Decimal::new(i128::from(cents), 2)
            .unwrap_or_else(|_| Decimal::new(0, 2).expect("zero with two places is representable"))
    }

    /// Some bytes.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// assert_eq!(Faker::new(Seed::new(1)).bytes(8).len(), 8);
    /// ```
    pub fn bytes(&mut self, length: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(length);
        while out.len() < length {
            out.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        out.truncate(length);
        out
    }

    /// A compact JSON object, for a `json`/`jsonb` column.
    ///
    /// ```
    /// use moso_test::factory::{Faker, Seed};
    ///
    /// let json = Faker::new(Seed::new(1)).json();
    /// assert!(json.starts_with('{') && json.ends_with('}'));
    /// ```
    pub fn json(&mut self) -> String {
        let key = self.word();
        let value = self.word();
        format!(r#"{{"{key}":"{value}"}}"#)
    }
}

/// `alpha beta` becomes `Alpha beta`. ASCII-only on purpose: the word lists are.
fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------

/// A password hash that is fast because it is not a password hash.
///
/// Argon2 is deliberately slow. A hundred users in a fixture at 100 ms each is
/// ten seconds per test file spent re-proving a property nobody doubts, and it
/// is the single most common reason a Rails or Django suite becomes too slow to
/// run. So fixtures use this instead.
///
/// It says so in its own text: every hash starts with [`PasswordHash::PREFIX`],
/// and [`PasswordHash::is_test_hash`] exists so that an authentication backend
/// can *refuse* one outside a test.
///
/// ```
/// use moso_test::factory::PasswordHash;
///
/// let hash = PasswordHash::test();
/// assert!(hash.verify(PasswordHash::DEFAULT_PASSWORD));
/// assert!(!hash.verify("something else"));
/// assert!(PasswordHash::is_test_hash(hash.as_str()));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PasswordHash(String);

impl PasswordHash {
    /// What every test hash starts with. Not a valid PHC identifier for any real
    /// algorithm, so nothing can mistake it for one.
    ///
    /// ```
    /// assert_eq!(moso_test::factory::PasswordHash::PREFIX, "$moso-test$v1$");
    /// ```
    pub const PREFIX: &'static str = "$moso-test$v1$";

    /// The password [`PasswordHash::test`] hashes.
    ///
    /// Long enough to pass a sensible length rule, so that a fixture does not
    /// trip the application's own validation.
    ///
    /// ```
    /// assert!(moso_test::factory::PasswordHash::DEFAULT_PASSWORD.len() >= 12);
    /// ```
    pub const DEFAULT_PASSWORD: &'static str = "correct horse battery staple";

    /// The hash of [`PasswordHash::DEFAULT_PASSWORD`].
    ///
    /// This is the `PasswordHash::test()` of `43-testing.md`.
    ///
    /// ```
    /// use moso_test::factory::PasswordHash;
    ///
    /// assert!(PasswordHash::test().verify(PasswordHash::DEFAULT_PASSWORD));
    /// ```
    #[must_use]
    pub fn test() -> Self {
        Self::of(Self::DEFAULT_PASSWORD)
    }

    /// The hash of a specific password.
    ///
    /// ```
    /// use moso_test::factory::PasswordHash;
    ///
    /// let hash = PasswordHash::of("hunter2");
    /// assert!(hash.verify("hunter2"));
    /// assert!(!hash.verify("hunter3"));
    /// ```
    #[must_use]
    pub fn of(password: &str) -> Self {
        Self(format!("{}{:016x}", Self::PREFIX, mix(password.as_bytes())))
    }

    /// Whether `password` is the one this hash was made from.
    ///
    /// ```
    /// # use moso_test::factory::PasswordHash;
    /// assert!(PasswordHash::of("a").verify("a"));
    /// ```
    #[must_use]
    pub fn verify(&self, password: &str) -> bool {
        self.0 == Self::of(password).0
    }

    /// The stored text.
    ///
    /// ```
    /// # use moso_test::factory::PasswordHash;
    /// assert!(PasswordHash::test().as_str().starts_with(PasswordHash::PREFIX));
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The stored text, owned.
    ///
    /// ```
    /// # use moso_test::factory::PasswordHash;
    /// assert!(PasswordHash::test().into_string().starts_with("$moso-test$"));
    /// ```
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    /// Whether `text` is one of these, and therefore must never be accepted by
    /// a production authentication backend.
    ///
    /// ```
    /// use moso_test::factory::PasswordHash;
    ///
    /// assert!(PasswordHash::is_test_hash(PasswordHash::test().as_str()));
    /// assert!(!PasswordHash::is_test_hash("$argon2id$v=19$m=19456,t=2,p=1$..."));
    /// ```
    #[must_use]
    pub fn is_test_hash(text: &str) -> bool {
        text.starts_with(Self::PREFIX)
    }
}

impl fmt::Display for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A few rounds of FNV-1a with a salt-shaped constant. Deliberately not a
/// password hash; see [`PasswordHash`].
fn mix(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for round in 0..4_u64 {
        hash ^= round.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    }
    hash
}

// ---------------------------------------------------------------------------
// Relation planning
// ---------------------------------------------------------------------------

/// One parent row a factory has to create before it can insert its own.
///
/// ```
/// use moso_sql::{Ident, TableRef};
/// use moso_test::factory::RelationStep;
///
/// let step = RelationStep::new(
///     Ident::from_static("org_id"),
///     TableRef::from_static("organisations"),
/// );
/// assert_eq!(step.column().as_str(), "org_id");
/// assert_eq!(step.table().name().as_str(), "organisations");
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct RelationStep {
    column: Ident,
    table: TableRef,
}

impl RelationStep {
    /// A step filling `column` from a new row in `table`.
    ///
    /// ```
    /// # use moso_sql::{Ident, TableRef};
    /// # use moso_test::factory::RelationStep;
    /// let step = RelationStep::new(Ident::from_static("a"), TableRef::from_static("b"));
    /// assert_eq!(step.column().as_str(), "a");
    /// ```
    #[must_use]
    pub const fn new(column: Ident, table: TableRef) -> Self {
        Self { column, table }
    }

    /// The local column that will receive the parent's key.
    ///
    /// ```
    /// # use moso_sql::{Ident, TableRef};
    /// # use moso_test::factory::RelationStep;
    /// # let step = RelationStep::new(Ident::from_static("a"), TableRef::from_static("b"));
    /// assert_eq!(step.column().as_str(), "a");
    /// ```
    #[must_use]
    pub const fn column(&self) -> &Ident {
        &self.column
    }

    /// The table the parent goes in.
    ///
    /// ```
    /// # use moso_sql::{Ident, TableRef};
    /// # use moso_test::factory::RelationStep;
    /// # let step = RelationStep::new(Ident::from_static("a"), TableRef::from_static("b"));
    /// assert_eq!(step.table().name().as_str(), "b");
    /// ```
    #[must_use]
    pub const fn table(&self) -> &TableRef {
        &self.table
    }
}

/// A foreign key a factory cannot invent its way out of.
///
/// ```
/// use moso_sql::Ident;
/// use moso_test::factory::Unsatisfiable;
///
/// let problem = Unsatisfiable::new(Ident::from_static("parent_id"), "it points at itself");
/// assert!(problem.reason().contains("itself"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsatisfiable {
    column: Ident,
    reason: String,
}

impl Unsatisfiable {
    /// Records a foreign key that cannot be filled in.
    ///
    /// ```
    /// # use moso_sql::Ident;
    /// # use moso_test::factory::Unsatisfiable;
    /// let problem = Unsatisfiable::new(Ident::from_static("a"), "because");
    /// assert_eq!(problem.column().as_str(), "a");
    /// ```
    #[must_use]
    pub fn new(column: Ident, reason: impl Into<String>) -> Self {
        Self {
            column,
            reason: reason.into(),
        }
    }

    /// The column.
    ///
    /// ```
    /// # use moso_sql::Ident;
    /// # use moso_test::factory::Unsatisfiable;
    /// # let p = Unsatisfiable::new(Ident::from_static("a"), "r");
    /// assert_eq!(p.column().as_str(), "a");
    /// ```
    #[must_use]
    pub const fn column(&self) -> &Ident {
        &self.column
    }

    /// Why the factory gave up, phrased for the message the test will read.
    ///
    /// ```
    /// # use moso_sql::Ident;
    /// # use moso_test::factory::Unsatisfiable;
    /// # let p = Unsatisfiable::new(Ident::from_static("a"), "r");
    /// assert_eq!(p.reason(), "r");
    /// ```
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Everything that has to exist before a row can be inserted.
///
/// ```
/// use moso_test::factory::RelationPlan;
///
/// let plan = RelationPlan::default();
/// assert!(plan.is_empty());
/// assert_eq!(plan.steps().len(), 0);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RelationPlan {
    steps: Vec<RelationStep>,
    unsatisfiable: Vec<Unsatisfiable>,
}

impl RelationPlan {
    /// The parents to create, in the order to create them.
    ///
    /// ```
    /// assert!(moso_test::factory::RelationPlan::default().steps().is_empty());
    /// ```
    #[must_use]
    pub fn steps(&self) -> &[RelationStep] {
        &self.steps
    }

    /// The foreign keys that need the caller's help.
    ///
    /// ```
    /// assert!(moso_test::factory::RelationPlan::default().unsatisfiable().is_empty());
    /// ```
    #[must_use]
    pub fn unsatisfiable(&self) -> &[Unsatisfiable] {
        &self.unsatisfiable
    }

    /// Whether nothing has to happen first.
    ///
    /// ```
    /// assert!(moso_test::factory::RelationPlan::default().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.unsatisfiable.is_empty()
    }
}

/// Works out which parent rows have to exist before one of `descriptor`'s can.
///
/// A foreign key needs a parent when **every** part of it is `NOT NULL`, has no
/// default, and was not supplied by the caller. Everything else is left alone:
/// a nullable key is legal as `NULL`, a defaulted one is the database's problem,
/// and a supplied one is the test being explicit.
///
/// ```
/// use moso_orm::descriptor::{ColumnDescriptor, EntityDescriptor, ForeignKeyDescriptor};
/// use moso_sql::{DataType, Ident, TableRef};
/// use moso_test::factory::plan_relations;
///
/// let descriptor = EntityDescriptor::builder("Post", TableRef::from_static("posts"))
///     .column(
///         ColumnDescriptor::builder(Ident::from_static("author_id"), DataType::BigInt).build(),
///     )
///     .foreign_key(
///         ForeignKeyDescriptor::builder("fk_author", TableRef::from_static("users"))
///             .column(Ident::from_static("author_id"), Ident::from_static("id"))
///             .build(),
///     )
///     .build();
///
/// // Nothing supplied: the author has to be created.
/// let plan = plan_relations(&descriptor, &[]);
/// assert_eq!(plan.steps().len(), 1);
/// assert_eq!(plan.steps()[0].table().name().as_str(), "users");
///
/// // Supplied: nothing to do.
/// let plan = plan_relations(&descriptor, &[Ident::from_static("author_id")]);
/// assert!(plan.is_empty());
/// ```
#[must_use]
pub fn plan_relations(descriptor: &EntityDescriptor, supplied: &[Ident]) -> RelationPlan {
    let mut plan = RelationPlan::default();
    for key in descriptor.foreign_keys() {
        match classify(descriptor, key, supplied) {
            Classification::Skip => {}
            Classification::Create(step) => plan.steps.push(step),
            Classification::Impossible(problem) => plan.unsatisfiable.push(problem),
        }
    }
    plan
}

/// What to do about one foreign key.
enum Classification {
    /// Nothing: nullable, defaulted, or already supplied.
    Skip,
    /// Create a parent.
    Create(RelationStep),
    /// The caller has to.
    Impossible(Unsatisfiable),
}

fn classify(
    descriptor: &EntityDescriptor,
    key: &ForeignKeyDescriptor,
    supplied: &[Ident],
) -> Classification {
    let columns = key.columns();
    if columns.is_empty() {
        return Classification::Skip;
    }
    if columns
        .iter()
        .any(|column| supplied.iter().any(|given| given == column))
    {
        return Classification::Skip;
    }

    let described: Vec<Option<&ColumnDescriptor>> = columns
        .iter()
        .map(|column| descriptor.column(column.as_str()))
        .collect();

    // A key that can legally be NULL needs no parent: the row is valid without
    // one, and inventing an organisation for every user is exactly the surprise
    // that makes people distrust factories.
    if described
        .iter()
        .all(|column| column.is_none_or(ColumnDescriptor::is_nullable))
    {
        return Classification::Skip;
    }
    if described
        .iter()
        .all(|column| column.is_some_and(|column| column.default().is_some()))
    {
        return Classification::Skip;
    }

    if columns.len() > 1 {
        return Classification::Impossible(Unsatisfiable::new(
            columns[0].clone(),
            format!(
                "it is part of a composite key over {}, and a factory cannot invent a matching \
                 tuple",
                columns
                    .iter()
                    .map(|column| format!("`{}`", column.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    if key.target().name() == descriptor.table().name() {
        return Classification::Impossible(Unsatisfiable::new(
            columns[0].clone(),
            format!(
                "it is a required self-reference to `{}`, so creating a parent would need a \
                 parent",
                key.target().name().as_str()
            ),
        ));
    }

    Classification::Create(RelationStep::new(columns[0].clone(), key.target().clone()))
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// Knows how to make a row in one table, so that a child factory can fill a
/// required foreign key.
///
/// ADR-0004 forbids link-time registries, so nothing here happens by magic:
/// registration is a line of code, and the error a missing one produces names
/// it.
///
/// ```
/// use moso_test::db::BoxFuture;
/// use moso_test::factory::{ParentFactory, Result};
/// use moso_sql::{TableRef, Value};
///
/// /// Organisations, for tests that only care about their users.
/// struct Organisations;
///
/// impl ParentFactory for Organisations {
///     fn table(&self) -> TableRef {
///         TableRef::from_static("organisations")
///     }
///
///     fn create_parent<'a>(&'a self, _db: &'a moso_orm::Db) -> BoxFuture<'a, Result<Value>> {
///         Box::pin(async move { Ok(Value::I64(1)) })
///     }
/// }
///
/// assert_eq!(Organisations.table().name().as_str(), "organisations");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot create a parent row",
    label = "not a parent factory",
    note = "a required foreign key needs a row on the other side before the child can be inserted",
    note = "help: implement `ParentFactory for {Self}` with `table` and `create_parent`",
    note = "help: or supply the key instead: `Child::factory().set(\"parent_id\", parent.id)`"
)]
pub trait ParentFactory: Send + Sync + 'static {
    /// Which table this makes rows in.
    ///
    /// ```
    /// # use moso_test::factory::ParentFactory;
    /// fn table_of(factory: &dyn ParentFactory) -> moso_sql::TableRef {
    ///     factory.table()
    /// }
    /// ```
    fn table(&self) -> TableRef;

    /// Creates one and returns its primary key.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```
    /// # use moso_test::db::BoxFuture;
    /// # use moso_test::factory::{ParentFactory, Result};
    /// fn make<'a>(f: &'a dyn ParentFactory, db: &'a moso_orm::Db)
    ///     -> BoxFuture<'a, Result<moso_sql::Value>>
    /// {
    ///     f.create_parent(db)
    /// }
    /// ```
    fn create_parent<'a>(&'a self, db: &'a Db) -> BoxFuture<'a, Result<Value>>;
}

/// The table-to-factory map [`EntityFactory`] consults for required parents.
///
/// ```
/// use moso_test::factory::FactoryRegistry;
///
/// let registry = FactoryRegistry::new();
/// assert!(registry.tables().is_empty());
/// assert!(registry.get("users").is_none());
/// ```
#[derive(Default)]
pub struct FactoryRegistry {
    by_table: Mutex<HashMap<String, Arc<dyn ParentFactory>>>,
}

impl FactoryRegistry {
    /// An empty registry.
    ///
    /// ```
    /// assert!(moso_test::factory::FactoryRegistry::new().tables().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The one every [`EntityFactory`] consults.
    ///
    /// ```
    /// let registry = moso_test::factory::FactoryRegistry::global();
    /// let _ = registry.tables();
    /// ```
    #[must_use]
    pub fn global() -> &'static Self {
        static REGISTRY: OnceLock<FactoryRegistry> = OnceLock::new();
        REGISTRY.get_or_init(Self::default)
    }

    /// Adds one, replacing any factory already registered for its table.
    ///
    /// ```
    /// # use moso_test::db::BoxFuture;
    /// # use moso_test::factory::{FactoryRegistry, ParentFactory, Result};
    /// # use moso_sql::{TableRef, Value};
    /// # struct Orgs;
    /// # impl ParentFactory for Orgs {
    /// #     fn table(&self) -> TableRef { TableRef::from_static("orgs") }
    /// #     fn create_parent<'a>(&'a self, _: &'a moso_orm::Db) -> BoxFuture<'a, Result<Value>> {
    /// #         Box::pin(async move { Ok(Value::I64(1)) })
    /// #     }
    /// # }
    /// let registry = FactoryRegistry::new();
    /// registry.register(Orgs);
    /// assert!(registry.get("orgs").is_some());
    /// ```
    pub fn register(&self, factory: impl ParentFactory) {
        let table = factory.table().name().as_str().to_owned();
        if let Ok(mut map) = self.by_table.lock() {
            map.insert(table, Arc::new(factory));
        }
    }

    /// The factory for `table`, if there is one.
    ///
    /// ```
    /// assert!(moso_test::factory::FactoryRegistry::new().get("nothing").is_none());
    /// ```
    #[must_use]
    pub fn get(&self, table: &str) -> Option<Arc<dyn ParentFactory>> {
        self.by_table
            .lock()
            .ok()
            .and_then(|map| map.get(table).cloned())
    }

    /// Every table that has one, sorted.
    ///
    /// ```
    /// assert!(moso_test::factory::FactoryRegistry::new().tables().is_empty());
    /// ```
    #[must_use]
    pub fn tables(&self) -> Vec<String> {
        let mut tables: Vec<String> = self
            .by_table
            .lock()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        tables.sort();
        tables
    }

    /// Forgets everything. For a test that is *about* the registry.
    ///
    /// ```
    /// let registry = moso_test::factory::FactoryRegistry::new();
    /// registry.clear();
    /// assert!(registry.tables().is_empty());
    /// ```
    pub fn clear(&self) {
        if let Ok(mut map) = self.by_table.lock() {
            map.clear();
        }
    }
}

impl fmt::Debug for FactoryRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FactoryRegistry")
            .field("tables", &self.tables())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// The factory
// ---------------------------------------------------------------------------

/// A closure applied to the `index`-th instance of a batch.
type SequenceStep<E> = Arc<dyn Fn(usize, &mut EntityFactory<E>) + Send + Sync>;

/// Builds rows for `E`, filling in whatever the test did not say.
///
/// ```
/// # use moso_orm::{ColumnDef, Entity, EntityDescriptor, Row};
/// # use moso_orm::row::DecodeError;
/// # use moso_sql::{TableRef, ValueKind};
/// # /// A tag.
/// # pub struct Tag { /// Its id.
/// #     pub id: i64 }
/// # impl Entity for Tag {
/// #     type Pk = i64;
/// #     const TABLE: TableRef = TableRef::from_static("tags");
/// #     const COLUMNS: &'static [ColumnDef] =
/// #         &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// #     const NAME: &'static str = "Tag";
/// #     fn pk(&self) -> i64 { self.id }
/// #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
/// #     fn descriptor() -> &'static EntityDescriptor {
/// #         static D: std::sync::OnceLock<EntityDescriptor> = std::sync::OnceLock::new();
/// #         D.get_or_init(|| EntityDescriptor::builder("Tag", Tag::TABLE).build())
/// #     }
/// # }
/// use moso_test::factory::EntityFactory;
///
/// let factory = EntityFactory::<Tag>::new().set("name", "rust").count(3);
/// assert_eq!(factory.instances(), 3);
/// assert_eq!(factory.overrides().len(), 1);
/// ```
pub struct EntityFactory<E> {
    faker: Faker,
    seed: Seed,
    overrides: Vec<(Ident, Expr)>,
    count: usize,
    sequence: Vec<SequenceStep<E>>,
    registry: Option<&'static FactoryRegistry>,
    marker: PhantomData<fn() -> E>,
}

impl<E> Clone for EntityFactory<E> {
    fn clone(&self) -> Self {
        Self {
            faker: self.faker.clone(),
            seed: self.seed,
            overrides: self.overrides.clone(),
            count: self.count,
            sequence: self.sequence.clone(),
            registry: self.registry,
            marker: PhantomData,
        }
    }
}

impl<E> fmt::Debug for EntityFactory<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntityFactory")
            .field("seed", &self.seed)
            .field("count", &self.count)
            .field(
                "overrides",
                &self
                    .overrides
                    .iter()
                    .map(|(column, _)| column.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl<E: Entity> Default for EntityFactory<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Entity> EntityFactory<E> {
    /// A factory seeded from the entity's name.
    ///
    /// Prefer [`EntityFactory::seeded`] with the test's own name when two tests
    /// in one file must not share fixture values.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>() -> EntityFactory<E> {
    /// EntityFactory::<E>::new()
    /// # }
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::seeded(Seed::of(E::NAME))
    }

    /// A factory with an explicit [`Seed`].
    ///
    /// ```
    /// # use moso_test::factory::{EntityFactory, Seed};
    /// # fn example<E: moso_orm::Entity>() -> EntityFactory<E> {
    /// EntityFactory::<E>::seeded(Seed::of("posts::list_is_paginated"))
    /// # }
    /// ```
    #[must_use]
    pub fn seeded(seed: Seed) -> Self {
        Self {
            faker: Faker::new(seed),
            seed,
            overrides: Vec::new(),
            count: 1,
            sequence: Vec::new(),
            registry: None,
            marker: PhantomData,
        }
    }

    /// The generator this factory fills unspecified columns from.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: &EntityFactory<E>) -> moso_test::factory::Seed {
    /// f.faker().seed()
    /// # }
    /// ```
    #[must_use]
    pub const fn faker(&self) -> &Faker {
        &self.faker
    }

    /// The same, mutably, for a typed setter that wants a fake value.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: &mut EntityFactory<E>) -> String {
    /// f.faker_mut().email()
    /// # }
    /// ```
    pub const fn faker_mut(&mut self) -> &mut Faker {
        &mut self.faker
    }

    /// The seed everything here came from. Print it in a failure message.
    ///
    /// ```
    /// # use moso_test::factory::{EntityFactory, Seed};
    /// # fn example<E: moso_orm::Entity>() -> Seed {
    /// EntityFactory::<E>::seeded(Seed::new(3)).seed()
    /// # }
    /// ```
    #[must_use]
    pub const fn seed(&self) -> Seed {
        self.seed
    }

    /// Pins one column.
    ///
    /// This is what `#[derive(Factory)]`'s typed setters call: `is_admin(true)`
    /// becomes `set("is_admin", true)`.
    ///
    /// # Panics
    ///
    /// If `column` is not a valid SQL identifier, which a generated setter's
    /// name always is.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: EntityFactory<E>) -> EntityFactory<E> {
    /// f.set("is_admin", true).set("name", "Ada")
    /// # }
    /// ```
    #[must_use]
    pub fn set(self, column: &str, value: impl moso_sql::Bindable) -> Self {
        self.set_expr(column, Expr::value(value))
    }

    /// Pins one column to an arbitrary expression — `now()`, a cast, a subquery.
    ///
    /// # Panics
    ///
    /// If `column` is not a valid SQL identifier.
    ///
    /// ```
    /// # use moso_sql::Expr;
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: EntityFactory<E>) -> EntityFactory<E> {
    /// f.set_expr("created_at", Expr::value(0_i64))
    /// # }
    /// ```
    #[must_use]
    pub fn set_expr(mut self, column: &str, value: Expr) -> Self {
        let ident = Ident::new(column).unwrap_or_else(|error| {
            panic!(
                "moso-test: `{}` is not a column name that can be inserted into: {error}\n  help: \
                 a factory setter takes the column, not the field: `set(\"is_admin\", true)`",
                column
            )
        });
        self.overrides.retain(|(existing, _)| *existing != ident);
        self.overrides.push((ident, value));
        self
    }

    /// Pins one column to `NULL`.
    ///
    /// # Panics
    ///
    /// If `column` is not a valid SQL identifier.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: EntityFactory<E>) -> EntityFactory<E> {
    /// f.set_null("deleted_at")
    /// # }
    /// ```
    #[must_use]
    pub fn set_null(self, column: &str) -> Self {
        self.set_expr(
            column,
            Expr::value(Value::Null(moso_sql::ValueKind::Unknown)),
        )
    }

    /// How many rows to make.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: EntityFactory<E>) -> usize {
    /// f.count(20).instances()
    /// # }
    /// ```
    #[must_use]
    pub const fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    /// How many rows this factory will make.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: EntityFactory<E>) -> usize {
    /// f.instances()
    /// # }
    /// ```
    #[must_use]
    pub const fn instances(&self) -> usize {
        self.count
    }

    /// The columns the test has pinned, in the order it pinned them.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: &EntityFactory<E>) -> usize {
    /// f.overrides().len()
    /// # }
    /// ```
    #[must_use]
    pub fn overrides(&self) -> &[(Ident, Expr)] {
        &self.overrides
    }

    /// Varies each row of a batch: `sequence(|i, p| p.title(format!("Post {i}")))`.
    ///
    /// The closure runs on a *clone* per row, so nothing it does leaks into the
    /// next one.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: EntityFactory<E>) -> EntityFactory<E> {
    /// f.count(3).sequence(|index, row| {
    ///     *row = row.clone().set("title", format!("Post {index}"));
    /// })
    /// # }
    /// ```
    #[must_use]
    pub fn sequence(mut self, step: impl Fn(usize, &mut Self) + Send + Sync + 'static) -> Self {
        self.sequence.push(Arc::new(step));
        self
    }

    /// Look required parents up here instead of in
    /// [`FactoryRegistry::global`].
    ///
    /// ```
    /// # use moso_test::factory::{EntityFactory, FactoryRegistry};
    /// # fn example<E: moso_orm::Entity>(f: EntityFactory<E>) -> EntityFactory<E> {
    /// f.registry(FactoryRegistry::global())
    /// # }
    /// ```
    #[must_use]
    pub const fn registry(mut self, registry: &'static FactoryRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// The `index`-th instance: this factory, with its sequence steps applied
    /// and its own derived seed.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: &EntityFactory<E>) -> EntityFactory<E> {
    /// f.instance(0)
    /// # }
    /// ```
    #[must_use]
    pub fn instance(&self, index: usize) -> Self {
        let mut instance = self.clone();
        instance.count = 1;
        instance.seed = self.seed.derive(index);
        instance.faker = Faker::new(instance.seed);
        for step in &self.sequence {
            step(index, &mut instance);
        }
        instance
    }

    /// The columns and values one row would be inserted with.
    ///
    /// The pinned columns first, then every writable, non-nullable, undefaulted
    /// column the entity declares, filled from the [`Faker`].
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: &EntityFactory<E>) -> usize {
    /// f.row().len()
    /// # }
    /// ```
    #[must_use]
    pub fn row(&self) -> Vec<(Ident, Expr)> {
        let mut instance = self.clone();
        instance.materialise()
    }

    /// The same as [`EntityFactory::row`], consuming the factory's faker so that
    /// two calls do not produce the same "unique" value.
    fn materialise(&mut self) -> Vec<(Ident, Expr)> {
        let descriptor = E::descriptor();
        let mut row = self.overrides.clone();
        for column in descriptor.insertable() {
            if row.iter().any(|(existing, _)| existing == column.name()) {
                continue;
            }
            if !needs_a_value(column) {
                continue;
            }
            let value = self.fake_for(column);
            row.push((column.name().clone(), value));
        }
        row
    }

    /// A plausible value for one column, from its name first and its type second.
    ///
    /// The name matters more than the type: `email TEXT` wants an address, and a
    /// fixture that puts `saffron thistle` in it fails the application's own
    /// validation for a reason that has nothing to do with the test.
    fn fake_for(&mut self, column: &ColumnDescriptor) -> Expr {
        let name = column.name().as_str();
        if let Some(expr) = self.fake_by_name(name, column) {
            return expr;
        }
        self.fake_by_type(column.data_type(), column.max_length())
    }

    fn fake_by_name(&mut self, name: &str, column: &ColumnDescriptor) -> Option<Expr> {
        if !is_texty(column.data_type()) {
            return None;
        }
        let lower = name.to_ascii_lowercase();
        let value = if lower.contains("password") || lower.contains("passwd") {
            PasswordHash::test().into_string()
        } else if lower.contains("email") {
            self.faker.email()
        } else if lower.contains("username") || lower == "handle" || lower == "login" {
            self.faker.username()
        } else if lower.contains("slug") {
            self.faker.slug()
        } else if lower.contains("url") || lower.contains("uri") || lower.contains("website") {
            self.faker.url()
        } else if lower.contains("title") || lower.contains("subject") {
            self.faker.title()
        } else if lower.contains("body")
            || lower.contains("content")
            || lower.contains("description")
            || lower.contains("summary")
        {
            self.faker.paragraph()
        } else if lower == "name" || lower.ends_with("_name") || lower.starts_with("name_") {
            self.faker.name()
        } else {
            return None;
        };
        Some(Expr::value(truncate(value, column.max_length())))
    }

    fn fake_by_type(&mut self, data_type: &DataType, max_length: Option<u32>) -> Expr {
        match data_type {
            DataType::Boolean => Expr::value(self.faker.bool()),
            DataType::SmallInt | DataType::SmallSerial => {
                Expr::value(i16::try_from(self.faker.i64_in(1, 1000)).unwrap_or(1))
            }
            DataType::Integer | DataType::Serial => {
                Expr::value(i32::try_from(self.faker.i64_in(1, 100_000)).unwrap_or(1))
            }
            DataType::BigInt | DataType::BigSerial => Expr::value(self.faker.i64_in(1, 1_000_000)),
            DataType::Real => {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "a fixture float does not need f64 precision"
                )]
                let value = self.faker.f64_in(0.0, 1000.0) as f32;
                Expr::value(value)
            }
            DataType::DoublePrecision => Expr::value(self.faker.f64_in(0.0, 1000.0)),
            DataType::Numeric { .. } => Expr::value(self.faker.decimal(0, 10_000)),
            DataType::Text | DataType::VarChar(_) | DataType::Char(_) => {
                Expr::value(truncate(self.faker.words(3), max_length))
            }
            DataType::Bytea => Expr::value(self.faker.bytes(16)),
            DataType::Uuid => Expr::value(self.faker.uuid()),
            DataType::Json | DataType::JsonB => {
                Expr::value(Value::json(&self.faker.json()).unwrap_or(Value::Text(String::new())))
            }
            DataType::Timestamp { .. } => Expr::value(self.faker.timestamp()),
            // Everything else — networks, ranges, vectors, enums, user types —
            // has no sensible invented value, and guessing one produces a
            // confusing driver error instead of a clear one. The column is left
            // out so the database says what it needs.
            _ => Expr::value(Value::Null(moso_sql::ValueKind::Unknown)),
        }
    }

    /// What has to exist before this row can be inserted.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: &EntityFactory<E>) -> bool {
    /// f.relation_plan().is_empty()
    /// # }
    /// ```
    #[must_use]
    pub fn relation_plan(&self) -> RelationPlan {
        let supplied: Vec<Ident> = self
            .overrides
            .iter()
            .map(|(column, _)| column.clone())
            .collect();
        plan_relations(E::descriptor(), &supplied)
    }

    /// Builds the statement one row would be inserted with.
    ///
    /// Useful on its own: a test that wants to see the SQL, or to insert through
    /// its own connection, can stop here.
    ///
    /// ```
    /// # use moso_test::factory::EntityFactory;
    /// # fn example<E: moso_orm::Entity>(f: &EntityFactory<E>) -> moso_sql::Statement {
    /// f.insert_statement(&f.row())
    /// # }
    /// ```
    #[must_use]
    pub fn insert_statement(&self, row: &[(Ident, Expr)]) -> moso_sql::Statement {
        Insert::into_table(E::TABLE)
            .columns(row.iter().map(|(column, _)| column.clone()))
            .values(row.iter().map(|(_, value)| value.clone()))
            .returning(Returning::All)
            .into_statement()
    }

    /// Creates one row and returns it.
    ///
    /// Required parents are created first, through
    /// [`FactoryRegistry`].
    ///
    /// # Errors
    ///
    /// [`Error::NoParentFactory`] when a required foreign key has nothing to
    /// point at, [`Error::Unsatisfiable`] when it cannot be invented, and
    /// [`Error::Database`] when the insert fails.
    ///
    /// ```no_run
    /// # use moso_test::factory::EntityFactory;
    /// # async fn example<E: moso_orm::Entity>(f: EntityFactory<E>, db: &moso_orm::Db)
    /// #     -> moso_test::factory::Result<E>
    /// # {
    /// f.create(db).await
    /// # }
    /// ```
    pub async fn create(self, db: &Db) -> Result<E> {
        let mut instance = self.instance(0);
        instance.create_one(db).await
    }

    /// Creates [`EntityFactory::count`] rows and returns them.
    ///
    /// # Errors
    ///
    /// As [`EntityFactory::create`]. Stops at the first failure; rows already
    /// created stay, because a test that fails half-way is easier to debug with
    /// the evidence still there.
    ///
    /// ```no_run
    /// # use moso_test::factory::EntityFactory;
    /// # async fn example<E: moso_orm::Entity>(f: EntityFactory<E>, db: &moso_orm::Db)
    /// #     -> moso_test::factory::Result<Vec<E>>
    /// # {
    /// f.count(20).create_many(db).await
    /// # }
    /// ```
    pub async fn create_many(self, db: &Db) -> Result<Vec<E>> {
        let mut created = Vec::with_capacity(self.count);
        for index in 0..self.count {
            let mut instance = self.instance(index);
            created.push(instance.create_one(db).await?);
        }
        Ok(created)
    }

    /// Inserts exactly one, resolving parents first.
    async fn create_one(&mut self, db: &Db) -> Result<E> {
        let registry = self.registry.unwrap_or_else(FactoryRegistry::global);
        let plan = self.relation_plan();

        if let Some(problem) = plan.unsatisfiable().first() {
            return Err(Error::Unsatisfiable {
                column: problem.column().as_str().to_owned(),
                reason: problem.reason().to_owned(),
                entity: E::NAME,
            });
        }

        for step in plan.steps() {
            let table = step.table().name().as_str();
            let parent = registry.get(table).ok_or_else(|| Error::NoParentFactory {
                table: table.to_owned(),
                column: step.column().as_str().to_owned(),
                entity: E::NAME,
            })?;
            let key = parent.create_parent(db).await?;
            self.overrides
                .push((step.column().clone(), Expr::value(key)));
        }

        let row = self.materialise();
        let statement = self.insert_statement(&row);
        let inserted = db
            .handle()
            .fetch_optional(&statement)
            .await
            .map_err(|error| Error::Database {
                entity: E::NAME,
                message: error.to_string(),
            })?;
        let inserted = inserted.ok_or(Error::NoRowReturned { entity: E::NAME })?;
        E::from_row(&inserted).map_err(|error| Error::Database {
            entity: E::NAME,
            message: error.to_string(),
        })
    }
}

/// Whether a column has to be given a value, or whether the database will.
fn needs_a_value(column: &ColumnDescriptor) -> bool {
    if column.role().is_framework_managed() {
        // `created_at`, `updated_at`, `version`: the ORM writes these, and a
        // fixture that fights it produces a row no application would have made.
        return false;
    }
    if column.default().is_some() || column.generated().is_some() {
        return false;
    }
    if column.is_primary_key() && is_serial(column.data_type()) {
        return false;
    }
    if column.is_nullable() {
        return false;
    }
    true
}

const fn is_serial(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::SmallSerial | DataType::Serial | DataType::BigSerial
    )
}

const fn is_texty(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Text | DataType::VarChar(_) | DataType::Char(_)
    )
}

/// Keeps a generated string inside a `varchar(n)`.
fn truncate(mut value: String, max_length: Option<u32>) -> String {
    if let Some(max) = max_length.and_then(|max| usize::try_from(max).ok())
        && value.chars().count() > max
    {
        value = value.chars().take(max).collect();
    }
    value
}

// ---------------------------------------------------------------------------
// The trait the derive implements
// ---------------------------------------------------------------------------

/// An entity with a factory.
///
/// `#[derive(Factory)]` in `moso-orm-macros` generates this impl from the
/// `#[factory(..)]` attributes; writing it by hand is one line, and
/// [`EntityFactory`] works without it.
///
/// ```
/// # use moso_orm::{ColumnDef, Entity, EntityDescriptor, Row};
/// # use moso_orm::row::DecodeError;
/// # use moso_sql::{TableRef, ValueKind};
/// # /// A tag.
/// # pub struct Tag { /// Its id.
/// #     pub id: i64 }
/// # impl Entity for Tag {
/// #     type Pk = i64;
/// #     const TABLE: TableRef = TableRef::from_static("tags");
/// #     const COLUMNS: &'static [ColumnDef] =
/// #         &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// #     const NAME: &'static str = "Tag";
/// #     fn pk(&self) -> i64 { self.id }
/// #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
/// #     fn descriptor() -> &'static EntityDescriptor {
/// #         static D: std::sync::OnceLock<EntityDescriptor> = std::sync::OnceLock::new();
/// #         D.get_or_init(|| EntityDescriptor::builder("Tag", Tag::TABLE).build())
/// #     }
/// # }
/// use moso_test::factory::{EntityFactory, Factory};
///
/// impl Factory for Tag {
///     fn defaults(factory: EntityFactory<Self>) -> EntityFactory<Self> {
///         factory.set("name", "rust")
///     }
/// }
///
/// assert_eq!(Tag::factory().overrides().len(), 1);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no factory",
    label = "no factory",
    note = "a factory invents the columns a test does not care about",
    note = "help: derive one: `#[derive(Entity, Factory)]`",
    note = "help: or write the empty impl: `impl moso_test::factory::Factory for {Self} {{}}`",
    note = "help: or skip the trait entirely: `EntityFactory::<{Self}>::new()`"
)]
pub trait Factory: Entity + Sized {
    /// The columns every instance starts with, before the faker fills the rest.
    ///
    /// This is where `#[factory(email = "...")]` ends up.
    ///
    /// ```
    /// # use moso_test::factory::{EntityFactory, Factory};
    /// fn defaults_of<E: Factory>(f: EntityFactory<E>) -> EntityFactory<E> {
    ///     E::defaults(f)
    /// }
    /// ```
    #[must_use]
    fn defaults(factory: EntityFactory<Self>) -> EntityFactory<Self> {
        factory
    }

    /// A factory seeded from the entity's name.
    ///
    /// ```
    /// # use moso_test::factory::{EntityFactory, Factory};
    /// fn make<E: Factory>() -> EntityFactory<E> {
    ///     E::factory()
    /// }
    /// ```
    #[must_use]
    fn factory() -> EntityFactory<Self> {
        Self::defaults(EntityFactory::new())
    }

    /// A factory with an explicit seed — the test's own name, usually.
    ///
    /// ```
    /// # use moso_test::factory::{EntityFactory, Factory, Seed};
    /// fn make<E: Factory>() -> EntityFactory<E> {
    ///     E::factory_seeded(Seed::of("posts::list"))
    /// }
    /// ```
    #[must_use]
    fn factory_seeded(seed: Seed) -> EntityFactory<Self> {
        Self::defaults(EntityFactory::seeded(seed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moso_orm::descriptor::ColumnDefault;
    use moso_orm::row::DecodeError;
    use moso_orm::{ColumnDef, ColumnRole, Row};
    use moso_sql::ValueKind;

    // -- seeds ------------------------------------------------------------

    #[test]
    fn a_seed_is_a_pure_function_of_the_name() {
        assert_eq!(Seed::of("users::create"), Seed::of("users::create"));
        assert_ne!(Seed::of("users::create"), Seed::of("users::update"));
        assert_ne!(Seed::of(""), Seed::of("a"));
    }

    #[test]
    fn a_derived_seed_depends_on_the_index_and_not_on_the_batch_size() {
        let seed = Seed::of("posts::list");
        assert_eq!(seed.derive(4), seed.derive(4));
        assert_ne!(seed.derive(4), seed.derive(5));
        assert_ne!(seed.derive(0), seed);
    }

    #[test]
    fn a_seed_prints_as_something_a_person_can_paste_back() {
        let rendered = Seed::new(0xdead_beef).to_string();
        assert!(rendered.starts_with("0x"));
        assert_eq!(rendered.len(), 18);
    }

    // -- the faker --------------------------------------------------------

    #[test]
    fn the_same_seed_produces_the_same_data_every_time() {
        let mut a = Faker::new(Seed::new(7));
        let mut b = Faker::new(Seed::new(7));
        for _ in 0..50 {
            assert_eq!(a.email(), b.email());
            assert_eq!(a.name(), b.name());
            assert_eq!(a.i64_in(0, 1_000_000), b.i64_in(0, 1_000_000));
            assert_eq!(a.uuid(), b.uuid());
            assert_eq!(a.timestamp().unix_seconds(), b.timestamp().unix_seconds());
        }
    }

    #[test]
    fn different_seeds_produce_different_data() {
        let mut a = Faker::new(Seed::new(1));
        let mut b = Faker::new(Seed::new(2));
        let left: Vec<String> = (0..10).map(|_| a.email()).collect();
        let right: Vec<String> = (0..10).map(|_| b.email()).collect();
        assert_ne!(left, right);
    }

    #[test]
    fn resetting_rewinds_exactly() {
        let mut faker = Faker::new(Seed::new(3));
        let first: Vec<String> = (0..5).map(|_| faker.email()).collect();
        faker.reset();
        let again: Vec<String> = (0..5).map(|_| faker.email()).collect();
        assert_eq!(first, again);
    }

    #[test]
    fn generated_addresses_can_never_reach_a_real_person() {
        let mut faker = Faker::new(Seed::new(1));
        for _ in 0..200 {
            let email = faker.email();
            let domain = email.split('@').nth(1).expect("an address has a domain");
            assert!(
                DOMAINS.contains(&domain),
                "{domain} is not reserved by RFC 2606"
            );
        }
    }

    #[test]
    fn unique_values_really_are_unique_within_one_generator() {
        let mut faker = Faker::new(Seed::new(1));
        let emails: std::collections::HashSet<String> = (0..500).map(|_| faker.email()).collect();
        assert_eq!(emails.len(), 500, "a unique index must not be violated");

        let slugs: std::collections::HashSet<String> = (0..500).map(|_| faker.slug()).collect();
        assert_eq!(slugs.len(), 500);
    }

    #[test]
    fn ranges_are_inclusive_and_never_escaped() {
        let mut faker = Faker::new(Seed::new(1));
        for _ in 0..2000 {
            let value = faker.i64_in(-5, 5);
            assert!((-5..=5).contains(&value), "{value}");
            let float = faker.f64_in(1.0, 2.0);
            assert!((1.0..=2.0).contains(&float), "{float}");
        }
        assert_eq!(faker.i64_in(3, 3), 3, "an empty range is its own bound");
        assert_eq!(faker.i64_in(9, 1), 9, "an inverted range does not panic");
    }

    #[test]
    fn a_generated_uuid_has_the_version_and_variant_bits_a_driver_checks() {
        let mut faker = Faker::new(Seed::new(1));
        for _ in 0..100 {
            let bytes = faker.uuid().into_bytes();
            assert_eq!(bytes[6] & 0xf0, 0x40, "version 4");
            assert_eq!(bytes[8] & 0xc0, 0x80, "RFC 4122 variant");
        }
    }

    #[test]
    fn text_helpers_produce_something_a_human_would_accept() {
        let mut faker = Faker::new(Seed::new(1));
        assert!(faker.name().contains(' '));
        assert!(faker.sentence().ends_with('.'));
        assert!(faker.paragraph().matches('.').count() >= 3);
        assert!(faker.url().starts_with("https://example."));
        assert!(faker.json().starts_with('{'));
        assert_eq!(faker.bytes(32).len(), 32);
    }

    #[test]
    #[should_panic(expected = "at least one choice")]
    fn one_of_nothing_says_what_went_wrong() {
        let empty: [u8; 0] = [];
        let _ = Faker::new(Seed::new(1)).one_of(&empty);
    }

    // -- passwords --------------------------------------------------------

    #[test]
    fn the_test_hash_verifies_its_own_password_and_nothing_else() {
        let hash = PasswordHash::test();
        assert!(hash.verify(PasswordHash::DEFAULT_PASSWORD));
        assert!(!hash.verify("correct horse battery stapl"));
        assert!(!hash.verify(""));
    }

    #[test]
    fn the_test_hash_announces_itself_so_production_can_refuse_it() {
        assert!(PasswordHash::is_test_hash(PasswordHash::test().as_str()));
        assert!(PasswordHash::is_test_hash(PasswordHash::of("x").as_str()));
        assert!(!PasswordHash::is_test_hash("$argon2id$v=19$m=19456$abc"));
        assert!(!PasswordHash::is_test_hash(""));
    }

    #[test]
    fn different_passwords_hash_differently_and_the_same_one_does_not() {
        assert_ne!(PasswordHash::of("a"), PasswordHash::of("b"));
        assert_eq!(PasswordHash::of("a"), PasswordHash::of("a"));
    }

    #[test]
    fn the_test_hash_is_fast_enough_to_be_the_point() {
        let started = std::time::Instant::now();
        for index in 0..10_000 {
            let _ = PasswordHash::of(&format!("password {index}"));
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "ten thousand fixtures must not cost a second; argon2 would take fifteen minutes"
        );
    }

    // -- relation planning ------------------------------------------------

    fn column(name: &'static str, data_type: DataType) -> ColumnDescriptor {
        ColumnDescriptor::builder(Ident::from_static(name), data_type).build()
    }

    fn nullable(name: &'static str, data_type: DataType) -> ColumnDescriptor {
        ColumnDescriptor::builder(Ident::from_static(name), data_type)
            .nullable()
            .build()
    }

    fn posts_with(
        columns: Vec<ColumnDescriptor>,
        keys: Vec<ForeignKeyDescriptor>,
    ) -> EntityDescriptor {
        let mut builder = EntityDescriptor::builder("Post", TableRef::from_static("posts"));
        for column in columns {
            builder = builder.column(column);
        }
        for key in keys {
            builder = builder.foreign_key(key);
        }
        builder.build()
    }

    fn key(name: &'static str, column: &'static str, table: &'static str) -> ForeignKeyDescriptor {
        ForeignKeyDescriptor::builder(name, TableRef::from_static(table))
            .column(Ident::from_static(column), Ident::from_static("id"))
            .build()
    }

    #[test]
    fn a_required_parent_is_planned() {
        let descriptor = posts_with(
            vec![column("author_id", DataType::BigInt)],
            vec![key("fk_author", "author_id", "users")],
        );
        let plan = plan_relations(&descriptor, &[]);
        assert_eq!(plan.steps().len(), 1);
        assert_eq!(plan.steps()[0].column().as_str(), "author_id");
        assert_eq!(plan.steps()[0].table().name().as_str(), "users");
        assert!(plan.unsatisfiable().is_empty());
    }

    #[test]
    fn a_supplied_parent_is_not_planned() {
        let descriptor = posts_with(
            vec![column("author_id", DataType::BigInt)],
            vec![key("fk_author", "author_id", "users")],
        );
        let plan = plan_relations(&descriptor, &[Ident::from_static("author_id")]);
        assert!(plan.is_empty(), "the test said which author it wanted");
    }

    #[test]
    fn a_nullable_parent_is_not_invented() {
        let descriptor = posts_with(
            vec![nullable("editor_id", DataType::BigInt)],
            vec![key("fk_editor", "editor_id", "users")],
        );
        assert!(
            plan_relations(&descriptor, &[]).is_empty(),
            "NULL is a legal editor, and inventing one surprises the test"
        );
    }

    #[test]
    fn a_defaulted_parent_is_left_to_the_database() {
        let descriptor = posts_with(
            vec![
                ColumnDescriptor::builder(Ident::from_static("org_id"), DataType::BigInt)
                    .default(ColumnDefault::value(Value::I64(1)))
                    .build(),
            ],
            vec![key("fk_org", "org_id", "organisations")],
        );
        assert!(plan_relations(&descriptor, &[]).is_empty());
    }

    #[test]
    fn a_composite_key_is_reported_rather_than_guessed() {
        let descriptor = posts_with(
            vec![
                column("tenant_id", DataType::BigInt),
                column("author_id", DataType::BigInt),
            ],
            vec![
                ForeignKeyDescriptor::builder("fk_author", TableRef::from_static("users"))
                    .column(
                        Ident::from_static("tenant_id"),
                        Ident::from_static("tenant_id"),
                    )
                    .column(Ident::from_static("author_id"), Ident::from_static("id"))
                    .build(),
            ],
        );
        let plan = plan_relations(&descriptor, &[]);
        assert!(plan.steps().is_empty());
        assert_eq!(plan.unsatisfiable().len(), 1);
        assert!(plan.unsatisfiable()[0].reason().contains("composite"));
        assert!(plan.unsatisfiable()[0].reason().contains("tenant_id"));
    }

    #[test]
    fn a_required_self_reference_is_reported_rather_than_looped_on() {
        let descriptor = posts_with(
            vec![column("parent_id", DataType::BigInt)],
            vec![key("fk_parent", "parent_id", "posts")],
        );
        let plan = plan_relations(&descriptor, &[]);
        assert!(plan.steps().is_empty());
        assert_eq!(plan.unsatisfiable().len(), 1);
        assert!(
            plan.unsatisfiable()[0].reason().contains("self-reference"),
            "{}",
            plan.unsatisfiable()[0].reason()
        );
    }

    #[test]
    fn several_required_parents_are_all_planned_in_declaration_order() {
        let descriptor = posts_with(
            vec![
                column("author_id", DataType::BigInt),
                column("org_id", DataType::BigInt),
            ],
            vec![
                key("fk_author", "author_id", "users"),
                key("fk_org", "org_id", "organisations"),
            ],
        );
        let plan = plan_relations(&descriptor, &[]);
        assert_eq!(plan.steps().len(), 2);
        assert_eq!(plan.steps()[0].table().name().as_str(), "users");
        assert_eq!(plan.steps()[1].table().name().as_str(), "organisations");
    }

    // -- the registry -----------------------------------------------------

    struct Organisations;

    impl ParentFactory for Organisations {
        fn table(&self) -> TableRef {
            TableRef::from_static("organisations")
        }

        fn create_parent<'a>(&'a self, _db: &'a Db) -> BoxFuture<'a, Result<Value>> {
            Box::pin(async move { Ok(Value::I64(42)) })
        }
    }

    #[test]
    fn a_registry_finds_a_factory_by_its_table() {
        let registry = FactoryRegistry::new();
        assert!(registry.get("organisations").is_none());
        registry.register(Organisations);
        assert!(registry.get("organisations").is_some());
        assert_eq!(registry.tables(), ["organisations"]);
        registry.clear();
        assert!(registry.tables().is_empty());
    }

    #[test]
    fn registering_twice_replaces_rather_than_duplicates() {
        let registry = FactoryRegistry::new();
        registry.register(Organisations);
        registry.register(Organisations);
        assert_eq!(registry.tables().len(), 1);
    }

    #[test]
    fn a_missing_parent_factory_names_the_table_and_both_ways_out() {
        let error = Error::NoParentFactory {
            table: "organisations".to_owned(),
            column: "org_id".to_owned(),
            entity: "User",
        };
        let rendered = error.to_string();
        assert!(rendered.contains("`User`"));
        assert!(rendered.contains("organisations"));
        assert!(rendered.contains("org_id"));
        assert!(rendered.contains("register"));
        assert!(rendered.contains("set(\"org_id\""));
    }

    // -- the factory ------------------------------------------------------

    /// An account, as wide as the value-generation rules need it to be.
    struct Account {
        id: i64,
    }

    impl Entity for Account {
        type Pk = i64;
        const TABLE: TableRef = TableRef::from_static("accounts");
        const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
        const NAME: &'static str = "Account";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| {
                EntityDescriptor::builder("Account", Account::TABLE)
                    .column(
                        ColumnDescriptor::builder(Ident::from_static("id"), DataType::BigSerial)
                            .primary_key()
                            .build(),
                    )
                    .column(column("email", DataType::Text))
                    .column(column("display_name", DataType::Text))
                    .column(column("password_hash", DataType::Text))
                    .column(column("slug", DataType::Text))
                    .column(column("website_url", DataType::Text))
                    .column(column("is_admin", DataType::Boolean))
                    .column(column("login_count", DataType::Integer))
                    .column(nullable("bio", DataType::Text))
                    .column(
                        ColumnDescriptor::builder(
                            Ident::from_static("created_at"),
                            DataType::Timestamp {
                                with_time_zone: true,
                            },
                        )
                        .role(ColumnRole::CreatedAt)
                        .build(),
                    )
                    .column(
                        ColumnDescriptor::builder(Ident::from_static("status"), DataType::Text)
                            .default(ColumnDefault::sql("'active'"))
                            .build(),
                    )
                    .build()
            })
        }
    }

    fn value_of(row: &[(Ident, Expr)], column: &str) -> Option<Value> {
        row.iter()
            .find(|(name, _)| name.as_str() == column)
            .and_then(|(_, expr)| match expr {
                Expr::Value(value) => Some(value.clone()),
                _ => None,
            })
    }

    fn text_of(row: &[(Ident, Expr)], column: &str) -> Option<String> {
        match value_of(row, column) {
            Some(Value::Text(text)) => Some(text),
            _ => None,
        }
    }

    #[test]
    fn a_factory_fills_every_column_the_database_will_not() {
        let row = EntityFactory::<Account>::new().row();
        let names: Vec<&str> = row.iter().map(|(name, _)| name.as_str()).collect();

        assert!(names.contains(&"email"));
        assert!(names.contains(&"display_name"));
        assert!(names.contains(&"is_admin"));
        assert!(names.contains(&"login_count"));

        assert!(
            !names.contains(&"id"),
            "a serial primary key is the database's"
        );
        assert!(!names.contains(&"created_at"), "the ORM writes this one");
        assert!(!names.contains(&"status"), "it has a default");
        assert!(
            !names.contains(&"bio"),
            "a nullable column needs no invention"
        );
    }

    #[test]
    fn a_column_is_filled_from_its_name_before_its_type() {
        let row = EntityFactory::<Account>::new().row();
        assert!(text_of(&row, "email").expect("email").contains('@'));
        assert!(
            text_of(&row, "display_name")
                .expect("display_name")
                .contains(' '),
            "a `_name` column gets a name, not three random words"
        );
        assert!(
            PasswordHash::is_test_hash(&text_of(&row, "password_hash").expect("password_hash")),
            "a fixture must never wait for argon2"
        );
        assert!(
            text_of(&row, "website_url")
                .expect("website_url")
                .starts_with("https://")
        );
        let slug = text_of(&row, "slug").expect("slug");
        assert!(
            slug.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
    }

    #[test]
    fn a_pinned_column_wins_over_the_faker() {
        let row = EntityFactory::<Account>::new()
            .set("email", "ada@example.com")
            .row();
        assert_eq!(text_of(&row, "email").as_deref(), Some("ada@example.com"));
    }

    #[test]
    fn pinning_the_same_column_twice_keeps_the_last_one() {
        let factory = EntityFactory::<Account>::new()
            .set("email", "first@example.com")
            .set("email", "second@example.com");
        assert_eq!(factory.overrides().len(), 1);
        assert_eq!(
            text_of(&factory.row(), "email").as_deref(),
            Some("second@example.com")
        );
    }

    #[test]
    fn a_factory_is_deterministic_and_two_of_them_agree() {
        let a = EntityFactory::<Account>::seeded(Seed::of("t")).row();
        let b = EntityFactory::<Account>::seeded(Seed::of("t")).row();
        assert_eq!(text_of(&a, "email"), text_of(&b, "email"));
        assert_eq!(text_of(&a, "display_name"), text_of(&b, "display_name"));
    }

    #[test]
    fn every_row_of_a_batch_is_different_but_reproducible() {
        let factory = EntityFactory::<Account>::seeded(Seed::of("batch")).count(50);
        let emails: Vec<Option<String>> = (0..50)
            .map(|index| text_of(&factory.instance(index).row(), "email"))
            .collect();
        let unique: std::collections::HashSet<&Option<String>> = emails.iter().collect();
        assert_eq!(unique.len(), 50, "a unique index must survive a batch");

        let again: Vec<Option<String>> = (0..50)
            .map(|index| text_of(&factory.instance(index).row(), "email"))
            .collect();
        assert_eq!(emails, again, "the same batch twice is the same data twice");
    }

    #[test]
    fn a_sequence_varies_each_row_without_leaking_into_the_next() {
        let factory = EntityFactory::<Account>::new()
            .count(3)
            .sequence(|index, row| {
                *row = row.clone().set("display_name", format!("Account {index}"));
            });
        for index in 0..3 {
            assert_eq!(
                text_of(&factory.instance(index).row(), "display_name").as_deref(),
                Some(format!("Account {index}").as_str())
            );
        }
        assert_eq!(
            factory.overrides().len(),
            0,
            "the sequence ran on clones, not on the factory"
        );
    }

    #[test]
    fn a_varchar_column_is_never_overrun() {
        /// A label with a hard length limit.
        struct Label {
            id: i64,
        }
        impl Entity for Label {
            type Pk = i64;
            const TABLE: TableRef = TableRef::from_static("labels");
            const COLUMNS: &'static [ColumnDef] =
                &[ColumnDef::new("id", ValueKind::I64).primary_key()];
            const NAME: &'static str = "Label";
            fn pk(&self) -> i64 {
                self.id
            }
            fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
                Ok(Self {
                    id: row.get_i64(0)?,
                })
            }
            fn descriptor() -> &'static EntityDescriptor {
                static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
                DESCRIPTOR.get_or_init(|| {
                    EntityDescriptor::builder("Label", Label::TABLE)
                        .column(
                            ColumnDescriptor::builder(
                                Ident::from_static("title"),
                                DataType::VarChar(Some(8)),
                            )
                            .max_length(8)
                            .build(),
                        )
                        .build()
                })
            }
        }

        let row = EntityFactory::<Label>::new().row();
        let title = text_of(&row, "title").expect("title");
        assert!(title.chars().count() <= 8, "{title:?} overruns varchar(8)");
    }

    #[test]
    fn the_insert_statement_names_the_table_and_asks_for_the_row_back() {
        let factory = EntityFactory::<Account>::new();
        let row = factory.row();
        let statement = factory.insert_statement(&row);
        let sql = statement
            .build(moso_orm::Backend::Postgres.dialect())
            .expect("a buildable insert");
        assert!(sql.text.contains("accounts"), "{}", sql.text);
        assert!(
            sql.text.to_lowercase().contains("returning"),
            "{}",
            sql.text
        );
        assert_eq!(
            sql.args.len(),
            row.len(),
            "every generated value is bound, never interpolated"
        );
    }

    #[test]
    fn a_factory_reports_the_parents_it_will_need() {
        /// A post that cannot exist without an author.
        struct Post {
            id: i64,
        }
        impl Entity for Post {
            type Pk = i64;
            const TABLE: TableRef = TableRef::from_static("posts");
            const COLUMNS: &'static [ColumnDef] =
                &[ColumnDef::new("id", ValueKind::I64).primary_key()];
            const NAME: &'static str = "Post";
            fn pk(&self) -> i64 {
                self.id
            }
            fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
                Ok(Self {
                    id: row.get_i64(0)?,
                })
            }
            fn descriptor() -> &'static EntityDescriptor {
                static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
                DESCRIPTOR.get_or_init(|| {
                    EntityDescriptor::builder("Post", Post::TABLE)
                        .column(column("author_id", DataType::BigInt))
                        .foreign_key(key("fk_author", "author_id", "users"))
                        .build()
                })
            }
        }

        let plan = EntityFactory::<Post>::new().relation_plan();
        assert_eq!(plan.steps().len(), 1);
        assert_eq!(plan.steps()[0].table().name().as_str(), "users");

        let plan = EntityFactory::<Post>::new()
            .set("author_id", 7_i64)
            .relation_plan();
        assert!(plan.is_empty(), "an explicit author needs no invention");
    }

    #[test]
    #[should_panic(expected = "is not a column name")]
    fn setting_a_column_that_cannot_be_a_column_says_so_immediately() {
        // A double quote is the one byte an identifier can never carry: it would
        // close the quoting the renderer adds. `moso_sql::Ident` allows a space,
        // because a quoted identifier legitimately may have one.
        let _ = EntityFactory::<Account>::new().set("col\"umn", 1_i64);
    }

    impl Factory for Account {}

    #[test]
    fn the_trait_default_is_the_empty_factory() {
        assert_eq!(Account::factory().instances(), 1);
        assert_eq!(Account::factory_seeded(Seed::new(1)).seed(), Seed::new(1));
    }
}
