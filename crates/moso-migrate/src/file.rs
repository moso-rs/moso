//! The migration file format, and the SQL statement splitter that reads it.
//!
//! Default is SQL, "because SQL is what you review, what your DBA reads, and
//! what you paste into an incident channel". The format is therefore a plain
//! `.sql` file that `psql -f` will run, with directives hidden in comments:
//!
//! ```sql
//! -- 20260729T101500_add_user_locale.sql
//! -- moso:generated-from .schema.json@a91f2c
//! -- moso:reversible
//! --
//! -- add `users.locale`
//!
//! -- +migrate up
//! ALTER TABLE "users" ADD COLUMN "locale" text NOT NULL DEFAULT 'en';
//!
//! -- +migrate down
//! ALTER TABLE "users" DROP COLUMN "locale";
//! ```
//!
//! # Destructive blocks
//!
//! A destructive statement is emitted commented out, between
//! `-- +migrate destructive` and `-- +migrate end`. `moso db migrate` refuses
//! to run a file that still has one, and says so; uncommenting the SQL is the
//! acknowledgement. That is the whole of safety-policy point 2, and it is
//! parseable rather than a convention:
//!
//! ```sql
//! -- ⚠ DESTRUCTIVE: this will permanently delete data in `users.legacy_id`.
//! -- +migrate destructive
//! -- ALTER TABLE "users" DROP COLUMN "legacy_id";
//! -- +migrate end
//! ```
//!
//! One block cannot be acknowledged that way, and the parser tells the two
//! apart: a block whose every line is still a comment *after* the block's own
//! `--` is stripped has no statements in it at all, which is the shape the
//! generator writes for a change it cannot express — removing an enum label.
//! [`PendingDestructive::is_manual`] is true for it, and
//! [`MigrationFile::statements_to_apply`] refuses it even under
//! `allow_destructive`, because a flag that "applies" nothing would record the
//! migration as done.
//!
//! ```
//! use moso_migrate::file::MigrationFile;
//!
//! let text = "-- +migrate up\nSELECT 1;\n-- +migrate down\nSELECT 2;\n";
//! let file = MigrationFile::parse("20260729T101500_x.sql", text)?;
//! assert_eq!(file.up().len(), 1);
//! assert!(file.is_reversible());
//! # Ok::<(), moso_migrate::Error>(())
//! ```

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::hash::Checksum;
use crate::plan::Plan;
use crate::version::{MigrationId, Version};

/// The directive that opens the forward half of a migration.
///
/// ```
/// assert_eq!(moso_migrate::file::UP_MARKER, "-- +migrate up");
/// ```
pub const UP_MARKER: &str = "-- +migrate up";

/// The directive that opens the reverse half.
///
/// ```
/// assert_eq!(moso_migrate::file::DOWN_MARKER, "-- +migrate down");
/// ```
pub const DOWN_MARKER: &str = "-- +migrate down";

/// The directive that opens a commented-out destructive block.
///
/// ```
/// assert_eq!(moso_migrate::file::DESTRUCTIVE_MARKER, "-- +migrate destructive");
/// ```
pub const DESTRUCTIVE_MARKER: &str = "-- +migrate destructive";

/// The directive that closes one.
///
/// ```
/// assert_eq!(moso_migrate::file::END_MARKER, "-- +migrate end");
/// ```
pub const END_MARKER: &str = "-- +migrate end";

/// The default `lock_timeout`, per safety-policy point 6.
///
/// Five seconds. A migration that cannot get its lock in five seconds is
/// queued behind a long transaction, and every query arriving after it is
/// queued behind *it* — which is how one `ALTER TABLE` takes a site down.
/// Failing fast turns an outage into a retry.
///
/// ```
/// use std::time::Duration;
/// assert_eq!(moso_migrate::file::DEFAULT_LOCK_TIMEOUT, Duration::from_secs(5));
/// ```
pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// The default `statement_timeout`, per safety-policy point 6.
///
/// ```
/// use std::time::Duration;
/// assert_eq!(moso_migrate::file::DEFAULT_STATEMENT_TIMEOUT, Duration::from_secs(60));
/// ```
pub const DEFAULT_STATEMENT_TIMEOUT: Duration = Duration::from_secs(60);

/// A commented-out destructive statement waiting for a human.
///
/// ```
/// use moso_migrate::file::PendingDestructive;
///
/// let pending = PendingDestructive::new(
///     "this will permanently delete data in `users.legacy_id`",
///     ["ALTER TABLE \"users\" DROP COLUMN \"legacy_id\""],
/// );
/// assert_eq!(pending.statements().len(), 1);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDestructive {
    reason: String,
    statements: Vec<String>,
}

