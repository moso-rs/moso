//! A five-field cron expression, parsed and evaluated in a named timezone.
//!
//! # Why this is here and not a dependency
//!
//! Five fields, four operators and a next-occurrence search is about two hundred
//! lines. The crates that do it are fine; they are also a dependency on the
//! critical path of `cargo add moso`, and
//! `docs/00-foundations/03-crate-layout.md` sets a budget on exactly that. The
//! timezone database *is* a dependency (`chrono-tz`), because nobody should
//! hand-roll one and daylight saving is the whole reason `Cron::timezone` takes
//! a name rather than an offset.
//!
//! # The grammar
//!
//! ```text
//! minute  hour  day-of-month  month  day-of-week
//! 0–59    0–23  1–31          1–12   0–6 (Sunday = 0, and 7 = Sunday too)
//! ```
//!
//! Each field is `*`, a number, a `first-last` range, a `a,b,c` list, or any of
//! those with a `/step`. Month and day names (`jan`, `mon`) are accepted because
//! `0 3 * * mon` is what people write. `@hourly`, `@daily`, `@weekly`,
//! `@monthly` and `@yearly` are accepted for the same reason.
//!
//! `day-of-month` and `day-of-week` are **or**-ed when both are restricted,
//! which is what `cron(5)` does and what surprises people who expect `and`; the
//! documentation on [`Cron`](crate::Cron) says so.

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc};

use crate::{Error, Result};

/// One parsed expression.
///
/// ```
/// use moso_jobs::cron::Expression;
///
/// let nightly = Expression::parse("0 3 * * *").expect("a valid expression");
/// assert!(nightly.matches_minute(3, 0));
/// assert!(!nightly.matches_minute(4, 0));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expression {
    /// Which minutes, 0–59.
    minutes: u64,
    /// Which hours, 0–23.
    hours: u32,
    /// Which days of the month, 1–31.
    days: u32,
    /// Which months, 1–12.
    months: u16,
    /// Which days of the week, 0–6 with Sunday at 0.
    weekdays: u8,
    /// Whether the day-of-month field was restricted.
    days_restricted: bool,
    /// Whether the day-of-week field was restricted.
    weekdays_restricted: bool,
}

