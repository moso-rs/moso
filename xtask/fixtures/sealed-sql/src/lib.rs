//! The same facade as `leaky-sql`, sealed properly — the other half of
//! `xtask check-sealed --self-test`.
//!
//! It offers the same capability: build a select statement, bind values, render
//! it for a dialect, report an error. It does it without naming
//! `fake_query_engine` anywhere a caller can see, so the engine underneath can
//! be swapped for another one in a patch release. That is exactly the promise
//! [ADR-0005] makes about `moso-sql`, written out at fixture scale.
//!
//! A checker with no false negatives is easy — flag everything. This crate is
//! what makes the gate keepable: it must come back clean.
//!
//! [ADR-0005]: ../../../../docs/adr/0005-sealed-sql-facade.md

#![forbid(unsafe_code)]

use fake_query_engine::{GenericBuilder, QueryBuilder, QueryValue, SelectStatement};

/// A select statement under construction. Opaque: the engine is a private field.
#[derive(Clone, Debug, Default)]
pub struct Select {
    statement: SelectStatement,
    bindings: Vec<Value>,
}

impl Select {
    /// An empty statement.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a column. Shape-stable: still a `Select`.
    #[must_use]
    pub fn column(mut self, name: &str) -> Self {
        self.statement = self.statement.column(name);
        self
    }

    /// Binds a parameter.
    #[must_use]
    pub fn bind(mut self, value: Value) -> Self {
        self.bindings.push(value);
        self
    }

    /// Renders the statement for a dialect.
    #[must_use]
    pub fn build(&self, dialect: Dialect) -> Sql {
        Sql {
            text: dialect.renderer().build(&self.statement),
            args: self.bindings.clone(),
        }
    }
}

/// A bound parameter — Moso's own type, converted at the boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// An integer.
    Int(i64),
    /// A string.
    Text(String),
}

impl Value {
    fn to_engine(&self) -> QueryValue {
        match self {
            Self::Int(value) => QueryValue::Int(*value),
            Self::Text(value) => QueryValue::Text(value.clone()),
        }
    }
}

/// Rendered SQL and its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sql {
    /// The statement text.
    pub text: String,
    /// The arguments, in order.
    pub args: Vec<Value>,
}

impl Sql {
    /// The arguments as the engine wants them. Private: the conversion happens
    /// here, not in the caller.
    fn engine_args(&self) -> Vec<QueryValue> {
        self.args.iter().map(Value::to_engine).collect()
    }

    /// How many arguments the statement binds.
    #[must_use]
    pub fn arity(&self) -> usize {
        self.engine_args().len()
    }
}

/// Which database the SQL is for. An enum rather than a trait, so no caller ever
/// has to satisfy a foreign bound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Dialect {
    /// The only one this fixture has.
    #[default]
    Generic,
}

impl Dialect {
    fn renderer(self) -> impl QueryBuilder {
        match self {
            Self::Generic => GenericBuilder,
        }
    }

    /// The dialect's name, for error messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Generic => "generic",
        }
    }
}

/// What can go wrong — Moso's own error, not the engine's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// What went wrong, in prose.
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_facade_still_builds_sql() {
        let sql = Select::new()
            .column("id")
            .bind(Value::Int(7))
            .build(Dialect::Generic);
        assert_eq!(sql.text, "SELECT id");
        assert_eq!(sql.arity(), 1);
        assert_eq!(Dialect::Generic.name(), "generic");
    }
}