impl PendingDestructive {
    /// A block with its warning and its statements.
    ///
    /// ```
    /// # use moso_migrate::file::PendingDestructive;
    /// assert_eq!(PendingDestructive::new("why", ["DROP TABLE t"]).reason(), "why");
    /// ```
    #[must_use]
    pub fn new(
        reason: impl Into<String>,
        statements: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            reason: reason.into(),
            statements: statements.into_iter().map(Into::into).collect(),
        }
    }

    /// Why it is destructive, in the words the file uses.
    ///
    /// ```
    /// # use moso_migrate::file::PendingDestructive;
    /// assert_eq!(PendingDestructive::new("why", ["DROP TABLE t"]).reason(), "why");
    /// ```
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// The statements, uncommented.
    ///
    /// ```
    /// # use moso_migrate::file::PendingDestructive;
    /// assert_eq!(PendingDestructive::new("w", ["DROP TABLE t"]).statements(), ["DROP TABLE t"]);
    /// ```
    #[must_use]
    pub fn statements(&self) -> &[String] {
        &self.statements
    }

    /// Whether the block is a template a human has to finish rather than SQL
    /// they can simply uncomment.
    ///
    /// Removing an enum label is the case: PostgreSQL has no
    /// `ALTER TYPE … DROP VALUE`, so the generator writes the shape of the plan
    /// and leaves the replacement value to somebody who knows the data.
    ///
    /// [`MigrationFile::statements_to_apply`] refuses a manual block whatever
    /// `allow_destructive` says, because the flag means "run the statements as
    /// written" and there are none.
    ///
    /// ```
    /// use moso_migrate::file::PendingDestructive;
    ///
    /// assert!(PendingDestructive::new("w", Vec::<String>::new()).is_manual());
    /// assert!(!PendingDestructive::new("w", ["DROP TABLE t"]).is_manual());
    /// ```
    #[must_use]
    pub fn is_manual(&self) -> bool {
        self.statements.is_empty()
    }

    /// How the block reads in an error message.
    ///
    /// ```
    /// use moso_migrate::file::PendingDestructive;
    ///
    /// let manual = PendingDestructive::new("the type has to be rewritten", Vec::<String>::new());
    /// assert_eq!(manual.describe(), ["(manual) the type has to be rewritten"]);
    /// ```
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        if self.statements.is_empty() {
            return vec![format!("(manual) {}", self.reason)];
        }
        self.statements.clone()
    }
}

/// One migration file, parsed.
///
/// ```
/// use moso_migrate::file::MigrationFile;
///
/// let file = MigrationFile::parse("20260101T000000_init.sql", "-- +migrate up\nSELECT 1;")?;
/// assert_eq!(file.id().name(), "init");
/// assert!(!file.is_reversible(), "no down section");
/// # Ok::<(), moso_migrate::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationFile {
    id: MigrationId,
    body: String,
    checksum: Checksum,
    up: Vec<String>,
    down: Vec<String>,
    pending_destructive: Vec<PendingDestructive>,
    reversible: bool,
    transactional: bool,
    lock_timeout: Option<Duration>,
    statement_timeout: Option<Duration>,
    generated_from: Option<String>,
    summary: Vec<String>,
    replaces: Vec<Version>,
}

impl MigrationFile {
    /// Parses a migration file's text.
    ///
    /// # Errors
    ///
    /// [`Error::MalformedMigration`] when the file name is not a version and a
    /// name, or when the body has no `-- +migrate up` section, or when a
    /// destructive block is not closed.
    ///
    /// ```
    /// use moso_migrate::file::MigrationFile;
    ///
    /// assert!(MigrationFile::parse("20260101T000000_x.sql", "SELECT 1;").is_err());
    /// ```
    pub fn parse(file_name: &str, text: &str) -> Result<Self> {
        let id = MigrationId::parse(file_name)?;
        let mut file = Self {
            id,
            body: text.to_owned(),
            checksum: Checksum::of_migration(text),
            up: Vec::new(),
            down: Vec::new(),
            pending_destructive: Vec::new(),
            reversible: false,
            transactional: true,
            lock_timeout: None,
            statement_timeout: None,
            generated_from: None,
            summary: Vec::new(),
            replaces: Vec::new(),
        };

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Section {
            Header,
            Up,
            Down,
        }
        let mut section = Section::Header;
        let mut up = String::new();
        let mut down = String::new();
        let mut in_destructive: Option<(String, String)> = None;
        let mut last_warning = String::new();
        let mut saw_up_marker = false;

        for line in text.lines() {
            let trimmed = line.trim_end();
            let compact = trimmed.trim();

            if let Some((reason, buffer)) = in_destructive.as_mut() {
                if compact == END_MARKER {
                    // A block whose lines are all still commented is pending,
                    // even when none of them parses as a statement: a manual
                    // template — the enum-label removal case — is exactly the
                    // situation where the migration must not proceed until a
                    // human has written the plan. An empty buffer means every
                    // line was uncommented, which IS the acknowledgement.
                    if !buffer.trim().is_empty() {
                        file.pending_destructive.push(PendingDestructive::new(
                            reason.clone(),
                            split_statements(buffer),
                        ));
                    }
                    in_destructive = None;
                    continue;
                }
                match compact.strip_prefix("--") {
                    // Still commented: it is pending.
                    Some(rest) => {
                        buffer.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                        buffer.push('\n');
                    }
                    // Uncommented by a human: that IS the acknowledgement, and
                    // it becomes an ordinary statement in whichever section we
                    // are in.
                    None => {
                        let target = if section == Section::Down {
                            &mut down
                        } else {
                            &mut up
                        };
                        target.push_str(trimmed);
                        target.push('\n');
                    }
                }
                continue;
            }

            if compact == UP_MARKER {
                section = Section::Up;
                saw_up_marker = true;
                continue;
            }
            if compact == DOWN_MARKER {
                section = Section::Down;
                file.reversible = true;
                continue;
            }
            if compact == DESTRUCTIVE_MARKER {
                in_destructive = Some((
                    if last_warning.is_empty() {
                        "this operation destroys data".to_owned()
                    } else {
                        last_warning.clone()
                    },
                    String::new(),
                ));
                continue;
            }
            if let Some(warning) = compact
                .strip_prefix("-- ⚠ DESTRUCTIVE:")
                .or_else(|| compact.strip_prefix("--⚠ DESTRUCTIVE:"))
            {
                last_warning = warning.trim().trim_end_matches('.').to_owned();
                continue;
            }

            if section == Section::Header {
                if let Some(directive) = compact.strip_prefix("-- moso:") {
                    file.apply_directive(directive.trim(), file_name)?;
                } else if let Some(note) = compact.strip_prefix("-- ")
                    && !note.starts_with('+')
                    && !note.ends_with(".sql")
                {
                    file.summary.push(note.to_owned());
                }
                continue;
            }

            let target = match section {
                Section::Down => &mut down,
                _ => &mut up,
            };
            target.push_str(trimmed);
            target.push('\n');
        }

        if let Some((reason, _)) = in_destructive {
            return Err(Error::MalformedMigration {
                path: file_name.into(),
                reason: format!("the destructive block `{reason}` is never closed"),
                help: format!("add a `{END_MARKER}` line after it"),
            });
        }
        if !saw_up_marker {
            return Err(Error::MalformedMigration {
                path: file_name.into(),
                reason: format!("there is no `{UP_MARKER}` line"),
                help: format!(
                    "every migration needs `{UP_MARKER}`, and a reversible one also needs \
                     `{DOWN_MARKER}`"
                ),
            });
        }

        file.up = split_statements(&up);
        file.down = split_statements(&down);
        Ok(file)
    }

