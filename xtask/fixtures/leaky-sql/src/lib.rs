//! Every way a sealed facade can spring a leak, in one crate.
//!
//! `xtask check-sealed --self-test` requires that this crate produce at least
//! eight findings across at least six distinct positions, and that the findings
//! name `fake_query_engine`. If it ever passes the gate, the gate is broken —
//! which is the only claim a self-test like this can make, and the one that
//! matters: a checker with no false negatives on a known-bad input.
//!
//! Each item below is annotated with the position `check-sealed` should report.
//! Do not "fix" them.

#![forbid(unsafe_code)]

use fake_query_engine::{BuildError, GenericBuilder, QueryBuilder, QueryValue, SelectStatement};

/// Leak 1 — a re-export. `leaky_sql::QueryValue` *is* the foreign type.
pub use fake_query_engine::QueryValue as ReExportedValue;

/// Leak 2 — a public field whose type is foreign.
#[derive(Clone, Debug, Default)]
pub struct Select {
    /// Callers can take this apart and keep it.
    pub statement: SelectStatement,
}

/// Leak 3 — an alias whose target is foreign.
pub type Statement = SelectStatement;

impl Select {
    /// Leak 4 — a parameter of a foreign type.
    #[must_use]
    pub fn from_statement(statement: SelectStatement) -> Self {
        Self { statement }
    }

    /// Leak 5 — a return type that is foreign.
    #[must_use]
    pub fn into_statement(self) -> SelectStatement {
        self.statement
    }

    /// Leak 6 — a foreign trait in a bound, which forces callers to depend on
    /// the engine to call this at all.
    pub fn render<B: QueryBuilder>(&self, builder: &B) -> String {
        builder.build(&self.statement)
    }

    /// Leak 7 — a foreign type inside a container in the return position.
    #[must_use]
    pub fn bindings(&self) -> Vec<QueryValue> {
        Vec::new()
    }

    /// Leak 8 — a foreign error type in a `Result`.
    ///
    /// # Errors
    ///
    /// Never, but the signature says otherwise, which is the point.
    pub fn validate(&self) -> Result<(), BuildError> {
        Ok(())
    }
}

/// Leak 9 — a supertrait from another crate.
pub trait Dialect: QueryBuilder {
    /// Leak 10 — an associated type bounded by a foreign trait.
    type Renderer: QueryBuilder;

    /// The dialect's name.
    fn name(&self) -> &'static str;
}

/// A dialect that leaks its associated type's value as well.
#[derive(Clone, Copy, Debug, Default)]
pub struct Generic;

impl QueryBuilder for Generic {
    fn build(&self, statement: &SelectStatement) -> String {
        statement.to_sql()
    }
}

impl Dialect for Generic {
    /// Leak 11 — the value of the associated type is chosen here, and it is
    /// foreign.
    type Renderer = GenericBuilder;

    fn name(&self) -> &'static str {
        "generic"
    }
}

/// Leak 12 — a generic argument of an implemented trait.
impl From<SelectStatement> for Select {
    fn from(statement: SelectStatement) -> Self {
        Self { statement }
    }
}

/// A constant of a foreign type would be leak 13, if a query builder had one
/// that was constructible in a `const`. The eleven above are enough.
pub const DEFAULT_DIALECT: Generic = Generic;
