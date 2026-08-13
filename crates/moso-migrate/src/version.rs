//! Migration versions: UTC timestamps, not sequence numbers.
//!
//! `docs/02-data/23-migrations.md` picks timestamps so that two developers on
//! two branches cannot collide. The cost is that "the latest migration" is not
//! "the last one applied" — a branch merged late produces a version that sorts
//! before something already in the database. That case is detected rather than
//! ignored; see [`Error::OutOfOrder`].

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// A migration version: `20260729T101500`, in UTC.
///
/// The type is deliberately not a `DateTime`: it is an identifier that happens
/// to be readable as a time. Comparison is lexicographic on the canonical
/// spelling, which for a fixed-width timestamp is the same as chronological.
///
/// ```
/// use moso_migrate::Version;
///
/// let version: Version = "20260729T101500".parse()?;
/// assert_eq!(version.to_string(), "20260729T101500");
/// assert!(version < "20260730T090000".parse()?);
/// # Ok::<(), moso_migrate::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
}

impl Version {
    /// The exact width of the canonical spelling, `YYYYMMDDTHHMMSS`.
    ///
    /// ```
    /// assert_eq!(moso_migrate::Version::WIDTH, 15);
    /// ```
    pub const WIDTH: usize = 15;

    /// Builds a version from its parts, without validating the calendar.
    ///
    /// A version is an identifier. `from_parts(2026, 2, 30, ..)` is a perfectly
    /// good identifier for a migration written on a machine with a confused
    /// clock, and refusing it would strand the file.
    ///
    /// ```
    /// use moso_migrate::Version;
    ///
    /// assert_eq!(Version::from_parts(2026, 7, 29, 10, 15, 0).to_string(), "20260729T101500");
    /// ```
    #[must_use]
    pub const fn from_parts(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    /// The current UTC time, truncated to the second.
    ///
    /// ```
    /// let now = moso_migrate::Version::now();
    /// assert_eq!(now.to_string().len(), moso_migrate::Version::WIDTH);
    /// ```
    #[must_use]
    pub fn now() -> Self {
        Self::from_utc(chrono::Utc::now())
    }

    /// A version for a specific instant, which is what makes the generator
    /// testable.
    ///
    /// ```
    /// use chrono::TimeZone;
    /// use moso_migrate::Version;
    ///
    /// let when = chrono::Utc.with_ymd_and_hms(2026, 7, 29, 10, 15, 0).unwrap();
    /// assert_eq!(Version::from_utc(when).to_string(), "20260729T101500");
    /// ```
    #[must_use]
    pub fn from_utc(when: chrono::DateTime<chrono::Utc>) -> Self {
        use chrono::{Datelike, Timelike};
        Self {
            year: u16::try_from(when.year()).unwrap_or(0),
            month: u8::try_from(when.month()).unwrap_or(1),
            day: u8::try_from(when.day()).unwrap_or(1),
            hour: u8::try_from(when.hour()).unwrap_or(0),
            minute: u8::try_from(when.minute()).unwrap_or(0),
            second: u8::try_from(when.second()).unwrap_or(0),
        }
    }

    /// The next distinct version after this one, for the rare case of two
    /// migrations generated in the same second.
    ///
    /// ```
    /// use moso_migrate::Version;
    ///
    /// let first = Version::from_parts(2026, 7, 29, 10, 15, 59);
    /// assert!(first.next() > first);
    /// ```
    #[must_use]
    pub const fn next(self) -> Self {
        let mut next = self;
        if next.second < 59 {
            next.second += 1;
            return next;
        }
        next.second = 0;
        if next.minute < 59 {
            next.minute += 1;
            return next;
        }
        next.minute = 0;
        if next.hour < 23 {
            next.hour += 1;
            return next;
        }
        next.hour = 0;
        next.day += 1;
        next
    }

    /// The year.
    ///
    /// ```
    /// assert_eq!(moso_migrate::Version::from_parts(2026, 7, 29, 0, 0, 0).year(), 2026);
    /// ```
    #[must_use]
    pub const fn year(self) -> u16 {
        self.year
    }

    /// The month, 1-12 for a version produced by [`Version::now`].
    ///
    /// ```
    /// assert_eq!(moso_migrate::Version::from_parts(2026, 7, 29, 0, 0, 0).month(), 7);
    /// ```
    #[must_use]
    pub const fn month(self) -> u8 {
        self.month
    }

    /// The day of the month.
    ///
    /// ```
    /// assert_eq!(moso_migrate::Version::from_parts(2026, 7, 29, 0, 0, 0).day(), 29);
    /// ```
    #[must_use]
    pub const fn day(self) -> u8 {
        self.day
    }

    /// The hour, 0-23.
    ///
    /// ```
    /// assert_eq!(moso_migrate::Version::from_parts(2026, 7, 29, 10, 0, 0).hour(), 10);
    /// ```
    #[must_use]
    pub const fn hour(self) -> u8 {
        self.hour
    }

    /// The minute, 0-59.
    ///
    /// ```
    /// assert_eq!(moso_migrate::Version::from_parts(2026, 7, 29, 10, 15, 0).minute(), 15);
    /// ```
    #[must_use]
    pub const fn minute(self) -> u8 {
        self.minute
    }

    /// The second, 0-59.
    ///
    /// ```
    /// assert_eq!(moso_migrate::Version::from_parts(2026, 7, 29, 10, 15, 30).second(), 30);
    /// ```
    #[must_use]
    pub const fn second(self) -> u8 {
        self.second
    }

    /// Parses a version, accepting the canonical form and the two spellings
    /// people type: `20260729101500` and `2026-07-29T10:15:00`.
    ///
    /// # Errors
    ///
    /// [`Error::MalformedMigration`] naming the file name it came from.
    ///
    /// ```
    /// use moso_migrate::Version;
    ///
    /// assert_eq!(
    ///     Version::parse("2026-07-29T10:15:00")?,
    ///     Version::parse("20260729T101500")?,
    /// );
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn parse(raw: &str) -> Result<Self> {
        let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
        if digits.len() != 14 {
            return Err(Error::MalformedMigration {
                path: raw.into(),
                reason: format!(
                    "`{raw}` has {} digits and a version needs 14 (YYYYMMDDHHMMSS)",
                    digits.len()
                ),
                help: "name the file `20260729T101500_add_user_locale.sql`; \
                       `moso db make-migration` does it for you"
                    .to_owned(),
            });
        }
        let at = |from: usize, len: usize| -> u32 {
            digits[from..from + len].parse::<u32>().unwrap_or_default()
        };
        Ok(Self {
            year: u16::try_from(at(0, 4)).unwrap_or(0),
            month: u8::try_from(at(4, 2)).unwrap_or(1),
            day: u8::try_from(at(6, 2)).unwrap_or(1),
            hour: u8::try_from(at(8, 2)).unwrap_or(0),
            minute: u8::try_from(at(10, 2)).unwrap_or(0),
            second: u8::try_from(at(12, 2)).unwrap_or(0),
        })
    }

    /// A 63-bit key derived from the version, for `pg_advisory_lock`.
    ///
    /// Not used to lock *per migration* — the runner takes one lock for the
    /// whole run — but exposed because a caller coordinating something else
    /// around a specific version needs a key that will not collide with an
    /// application's own.
    ///
    /// ```
    /// use moso_migrate::Version;
    ///
    /// let key = Version::from_parts(2026, 7, 29, 10, 15, 0).advisory_key();
    /// assert!(key > 0);
    /// ```
    #[must_use]
    pub fn advisory_key(self) -> i64 {
        let hash = crate::hash::fnv1a(self.to_string().as_bytes());
        // Keep it positive: `pg_advisory_lock` takes a signed bigint and a
        // negative key reads as a bug in a `pg_locks` dump.
        i64::try_from(hash >> 1).unwrap_or(1)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}{:02}{:02}T{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

impl FromStr for Version {
    type Err = Error;

    fn from_str(raw: &str) -> Result<Self> {
        Self::parse(raw)
    }
}

impl serde::Serialize for Version {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// A migration's identity: its version and the human-readable name that
/// follows it in the file name.
///
/// ```
/// use moso_migrate::MigrationId;
///
/// let id = MigrationId::parse("20260729T101500_add_user_locale.sql")?;
/// assert_eq!(id.name(), "add_user_locale");
/// assert_eq!(id.file_name("sql"), "20260729T101500_add_user_locale.sql");
/// # Ok::<(), moso_migrate::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MigrationId {
    version: Version,
    name: String,
}

impl MigrationId {
    /// Builds an identity, slugifying the name.
    ///
    /// ```
    /// use moso_migrate::{MigrationId, Version};
    ///
    /// let id = MigrationId::new(Version::from_parts(2026, 7, 29, 10, 15, 0), "Add user locale!");
    /// assert_eq!(id.name(), "add_user_locale");
    /// ```
    #[must_use]
    pub fn new(version: Version, name: &str) -> Self {
        Self {
            version,
            name: slugify(name),
        }
    }

    /// Parses `20260729T101500_add_user_locale.sql`.
    ///
    /// # Errors
    ///
    /// [`Error::MalformedMigration`] when the version is not parseable or the
    /// underscore separating it from the name is missing.
    ///
    /// ```
    /// use moso_migrate::MigrationId;
    ///
    /// assert!(MigrationId::parse("nope.sql").is_err());
    /// ```
    pub fn parse(file_name: &str) -> Result<Self> {
        let stem = file_name
            .rsplit_once('.')
            .map_or(file_name, |(stem, _)| stem);
        let (version, name) = stem
            .split_once('_')
            .ok_or_else(|| Error::MalformedMigration {
                path: file_name.into(),
                reason: "the file name has no `_` separating the version from the name".to_owned(),
                help: "name it `20260729T101500_add_user_locale.sql`".to_owned(),
            })?;
        Ok(Self {
            version: Version::parse(version)?,
            name: name.to_owned(),
        })
    }

    /// The version.
    ///
    /// ```
    /// # use moso_migrate::{MigrationId, Version};
    /// let id = MigrationId::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "init");
    /// assert_eq!(id.version().year(), 2026);
    /// ```
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// The slugified name.
    ///
    /// ```
    /// # use moso_migrate::{MigrationId, Version};
    /// let id = MigrationId::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "init");
    /// assert_eq!(id.name(), "init");
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The file name, with the given extension.
    ///
    /// ```
    /// # use moso_migrate::{MigrationId, Version};
    /// let id = MigrationId::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "init");
    /// assert_eq!(id.file_name("rs"), "20260101T000000_init.rs");
    /// ```
    #[must_use]
    pub fn file_name(&self, extension: &str) -> String {
        format!("{}_{}.{extension}", self.version, self.name)
    }
}

impl fmt::Display for MigrationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}", self.version, self.name)
    }
}