    /// Reads and parses a file from disk.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the file cannot be read, plus everything
    /// [`MigrationFile::parse`] returns.
    ///
    /// ```no_run
    /// use moso_migrate::file::MigrationFile;
    ///
    /// let file = MigrationFile::read("migrations/20260101T000000_init.sql")?;
    /// println!("{}", file.id());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| {
            Error::io(
                "reading",
                path,
                "check that the migrations directory is the one you think it is",
                source,
            )
        })?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        Self::parse(file_name, &text)
    }

    fn apply_directive(&mut self, directive: &str, file_name: &str) -> Result<()> {
        let (key, value) = directive
            .split_once(char::is_whitespace)
            .unwrap_or((directive, ""));
        let value = value.trim();
        match key {
            "reversible" => self.reversible = true,
            "irreversible" => self.reversible = false,
            "generated-from" => self.generated_from = Some(value.to_owned()),
            "transactional" => {
                self.transactional = !matches!(value, "false" | "no" | "off");
            }
            "destructive" => {}
            "replaces" => {
                for version in value.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                    self.replaces.push(Version::parse(version)?);
                }
            }
            "lock-timeout" => self.lock_timeout = Some(parse_duration(value, file_name, key)?),
            "statement-timeout" => {
                self.statement_timeout = Some(parse_duration(value, file_name, key)?);
            }
            other => {
                return Err(Error::MalformedMigration {
                    path: file_name.into(),
                    reason: format!("`-- moso:{other}` is not a directive this build knows"),
                    help: "the directives are `reversible`, `irreversible`, `transactional`, \
                           `lock-timeout`, `statement-timeout`, `destructive`, `replaces` and \
                           `generated-from`"
                        .to_owned(),
                });
            }
        }
        Ok(())
    }

    /// The migration's identity.
    ///
    /// ```
    /// # use moso_migrate::file::MigrationFile;
    /// let file = MigrationFile::parse("20260101T000000_init.sql", "-- +migrate up\nSELECT 1;")?;
    /// assert_eq!(file.id().name(), "init");
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub const fn id(&self) -> &MigrationId {
        &self.id
    }

    /// The version.
    ///
    /// ```
    /// # use moso_migrate::file::MigrationFile;
    /// let file = MigrationFile::parse("20260101T000000_init.sql", "-- +migrate up\nSELECT 1;")?;
    /// assert_eq!(file.version().year(), 2026);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub const fn version(&self) -> Version {
        self.id.version()
    }

    /// The file's checksum, as recorded in `moso_migrations`.
    ///
    /// ```
    /// # use moso_migrate::file::MigrationFile;
    /// let file = MigrationFile::parse("20260101T000000_init.sql", "-- +migrate up\nSELECT 1;")?;
    /// assert_eq!(file.checksum().to_string().len(), 64);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub const fn checksum(&self) -> Checksum {
        self.checksum
    }

    /// The whole file, verbatim.
    ///
    /// ```
    /// # use moso_migrate::file::MigrationFile;
    /// let text = "-- +migrate up\nSELECT 1;";
    /// let file = MigrationFile::parse("20260101T000000_init.sql", text)?;
    /// assert_eq!(file.body(), text);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// The forward statements, split and stripped of comments.
    ///
    /// ```
    /// # use moso_migrate::file::MigrationFile;
    /// let file = MigrationFile::parse("20260101T000000_x.sql", "-- +migrate up\nSELECT 1;\nSELECT 2;")?;
    /// assert_eq!(file.up().len(), 2);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub fn up(&self) -> &[String] {
        &self.up
    }

    /// The reverse statements.
    ///
    /// ```
    /// # use moso_migrate::file::MigrationFile;
    /// let file = MigrationFile::parse("20260101T000000_x.sql", "-- +migrate up\nSELECT 1;")?;
    /// assert!(file.down().is_empty());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub fn down(&self) -> &[String] {
        &self.down
    }

    /// The destructive blocks nobody has uncommented.
    ///
    /// ```
    /// use moso_migrate::file::MigrationFile;
    ///
    /// let text = "-- +migrate up\n-- ⚠ DESTRUCTIVE: it deletes rows.\n\
    ///             -- +migrate destructive\n-- DROP TABLE \"t\";\n-- +migrate end\n";
    /// let file = MigrationFile::parse("20260101T000000_x.sql", text)?;
    /// assert_eq!(file.pending_destructive().len(), 1);
    /// assert!(file.up().is_empty(), "the block is not applied");
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub fn pending_destructive(&self) -> &[PendingDestructive] {
        &self.pending_destructive
    }

    /// Whether the file has a `-- +migrate down` section.
    ///
    /// ```
    /// # use moso_migrate::file::MigrationFile;
    /// let file = MigrationFile::parse("20260101T000000_x.sql", "-- +migrate up\nSELECT 1;")?;
    /// assert!(!file.is_reversible());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub const fn is_reversible(&self) -> bool {
        self.reversible
    }

    /// Whether it runs inside a transaction.
    ///
    /// ```
    /// # use moso_migrate::file::MigrationFile;
    /// let file = MigrationFile::parse("20260101T000000_x.sql", "-- +migrate up\nSELECT 1;")?;
    /// assert!(file.is_transactional());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub const fn is_transactional(&self) -> bool {
        self.transactional
    }

    /// The `lock_timeout` this file asks for, or the default.
    ///
    /// ```
    /// # use moso_migrate::file::{MigrationFile, DEFAULT_LOCK_TIMEOUT};
    /// let file = MigrationFile::parse("20260101T000000_x.sql", "-- +migrate up\nSELECT 1;")?;
    /// assert_eq!(file.lock_timeout(), DEFAULT_LOCK_TIMEOUT);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub fn lock_timeout(&self) -> Duration {
        self.lock_timeout.unwrap_or(DEFAULT_LOCK_TIMEOUT)
    }

    /// The `statement_timeout` this file asks for, or the default.
    ///
    /// ```
    /// # use moso_migrate::file::{MigrationFile, DEFAULT_STATEMENT_TIMEOUT};
    /// let file = MigrationFile::parse("20260101T000000_x.sql", "-- +migrate up\nSELECT 1;")?;
    /// assert_eq!(file.statement_timeout(), DEFAULT_STATEMENT_TIMEOUT);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub fn statement_timeout(&self) -> Duration {
        self.statement_timeout.unwrap_or(DEFAULT_STATEMENT_TIMEOUT)
    }

    /// The snapshot checksum this migration was generated from, when it was
    /// generated rather than hand-written.
    ///
    /// ```
    /// # use moso_migrate::file::MigrationFile;
    /// let file = MigrationFile::parse("20260101T000000_x.sql", "-- +migrate up\nSELECT 1;")?;
    /// assert_eq!(file.generated_from(), None);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub fn generated_from(&self) -> Option<&str> {
        self.generated_from.as_deref()
    }

    /// The versions this file stands in for, from `-- moso:replaces`.
    ///
    /// A database that has already applied every one of them records this file
    /// as applied without running it — the mechanism that makes
    /// [`squash`](crate::squash) safe on an existing database.
    ///
    /// ```
    /// # use moso_migrate::file::MigrationFile;
    /// let text = "-- moso:replaces 20260101T000000,20260201T000000\n-- +migrate up\nSELECT 1;";
    /// let file = MigrationFile::parse("20260101T000000_squashed.sql", text)?;
    /// assert_eq!(file.replaces().len(), 2);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub fn replaces(&self) -> &[Version] {
        &self.replaces
    }

    /// The one-line-per-change summary from the header.
    ///
    /// ```
    /// # use moso_migrate::file::MigrationFile;
    /// let text = "-- add `users.locale`\n-- +migrate up\nSELECT 1;";
    /// let file = MigrationFile::parse("20260101T000000_x.sql", text)?;
    /// assert_eq!(file.summary(), ["add `users.locale`"]);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub fn summary(&self) -> &[String] {
        &self.summary
    }

    /// The statements to run, including the destructive ones when the caller
    /// has passed `--allow-destructive`.
    ///
    /// `allow_destructive` is not a universal override. It means "run the
    /// statements in the block as written", and a block that is a *template* —
    /// [`PendingDestructive::is_manual`] — has no statements to run, so
    /// honouring the flag would record the migration as applied having changed
    /// nothing. Those are refused whatever the flag says.
    ///
    /// # Errors
    ///
    /// [`Error::Destructive`] when there are pending destructive blocks and
    /// `allow_destructive` is false. The message names the file and every
    /// statement, and gives both fixes.
    /// [`Error::ManualMigrationRequired`] when a pending block is a template,
    /// whether or not `allow_destructive` is set.
    ///
    /// ```
    /// use moso_migrate::file::MigrationFile;
    ///
    /// let text = "-- +migrate up\nSELECT 1;\n-- ⚠ DESTRUCTIVE: rows go.\n\
    ///             -- +migrate destructive\n-- DROP TABLE \"t\";\n-- +migrate end\n";
    /// let file = MigrationFile::parse("20260101T000000_x.sql", text)?;
    /// assert!(file.statements_to_apply(false).is_err());
    /// assert_eq!(file.statements_to_apply(true)?.len(), 2);
    ///
    /// let manual = "-- +migrate up\n-- ⚠ DESTRUCTIVE: the type is rewritten.\n\
    ///               -- +migrate destructive\n-- -- CREATE TYPE ...\n-- +migrate end\n";
    /// let file = MigrationFile::parse("20260101T000000_y.sql", manual)?;
    /// assert!(file.statements_to_apply(true).is_err(), "no flag applies a template");
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn statements_to_apply(&self, allow_destructive: bool) -> Result<Vec<String>> {
        if self.pending_destructive.is_empty() {
            return Ok(self.up.clone());
        }

        // A template is refused first, and refused regardless of the flag: the
        // message a reader needs is "write the SQL", not "pass the flag you
        // already passed".
        let manual: Vec<String> = self
            .pending_destructive
            .iter()
            .filter(|block| block.is_manual())
            .map(|block| block.reason().to_owned())
            .collect();
        if !manual.is_empty() {
            return Err(Error::ManualMigrationRequired {
                file: self.id.file_name("sql"),
                reasons: manual,
            });
        }

        if !allow_destructive {
            return Err(Error::Destructive {
                file: self.id.file_name("sql"),
                operations: self
                    .pending_destructive
                    .iter()
                    .flat_map(PendingDestructive::describe)
                    .collect(),
            });
        }
        let mut statements = self.up.clone();
        statements.extend(
            self.pending_destructive
                .iter()
                .flat_map(|block| block.statements().iter().cloned()),
        );
        Ok(statements)
    }
}

