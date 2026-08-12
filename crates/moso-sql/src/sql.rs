//! The rendered statement: text plus the parameters that go with it.

use core::fmt;

use crate::value::Value;

/// A statement rendered for one dialect.
///
/// The text carries placeholders — `$1`, `$2` for PostgreSQL, `?` for SQLite —
/// and [`Sql::args`] holds the values in placeholder order. The two are handed
/// to the driver separately, which is why no value in Moso is ever formatted
/// into SQL text.
///
/// The fields are public because this is the boundary type: the execution
/// layer destructures it, and hiding it behind accessors would buy nothing.
///
/// ```
/// use moso_sql::{Sql, Value};
///
/// let sql = Sql::new("SELECT * FROM \"users\" WHERE \"id\" = $1", [Value::I64(7)]);
/// assert_eq!(sql.args.len(), 1);
/// assert!(sql.text.contains("$1"));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Sql {
    /// The statement text, with placeholders.
    pub text: String,
    /// The bound parameters, in placeholder order.
    pub args: Vec<Value>,
}

impl Sql {
    /// Pairs text with its parameters.
    ///
    /// ```
    /// use moso_sql::{Sql, Value};
    ///
    /// let sql = Sql::new("select ?", [Value::I32(1)]);
    /// assert_eq!(sql.args.len(), 1);
    /// ```
    #[must_use]
    pub fn new(text: impl Into<String>, args: impl IntoIterator<Item = Value>) -> Self {
        Self {
            text: text.into(),
            args: args.into_iter().collect(),
        }
    }

    /// A statement with no parameters.
    ///
    /// ```
    /// assert!(moso_sql::Sql::text_only("select 1").args.is_empty());
    /// ```
    #[must_use]
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            args: Vec::new(),
        }
    }

    /// The statement text.
    ///
    /// ```
    /// assert_eq!(moso_sql::Sql::text_only("select 1").as_str(), "select 1");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// How many parameters the statement binds.
    ///
    /// ```
    /// assert_eq!(moso_sql::Sql::text_only("select 1").arg_count(), 0);
    /// ```
    #[must_use]
    pub fn arg_count(&self) -> usize {
        self.args.len()
    }

    /// Splits the pair into its two halves, which is what a driver call
    /// wants.
    ///
    /// ```
    /// use moso_sql::{Sql, Value};
    ///
    /// let (text, args) = Sql::new("select ?", [Value::I32(1)]).into_parts();
    /// assert_eq!(text, "select ?");
    /// assert_eq!(args.len(), 1);
    /// ```
    #[must_use]
    pub fn into_parts(self) -> (String, Vec<Value>) {
        (self.text, self.args)
    }
}

/// Renders the statement text only.
///
/// The parameters are deliberately **not** shown: a `Display` that interpolated
/// them would be pasted into a terminal, and then into a bug report, and then
/// into a log aggregator, and one of those values will one day be a password.
///
/// ```
/// use moso_sql::{Sql, Value};
///
/// let sql = Sql::new("select ?", [Value::text("hunter2")]);
/// assert_eq!(sql.to_string(), "select ?");
/// ```
impl fmt::Display for Sql {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_never_shows_a_parameter() {
        let sql = Sql::new("select ?", [Value::text("hunter2")]);
        assert_eq!(sql.to_string(), "select ?");
        assert!(!sql.to_string().contains("hunter2"));
    }

    #[test]
    fn into_parts_round_trips() {
        let (text, args) = Sql::new("select ?", [Value::I32(1)]).into_parts();
        assert_eq!(Sql::new(text, args), Sql::new("select ?", [Value::I32(1)]));
    }
}
