//! A stand-in for `sea-query`, so that `xtask check-sealed --self-test` has a
//! foreign crate to leak without depending on a real one.
//!
//! Everything here is deliberately shaped like the parts of a query builder
//! that are tempting to expose: a statement type, a value enum, a dialect trait
//! and an error. The self-test is only interesting if the fixture leaks the same
//! *kinds* of thing a careless `moso-sql` would.

#![forbid(unsafe_code)]

/// A statement under construction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectStatement {
    columns: Vec<String>,
}

impl SelectStatement {
    /// An empty statement.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a column and returns the statement, builder-style.
    #[must_use]
    pub fn column(mut self, name: &str) -> Self {
        self.columns.push(name.to_owned());
        self
    }

    /// Renders the statement.
    #[must_use]
    pub fn to_sql(&self) -> String {
        let columns = if self.columns.is_empty() {
            "*".to_owned()
        } else {
            self.columns.join(", ")
        };
        format!("SELECT {columns}")
    }
}

/// A bound parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryValue {
    /// An integer.
    Int(i64),
    /// A string.
    Text(String),
}

/// How a statement is rendered for a particular database.
pub trait QueryBuilder {
    /// Renders the statement.
    fn build(&self, statement: &SelectStatement) -> String;
}

/// The one dialect this stand-in knows.
#[derive(Clone, Copy, Debug, Default)]
pub struct GenericBuilder;

impl QueryBuilder for GenericBuilder {
    fn build(&self, statement: &SelectStatement) -> String {
        statement.to_sql()
    }
}

/// What can go wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildError {
    /// What went wrong.
    pub message: String,
}