fn parse_duration(value: &str, file_name: &str, key: &str) -> Result<Duration> {
    let malformed = || Error::MalformedMigration {
        path: file_name.into(),
        reason: format!("`-- moso:{key} {value}` is not a duration"),
        help: "write it as `5s`, `500ms` or `2min`".to_owned(),
    };
    let (number, unit) = value.split_at(
        value
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(value.len()),
    );
    let number: u64 = number.parse().map_err(|_| malformed())?;
    Ok(match unit.trim() {
        "" | "s" | "sec" | "secs" => Duration::from_secs(number),
        "ms" => Duration::from_millis(number),
        "min" | "m" => Duration::from_secs(number * 60),
        _ => return Err(malformed()),
    })
}

/// Splits SQL into statements on top-level semicolons.
///
/// Aware of line comments, block comments, single-quoted strings with their
/// `''` escape, double-quoted identifiers, and PostgreSQL dollar quoting — a
/// naive `split(';')` mangles a trigger body on its first outing.
///
/// ```
/// use moso_migrate::file::split_statements;
///
/// let sql = "SELECT ';';\n-- a comment;\nSELECT $$ a ; b $$;";
/// assert_eq!(split_statements(sql), ["SELECT ';'", "SELECT $$ a ; b $$"]);
/// ```
#[must_use]
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let bytes: Vec<char> = sql.chars().collect();
    let mut at = 0;

    while at < bytes.len() {
        let ch = bytes[at];
        let next = bytes.get(at + 1).copied();

        // Line comment.
        if ch == '-' && next == Some('-') {
            while at < bytes.len() && bytes[at] != '\n' {
                at += 1;
            }
            // Keep the newline so tokens on either side stay separated.
            if at < bytes.len() {
                current.push('\n');
                at += 1;
            }
            continue;
        }
        // Block comment, which nests in PostgreSQL.
        if ch == '/' && next == Some('*') {
            let mut depth = 1_usize;
            at += 2;
            while at < bytes.len() && depth > 0 {
                if bytes[at] == '/' && bytes.get(at + 1) == Some(&'*') {
                    depth += 1;
                    at += 2;
                } else if bytes[at] == '*' && bytes.get(at + 1) == Some(&'/') {
                    depth -= 1;
                    at += 2;
                } else {
                    at += 1;
                }
            }
            current.push(' ');
            continue;
        }
        // Single-quoted string.
        if ch == '\'' {
            current.push(ch);
            at += 1;
            while at < bytes.len() {
                current.push(bytes[at]);
                if bytes[at] == '\'' {
                    if bytes.get(at + 1) == Some(&'\'') {
                        current.push('\'');
                        at += 2;
                        continue;
                    }
                    at += 1;
                    break;
                }
                at += 1;
            }
            continue;
        }
        // Double-quoted identifier.
        if ch == '"' {
            current.push(ch);
            at += 1;
            while at < bytes.len() {
                current.push(bytes[at]);
                if bytes[at] == '"' {
                    if bytes.get(at + 1) == Some(&'"') {
                        current.push('"');
                        at += 2;
                        continue;
                    }
                    at += 1;
                    break;
                }
                at += 1;
            }
            continue;
        }
        // Dollar quoting: `$$`, `$tag$`.
        if ch == '$'
            && let Some(tag_end) = dollar_tag_end(&bytes, at)
        {
            let tag: String = bytes[at..tag_end].iter().collect();
            current.push_str(&tag);
            at = tag_end;
            while at < bytes.len() {
                if bytes[at] == '$'
                    && bytes[at..].len() >= tag.len()
                    && bytes[at..at + tag.len()].iter().collect::<String>() == tag
                {
                    current.push_str(&tag);
                    at += tag.len();
                    break;
                }
                current.push(bytes[at]);
                at += 1;
            }
            continue;
        }
        if ch == ';' {
            push_statement(&mut statements, &mut current);
            at += 1;
            continue;
        }
        current.push(ch);
        at += 1;
    }
    push_statement(&mut statements, &mut current);
    statements
}