impl Expression {
    /// Parse a five-field expression, or one of the `@` shorthands.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming the field and what it accepts. Reported at boot
    /// by [`JobRegistry::validate`](crate::JobRegistry::validate), with every
    /// other problem, rather than one per restart.
    ///
    /// ```
    /// use moso_jobs::cron::Expression;
    ///
    /// assert!(Expression::parse("*/15 * * * *").is_ok());
    /// assert!(Expression::parse("@daily").is_ok());
    /// assert!(Expression::parse("0 3 * *").is_err(), "four fields is not five");
    /// ```
    pub fn parse(expression: &str) -> Result<Self> {
        let expression = expression.trim();
        let expanded = match expression.to_ascii_lowercase().as_str() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            _ => expression,
        };

        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(Error::config(format!(
                "`{expression}` has {} field(s); a cron expression has five\n\
                 help: minute hour day-of-month month day-of-week — `0 3 * * *` is 03:00 daily\n\
                 help: `@hourly`, `@daily`, `@weekly`, `@monthly` and `@yearly` also work",
                fields.len()
            )));
        }

        let minutes = parse_field(fields[0], 0, 59, expression, "minute", &[])?;
        let hours = parse_field(fields[1], 0, 23, expression, "hour", &[])? as u32;
        let days = parse_field(fields[2], 1, 31, expression, "day-of-month", &[])? as u32;
        let months = parse_field(fields[3], 1, 12, expression, "month", MONTHS)? as u16;
        // 7 is Sunday as well as 0, which is what every crontab in the world
        // accepts and what somebody porting one will write.
        let weekdays = parse_field(fields[4], 0, 7, expression, "day-of-week", WEEKDAYS)?;
        let weekdays = ((weekdays | (weekdays >> 7)) & 0x7f) as u8;

        Ok(Self {
            minutes,
            hours,
            days,
            months,
            weekdays,
            days_restricted: fields[2] != "*",
            weekdays_restricted: fields[4] != "*",
        })
    }

    /// Whether this expression fires at `hour:minute` on some day.
    ///
    /// ```
    /// use moso_jobs::cron::Expression;
    ///
    /// assert!(Expression::parse("30 9 * * *").unwrap().matches_minute(9, 30));
    /// ```
    #[must_use]
    pub const fn matches_minute(&self, hour: u32, minute: u32) -> bool {
        self.hours & (1 << hour) != 0 && self.minutes & (1_u64 << minute) != 0
    }

    /// Whether this expression fires on `date`.
    ///
    /// `day-of-month` and `day-of-week` are **or**-ed when both are restricted,
    /// as `cron(5)` specifies.
    ///
    /// ```
    /// use chrono::NaiveDate;
    /// use moso_jobs::cron::Expression;
    ///
    /// let first_of_the_month = Expression::parse("0 0 1 * *").unwrap();
    /// assert!(first_of_the_month.matches_date(NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()));
    /// assert!(!first_of_the_month.matches_date(NaiveDate::from_ymd_opt(2026, 3, 2).unwrap()));
    /// ```
    #[must_use]
    pub fn matches_date(&self, date: NaiveDate) -> bool {
        if self.months & (1 << date.month() as u16) == 0 {
            return false;
        }
        let day_ok = self.days & (1 << date.day()) != 0;
        // `chrono` numbers Monday 0; cron numbers Sunday 0.
        let weekday = (date.weekday().num_days_from_monday() + 1) % 7;
        let weekday_ok = self.weekdays & (1 << weekday) != 0;

        match (self.days_restricted, self.weekdays_restricted) {
            (true, true) => day_ok || weekday_ok,
            (true, false) => day_ok,
            (false, true) => weekday_ok,
            (false, false) => true,
        }
    }

    /// The first occurrence strictly after `after`, in `zone`.
    ///
    /// `None` when the expression never matches again — which a cron expression
    /// genuinely can be: `0 0 30 2 *` is the 30th of February.
    ///
    /// ```
    /// use chrono::{TimeZone as _, Utc};
    /// use moso_jobs::cron::{Expression, Timezone};
    ///
    /// let nightly = Expression::parse("0 3 * * *").unwrap();
    /// let from = Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap();
    /// let next = nightly.next_after(from, Timezone::utc()).expect("there is one");
    /// assert_eq!(next.to_rfc3339(), "2026-03-02T03:00:00+00:00");
    /// ```
    #[must_use]
    pub fn next_after(&self, after: DateTime<Utc>, zone: Timezone) -> Option<DateTime<Utc>> {
        // Four years covers every expression that ever fires, including the
        // 29th of February — and bounds the search for the ones that never do.
        const MAX_MINUTES: i64 = 4 * 366 * 24 * 60;

        let zone = zone.0;
        let local = after.with_timezone(&zone);
        // Start at the next whole minute: `next_after` is strictly after.
        let mut cursor = local
            .with_second(0)?
            .with_nanosecond(0)?
            .checked_add_signed(chrono::Duration::minutes(1))?;

        // Days first, then minutes inside a matching day: an expression that
        // fires once a year would otherwise walk half a million minutes.
        let mut scanned = 0_i64;
        while scanned < MAX_MINUTES {
            if !self.matches_date(cursor.date_naive()) {
                // Jump to the start of the next day rather than stepping.
                let next_day = cursor.date_naive().succ_opt()?;
                let midnight = zone
                    .from_local_datetime(&next_day.and_hms_opt(0, 0, 0)?)
                    .earliest()
                    // A day whose midnight does not exist — Brazil used to skip
                    // it — starts an hour later.
                    .or_else(|| {
                        zone.from_local_datetime(&next_day.and_hms_opt(1, 0, 0)?)
                            .earliest()
                    })?;
                scanned += (midnight - cursor).num_minutes().max(1);
                cursor = midnight;
                continue;
            }

            if self.matches_minute(cursor.hour(), cursor.minute()) {
                return Some(cursor.with_timezone(&Utc));
            }
            cursor = cursor.checked_add_signed(chrono::Duration::minutes(1))?;
            scanned += 1;
        }
        None
    }
}