/// Lower-cases, replaces runs of non-alphanumerics with `_`, and trims.
fn slugify(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_separator = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_separator && !out.is_empty() {
                out.push('_');
            }
            pending_separator = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_separator = true;
        }
    }
    if out.is_empty() {
        "migration".to_owned()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_spelling_is_fixed_width() {
        let version = Version::from_parts(2026, 1, 2, 3, 4, 5);
        assert_eq!(version.to_string(), "20260102T030405");
        assert_eq!(version.to_string().len(), Version::WIDTH);
    }

    #[test]
    fn lexicographic_order_is_chronological() {
        let mut versions = [
            Version::from_parts(2026, 7, 30, 9, 0, 0),
            Version::from_parts(2026, 7, 29, 10, 15, 0),
            Version::from_parts(2025, 12, 31, 23, 59, 59),
        ];
        versions.sort_unstable();
        let spellings: Vec<String> = versions.iter().map(ToString::to_string).collect();
        let mut sorted = spellings.clone();
        sorted.sort();
        assert_eq!(spellings, sorted);
    }

    #[test]
    fn parse_accepts_the_three_spellings() {
        let canonical = Version::parse("20260729T101500").expect("canonical");
        assert_eq!(Version::parse("20260729101500").expect("digits"), canonical);
        assert_eq!(
            Version::parse("2026-07-29T10:15:00").expect("iso"),
            canonical
        );
    }

    #[test]
    fn parse_rejects_the_wrong_number_of_digits() {
        let error = Version::parse("2026").expect_err("too short");
        assert!(error.to_string().contains("14"), "{error}");
    }

    #[test]
    fn next_rolls_over() {
        assert_eq!(
            Version::from_parts(2026, 7, 29, 10, 15, 59).next(),
            Version::from_parts(2026, 7, 29, 10, 16, 0)
        );
        assert_eq!(
            Version::from_parts(2026, 7, 29, 23, 59, 59).next(),
            Version::from_parts(2026, 7, 30, 0, 0, 0)
        );
    }

    #[test]
    fn ids_round_trip_through_file_names() {
        let id = MigrationId::parse("20260729T101500_add_user_locale.sql").expect("parses");
        assert_eq!(id.name(), "add_user_locale");
        assert_eq!(id.file_name("sql"), "20260729T101500_add_user_locale.sql");
    }

    #[test]
    fn slugify_is_stable() {
        assert_eq!(slugify("Add user locale!"), "add_user_locale");
        assert_eq!(slugify("  --  "), "migration");
        assert_eq!(slugify("AddUserLocale"), "adduserlocale");
    }

    #[test]
    fn serde_round_trips() {
        let version = Version::from_parts(2026, 7, 29, 10, 15, 0);
        let json = serde_json::to_string(&version).expect("serialises");
        assert_eq!(json, "\"20260729T101500\"");
        let back: Version = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back, version);
    }

    #[test]
    fn advisory_keys_are_positive_and_distinct() {
        let first = Version::from_parts(2026, 7, 29, 10, 15, 0).advisory_key();
        let second = Version::from_parts(2026, 7, 29, 10, 15, 1).advisory_key();
        assert!(first > 0 && second > 0);
        assert_ne!(first, second);
    }
}
