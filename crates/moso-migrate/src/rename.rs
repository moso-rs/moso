//! Rename detection.
//!
//! A schema diff cannot tell a rename from a drop-and-add: both look like
//! "`name` is gone, `full_name` is new". The difference is whether the data
//! survives, which is not a question a machine can answer. So the generator
//! finds the candidates and *asks*, and the operation table marks those rows ⚠
//! for exactly that reason.
//!
//! There are three ways to answer:
//!
//! | Oracle | Used by |
//! | --- | --- |
//! | [`Prompt`] | `moso db make-migration` on a terminal |
//! | [`Scripted`] | `moso db make-migration --rename users.name:full_name` |
//! | [`DropAndAdd`] | a caller that has decided nothing is ever a rename |
//! | [`RefuseToGuess`] | CI, where a wrong guess is a silent data loss |
//!
//! ```
//! use moso_migrate::rename::{Oracle, RenameAnswer, RenameQuestion, Scripted};
//!
//! let oracle = Scripted::parse(["users.name:full_name"])?;
//! let question = RenameQuestion::column("users", "name", "full_name");
//! assert_eq!(oracle.answer(&question)?, RenameAnswer::Rename);
//! # Ok::<(), moso_migrate::Error>(())
//! ```

use std::io::{BufRead, Write};
use std::sync::Mutex;

use crate::error::{Error, Result};

/// What kind of thing might have been renamed.
///
/// ```
/// assert_ne!(moso_migrate::rename::RenameKind::Table, moso_migrate::rename::RenameKind::Column);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameKind {
    /// A whole table, because an entity was renamed.
    Table,
    /// A column, because a field was renamed.
    Column,
    /// An index, which is never destructive either way but still changes the
    /// generated SQL from drop+create to a cheap `ALTER INDEX … RENAME`.
    Index,
}

impl RenameKind {
    /// The word to use in a question.
    ///
    /// ```
    /// assert_eq!(moso_migrate::rename::RenameKind::Column.noun(), "column");
    /// ```
    #[must_use]
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Column => "column",
            Self::Index => "index",
        }
    }
}

/// One candidate rename, phrased so that it can be printed at a human.
///
/// ```
/// use moso_migrate::rename::RenameQuestion;
///
/// let question = RenameQuestion::column("users", "name", "full_name");
/// assert!(question.prompt().contains("users.name"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameQuestion {
    kind: RenameKind,
    table: Option<String>,
    from: String,
    to: String,
}

impl RenameQuestion {
    /// A candidate table rename.
    ///
    /// ```
    /// use moso_migrate::rename::{RenameKind, RenameQuestion};
    ///
    /// assert_eq!(RenameQuestion::table("user", "users").kind(), RenameKind::Table);
    /// ```
    #[must_use]
    pub fn table(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            kind: RenameKind::Table,
            table: None,
            from: from.into(),
            to: to.into(),
        }
    }

    /// A candidate column rename.
    ///
    /// ```
    /// use moso_migrate::rename::RenameQuestion;
    ///
    /// assert_eq!(RenameQuestion::column("users", "name", "full_name").from(), "name");
    /// ```
    #[must_use]
    pub fn column(
        table: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Self {
        Self {
            kind: RenameKind::Column,
            table: Some(table.into()),
            from: from.into(),
            to: to.into(),
        }
    }

    /// A candidate index rename.
    ///
    /// ```
    /// use moso_migrate::rename::{RenameKind, RenameQuestion};
    ///
    /// assert_eq!(RenameQuestion::index("users", "a", "b").kind(), RenameKind::Index);
    /// ```
    #[must_use]
    pub fn index(table: impl Into<String>, from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            kind: RenameKind::Index,
            table: Some(table.into()),
            from: from.into(),
            to: to.into(),
        }
    }

    /// What kind of object.
    ///
    /// ```
    /// # use moso_migrate::rename::{RenameKind, RenameQuestion};
    /// assert_eq!(RenameQuestion::table("a", "b").kind(), RenameKind::Table);
    /// ```
    #[must_use]
    pub const fn kind(&self) -> RenameKind {
        self.kind
    }

    /// The table, for a column or an index.
    ///
    /// ```
    /// # use moso_migrate::rename::RenameQuestion;
    /// assert_eq!(RenameQuestion::table("a", "b").table_name(), None);
    /// ```
    #[must_use]
    pub fn table_name(&self) -> Option<&str> {
        self.table.as_deref()
    }

    /// The old name.
    ///
    /// ```
    /// # use moso_migrate::rename::RenameQuestion;
    /// assert_eq!(RenameQuestion::table("a", "b").from(), "a");
    /// ```
    #[must_use]
    pub fn from(&self) -> &str {
        &self.from
    }

    /// The new name.
    ///
    /// ```
    /// # use moso_migrate::rename::RenameQuestion;
    /// assert_eq!(RenameQuestion::table("a", "b").to(), "b");
    /// ```
    #[must_use]
    pub fn to(&self) -> &str {
        &self.to
    }

    /// The fully-qualified old name, which is what `--rename` takes on its
    /// left-hand side.
    ///
    /// ```
    /// # use moso_migrate::rename::RenameQuestion;
    /// assert_eq!(RenameQuestion::column("users", "name", "full_name").key(), "users.name");
    /// ```
    #[must_use]
    pub fn key(&self) -> String {
        match &self.table {
            Some(table) => format!("{table}.{}", self.from),
            None => self.from.clone(),
        }
    }

    /// The question, as it is printed.
    ///
    /// ```
    /// use moso_migrate::rename::RenameQuestion;
    ///
    /// let prompt = RenameQuestion::column("users", "name", "full_name").prompt();
    /// assert!(prompt.contains("rename"));
    /// assert!(prompt.contains("drop"));
    /// ```
    #[must_use]
    pub fn prompt(&self) -> String {
        let noun = self.kind.noun();
        format!(
            "did you rename the {noun} `{}` to `{}`, or drop `{}` and add `{}`?",
            self.key(),
            self.to,
            self.key(),
            self.to
        )
    }

    /// The `--rename` argument that answers this question without a terminal.
    ///
    /// ```
    /// use moso_migrate::rename::RenameQuestion;
    ///
    /// assert_eq!(
    ///     RenameQuestion::column("users", "name", "full_name").flag(),
    ///     "--rename users.name:full_name",
    /// );
    /// ```
    #[must_use]
    pub fn flag(&self) -> String {
        format!("--rename {}:{}", self.key(), self.to)
    }
}