fn push_statement(statements: &mut Vec<String>, current: &mut String) {
    let trimmed: Vec<&str> = current
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let statement = trimmed.join("\n");
    let statement = statement.trim();
    if !statement.is_empty() {
        statements.push(statement.to_owned());
    }
    current.clear();
}

/// The index one past the closing `$` of a dollar-quote tag starting at `at`,
/// or `None` if this `$` does not open one.
fn dollar_tag_end(chars: &[char], at: usize) -> Option<usize> {
    let mut cursor = at + 1;
    while cursor < chars.len() {
        let ch = chars[cursor];
        if ch == '$' {
            return Some(cursor + 1);
        }
        if !ch.is_alphanumeric() && ch != '_' {
            return None;
        }
        cursor += 1;
    }
    None
}

/// Writes a migration file from a plan.
///
/// ```
/// use moso_migrate::file::write_migration;
/// use moso_migrate::plan::{Operation, Plan};
/// use moso_migrate::{MigrationId, Version};
/// use moso_orm::Backend;
///
/// let mut plan = Plan::empty(Backend::Postgres);
/// plan.push(
///     Operation::new("create the table `users`", ["CREATE TABLE \"users\" ()"])
///         .reversed_by(["DROP TABLE \"users\""]),
/// );
///
/// let id = MigrationId::new(Version::from_parts(2026, 7, 29, 10, 15, 0), "create_users");
/// let text = write_migration(&id, &plan, Some("a91f2c"));
/// assert!(text.contains("-- +migrate up"));
/// assert!(text.contains("-- +migrate down"));
/// assert!(text.contains("-- moso:generated-from .schema.json@a91f2c"));
/// ```
#[must_use]
pub fn write_migration(id: &MigrationId, plan: &Plan, snapshot: Option<&str>) -> String {
    let mut out = String::with_capacity(512);
    let _ = writeln!(out, "-- {}", id.file_name("sql"));
    if let Some(snapshot) = snapshot {
        let _ = writeln!(out, "-- moso:generated-from .schema.json@{snapshot}");
    }
    if plan.is_reversible() {
        out.push_str("-- moso:reversible\n");
    } else {
        out.push_str("-- moso:irreversible\n");
    }
    if plan.requires_no_transaction() {
        out.push_str("-- moso:transactional false\n");
    }
    if plan.is_destructive() {
        out.push_str("-- moso:destructive\n");
    }
    out.push_str("--\n");
    for operation in plan.operations() {
        let _ = writeln!(out, "-- {}", operation.description());
    }

    out.push('\n');
    out.push_str(UP_MARKER);
    out.push('\n');
    for operation in plan.operations() {
        if operation.up().is_empty() {
            continue;
        }
        out.push('\n');
        write_operation(
            &mut out,
            operation,
            operation.up(),
            operation.is_destructive(),
        );
    }

    if plan.is_reversible() {
        out.push('\n');
        out.push_str(DOWN_MARKER);
        out.push('\n');
        // Undone in reverse: the last thing done is the first thing undone.
        for operation in plan.operations().iter().rev() {
            if operation.down().is_empty() {
                continue;
            }
            out.push('\n');
            let _ = writeln!(out, "-- undo: {}", operation.description());
            for statement in operation.down() {
                let _ = writeln!(out, "{statement};");
            }
        }
    }
    out
}