/// A named IANA timezone.
///
/// Named and not an offset: `Europe/Rome` handles the clocks changing and
/// `+01:00` does not, and a nightly job that runs at 02:00 in summer is a
/// support ticket. A newtype rather than a re-export, so the timezone database
/// underneath is an implementation detail this crate can replace.
///
/// ```
/// use moso_jobs::cron::Timezone;
///
/// assert_eq!(Timezone::utc().name(), "UTC");
/// assert!(Timezone::parse("Europe/Rome").is_ok());
/// assert!(Timezone::parse("Middle/Earth").is_err());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timezone(chrono_tz::Tz);

impl Timezone {
    /// Look up a zone by its IANA name.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] listing what a name looks like.
    /// Reported at boot with every other problem, not at the first occurrence.
    ///
    /// ```
    /// use moso_jobs::cron::Timezone;
    ///
    /// assert!(Timezone::parse("America/Sao_Paulo").is_ok());
    /// ```
    pub fn parse(name: &str) -> Result<Self> {
        name.parse::<chrono_tz::Tz>().map(Self).map_err(|_| {
            Error::config(format!(
                "`{name}` is not an IANA timezone\n\
                 help: names look like `UTC`, `Europe/Rome` or `America/Sao_Paulo`; a fixed \
                 offset such as `+01:00` is deliberately not accepted, because it does not \
                 follow the clocks changing"
            ))
        })
    }

    /// UTC, the default.
    ///
    /// ```
    /// use moso_jobs::cron::Timezone;
    ///
    /// assert_eq!(Timezone::default(), Timezone::utc());
    /// ```
    #[must_use]
    pub const fn utc() -> Self {
        Self(chrono_tz::UTC)
    }

    /// The zone's IANA name.
    ///
    /// ```
    /// # use moso_jobs::cron::Timezone;
    /// assert_eq!(Timezone::parse("Europe/Rome").unwrap().name(), "Europe/Rome");
    /// ```
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.0.name()
    }
}

impl Default for Timezone {
    fn default() -> Self {
        Self::utc()
    }
}

impl core::fmt::Display for Timezone {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// Month names, for `jan`–`dec`.
const MONTHS: &[(&str, u8)] = &[
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

/// Day names, for `sun`–`sat`.
const WEEKDAYS: &[(&str, u8)] = &[
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

/// Parse one field into a bitmask over `min..=max`.
fn parse_field(
    field: &str,
    min: u8,
    max: u8,
    expression: &str,
    name: &'static str,
    names: &[(&str, u8)],
) -> Result<u64> {
    let mut mask = 0_u64;
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(bad_field(expression, name, part, min, max));
        }

        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u8 = step
                    .parse()
                    .map_err(|_| bad_field(expression, name, part, min, max))?;
                if step == 0 {
                    return Err(Error::config(format!(
                        "the {name} field of `{expression}` has a step of zero\n\
                         help: `*/5` means every fifth value; a step must be at least 1"
                    )));
                }
                (range, step)
            }
            None => (part, 1),
        };

        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((lo, hi)) = range.split_once('-') {
            (
                value_of(lo, names).ok_or_else(|| bad_field(expression, name, part, min, max))?,
                value_of(hi, names).ok_or_else(|| bad_field(expression, name, part, min, max))?,
            )
        } else {
            let single = value_of(range, names)
                .ok_or_else(|| bad_field(expression, name, part, min, max))?;
            // `5/10` means "from 5 to the end of the range, every ten", which
            // is what every cron implementation does with a bare start.
            if step > 1 {
                (single, max)
            } else {
                (single, single)
            }
        };

        if lo < min || hi > max || lo > hi {
            return Err(bad_field(expression, name, part, min, max));
        }
        let mut value = lo;
        while value <= hi {
            mask |= 1_u64 << value;
            value = match value.checked_add(step) {
                Some(next) => next,
                None => break,
            };
        }
    }
    Ok(mask)
}

/// A number, or one of the three-letter names.
fn value_of(text: &str, names: &[(&str, u8)]) -> Option<u8> {
    let text = text.trim();
    if let Ok(number) = text.parse::<u8>() {
        return Some(number);
    }
    let lowered = text.to_ascii_lowercase();
    names
        .iter()
        .find(|(name, _)| *name == lowered)
        .map(|(_, value)| *value)
}