/// The answer to one candidate rename.
///
/// ```
/// assert_ne!(
///     moso_migrate::rename::RenameAnswer::Rename,
///     moso_migrate::rename::RenameAnswer::DropAndAdd,
/// );
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RenameAnswer {
    /// It is the same object under a new name; the data stays.
    Rename,
    /// They are two different objects; the old one's data goes. The default,
    /// because it is the answer that does not silently keep a column the
    /// entities no longer describe.
    #[default]
    DropAndAdd,
}

/// Answers rename questions.
///
/// Implement it to plug in a different UI — an editor prompt, a web form, a
/// test double. The generator asks at most once per candidate.
///
/// ```
/// use moso_migrate::rename::{Oracle, RenameAnswer, RenameQuestion};
/// use moso_migrate::Result;
///
/// /// An oracle that says yes to everything, for a codebase where columns are
/// /// never deleted, only renamed.
/// struct AlwaysRename;
///
/// impl Oracle for AlwaysRename {
///     fn answer(&self, _question: &RenameQuestion) -> Result<RenameAnswer> {
///         Ok(RenameAnswer::Rename)
///     }
/// }
///
/// assert_eq!(
///     AlwaysRename.answer(&RenameQuestion::table("a", "b"))?,
///     RenameAnswer::Rename,
/// );
/// # Ok::<(), moso_migrate::Error>(())
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot answer a rename question",
    label = "not a rename oracle",
    note = "the migration generator has to ask whether a column was renamed or dropped-and-added, \
            because the two produce the same diff and only one keeps the data",
    note = "help: implement `fn answer(&self, question: &RenameQuestion) -> Result<RenameAnswer>` \
            for `{Self}`",
    note = "help: or pass one of the built-in oracles: `Prompt::stdio()`, \
            `Scripted::parse(..)`, `DropAndAdd` or `RefuseToGuess`"
)]
pub trait Oracle {
    /// Answers one question.
    ///
    /// # Errors
    ///
    /// [`Error::NeedsAnswer`] when the oracle cannot answer and the caller has
    /// to be told how to supply one.
    fn answer(&self, question: &RenameQuestion) -> Result<RenameAnswer>;
}

/// Treats every candidate as a drop and an add.
///
/// The conservative default in one sense — it never silently keeps a column
/// under a new name — and the *dangerous* one in another, because it drops
/// data. It exists for callers that have decided, not as a fallback.
///
/// ```
/// use moso_migrate::rename::{DropAndAdd, Oracle, RenameAnswer, RenameQuestion};
///
/// assert_eq!(
///     DropAndAdd.answer(&RenameQuestion::table("a", "b"))?,
///     RenameAnswer::DropAndAdd,
/// );
/// # Ok::<(), moso_migrate::Error>(())
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct DropAndAdd;