fn write_operation(
    out: &mut String,
    operation: &crate::plan::Operation,
    statements: &[String],
    destructive: bool,
) {
    for note in operation.notes() {
        for line in wrap_comment(note) {
            let _ = writeln!(out, "-- {line}");
        }
    }
    if destructive {
        let _ = writeln!(out, "-- ⚠ DESTRUCTIVE: {}.", operation.description());
        out.push_str(
            "-- Uncomment the block below to apply it, after confirming that no running version\n\
             -- of the application still depends on what it removes.\n",
        );
        let _ = writeln!(out, "{DESTRUCTIVE_MARKER}");
        for statement in statements {
            for line in statement.lines() {
                let _ = writeln!(out, "-- {line}");
            }
            // A statement whose body is already a comment template carries no
            // terminator of its own.
            if !statement.trim_start().starts_with("--") {
                let _ = writeln!(out, "-- ;");
            }
        }
        let _ = writeln!(out, "{END_MARKER}");
        return;
    }
    for statement in statements {
        if statement.trim_start().starts_with("--") {
            let _ = writeln!(out, "{statement}");
        } else {
            let _ = writeln!(out, "{statement};");
        }
    }
}

/// Wraps a note at roughly 90 columns so a generated file does not have a
/// 300-character comment in it.
fn wrap_comment(note: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in note.split_whitespace() {
        if !current.is_empty() && current.len() + word.len() + 1 > 90 {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Operation;
    use moso_orm::Backend;

    #[test]
    fn a_round_trip_through_the_writer_and_the_parser() {
        let mut plan = Plan::empty(Backend::Postgres);
        plan.push(
            Operation::new(
                "add `users.locale`",
                ["ALTER TABLE \"users\" ADD COLUMN \"locale\" text NOT NULL DEFAULT 'en'"],
            )
            .reversed_by(["ALTER TABLE \"users\" DROP COLUMN \"locale\""]),
        );

        let id = MigrationId::new(
            Version::from_parts(2026, 7, 29, 10, 15, 0),
            "add_user_locale",
        );
        let text = write_migration(&id, &plan, Some("a91f2c"));
        let parsed = MigrationFile::parse(&id.file_name("sql"), &text).expect("parses");

        assert_eq!(parsed.up().len(), 1);
        assert_eq!(parsed.down().len(), 1);
        assert!(parsed.is_reversible());
        assert!(parsed.is_transactional());
        assert_eq!(parsed.generated_from(), Some(".schema.json@a91f2c"));
        assert_eq!(parsed.summary(), ["add `users.locale`"]);
        assert!(parsed.pending_destructive().is_empty());
    }

    #[test]
    fn a_destructive_operation_is_written_commented_and_read_back_as_pending() {
        let mut plan = Plan::empty(Backend::Postgres);
        plan.push(
            Operation::new(
                "drop `users.legacy_id`",
                ["ALTER TABLE \"users\" DROP COLUMN \"legacy_id\""],
            )
            .destructive()
            .reversed_by(["ALTER TABLE \"users\" ADD COLUMN \"legacy_id\" integer"]),
        );

        let id = MigrationId::new(Version::from_parts(2026, 7, 29, 10, 15, 0), "drop_legacy");
        let text = write_migration(&id, &plan, None);
        assert!(text.contains("⚠ DESTRUCTIVE"), "{text}");
        assert!(
            text.contains("-- ALTER TABLE \"users\" DROP COLUMN \"legacy_id\""),
            "{text}"
        );

        let parsed = MigrationFile::parse(&id.file_name("sql"), &text).expect("parses");
        assert!(
            parsed.up().is_empty(),
            "nothing runs until it is acknowledged"
        );
        assert_eq!(parsed.pending_destructive().len(), 1);

        let error = parsed.statements_to_apply(false).expect_err("refused");
        assert!(error.to_string().contains("--allow-destructive"), "{error}");
        assert_eq!(parsed.statements_to_apply(true).expect("allowed").len(), 1);
    }

    #[test]
    fn a_template_block_is_refused_even_with_allow_destructive() {
        // Every line of the block is a comment, so there is nothing for the
        // flag to run — and applying the file would record it as done.
        let text = "-- +migrate up\n\
                    -- ⚠ DESTRUCTIVE: rewrite the type `user_role`.\n\
                    -- +migrate destructive\n\
                    -- -- CREATE TYPE \"user_role_new\" AS ENUM ('admin');\n\
                    -- -- DROP TYPE \"user_role\";\n\
                    -- +migrate end\n";
        let parsed = MigrationFile::parse("20260101T000000_x.sql", text).expect("parses");
        assert_eq!(parsed.pending_destructive().len(), 1);
        assert!(parsed.pending_destructive()[0].is_manual());

        for allowed in [false, true] {
            let error = parsed
                .statements_to_apply(allowed)
                .expect_err("a template is not a statement");
            assert!(
                error.to_string().contains("cannot apply"),
                "{allowed}: {error}"
            );
        }
    }

    #[test]
    fn writing_the_statements_into_a_template_block_finishes_it() {
        let text = "-- +migrate up\n\
                    -- ⚠ DESTRUCTIVE: rewrite the type `user_role`.\n\
                    -- +migrate destructive\n\
                    CREATE TYPE \"user_role_new\" AS ENUM ('admin');\n\
                    DROP TYPE \"user_role\";\n\
                    -- +migrate end\n";
        let parsed = MigrationFile::parse("20260101T000000_x.sql", text).expect("parses");
        assert!(parsed.pending_destructive().is_empty());
        assert_eq!(parsed.statements_to_apply(false).expect("done").len(), 2);
    }

    #[test]
    fn uncommenting_the_block_is_the_acknowledgement() {
        let text = "-- +migrate up\n\
                    -- ⚠ DESTRUCTIVE: it deletes rows.\n\
                    -- +migrate destructive\n\
                    ALTER TABLE \"users\" DROP COLUMN \"legacy_id\";\n\
                    -- +migrate end\n";
        let parsed = MigrationFile::parse("20260101T000000_x.sql", text).expect("parses");
        assert!(parsed.pending_destructive().is_empty());
        assert_eq!(parsed.up().len(), 1);
        assert_eq!(parsed.statements_to_apply(false).expect("allowed").len(), 1);
    }

    #[test]
    fn directives_are_read() {
        let text = "-- moso:transactional false\n\
                    -- moso:lock-timeout 30s\n\
                    -- moso:statement-timeout 5min\n\
                    -- +migrate up\nSELECT 1;\n";
        let parsed = MigrationFile::parse("20260101T000000_x.sql", text).expect("parses");
        assert!(!parsed.is_transactional());
        assert_eq!(parsed.lock_timeout(), Duration::from_secs(30));
        assert_eq!(parsed.statement_timeout(), Duration::from_secs(300));
    }

    #[test]
    fn an_unknown_directive_lists_the_known_ones() {
        let text = "-- moso:trasnactional false\n-- +migrate up\nSELECT 1;\n";
        let error = MigrationFile::parse("20260101T000000_x.sql", text).expect_err("typo");
        assert!(error.to_string().contains("`lock-timeout`"), "{error}");
    }

    #[test]
    fn a_file_with_no_up_marker_is_refused() {
        let error = MigrationFile::parse("20260101T000000_x.sql", "SELECT 1;").expect_err("no up");
        assert!(error.to_string().contains("+migrate up"), "{error}");
    }

    #[test]
    fn an_unclosed_destructive_block_is_refused() {
        let text = "-- +migrate up\n-- +migrate destructive\n-- DROP TABLE t;\n";
        let error = MigrationFile::parse("20260101T000000_x.sql", text).expect_err("unclosed");
        assert!(error.to_string().contains("+migrate end"), "{error}");
    }

    #[test]
    fn checksums_ignore_reformatting_but_not_content() {
        let a = MigrationFile::parse("20260101T000000_x.sql", "-- +migrate up\nSELECT 1;\n")
            .expect("parses");
        let b = MigrationFile::parse(
            "20260101T000000_x.sql",
            "-- +migrate up\r\n\r\nSELECT 1;  \r\n",
        )
        .expect("parses");
        assert_eq!(a.checksum(), b.checksum());

        let c = MigrationFile::parse("20260101T000000_x.sql", "-- +migrate up\nSELECT 2;\n")
            .expect("parses");
        assert_ne!(a.checksum(), c.checksum());
    }

    #[test]
    fn the_splitter_handles_the_hard_cases() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2;"),
            ["SELECT 1", "SELECT 2"]
        );
        assert_eq!(split_statements("SELECT ';'"), ["SELECT ';'"]);
        assert_eq!(
            split_statements("SELECT 'it''s; fine'"),
            ["SELECT 'it''s; fine'"]
        );
        assert_eq!(split_statements("SELECT \"a;b\""), ["SELECT \"a;b\""]);
        assert_eq!(split_statements("-- SELECT 1;\nSELECT 2;"), ["SELECT 2"]);
        assert_eq!(split_statements("/* a ; b */ SELECT 1;"), ["SELECT 1"]);
        assert_eq!(
            split_statements("/* a /* b ; */ c */ SELECT 1;"),
            ["SELECT 1"]
        );
        assert_eq!(split_statements(""), Vec::<String>::new());
        assert_eq!(split_statements(";;;"), Vec::<String>::new());
    }

    #[test]
    fn the_splitter_keeps_a_dollar_quoted_body_whole() {
        let sql = "CREATE FUNCTION f() RETURNS trigger AS $$\n\
                   BEGIN\n  RETURN NEW;\nEND;\n$$ LANGUAGE plpgsql;\nSELECT 1;";
        let statements = split_statements(sql);
        assert_eq!(statements.len(), 2, "{statements:?}");
        assert!(statements[0].contains("RETURN NEW;"), "{statements:?}");
        assert_eq!(statements[1], "SELECT 1");
    }

    #[test]
    fn the_splitter_handles_a_tagged_dollar_quote() {
        let sql = "SELECT $body$ a ; b $body$; SELECT 2;";
        assert_eq!(
            split_statements(sql),
            ["SELECT $body$ a ; b $body$", "SELECT 2"]
        );
    }

    #[test]
    fn a_dollar_that_is_not_a_quote_is_left_alone() {
        assert_eq!(split_statements("SELECT $1;"), ["SELECT $1"]);
    }

    #[test]
    fn the_down_section_is_written_in_reverse() {
        let mut plan = Plan::empty(Backend::Postgres);
        plan.push(Operation::new("first", ["CREATE TABLE a ()"]).reversed_by(["DROP TABLE a"]));
        plan.push(Operation::new("second", ["CREATE TABLE b ()"]).reversed_by(["DROP TABLE b"]));

        let id = MigrationId::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "two");
        let parsed = MigrationFile::parse(&id.file_name("sql"), &write_migration(&id, &plan, None))
            .expect("parses");
        assert_eq!(parsed.up(), ["CREATE TABLE a ()", "CREATE TABLE b ()"]);
        assert_eq!(parsed.down(), ["DROP TABLE b", "DROP TABLE a"]);
    }

    #[test]
    fn an_irreversible_plan_writes_no_down_section() {
        let mut plan = Plan::empty(Backend::Postgres);
        plan.push(Operation::new("one way", ["ALTER TYPE t ADD VALUE 'x'"]));
        let id = MigrationId::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "one_way");
        let text = write_migration(&id, &plan, None);
        assert!(text.contains("-- moso:irreversible"), "{text}");
        assert!(!text.contains(DOWN_MARKER), "{text}");
    }

    #[test]
    fn writing_is_deterministic() {
        let mut plan = Plan::empty(Backend::Postgres);
        plan.push(Operation::new("x", ["SELECT 1"]).reversed_by(["SELECT 2"]));
        let id = MigrationId::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x");
        let first = write_migration(&id, &plan, Some("abc"));
        for _ in 0..4 {
            assert_eq!(write_migration(&id, &plan, Some("abc")), first);
        }
    }

    #[test]
    fn durations_parse_in_the_spellings_people_write() {
        assert_eq!(
            parse_duration("5s", "f", "k").expect("5s"),
            Duration::from_secs(5)
        );
        assert_eq!(
            parse_duration("500ms", "f", "k").expect("ms"),
            Duration::from_millis(500)
        );
        assert_eq!(
            parse_duration("2min", "f", "k").expect("min"),
            Duration::from_secs(120)
        );
        assert_eq!(
            parse_duration("60", "f", "k").expect("bare"),
            Duration::from_secs(60)
        );
        assert!(parse_duration("soon", "f", "k").is_err());
    }
}