/// The message for a field that does not parse.
fn bad_field(expression: &str, name: &'static str, part: &str, min: u8, max: u8) -> Error {
    Error::config(format!(
        "the {name} field of `{expression}` does not accept `{part}`\n\
         help: {name} is {min}–{max}, and each field is `*`, a number, `{min}-{max}`, a \
         comma-separated list, or any of those with `/step`"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("a real time")
    }

    /// The expression a nightly cleanup is written with, and the one the
    /// documentation quotes.
    #[test]
    fn the_nightly_expression_fires_once_a_day() {
        let cron = Expression::parse("0 3 * * *").expect("valid");
        let next = cron
            .next_after(at(2026, 3, 1, 12, 0), Timezone::utc())
            .expect("there is one");
        assert_eq!(next, at(2026, 3, 2, 3, 0));

        // And from just before, the same day.
        let next = cron
            .next_after(at(2026, 3, 1, 2, 59), Timezone::utc())
            .expect("there is one");
        assert_eq!(next, at(2026, 3, 1, 3, 0));
    }

    /// Every operator, since the parser is the part a wrong answer hides in.
    #[test]
    fn every_operator_parses_to_what_it_means() {
        let every_quarter = Expression::parse("*/15 * * * *").expect("valid");
        for minute in [0, 15, 30, 45] {
            assert!(every_quarter.matches_minute(0, minute), "{minute}");
        }
        assert!(!every_quarter.matches_minute(0, 7));

        let business = Expression::parse("0 9-17 * * 1-5").expect("valid");
        assert!(business.matches_minute(9, 0));
        assert!(business.matches_minute(17, 0));
        assert!(!business.matches_minute(18, 0));
        assert!(business.matches_date(NaiveDate::from_ymd_opt(2026, 3, 2).unwrap()));
        assert!(!business.matches_date(NaiveDate::from_ymd_opt(2026, 3, 7).unwrap()));

        let listed = Expression::parse("0,30 0 * * *").expect("valid");
        assert!(listed.matches_minute(0, 0));
        assert!(listed.matches_minute(0, 30));
        assert!(!listed.matches_minute(0, 15));

        let named = Expression::parse("0 0 * jan,jul mon").expect("valid");
        assert!(named.matches_date(NaiveDate::from_ymd_opt(2026, 1, 5).unwrap()));
        assert!(!named.matches_date(NaiveDate::from_ymd_opt(2026, 2, 2).unwrap()));

        // A bare start with a step means "from here, every n".
        let from_ten = Expression::parse("10/20 * * * *").expect("valid");
        assert!(from_ten.matches_minute(0, 10));
        assert!(from_ten.matches_minute(0, 30));
        assert!(from_ten.matches_minute(0, 50));
        assert!(!from_ten.matches_minute(0, 0));
    }

    /// `0` and `7` both mean Sunday, because every crontab in the world says so
    /// and somebody porting one will write whichever they learned.
    #[test]
    fn sunday_is_both_zero_and_seven() {
        let sunday = NaiveDate::from_ymd_opt(2026, 3, 1).expect("a Sunday");
        assert_eq!(sunday.weekday(), chrono::Weekday::Sun);
        assert!(Expression::parse("0 0 * * 0").unwrap().matches_date(sunday));
        assert!(Expression::parse("0 0 * * 7").unwrap().matches_date(sunday));
        assert!(
            Expression::parse("0 0 * * sun")
                .unwrap()
                .matches_date(sunday)
        );
    }

    /// The shorthands, since a migration from another scheduler will use them.
    #[test]
    fn the_shorthands_expand() {
        assert_eq!(
            Expression::parse("@daily").unwrap(),
            Expression::parse("0 0 * * *").unwrap()
        );
        assert_eq!(
            Expression::parse("@hourly").unwrap(),
            Expression::parse("0 * * * *").unwrap()
        );
        assert_eq!(
            Expression::parse("@weekly").unwrap(),
            Expression::parse("0 0 * * 0").unwrap()
        );
        assert_eq!(
            Expression::parse("@monthly").unwrap(),
            Expression::parse("0 0 1 * *").unwrap()
        );
        assert_eq!(
            Expression::parse("@yearly").unwrap(),
            Expression::parse("0 0 1 1 *").unwrap()
        );
    }

    /// `cron(5)` **or**-s day-of-month and day-of-week when both are
    /// restricted. It surprises everyone; matching the standard is still right.
    #[test]
    fn day_of_month_and_day_of_week_are_ored() {
        let both = Expression::parse("0 0 1 * mon").expect("valid");
        // The 1st, which is a Thursday in January 2026.
        assert!(both.matches_date(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()));
        // A Monday that is not the 1st.
        assert!(both.matches_date(NaiveDate::from_ymd_opt(2026, 1, 5).unwrap()));
        // Neither.
        assert!(!both.matches_date(NaiveDate::from_ymd_opt(2026, 1, 6).unwrap()));
    }

    /// The whole reason `timezone` takes a name and not an offset: a nightly
    /// job must stay at 03:00 local across the clocks changing.
    #[test]
    fn a_named_zone_survives_the_clocks_changing() {
        let cron = Expression::parse("0 3 * * *").expect("valid");
        let rome = Timezone::parse("Europe/Rome").expect("a real zone");

        // 2026-03-29 is when Rome goes to summer time.
        let winter = cron
            .next_after(at(2026, 3, 27, 12, 0), rome)
            .expect("there is one");
        // 03:00 CET is 02:00 UTC.
        assert_eq!(winter, at(2026, 3, 28, 2, 0));

        let summer = cron
            .next_after(at(2026, 3, 30, 12, 0), rome)
            .expect("there is one");
        // 03:00 CEST is 01:00 UTC — a different UTC hour, the same local one,
        // which a fixed offset could not have done.
        assert_eq!(summer, at(2026, 3, 31, 1, 0));
    }

    /// An expression that never fires again has to say so rather than looping.
    #[test]
    fn an_impossible_expression_has_no_next_occurrence() {
        let never = Expression::parse("0 0 30 2 *").expect("parses; the 30th of February");
        assert!(
            never
                .next_after(at(2026, 3, 1, 0, 0), Timezone::utc())
                .is_none()
        );
    }

    /// A leap day fires only in a leap year, which the four-year search window
    /// has to be wide enough to find.
    #[test]
    fn the_leap_day_is_found_across_years() {
        let leap = Expression::parse("0 0 29 2 *").expect("valid");
        let next = leap
            .next_after(at(2026, 3, 1, 0, 0), Timezone::utc())
            .expect("2028 is a leap year");
        assert_eq!(next, at(2028, 2, 29, 0, 0));
    }

    /// Every way an expression can be wrong, with a message that names the
    /// field — because this is reported at boot and the operator has one shot.
    #[test]
    fn a_malformed_expression_names_the_field_and_its_range() {
        let error = Expression::parse("0 3 * *").expect_err("four fields");
        assert!(error.to_string().contains("has 4 field(s)"), "{error}");
        assert!(
            error.to_string().contains("minute hour day-of-month"),
            "{error}"
        );

        let error = Expression::parse("0 25 * * *").expect_err("hour 25");
        assert!(error.to_string().contains("hour field"), "{error}");
        assert!(error.to_string().contains("0–23"), "{error}");

        let error = Expression::parse("60 * * * *").expect_err("minute 60");
        assert!(error.to_string().contains("minute field"), "{error}");

        let error = Expression::parse("*/0 * * * *").expect_err("a zero step");
        assert!(error.to_string().contains("step of zero"), "{error}");

        let error = Expression::parse("0 0 * * funday").expect_err("no such day");
        assert!(error.to_string().contains("day-of-week field"), "{error}");

        let error = Expression::parse("10-5 * * * *").expect_err("a backwards range");
        assert!(error.to_string().contains("minute field"), "{error}");

        assert!(Expression::parse("").is_err());
        assert!(Expression::parse("not a cron expression").is_err());
    }

    /// A search that starts exactly on an occurrence must return the *next*
    /// one, or a scheduler fires the same minute twice.
    #[test]
    fn the_search_is_strictly_after() {
        let hourly = Expression::parse("0 * * * *").expect("valid");
        let on_the_hour = at(2026, 3, 1, 12, 0);
        assert_eq!(
            hourly.next_after(on_the_hour, Timezone::utc()),
            Some(at(2026, 3, 1, 13, 0))
        );
    }
}