impl Oracle for DropAndAdd {
    fn answer(&self, _question: &RenameQuestion) -> Result<RenameAnswer> {
        Ok(RenameAnswer::DropAndAdd)
    }
}

/// Refuses to guess, and says how to answer.
///
/// This is what CI gets. A generator that guesses in CI produces a migration
/// nobody reviewed that either drops a column or does not, and the failure mode
/// is discovered in production.
///
/// ```
/// use moso_migrate::rename::{Oracle, RefuseToGuess, RenameQuestion};
///
/// let error = RefuseToGuess
///     .answer(&RenameQuestion::column("users", "name", "full_name"))
///     .expect_err("it refuses");
/// assert!(error.to_string().contains("--rename users.name:full_name"));
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct RefuseToGuess;

impl Oracle for RefuseToGuess {
    fn answer(&self, question: &RenameQuestion) -> Result<RenameAnswer> {
        Err(Error::NeedsAnswer {
            question: question.prompt(),
            flag: question.flag(),
        })
    }
}

/// Answers from a list of `old:new` pairs, as `--rename` supplies them.
///
/// A candidate that is not in the list is *refused* unless
/// [`Scripted::otherwise`] says what to do with it. That is the right default
/// for CI: "these are the renames I know about; tell me if there are others" is
/// safe, and "anything else is a drop" silently deletes a column.
///
/// ```
/// use moso_migrate::rename::{Oracle, RenameAnswer, RenameQuestion, Scripted};
///
/// let oracle = Scripted::parse(["users.name:full_name", "post:posts"])?
///     .otherwise(Some(RenameAnswer::DropAndAdd));
/// assert_eq!(
///     oracle.answer(&RenameQuestion::column("users", "name", "full_name"))?,
///     RenameAnswer::Rename,
/// );
/// assert_eq!(
///     oracle.answer(&RenameQuestion::column("users", "bio", "about"))?,
///     RenameAnswer::DropAndAdd,
/// );
/// # Ok::<(), moso_migrate::Error>(())
/// ```
#[derive(Clone, Debug, Default)]
pub struct Scripted {
    pairs: Vec<(String, String)>,
    otherwise: Option<RenameAnswer>,
}

impl Scripted {
    /// Parses `--rename` arguments of the form `old:new`.
    ///
    /// The left-hand side is `table.column` for a column and `table` for a
    /// table, matching what the question's `flag()` prints.
    ///
    /// # Errors
    ///
    /// [`Error::NeedsAnswer`] when an argument has no `:`, with the correct
    /// spelling in the help line.
    ///
    /// ```
    /// use moso_migrate::rename::Scripted;
    ///
    /// assert!(Scripted::parse(["users.name:full_name"]).is_ok());
    /// assert!(Scripted::parse(["users.name"]).is_err());
    /// ```
    pub fn parse<S: AsRef<str>>(arguments: impl IntoIterator<Item = S>) -> Result<Self> {
        let mut pairs = Vec::new();
        for argument in arguments {
            let argument = argument.as_ref();
            let (from, to) = argument.split_once(':').ok_or_else(|| Error::NeedsAnswer {
                question: format!("`{argument}` is not a rename"),
                flag: "--rename users.name:full_name".to_owned(),
            })?;
            if from.is_empty() || to.is_empty() {
                return Err(Error::NeedsAnswer {
                    question: format!("`{argument}` has an empty side"),
                    flag: "--rename users.name:full_name".to_owned(),
                });
            }
            pairs.push((from.to_owned(), to.to_owned()));
        }
        Ok(Self {
            pairs,
            otherwise: None,
        })
    }

    /// What to answer for a candidate that is not in the list.
    ///
    /// Passing `None` makes an unlisted candidate an error, which is the right
    /// setting for CI: it says "these are the renames I know about, tell me if
    /// there are others" rather than "anything else is a drop".
    ///
    /// ```
    /// use moso_migrate::rename::{Oracle, RenameQuestion, Scripted};
    ///
    /// let strict = Scripted::parse(["a:b"])?.otherwise(None);
    /// assert!(strict.answer(&RenameQuestion::table("c", "d")).is_err());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub fn otherwise(mut self, answer: Option<RenameAnswer>) -> Self {
        self.otherwise = answer;
        self
    }

    /// The pairs, as parsed.
    ///
    /// ```
    /// # use moso_migrate::rename::Scripted;
    /// assert_eq!(Scripted::parse(["a:b"])?.pairs().len(), 1);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    #[must_use]
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }
}

