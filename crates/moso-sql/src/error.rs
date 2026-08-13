//! What can go wrong between a statement and its SQL.
//!
//! Every variant names the problem and offers a fix, per the style guide in
//! `docs/04-devex/41-diagnostics.md`. None of them is a database error: those
//! belong to the execution layer. These are the mistakes that are caught before
//! a single byte goes over the wire.

use crate::ident::IdentError;
use crate::value::ValueError;

/// A statement that could not be rendered.
///
/// ```
/// use moso_sql::Error;
///
/// let error = Error::Incomplete {
///     statement: "INSERT",
///     missing: "any rows to insert",
///     help: "call `.values(..)`, `.from_select(..)` or `.default_values()`",
/// };
/// assert!(error.to_string().contains("help:"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An identifier was rejected.
    #[error(transparent)]
    Ident(#[from] IdentError),

    /// A value was rejected.
    #[error(transparent)]
    Value(#[from] ValueError),

    /// The dialect has no such construct, and pretending otherwise would
    /// produce SQL that means something different.
    #[error(
        "{dialect} has no {construct}\n\
         help: {help}"
    )]
    Unsupported {
        /// The dialect that refused, as [`Dialect::name`](crate::Dialect::name)
        /// spells it.
        dialect: &'static str,
        /// The construct, in the words a user would search for.
        construct: &'static str,
        /// What to do instead. Always a concrete alternative.
        help: &'static str,
    },

    /// The statement is missing a clause it cannot be rendered without.
    #[error(
        "this {statement} is missing {missing}\n\
         help: {help}"
    )]
    Incomplete {
        /// The statement keyword.
        statement: &'static str,
        /// What is missing.
        missing: &'static str,
        /// The call that supplies it.
        help: &'static str,
    },

    /// An `INSERT` row does not have one value per column.
    #[error(
        "row {row} of this INSERT has {found} value(s) and the column list has {expected}\n\
         help: every row must line up with the columns passed to `.columns(..)`; a column with a \
         database default still needs an explicit `Expr::Default` in the row"
    )]
    RowArity {
        /// The zero-based index of the offending row.
        row: usize,
        /// How many columns the statement declares.
        expected: usize,
        /// How many values the row has.
        found: usize,
    },

    /// A raw fragment's placeholders and bound values do not match.
    #[error(
        "the raw SQL fragment has {expected} placeholder(s) and {found} bound value(s)\n\
         fragment: {fragment}\n\
         help: bind one value per `?`; write `??` for a literal question mark"
    )]
    RawArity {
        /// The fragment, so the mismatch is visible without a debugger.
        fragment: String,
        /// How many placeholders the fragment has.
        expected: usize,
        /// How many values were bound.
        found: usize,
    },

    /// The statement binds more parameters than the protocol allows.
    #[error(
        "this statement binds {found} parameters and {dialect} allows {limit}\n\
         help: insert in chunks — `rows.chunks({suggested})` — or send the rows as one array \
         parameter and `unnest` it"
    )]
    TooManyParameters {
        /// The dialect's name.
        dialect: &'static str,
        /// The protocol limit.
        limit: usize,
        /// How many parameters the statement would bind.
        found: usize,
        /// A chunk size that fits under the limit.
        suggested: usize,
    },

    /// A window frame, a lock, or another clause was given in a combination
    /// SQL does not allow.
    #[error(
        "{clause} is not valid here: {reason}\n\
         help: {help}"
    )]
    InvalidClause {
        /// The clause at fault.
        clause: &'static str,
        /// Why it is not valid.
        reason: &'static str,
        /// The fix.
        help: &'static str,
    },
}

impl Error {
    /// Builds an [`Error::Unsupported`].
    ///
    /// ```
    /// use moso_sql::Error;
    ///
    /// let error = Error::unsupported("SQLite", "ILIKE", "use `like` on lowered values");
    /// assert!(error.to_string().contains("ILIKE"));
    /// ```
    #[must_use]
    pub const fn unsupported(
        dialect: &'static str,
        construct: &'static str,
        help: &'static str,
    ) -> Self {
        Self::Unsupported {
            dialect,
            construct,
            help,
        }
    }

    /// Builds an [`Error::Incomplete`].
    ///
    /// ```
    /// use moso_sql::Error;
    ///
    /// let error = Error::incomplete("SELECT", "a projection", "call `.select_all()`");
    /// assert!(error.to_string().contains("projection"));
    /// ```
    #[must_use]
    pub const fn incomplete(
        statement: &'static str,
        missing: &'static str,
        help: &'static str,
    ) -> Self {
        Self::Incomplete {
            statement,
            missing,
            help,
        }
    }

    /// Whether this error means the *dialect* is the problem rather than the
    /// statement — the signal a caller uses to decide between "fix your query"
    /// and "this backend cannot do it".
    ///
    /// ```
    /// use moso_sql::Error;
    ///
    /// assert!(Error::unsupported("SQLite", "ILIKE", "lower both sides").is_dialect_gap());
    /// assert!(!Error::incomplete("SELECT", "a projection", "call `.select_all()`").is_dialect_gap());
    /// ```
    #[must_use]
    pub const fn is_dialect_gap(&self) -> bool {
        matches!(self, Self::Unsupported { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::Ident;

    #[test]
    fn every_message_offers_a_fix() {
        let errors = [
            Error::unsupported("SQLite", "ILIKE", "lower both sides and use LIKE"),
            Error::incomplete("INSERT", "any rows", "call `.values(..)`"),
            Error::RowArity {
                row: 1,
                expected: 3,
                found: 2,
            },
            Error::RawArity {
                fragment: "a = ?".into(),
                expected: 1,
                found: 0,
            },
            Error::TooManyParameters {
                dialect: "PostgreSQL",
                limit: 65_535,
                found: 70_000,
                suggested: 1_000,
            },
            Error::InvalidClause {
                clause: "FOR UPDATE",
                reason: "the query groups rows",
                help: "drop the lock, or lock the rows in a separate statement",
            },
        ];
        for error in errors {
            let message = error.to_string();
            assert!(
                message.contains("help:"),
                "every diagnostic must end in a fix: {message}"
            );
        }
    }

    #[test]
    fn an_identifier_error_passes_through_untouched() {
        let inner = Ident::new("").expect_err("empty");
        let error = Error::from(inner.clone());
        assert_eq!(error.to_string(), inner.to_string());
    }
}