impl Oracle for Scripted {
    fn answer(&self, question: &RenameQuestion) -> Result<RenameAnswer> {
        let key = question.key();
        let matched = self
            .pairs
            .iter()
            .any(|(from, to)| (from == &key || from == question.from()) && to == question.to());
        if matched {
            return Ok(RenameAnswer::Rename);
        }
        // A pair whose left side matches but whose right side does not is a
        // deliberate statement that this is NOT the rename the user meant.
        let claimed_elsewhere = self
            .pairs
            .iter()
            .any(|(from, _)| from == &key || from == question.from());
        if claimed_elsewhere {
            return Ok(RenameAnswer::DropAndAdd);
        }
        self.otherwise.map_or_else(
            || {
                Err(Error::NeedsAnswer {
                    question: question.prompt(),
                    flag: question.flag(),
                })
            },
            Ok,
        )
    }
}

/// Asks on a terminal.
///
/// Generic over the reader and writer so it can be driven by a test rather than
/// by a person: `Prompt::new(&b"y\n"[..], Vec::new())` answers "yes" once.
///
/// ```
/// use moso_migrate::rename::{Oracle, Prompt, RenameAnswer, RenameQuestion};
///
/// let prompt = Prompt::new(&b"r\n"[..], Vec::new());
/// assert_eq!(
///     prompt.answer(&RenameQuestion::column("users", "name", "full_name"))?,
///     RenameAnswer::Rename,
/// );
/// # Ok::<(), moso_migrate::Error>(())
/// ```
#[derive(Debug)]
pub struct Prompt<R, W> {
    io: Mutex<(R, W)>,
}

impl<R: BufRead, W: Write> Prompt<R, W> {
    /// A prompt over any reader and writer.
    ///
    /// ```
    /// use moso_migrate::rename::Prompt;
    ///
    /// let prompt = Prompt::new(&b"d\n"[..], Vec::new());
    /// assert!(format!("{prompt:?}").contains("Prompt"));
    /// ```
    #[must_use]
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            io: Mutex::new((reader, writer)),
        }
    }
}

impl Prompt<std::io::BufReader<std::io::Stdin>, std::io::Stderr> {
    /// A prompt over the process's own terminal: reads standard input, writes
    /// standard **error**.
    ///
    /// Not standard output, deliberately. `moso db` drives an application's own
    /// binary and reads exactly one JSON document off its standard output, so a
    /// question printed there would arrive at the CLI as "answered with
    /// something that is not JSON". A prompt on standard error still reaches a
    /// terminal, and still reaches a person watching a pipeline's logs.
    ///
    /// ```
    /// let prompt = moso_migrate::rename::Prompt::stdio();
    /// assert!(format!("{prompt:?}").contains("Prompt"));
    /// ```
    #[must_use]
    pub fn stdio() -> Self {
        Self::new(std::io::BufReader::new(std::io::stdin()), std::io::stderr())
    }
}

impl<R: BufRead, W: Write> Oracle for Prompt<R, W> {
    fn answer(&self, question: &RenameQuestion) -> Result<RenameAnswer> {
        let mut guard = self.io.lock().map_err(|_| Error::NeedsAnswer {
            question: question.prompt(),
            flag: question.flag(),
        })?;
        let (reader, writer) = &mut *guard;

        let noun = question.kind().noun();
        let prompt = format!(
            "\n{} `{}` is gone and `{}` is new.\n  \
             [r] renamed  — keep the data, emit RENAME {}\n  \
             [d] dropped  — the old {noun} and its data go, the new one starts empty\n\
             r/d> ",
            capitalise(noun),
            question.key(),
            question.to(),
            noun.to_uppercase()
        );

        for _ in 0..3 {
            write!(writer, "{prompt}").map_err(|source| prompt_io(question, source))?;
            writer
                .flush()
                .map_err(|source| prompt_io(question, source))?;

            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .map_err(|source| prompt_io(question, source))?;
            if read == 0 {
                // No terminal. Say so with the flag rather than guessing.
                return Err(Error::NeedsAnswer {
                    question: question.prompt(),
                    flag: question.flag(),
                });
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "r" | "rename" | "renamed" | "y" | "yes" => return Ok(RenameAnswer::Rename),
                "d" | "drop" | "dropped" | "n" | "no" => return Ok(RenameAnswer::DropAndAdd),
                _ => {
                    let _ = writeln!(writer, "  please answer `r` or `d`.");
                }
            }
        }
        Err(Error::NeedsAnswer {
            question: question.prompt(),
            flag: question.flag(),
        })
    }
}

fn prompt_io(question: &RenameQuestion, source: std::io::Error) -> Error {
    Error::io(
        "prompting for",
        question.key(),
        "run `moso db make-migration` from a terminal, or pass the answer with `--rename`",
        source,
    )
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_matches_qualified_and_bare_names() {
        let oracle = Scripted::parse(["users.name:full_name", "post:posts"]).expect("parses");
        assert_eq!(
            oracle
                .answer(&RenameQuestion::column("users", "name", "full_name"))
                .expect("answers"),
            RenameAnswer::Rename
        );
        assert_eq!(
            oracle
                .answer(&RenameQuestion::table("post", "posts"))
                .expect("answers"),
            RenameAnswer::Rename
        );
    }

    #[test]
    fn scripted_says_no_when_the_left_side_is_claimed_for_something_else() {
        let oracle = Scripted::parse(["users.name:display_name"]).expect("parses");
        assert_eq!(
            oracle
                .answer(&RenameQuestion::column("users", "name", "full_name"))
                .expect("answers"),
            RenameAnswer::DropAndAdd
        );
    }

    #[test]
    fn scripted_refuses_an_unlisted_candidate_by_default() {
        let oracle = Scripted::parse(["a:b"]).expect("parses");
        let error = oracle
            .answer(&RenameQuestion::column("users", "bio", "about"))
            .expect_err("unlisted");
        assert!(
            error.to_string().contains("--rename users.bio:about"),
            "{error}"
        );
    }

    #[test]
    fn scripted_can_default_to_drop_and_add() {
        let oracle = Scripted::parse(["a:b"])
            .expect("parses")
            .otherwise(Some(RenameAnswer::DropAndAdd));
        assert_eq!(
            oracle
                .answer(&RenameQuestion::column("users", "bio", "about"))
                .expect("answers"),
            RenameAnswer::DropAndAdd
        );
    }

    #[test]
    fn parse_rejects_a_missing_colon_with_the_right_spelling() {
        let error = Scripted::parse(["users.name"]).expect_err("no colon");
        assert!(
            error.to_string().contains("users.name:full_name"),
            "{error}"
        );
        assert!(Scripted::parse([":b"]).is_err());
        assert!(Scripted::parse(["a:"]).is_err());
    }

    #[test]
    fn the_prompt_reads_r_and_d() {
        let question = RenameQuestion::column("users", "name", "full_name");

        let rename = Prompt::new(&b"r\n"[..], Vec::new());
        assert_eq!(
            rename.answer(&question).expect("answers"),
            RenameAnswer::Rename
        );

        let drop = Prompt::new(&b"d\n"[..], Vec::new());
        assert_eq!(
            drop.answer(&question).expect("answers"),
            RenameAnswer::DropAndAdd
        );

        let spelled_out = Prompt::new(&b"renamed\n"[..], Vec::new());
        assert_eq!(
            spelled_out.answer(&question).expect("answers"),
            RenameAnswer::Rename
        );
    }

    #[test]
    fn the_prompt_reasks_then_gives_up_with_the_flag() {
        let question = RenameQuestion::column("users", "name", "full_name");
        let prompt = Prompt::new(&b"what?\nhuh?\nnope?\n"[..], Vec::new());
        let error = prompt.answer(&question).expect_err("three bad answers");
        assert!(error.to_string().contains("--rename"), "{error}");
    }

    #[test]
    fn a_closed_input_is_not_a_guess() {
        let question = RenameQuestion::table("post", "posts");
        let prompt = Prompt::new(&b""[..], Vec::new());
        let error = prompt.answer(&question).expect_err("no terminal");
        assert!(error.to_string().contains("--rename post:posts"), "{error}");
    }

    #[test]
    fn the_prompt_prints_both_consequences() {
        let question = RenameQuestion::column("users", "name", "full_name");
        let mut output = Vec::new();
        {
            let prompt = Prompt::new(&b"r\n"[..], &mut output);
            prompt.answer(&question).expect("answers");
        }
        let text = String::from_utf8(output).expect("utf-8");
        assert!(text.contains("keep the data"), "{text}");
        assert!(text.contains("data go"), "{text}");
    }

    #[test]
    fn refuse_to_guess_names_the_flag() {
        let error = RefuseToGuess
            .answer(&RenameQuestion::table("post", "posts"))
            .expect_err("refuses");
        assert!(error.to_string().contains("--rename post:posts"), "{error}");
    }

    #[test]
    fn drop_and_add_never_asks() {
        assert_eq!(
            DropAndAdd
                .answer(&RenameQuestion::column("t", "a", "b"))
                .expect("answers"),
            RenameAnswer::DropAndAdd
        );
    }
}
