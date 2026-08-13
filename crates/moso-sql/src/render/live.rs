//! The generated SQL, run past the real parsers.
//!
//! A snapshot test proves the renderer is stable. It does not prove the text is
//! *SQL* — a missing space, a keyword in the wrong order or a clause PostgreSQL
//! spells differently produces a perfectly stable snapshot and a runtime error
//! on the first request. So every construct in the corpus below is handed to a
//! real server:
//!
//! * **PostgreSQL** through `psql`, against `DATABASE_URL`. Queries go through
//!   `PREPARE`, which parses, resolves every name and plans the statement while
//!   accepting the `$1` placeholders a rendered query carries. Schema changes
//!   run for real inside a temporary schema that is dropped afterwards.
//! * **SQLite** through the `sqlite3` binary, against a temporary file. Queries
//!   go through `EXPLAIN`, which prepares the statement.
//!
//! Both legs skip with a printed reason when the tool or the database is not
//! there, so the suite still passes on a machine with neither.

use std::process::Command;

use super::*;
use crate::ddl::{IndexTarget, TableConstraint};

// ── the corpus ──────────────────────────────────────────────────────────────

/// Which dialects a case is meaningful for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Both servers must accept it.
    Both,
    /// PostgreSQL only; SQLite is expected to refuse at build time.
    PostgresOnly,
    /// SQLite only.
    SqliteOnly,
}

/// How the statement is handed to the server.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A query: `PREPARE` on PostgreSQL, `EXPLAIN` on SQLite.
    Query,
    /// A schema change: executed.
    Schema,
    /// A schema change PostgreSQL refuses to run inside a transaction.
    NonTransactional,
}

/// One statement to put past a real parser.
struct Case {
    name: &'static str,
    statement: Statement,
    scope: Scope,
    kind: Kind,
}

impl Case {
    /// A statement both servers must accept.
    fn both(name: &'static str, statement: Statement) -> Self {
        Self {
            name,
            statement,
            scope: Scope::Both,
            kind: Kind::Query,
        }
    }

    /// A statement only PostgreSQL has.
    fn postgres(name: &'static str, statement: Statement) -> Self {
        Self {
            name,
            statement,
            scope: Scope::PostgresOnly,
            kind: Kind::Query,
        }
    }

    /// A statement only SQLite has.
    fn sqlite(name: &'static str, statement: Statement) -> Self {
        Self {
            name,
            statement,
            scope: Scope::SqliteOnly,
            kind: Kind::Query,
        }
    }

    /// Marks the case a schema change rather than a query.
    const fn schema(mut self) -> Self {
        self.kind = Kind::Schema;
        self
    }

    /// Marks the case a schema change that cannot run in a transaction.
    const fn non_transactional(mut self) -> Self {
        self.kind = Kind::NonTransactional;
        self
    }
}

/// A column of the fixture schema.
fn col(name: &'static str) -> Expr {
    Expr::col(Ident::from_static(name))
}

/// The tables everything below is written against.
///
/// Two versions, because the point is to prove the *generated* SQL parses, and
/// a fixture written in one dialect's types would not create on the other.
const POSTGRES_FIXTURE: &str = r#"
CREATE TABLE users (
    id          uuid PRIMARY KEY,
    email       text NOT NULL,
    name        text,
    is_admin    boolean NOT NULL DEFAULT false,
    age         integer,
    login_count integer NOT NULL DEFAULT 0,
    prefs       jsonb,
    tags        text[],
    score       numeric(10, 2),
    search      tsvector,
    deleted_at  timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE posts (
    id         bigint PRIMARY KEY,
    author_id  uuid NOT NULL REFERENCES users (id),
    title      text NOT NULL,
    body       text,
    published  boolean NOT NULL DEFAULT false,
    search     tsvector,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE post_stats (id bigint PRIMARY KEY, views bigint NOT NULL DEFAULT 0);
CREATE TABLE scratch (a integer, b text, c integer);
CREATE UNIQUE INDEX idx_users_email ON users (email);
-- The arbiter for the partial-index upsert below: an `ON CONFLICT` target only
-- matches a partial index when it repeats the index's `WHERE`.
CREATE UNIQUE INDEX idx_users_email_live ON users (email) WHERE deleted_at IS NULL;
"#;

/// The same fixture in SQLite's storage classes.
const SQLITE_FIXTURE: &str = r#"
CREATE TABLE users (
    id          text PRIMARY KEY,
    email       text NOT NULL,
    name        text,
    is_admin    integer NOT NULL DEFAULT 0,
    age         integer,
    login_count integer NOT NULL DEFAULT 0,
    prefs       text,
    tags        text,
    score       numeric,
    search      text,
    deleted_at  text,
    created_at  text NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE posts (
    id         integer PRIMARY KEY,
    author_id  text NOT NULL REFERENCES users (id),
    title      text NOT NULL,
    body       text,
    published  integer NOT NULL DEFAULT 0,
    search     text,
    created_at text NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE post_stats (id integer PRIMARY KEY, views integer NOT NULL DEFAULT 0);
CREATE TABLE scratch (a integer, b text, c integer);
CREATE UNIQUE INDEX idx_users_email ON users (email);
CREATE UNIQUE INDEX idx_users_email_live ON users (email) WHERE deleted_at IS NULL;
"#;

/// Every statement the live legs check.
#[allow(clippy::too_many_lines)]
fn corpus() -> Vec<Case> {
    let users = TableRef::from_static("users");
    let posts = TableRef::from_static("posts");
    let scratch = TableRef::from_static("scratch");

    let mut cases = vec![
        Case::both(
            "select all",
            Select::from_table(users.clone())
                .select_all()
                .into_statement(),
        ),
        Case::both(
            "filters, and-ed",
            Select::from_table(users.clone())
                .select_all()
                .filter(col("email").eq(Expr::value("a@example.com")))
                .filter(col("age").ge(Expr::value(18_i32)))
                .into_statement(),
        ),
        Case::both(
            "an or that must stay grouped",
            Select::from_table(users.clone())
                .select_all()
                .filter(col("age").lt(Expr::value(18_i32)) | col("age").is_null())
                .filter(col("is_admin").eq(Expr::value(false)))
                .into_statement(),
        ),
        Case::both(
            "arithmetic precedence",
            Select::from_table(users.clone())
                .select_expr(col("login_count") + col("age") * Expr::value(2_i32))
                .select_expr((col("login_count") + col("age")) * Expr::value(2_i32))
                .select_expr(col("login_count") - (col("age") - Expr::value(1_i32)))
                .into_statement(),
        ),
        Case::both(
            "null tests, between, in, like",
            Select::from_table(users.clone())
                .select_all()
                .filter(col("name").is_not_null())
                .filter(col("age").between(Expr::value(18_i32), Expr::value(99_i32)))
                .filter(col("age").not_between(Expr::value(1_i32), Expr::value(2_i32)))
                .filter(col("email").in_list([Expr::value("a"), Expr::value("b")]))
                .filter(col("email").not_in_list([Expr::value("c")]))
                .filter(col("name").like(Expr::value("A%")))
                .filter(col("name").not_like(Expr::value(r"100\%")).escape('\\'))
                .into_statement(),
        ),
        Case::both(
            "an empty in list is a constant",
            Select::from_table(users.clone())
                .select_all()
                .filter(col("email").in_list([]))
                .filter(col("email").not_in_list([]))
                .into_statement(),
        ),
        Case::both(
            "is distinct from",
            Select::from_table(users.clone())
                .select_all()
                .filter(col("name").is_distinct_from(col("email")))
                .filter(col("name").is_not_distinct_from(col("email")))
                .into_statement(),
        ),
        Case::both(
            "case",
            Select::from_table(users.clone())
                .select_expr(
                    crate::Case::new()
                        .when(col("age").ge(Expr::value(65_i32)), Expr::value("senior"))
                        .otherwise(Expr::value("other"))
                        .into_expr(),
                )
                .select_expr(
                    crate::Case::on(col("is_admin"))
                        .when(Expr::value(true), Expr::value("admin"))
                        .into_expr(),
                )
                .into_statement(),
        ),
        Case::both(
            "cast",
            Select::from_table(users.clone())
                .select_expr(col("age").cast(DataType::Text))
                .into_statement(),
        ),
        Case::both(
            "scalar functions",
            Select::from_table(users.clone())
                .select_expr(Function::Coalesce(vec![col("name"), col("email")]).into_expr())
                .select_expr(
                    Function::NullIf(Box::new(col("name")), Box::new(Expr::value(""))).into_expr(),
                )
                .select_expr(Function::Abs(Box::new(col("age"))).into_expr())
                .select_expr(
                    Function::Round {
                        operand: Box::new(col("score")),
                        decimals: Some(Box::new(Expr::value(1_i32))),
                    }
                    .into_expr(),
                )
                .select_expr(Function::Floor(Box::new(col("score"))).into_expr())
                .select_expr(Function::Ceil(Box::new(col("score"))).into_expr())
                .select_expr(Function::Lower(Box::new(col("email"))).into_expr())
                .select_expr(Function::Upper(Box::new(col("email"))).into_expr())
                .select_expr(Function::Length(Box::new(col("email"))).into_expr())
                .select_expr(
                    Function::Replace {
                        operand: Box::new(col("email")),
                        from: Box::new(Expr::value("a")),
                        to: Box::new(Expr::value("b")),
                    }
                    .into_expr(),
                )
                .select_expr(Function::Now.into_expr())
                .select_expr(Function::CurrentDate.into_expr())
                .select_expr(Function::CurrentTimestamp.into_expr())
                .select_expr(Function::Random.into_expr())
                .into_statement(),
        ),
        Case::both(
            "the functions each dialect spells differently",
            Select::from_table(users.clone())
                .select_expr(Function::Greatest(vec![col("age"), col("login_count")]).into_expr())
                .select_expr(Function::Least(vec![col("age"), col("login_count")]).into_expr())
                .select_expr(
                    Function::Trim {
                        operand: Box::new(col("email")),
                        mode: TrimMode::Leading,
                        characters: Some(Box::new(Expr::value("a"))),
                    }
                    .into_expr(),
                )
                .select_expr(
                    Function::Trim {
                        operand: Box::new(col("email")),
                        mode: TrimMode::Trailing,
                        characters: None,
                    }
                    .into_expr(),
                )
                .select_expr(
                    Function::Trim {
                        operand: Box::new(col("email")),
                        mode: TrimMode::Both,
                        characters: None,
                    }
                    .into_expr(),
                )
                .select_expr(
                    Function::Substring {
                        operand: Box::new(col("email")),
                        from: Some(Box::new(Expr::value(2_i32))),
                        length: Some(Box::new(Expr::value(3_i32))),
                    }
                    .into_expr(),
                )
                .select_expr(
                    Function::Substring {
                        operand: Box::new(col("email")),
                        from: None,
                        length: Some(Box::new(Expr::value(3_i32))),
                    }
                    .into_expr(),
                )
                .into_statement(),
        ),
        Case::both(
            "aggregates with distinct and filter",
            Select::from_table(posts.clone())
                .select_expr(Aggregate::count_star().into_expr())
                .select_expr(
                    Aggregate::new(AggregateFunc::Count, [col("author_id")])
                        .distinct()
                        .into_expr(),
                )
                .select_expr(Aggregate::count_star().filter(col("published")).into_expr())
                .select_expr(Aggregate::new(AggregateFunc::Min, [col("created_at")]).into_expr())
                .select_expr(Aggregate::new(AggregateFunc::Max, [col("created_at")]).into_expr())
                .group_by(col("author_id"))
                .having(Aggregate::count_star().into_expr().gt(Expr::value(1_i64)))
                .into_statement(),
        ),
        Case::both(
            "string and json aggregates",
            Select::from_table(posts.clone())
                .select_expr(
                    Aggregate::new(AggregateFunc::StringAgg, [col("title"), Expr::value(",")])
                        .order_by(OrderTerm::asc(col("title")))
                        .into_expr(),
                )
                .select_expr(Aggregate::new(AggregateFunc::JsonAgg, [col("title")]).into_expr())
                .select_expr(
                    Aggregate::new(AggregateFunc::JsonObjectAgg, [col("title"), col("body")])
                        .into_expr(),
                )
                .into_statement(),
        ),
        Case::both(
            "window functions and frames",
            Select::from_table(posts.clone())
                .select_expr(
                    WindowExpr::new(
                        WindowFunc::RowNumber,
                        [],
                        WindowSpec::new()
                            .partition_by(col("author_id"))
                            .order_by(OrderTerm::desc(col("created_at"))),
                    )
                    .into_expr(),
                )
                .select_expr(
                    WindowExpr::new(
                        WindowFunc::Aggregate(Box::new(Aggregate::new(
                            AggregateFunc::Count,
                            [col("id")],
                        ))),
                        [],
                        WindowSpec::new()
                            .order_by(OrderTerm::asc(col("created_at")))
                            .frame(
                                Frame::new(FrameUnits::Rows, FrameBound::UnboundedPreceding)
                                    .to(FrameBound::CurrentRow),
                            ),
                    )
                    .into_expr(),
                )
                .select_expr(
                    WindowExpr::new(
                        WindowFunc::Lag,
                        [col("title")],
                        WindowSpec::new().order_by(OrderTerm::asc(col("id"))),
                    )
                    .into_expr(),
                )
                .select_expr(
                    WindowExpr::over_named(WindowFunc::Rank, [], Ident::from_static("w"))
                        .into_expr(),
                )
                .window(
                    Ident::from_static("w"),
                    WindowSpec::new().order_by(OrderTerm::asc(col("id"))).frame(
                        Frame::new(FrameUnits::Groups, FrameBound::Preceding(1))
                            .to(FrameBound::Following(1))
                            .exclude(FrameExclusion::Ties),
                    ),
                )
                .into_statement(),
        ),
        Case::both(
            "joins",
            Select::from_table(posts.clone())
                .select_all()
                .inner_join(
                    FromItem::table_as(users.clone(), Ident::from_static("u")),
                    Expr::column(posts.column(Ident::from_static("author_id"))).eq(Expr::column(
                        ColumnRef::qualified(Ident::from_static("u"), Ident::from_static("id")),
                    )),
                )
                .left_join(
                    FromItem::table_as(posts.clone(), Ident::from_static("p2")),
                    Expr::column(ColumnRef::qualified(
                        Ident::from_static("p2"),
                        Ident::from_static("id"),
                    ))
                    .eq(Expr::column(posts.column(Ident::from_static("id")))),
                )
                .into_statement(),
        ),
        Case::both(
            "right, full and cross joins",
            Select::from_table(posts.clone())
                .select_all()
                .right_join(
                    FromItem::table_as(users.clone(), Ident::from_static("u")),
                    Expr::value(true).eq(Expr::value(true)),
                )
                .full_join(
                    FromItem::table_as(users.clone(), Ident::from_static("v")),
                    Expr::value(true).eq(Expr::value(true)),
                )
                .cross_join(FromItem::table_as(users.clone(), Ident::from_static("w")))
                .into_statement(),
        ),
        Case::both(
            "a join with USING",
            Select::from_table(posts.clone())
                .select_all()
                .join(Join::using(
                    JoinKind::Inner,
                    FromItem::table(TableRef::from_static("post_stats")),
                    [Ident::from_static("id")],
                ))
                .into_statement(),
        ),
        Case::both(
            "subquery predicates",
            Select::from_table(users.clone())
                .select_all()
                .filter(
                    col("id").in_subquery(
                        Select::from_table(posts.clone())
                            .select_column(ColumnRef::from_static("author_id")),
                    ),
                )
                .filter(Expr::exists(
                    Select::from_table(posts.clone())
                        .select_expr(Expr::value(1_i32))
                        .filter(
                            Expr::column(posts.column(Ident::from_static("author_id")))
                                .eq(Expr::column(users.column(Ident::from_static("id")))),
                        ),
                ))
                .select_expr(Expr::scalar(
                    Select::from_table(posts.clone())
                        .select_expr(Aggregate::count_star().into_expr()),
                ))
                .into_statement(),
        ),
        Case::both(
            "a subquery in the FROM clause",
            Select::new()
                .select_all()
                .from(FromItem::subquery(
                    Select::from_table(posts.clone()).select_all(),
                    Ident::from_static("recent"),
                ))
                .into_statement(),
        ),
        Case::both(
            "order by, nulls placement, limit and offset",
            Select::from_table(posts.clone())
                .select_all()
                .order_by(OrderTerm::desc(col("created_at")).nulls_last())
                .order_by(OrderTerm::asc(col("id")).nulls_first())
                .limit(20)
                .offset(40)
                .into_statement(),
        ),
        Case::both(
            "offset without a limit",
            Select::from_table(posts.clone())
                .select_all()
                .offset(40)
                .into_statement(),
        ),
        Case::both(
            "distinct",
            Select::from_table(posts.clone())
                .select_column(ColumnRef::from_static("author_id"))
                .distinct()
                .into_statement(),
        ),
        Case::both(
            "set operations",
            Select::from_table(posts.clone())
                .select_column(ColumnRef::from_static("id"))
                .union(
                    Select::from_table(posts.clone()).select_column(ColumnRef::from_static("id")),
                )
                .union_all(
                    Select::from_table(posts.clone()).select_column(ColumnRef::from_static("id")),
                )
                .intersect(
                    Select::from_table(posts.clone()).select_column(ColumnRef::from_static("id")),
                )
                .except(
                    Select::from_table(posts.clone()).select_column(ColumnRef::from_static("id")),
                )
                .order_by(OrderTerm::asc(col("id")))
                .limit(5)
                .into_statement(),
        ),
        Case::both(
            "a common table expression",
            Select::from_table(TableRef::from_static("recent"))
                .select_all()
                .with(
                    Cte::new(
                        Ident::from_static("recent"),
                        Select::from_table(posts.clone()).select_all().limit(10),
                    )
                    .materialized(false),
                )
                .into_statement(),
        ),
        Case::both(
            "a recursive common table expression",
            Select::from_table(TableRef::from_static("counter"))
                .select_all()
                .with(
                    Cte::new(
                        Ident::from_static("counter"),
                        Select::new()
                            // The casts are the caller's job, not the
                            // renderer's: `SELECT $1` has no context a server
                            // could infer a type from.
                            .select_expr_as(
                                Expr::value(1_i32).cast(DataType::Integer),
                                Ident::from_static("n"),
                            )
                            .union_all(
                                Select::from_table(TableRef::from_static("counter"))
                                    .select_expr(
                                        col("n") + Expr::value(1_i32).cast(DataType::Integer),
                                    )
                                    .filter(
                                        col("n").lt(Expr::value(5_i32).cast(DataType::Integer)),
                                    ),
                            ),
                    )
                    .columns([Ident::from_static("n")]),
                )
                .recursive(true)
                .into_statement(),
        ),
        Case::both(
            "an insert with several rows",
            Insert::into_table(scratch.clone())
                .columns([Ident::from_static("a"), Ident::from_static("b")])
                .values([Expr::value(1_i32), Expr::value("x")])
                .values([Expr::value(2_i32), Expr::value("y")])
                .into_statement(),
        ),
        Case::both(
            "insert … default values",
            Insert::into_table(scratch.clone())
                .default_values()
                .into_statement(),
        ),
        Case::both(
            "insert … select",
            Insert::into_table(scratch.clone())
                .columns([Ident::from_static("a")])
                .from_select(
                    Select::from_table(scratch.clone()).select_column(ColumnRef::from_static("a")),
                )
                .into_statement(),
        ),
        Case::both(
            "an upsert that returns",
            Insert::into_table(users.clone())
                .columns([Ident::from_static("id"), Ident::from_static("email")])
                .values([Expr::value("x"), Expr::value("a@example.com")])
                .on_conflict(
                    OnConflict::columns([Ident::from_static("email")])
                        .do_update_columns([Ident::from_static("id")])
                        .update_where(col("users").is_not_null()),
                )
                .returning(Returning::All)
                .into_statement(),
        ),
        Case::both(
            "an upsert that targets a partial unique index",
            Insert::into_table(users.clone())
                .columns([Ident::from_static("id"), Ident::from_static("email")])
                .values([Expr::value("x"), Expr::value("a@example.com")])
                .on_conflict(
                    OnConflict::columns([Ident::from_static("email")])
                        .target_where(col("deleted_at").is_null())
                        .do_update_columns([Ident::from_static("name")]),
                )
                .into_statement(),
        ),
        Case::both(
            "on conflict do nothing",
            Insert::into_table(users.clone())
                .columns([Ident::from_static("id"), Ident::from_static("email")])
                .values([Expr::value("x"), Expr::value("a@example.com")])
                .on_conflict(OnConflict::any().do_nothing())
                .into_statement(),
        ),
        Case::both(
            "an atomic increment that returns",
            Update::table(users.clone())
                .set_with(Ident::from_static("login_count"), |current| {
                    current.plus(Expr::value(1_i32))
                })
                .filter(col("email").eq(Expr::value("a@example.com")))
                .returning(Returning::columns([ColumnRef::from_static("login_count")]))
                .into_statement(),
        ),
        Case::both(
            "a delete that returns",
            Delete::from_table(scratch.clone())
                .filter(col("a").eq(Expr::value(1_i32)))
                .returning(Returning::All)
                .into_statement(),
        ),
        Case::both(
            "a raw fragment with its own parameters",
            Select::from_table(users.clone())
                .select_all()
                .filter(Expr::raw(RawExpr::new("length(email) > ?").bind(3_i32)))
                .into_statement(),
        ),
        Case::both(
            "concat and concat_ws, which SQLite has spelled this way since 3.44",
            Select::from_table(users.clone())
                .select_expr(Function::Concat(vec![col("email"), col("name")]).into_expr())
                .select_expr(
                    Function::ConcatWs {
                        separator: Box::new(Expr::value(" ")),
                        items: vec![col("email"), col("name")],
                    }
                    .into_expr(),
                )
                .into_statement(),
        ),
        Case::both(
            "an explicitly grouped expression and a NOT over a tree",
            Select::from_table(users.clone())
                .select_all()
                .filter(
                    !(col("is_admin").eq(Expr::value(true))
                        & (col("age").is_null() | col("age").gt(Expr::value(1_i32)))),
                )
                .filter(col("age").nested().ge(Expr::value(0_i32)))
                .into_statement(),
        ),
        Case::both(
            "a tuple comparison, the shape keyset pagination uses",
            Select::from_table(posts.clone())
                .select_all()
                .filter(Expr::tuple([col("created_at"), col("id")]).lt(Expr::tuple([
                    Expr::value("2026-01-01T00:00:00Z").cast(DataType::Timestamp {
                        with_time_zone: true,
                    }),
                    Expr::value(1_i64),
                ])))
                .into_statement(),
        ),
    ];

    // ── PostgreSQL only ─────────────────────────────────────────────────────
    cases.extend([
        Case::postgres(
            "distinct on",
            Select::from_table(posts.clone())
                .select_all()
                .distinct_on([col("author_id")])
                .order_by(OrderTerm::asc(col("author_id")))
                .order_by(OrderTerm::desc(col("created_at")))
                .into_statement(),
        ),
        Case::postgres(
            "the job-queue lock",
            Select::from_table(posts.clone())
                .select_all()
                .filter(col("published").eq(Expr::value(false)))
                .order_by(OrderTerm::asc(col("created_at")))
                .limit(1)
                .lock(
                    Lock::new(LockStrength::Update)
                        .of(posts.clone())
                        .skip_locked(),
                )
                .into_statement(),
        ),
        Case::postgres(
            "for share, nowait",
            Select::from_table(posts.clone())
                .select_all()
                .lock(Lock::new(LockStrength::Share).nowait())
                .into_statement(),
        ),
        Case::postgres(
            "a lateral join",
            Select::from_table(users.clone())
                .select_all()
                .join(Join::new(
                    JoinKind::Left,
                    FromItem::lateral(
                        Select::from_table(posts.clone())
                            .select_all()
                            .filter(
                                Expr::column(posts.column(Ident::from_static("author_id")))
                                    .eq(Expr::column(users.column(Ident::from_static("id")))),
                            )
                            .limit(3),
                        Ident::from_static("recent"),
                    ),
                    Expr::value(true).eq(Expr::value(true)),
                ))
                .into_statement(),
        ),
        Case::postgres(
            "a VALUES list as a table",
            Select::new()
                .select_all()
                .from(FromItem::values(
                    [
                        vec![Expr::value(1_i32), Expr::value("a")],
                        vec![Expr::value(2_i32), Expr::value("b")],
                    ],
                    Ident::from_static("v"),
                    [Ident::from_static("id"), Ident::from_static("name")],
                ))
                .into_statement(),
        ),
        Case::postgres(
            "the jsonb operators",
            Select::from_table(users.clone())
                .select_expr(col("prefs").json(JsonOp::Get, Expr::value("theme")))
                .select_expr(col("prefs").json(JsonOp::GetText, Expr::value("theme")))
                .select_expr(col("prefs").json(JsonOp::GetPath, Expr::value(Array::of(["a", "b"]))))
                .select_expr(
                    col("prefs").json(JsonOp::GetPathText, Expr::value(Array::of(["a", "b"]))),
                )
                .select_expr(
                    col("prefs").json(JsonOp::Contains, Expr::value("{}").cast(DataType::JsonB)),
                )
                .select_expr(
                    col("prefs").json(JsonOp::ContainedBy, Expr::value("{}").cast(DataType::JsonB)),
                )
                .select_expr(col("prefs").json(JsonOp::HasKey, Expr::value("theme")))
                .select_expr(col("prefs").json(JsonOp::HasAnyKey, Expr::value(Array::of(["a"]))))
                .select_expr(col("prefs").json(JsonOp::HasAllKeys, Expr::value(Array::of(["a"]))))
                .select_expr(
                    col("prefs").json(JsonOp::Concat, Expr::value("{}").cast(DataType::JsonB)),
                )
                .select_expr(col("prefs").json(JsonOp::Remove, Expr::value("theme")))
                .select_expr(col("prefs").json(JsonOp::RemovePath, Expr::value(Array::of(["a"]))))
                .into_statement(),
        ),
        Case::postgres(
            "the array operators and a quantified comparison",
            Select::from_table(users.clone())
                .select_all()
                .filter(col("tags").binary(BinOp::ArrayContains, Expr::value(Array::of(["rust"]))))
                .filter(
                    col("tags").binary(BinOp::ArrayContainedBy, Expr::value(Array::of(["rust"]))),
                )
                .filter(col("tags").binary(BinOp::ArrayOverlaps, Expr::value(Array::of(["rust"]))))
                .filter(col("email").any(BinOp::Eq, Expr::value(Array::of(["a", "b"]))))
                .filter(col("age").all(BinOp::Gt, Expr::value(Array::of([1_i32]))))
                .into_statement(),
        ),
        Case::postgres(
            "full-text search, ranking and highlighting",
            Select::from_table(posts.clone())
                .select_expr(
                    Function::TsRank {
                        vector: Box::new(Expr::Function(Function::ToTsVector {
                            config: Some(Ident::from_static("english")),
                            document: Box::new(col("body")),
                        })),
                        query: Box::new(Expr::Function(Function::ToTsQuery {
                            config: Some(Ident::from_static("english")),
                            query: TextQuery::Websearch("rust orm".to_owned()),
                        })),
                        normalization: Some(2),
                    }
                    .into_expr(),
                )
                .select_expr(
                    Function::TsHeadline {
                        config: Some(Ident::from_static("english")),
                        document: Box::new(col("body")),
                        query: Box::new(Expr::Function(Function::ToTsQuery {
                            config: Some(Ident::from_static("english")),
                            query: TextQuery::Plain("rust".to_owned()),
                        })),
                        options: Some("MaxWords=20".to_owned()),
                    }
                    .into_expr(),
                )
                .filter(Expr::text_match(
                    col("body"),
                    TextQuery::Websearch("rust orm".to_owned()),
                    Some(Ident::from_static("english")),
                ))
                .filter(Expr::text_match_vector(
                    col("search"),
                    TextQuery::Phrase("rust orm".to_owned()),
                    None,
                ))
                .into_statement(),
        ),
        Case::postgres(
            "the regular-expression operators",
            Select::from_table(users.clone())
                .select_all()
                .filter(col("email").binary(BinOp::Regex, Expr::value("^a")))
                .filter(col("email").binary(BinOp::RegexCaseInsensitive, Expr::value("^a")))
                .filter(col("email").binary(BinOp::NotRegex, Expr::value("^b")))
                .filter(col("email").binary(BinOp::NotRegexCaseInsensitive, Expr::value("^b")))
                .into_statement(),
        ),
        Case::both(
            "ilike, which SQLite lowers on both sides",
            Select::from_table(users.clone())
                .select_all()
                .filter(col("email").ilike(Expr::value("A%")))
                .filter(col("name").not_ilike(Expr::value("B%")))
                .into_statement(),
        ),
        Case::postgres(
            "exponentiation and bitwise exclusive or",
            Select::from_table(users.clone())
                .select_expr(col("age").binary(BinOp::Exp, Expr::value(2_i32)))
                .select_expr(col("age").binary(BinOp::BitXor, Expr::value(2_i32)))
                .into_statement(),
        ),
        Case::postgres(
            "an array constructor",
            Select::from_table(users.clone())
                .select_expr(Expr::array([Expr::value("a"), Expr::value("b")]))
                .into_statement(),
        ),
        Case::both(
            "an update with a from clause",
            Update::table(users.clone())
                .alias(Ident::from_static("u"))
                .set(Ident::from_static("login_count"), Expr::value(0_i32))
                .from(FromItem::table_as(posts.clone(), Ident::from_static("p")))
                .filter(
                    Expr::column(ColumnRef::qualified(
                        Ident::from_static("p"),
                        Ident::from_static("author_id"),
                    ))
                    .eq(Expr::column(ColumnRef::qualified(
                        Ident::from_static("u"),
                        Ident::from_static("id"),
                    ))),
                )
                .into_statement(),
        ),
        Case::postgres(
            "a delete with a using clause",
            Delete::from_table(posts.clone())
                .alias(Ident::from_static("p"))
                .using(FromItem::table_as(users.clone(), Ident::from_static("u")))
                .filter(
                    Expr::column(ColumnRef::qualified(
                        Ident::from_static("p"),
                        Ident::from_static("author_id"),
                    ))
                    .eq(Expr::column(ColumnRef::qualified(
                        Ident::from_static("u"),
                        Ident::from_static("id"),
                    ))),
                )
                .into_statement(),
        ),
        Case::postgres(
            "a data-modifying common table expression",
            Insert::into_table(scratch.clone())
                .columns([Ident::from_static("a")])
                .from_select(
                    Select::from_table(TableRef::from_static("gone"))
                        .select_column(ColumnRef::from_static("a")),
                )
                .with(Cte::from_statement(
                    Ident::from_static("gone"),
                    Delete::from_table(scratch.clone())
                        .filter(col("a").eq(Expr::value(9_i32)))
                        .returning(Returning::All)
                        .into_statement(),
                ))
                .into_statement(),
        ),
        Case::postgres(
            "a set-returning function in the FROM clause",
            Select::new()
                .select_all()
                .from(FromItem::function(
                    Function::custom(
                        Ident::from_static("unnest"),
                        [Expr::value(Array::of(["a", "b"]))
                            .cast(DataType::array_of(DataType::Text))],
                    ),
                    Some(Ident::from_static("u")),
                ))
                .into_statement(),
        ),
    ]);

    // ── SQLite only ─────────────────────────────────────────────────────────
    cases.push(Case::sqlite(
        "the lowered ilike and the two json accessors",
        Select::from_table(users.clone())
            .select_expr(col("prefs").json(JsonOp::Get, Expr::value("theme")))
            .select_expr(col("prefs").json(JsonOp::GetText, Expr::value("theme")))
            .filter(col("email").ilike(Expr::value("A%")))
            .filter(col("name").not_ilike(Expr::value("B%")))
            .into_statement(),
    ));

    // ── schema changes ──────────────────────────────────────────────────────
    cases.extend([
        Case::both(
            "create table",
            Ddl::CreateTable(
                CreateTable::new(TableRef::from_static("made"))
                    .if_not_exists()
                    .column(ColumnSpec::new(Ident::from_static("id"), DataType::BigInt).not_null())
                    .column(
                        ColumnSpec::new(Ident::from_static("email"), DataType::Text)
                            .not_null()
                            .unique(),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("flag"), DataType::Boolean)
                            .not_null()
                            .default(Expr::value(false)),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("label"), DataType::Text)
                            .default(Expr::value("it's fine")),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("n"), DataType::Integer)
                            .check(col("n").ge(Expr::value(0_i32))),
                    )
                    .column(ColumnSpec::new(
                        Ident::from_static("owner_id"),
                        DataType::BigInt,
                    ))
                    .constraint(TableConstraint::primary_key(
                        Some(Ident::from_static("made_pkey")),
                        [Ident::from_static("id")],
                    ))
                    .constraint(TableConstraint::unique(
                        Some(Ident::from_static("made_email_n_key")),
                        [Ident::from_static("email"), Ident::from_static("n")],
                    ))
                    .constraint(TableConstraint::check(
                        Some(Ident::from_static("made_n_upper_check")),
                        col("n").lt(Expr::value(1_000_i32)),
                    )),
            )
            .into_statement(),
        )
        .schema(),
        Case::both(
            "every literal a DDL default can carry",
            Ddl::CreateTable(
                CreateTable::new(TableRef::from_static("literals"))
                    .column(
                        ColumnSpec::new(Ident::from_static("b"), DataType::Boolean)
                            .default(Expr::value(true)),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("i"), DataType::BigInt)
                            .default(Expr::value(-7_i64)),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("f"), DataType::DoublePrecision)
                            .default(Expr::value(1.0_f64)),
                    )
                    .column(
                        ColumnSpec::new(
                            Ident::from_static("d"),
                            DataType::Numeric {
                                precision: Some(10),
                                scale: Some(2),
                            },
                        )
                        .default(Expr::bound(Value::Decimal(
                            crate::Decimal::new(1999, 2).expect("in range"),
                        ))),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("t"), DataType::Text)
                            .default(Expr::value("it's fine")),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("bin"), DataType::Bytea)
                            .default(Expr::value(vec![0xDE_u8, 0xAD])),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("day"), DataType::Date).default(
                            Expr::bound(Value::Date(crate::Date::new(2026, 7, 30).expect("valid"))),
                        ),
                    )
                    .column(
                        ColumnSpec::new(
                            Ident::from_static("at"),
                            DataType::Timestamp {
                                with_time_zone: true,
                            },
                        )
                        .default(Expr::bound(Value::Timestamp(
                            Timestamp::new(1_769_000_000, 0).expect("valid"),
                        ))),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("made"), DataType::Text)
                            .default(Expr::Function(Function::Now)),
                    ),
            )
            .into_statement(),
        )
        .schema(),
        Case::postgres(
            "the literals only PostgreSQL has",
            Ddl::CreateTable(
                CreateTable::new(TableRef::from_static("pg_literals"))
                    .column(
                        ColumnSpec::new(Ident::from_static("gap"), DataType::Interval).default(
                            Expr::bound(Value::Interval(crate::Interval::new(1, 2, 3_500_000))),
                        ),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("id"), DataType::Uuid)
                            .default(Expr::bound(Value::Uuid(crate::Uuid::NIL))),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("doc"), DataType::JsonB).default(
                            Expr::bound(Value::Json(
                                crate::Json::parse(r#"{"a": 1}"#).expect("valid"),
                            )),
                        ),
                    )
                    .column(
                        ColumnSpec::new(
                            Ident::from_static("tags"),
                            DataType::array_of(DataType::Text),
                        )
                        .default(Expr::bound(Value::Array(Array::of(["a", "b"])))),
                    )
                    .column(
                        ColumnSpec::new(
                            Ident::from_static("empty"),
                            DataType::array_of(DataType::Text),
                        )
                        .default(Expr::bound(Value::Array(Array::empty(ValueKind::Text)))),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("nan"), DataType::DoublePrecision)
                            .default(Expr::value(f64::NAN)),
                    ),
            )
            .into_statement(),
        )
        .schema(),
        Case::both(
            "create index",
            Ddl::CreateIndex(
                CreateIndex::new(
                    Ident::from_static("idx_scratch_a"),
                    TableRef::from_static("scratch"),
                    [
                        IndexTarget::column(Ident::from_static("a")),
                        IndexTarget::expr(Expr::Function(Function::Lower(Box::new(col("b")))))
                            .order(Order::Desc),
                    ],
                )
                .if_not_exists()
                .where_(col("a").is_not_null()),
            )
            .into_statement(),
        )
        .schema(),
        Case::both(
            "alter table: add and drop a column, rename one",
            Ddl::AlterTable(
                AlterTable::new(TableRef::from_static("scratch"))
                    .add_column(ColumnSpec::new(Ident::from_static("added"), DataType::Text))
                    .drop_column(Ident::from_static("c"))
                    .action(AlterTableAction::RenameColumn {
                        from: Ident::from_static("b"),
                        to: Ident::from_static("renamed"),
                    }),
            )
            .into_statement(),
        )
        .schema(),
        Case::both(
            "truncate",
            Ddl::Truncate(Truncate::new([scratch.clone()])).into_statement(),
        )
        .schema(),
        Case::both(
            "drop index",
            Ddl::DropIndex(DropIndex::new(Ident::from_static("idx_scratch_a")).if_exists())
                .into_statement(),
        )
        .schema(),
        Case::both(
            "rename table",
            Ddl::RenameTable(RenameTable::new(
                TableRef::from_static("made"),
                Ident::from_static("made2"),
            ))
            .into_statement(),
        )
        .schema(),
        Case::both(
            "drop table",
            Ddl::DropTable(DropTable::new([TableRef::from_static("made2")]).if_exists())
                .into_statement(),
        )
        .schema(),
        Case::postgres(
            "create table with everything PostgreSQL has",
            Ddl::CreateTable(
                CreateTable::new(TableRef::from_static("full"))
                    .comment("Everything at once.")
                    .column(
                        ColumnSpec::new(Ident::from_static("id"), DataType::BigInt)
                            .not_null()
                            .identity(Identity::Always)
                            .primary_key()
                            .comment("The key."),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("title"), DataType::Text)
                            .not_null()
                            .collate(Ident::from_static("C")),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("search"), DataType::TsVector)
                            .generated(Generated::stored(Expr::Function(Function::ToTsVector {
                                config: Some(Ident::from_static("english")),
                                document: Box::new(col("title")),
                            }))),
                    )
                    .column(
                        ColumnSpec::new(Ident::from_static("owner"), DataType::Uuid).references(
                            ForeignKey::new(
                                Some(Ident::from_static("full_owner_fk")),
                                [Ident::from_static("owner")],
                                users.clone(),
                                [Ident::from_static("id")],
                            )
                            .on_delete(ReferentialAction::Cascade)
                            .on_update(ReferentialAction::Restrict)
                            .deferrable(true),
                        ),
                    )
                    .column(ColumnSpec::new(
                        Ident::from_static("tags"),
                        DataType::array_of(DataType::Text),
                    ))
                    .constraint(TableConstraint::Unique {
                        name: Some(Ident::from_static("full_title_key")),
                        columns: vec![Ident::from_static("title")],
                        nulls_not_distinct: true,
                    }),
            )
            .into_statement(),
        )
        .schema(),
        Case::postgres(
            "alter table: the zero-downtime constraint dance",
            Ddl::AlterTable(
                AlterTable::new(TableRef::from_static("scratch"))
                    .action(AlterTableAction::AddConstraint(TableConstraint::Check {
                        name: Some(Ident::from_static("scratch_a_positive")),
                        expr: col("a").ge(Expr::value(0_i32)),
                        not_valid: true,
                    }))
                    .action(AlterTableAction::ValidateConstraint(Ident::from_static(
                        "scratch_a_positive",
                    )))
                    .action(AlterTableAction::SetDefault {
                        name: Ident::from_static("a"),
                        value: Expr::value(0_i32),
                    })
                    .action(AlterTableAction::SetNotNull(Ident::from_static("a")))
                    .action(AlterTableAction::AlterColumnType {
                        name: Ident::from_static("a"),
                        data_type: DataType::BigInt,
                        using: Some(col("a").cast(DataType::BigInt)),
                        lossy: false,
                    }),
            )
            .into_statement(),
        )
        .schema(),
        Case::postgres(
            "add unique using index",
            Ddl::AlterTable(AlterTable::new(users.clone()).action(
                AlterTableAction::AddUniqueUsingIndex {
                    name: Some(Ident::from_static("users_email_uq")),
                    index: Ident::from_static("idx_users_email"),
                },
            ))
            .into_statement(),
        )
        .schema(),
        Case::postgres(
            "an enum type, altered and dropped",
            Ddl::CreateType(CreateType::new(
                TypeRef::from_static("mood"),
                TypeBody::enumeration(["sad", "ok", "it's great"]),
            ))
            .into_statement(),
        )
        .schema(),
        Case::postgres(
            "alter type … add value",
            Ddl::AlterType(AlterType::new(
                TypeRef::from_static("mood"),
                AlterTypeAction::add_value("elated"),
            ))
            .into_statement(),
        )
        .schema(),
        Case::postgres(
            "alter type … rename value",
            Ddl::AlterType(AlterType::new(
                TypeRef::from_static("mood"),
                AlterTypeAction::RenameValue {
                    from: "ok".to_owned(),
                    to: "fine".to_owned(),
                },
            ))
            .into_statement(),
        )
        .schema(),
        Case::postgres(
            "drop type",
            Ddl::DropType(
                DropType::new(TypeRef::from_static("mood"))
                    .if_exists()
                    .cascade(),
            )
            .into_statement(),
        )
        .schema(),
        Case::postgres(
            "a partitioned table and a partition attached to it",
            Ddl::CreateTable(
                CreateTable::new(TableRef::from_static("events"))
                    .column(
                        ColumnSpec::new(
                            Ident::from_static("at"),
                            DataType::Timestamp {
                                with_time_zone: true,
                            },
                        )
                        .not_null(),
                    )
                    .partition_by(Partitioning::new(
                        PartitionStrategy::Range,
                        [Ident::from_static("at")],
                    )),
            )
            .into_statement(),
        )
        .schema(),
        Case::postgres(
            "comment on",
            Ddl::Comment(CommentOn::new(
                CommentTarget::Column {
                    table: users.clone(),
                    column: Ident::from_static("email"),
                },
                Some("It's the handle.".to_owned()),
            ))
            .into_statement(),
        )
        .schema(),
        Case::postgres(
            "create and drop a schema",
            Ddl::CreateSchema(CreateSchema::new(Ident::from_static("moso_live_scratch")))
                .into_statement(),
        )
        .schema(),
        Case::postgres(
            "an exclusion constraint",
            Ddl::CreateTable(
                CreateTable::new(TableRef::from_static("rooms"))
                    .column(
                        ColumnSpec::new(Ident::from_static("room"), DataType::Integer).not_null(),
                    )
                    .constraint(TableConstraint::Exclude {
                        name: Some(Ident::from_static("rooms_unique")),
                        method: Some(Ident::from_static("btree")),
                        elements: vec![(col("room"), Ident::from_static("="))],
                        predicate: Some(col("room").gt(Expr::value(0_i32))),
                    }),
            )
            .into_statement(),
        )
        .schema(),
        Case::postgres(
            "create index concurrently",
            Ddl::CreateIndex(
                CreateIndex::new(
                    Ident::from_static("idx_users_live"),
                    users.clone(),
                    [IndexTarget::column(Ident::from_static("email"))
                        .operator_class(Ident::from_static("text_pattern_ops"))
                        .order(Order::Desc)
                        .nulls(Nulls::Last)],
                )
                .concurrently()
                .if_not_exists()
                .using(IndexMethod::BTree)
                .include([Ident::from_static("name")])
                .where_(col("deleted_at").is_null()),
            )
            .into_statement(),
        )
        .non_transactional(),
        Case::postgres(
            "drop index concurrently",
            Ddl::DropIndex(
                DropIndex::new(Ident::from_static("idx_users_live"))
                    .concurrently()
                    .if_exists(),
            )
            .into_statement(),
        )
        .non_transactional(),
    ]);

    cases
}

// ── PostgreSQL ──────────────────────────────────────────────────────────────

/// Runs the whole corpus past a real PostgreSQL parser and planner.
///
/// Skips, loudly, when `DATABASE_URL` is unset or `psql` is not installed: the
/// suite has to pass on a laptop with no Docker.
#[test]
fn every_construct_parses_and_plans_on_a_real_postgres() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skipping the PostgreSQL leg: DATABASE_URL is not set.\n\
             help: start the test database and export it — \
             `DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test`"
        );
        return;
    };
    if which("psql").is_none() {
        eprintln!(
            "skipping the PostgreSQL leg: `psql` is not on PATH.\n\
             help: install the PostgreSQL client tools, or run this test in CI"
        );
        return;
    }

    // A schema of its own, so that concurrent work on the same database cannot
    // collide and so that the cleanup is one statement.
    let schema = format!("moso_sql_live_{}", std::process::id());
    let mut script = String::new();
    let _ = writeln!(script, "\\set ON_ERROR_STOP on");
    let _ = writeln!(script, "DROP SCHEMA IF EXISTS {schema} CASCADE;");
    let _ = writeln!(script, "CREATE SCHEMA {schema};");
    let _ = writeln!(script, "SET search_path = {schema};");
    script.push_str(POSTGRES_FIXTURE);

    let cases = corpus();
    let mut checked = 0_usize;

    // The statements that cannot run inside a transaction go first, against the
    // fixture, and are undone by dropping the schema.
    for case in &cases {
        if case.scope == Scope::SqliteOnly || case.kind != Kind::NonTransactional {
            continue;
        }
        let sql = render_for(&Postgres, case);
        let _ = writeln!(script, "-- {}\n{sql};", case.name);
        checked += 1;
    }

    let _ = writeln!(script, "BEGIN;");
    for case in &cases {
        if case.scope == Scope::SqliteOnly || case.kind == Kind::NonTransactional {
            continue;
        }
        let sql = render_for(&Postgres, case);
        match case.kind {
            // `PREPARE` parses, resolves every name and plans, and unlike
            // `EXPLAIN` it accepts the `$n` placeholders a rendered query has.
            Kind::Query => {
                let _ = writeln!(
                    script,
                    "-- {}\nPREPARE moso_live_{checked} AS {sql};",
                    case.name
                );
            }
            Kind::Schema => {
                let _ = writeln!(script, "-- {}\n{sql};", case.name);
            }
            Kind::NonTransactional => unreachable!("filtered above"),
        }
        checked += 1;
    }
    let _ = writeln!(script, "ROLLBACK;");
    let _ = writeln!(script, "DROP SCHEMA IF EXISTS {schema} CASCADE;");

    let output = Command::new("psql")
        .arg(&url)
        .arg("--quiet")
        .arg("--no-psqlrc")
        .arg("--file=-")
        .arg("--set=ON_ERROR_STOP=1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .as_mut()
                .expect("piped")
                .write_all(script.as_bytes())?;
            child.wait_with_output()
        });

    let output = match output {
        Ok(output) => output,
        Err(error) => {
            eprintln!("skipping the PostgreSQL leg: could not run `psql`: {error}");
            return;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("could not connect") || stderr.contains("Connection refused") {
            eprintln!(
                "skipping the PostgreSQL leg: no server at {url}.\n\
                 help: `docker compose -f compose.test.yaml up -d`"
            );
            return;
        }
        panic!(
            "PostgreSQL rejected generated SQL:\n{stderr}\n\
             --- the script that was sent ---\n{script}"
        );
    }
    assert!(checked > 40, "only {checked} statements were checked");
}

// ── SQLite ──────────────────────────────────────────────────────────────────

/// Runs the whole corpus past a real SQLite parser.
#[test]
fn every_construct_parses_on_a_real_sqlite() {
    if which("sqlite3").is_none() {
        eprintln!(
            "skipping the SQLite leg: `sqlite3` is not on PATH.\n\
             help: install it — macOS ships one at /usr/bin/sqlite3"
        );
        return;
    }

    let mut script = String::from(".bail on\n");
    script.push_str(SQLITE_FIXTURE);

    let cases = corpus();
    let mut checked = 0_usize;
    for case in &cases {
        if case.scope == Scope::PostgresOnly {
            // The build must refuse it rather than emit SQL SQLite cannot run.
            let error = case
                .statement
                .build(&Sqlite)
                .expect_err(&format!("`{}` should be refused on SQLite", case.name));
            assert!(
                error.to_string().contains("help:"),
                "`{}`: every refusal offers a fix, got: {error}",
                case.name
            );
            continue;
        }
        let sql = render_for(&Sqlite, case);
        // `EXPLAIN` prepares the statement, which is the parse and the name
        // resolution, without running it.
        match case.kind {
            Kind::Query => {
                let _ = writeln!(script, "-- {}\nEXPLAIN {sql};", case.name);
            }
            Kind::Schema | Kind::NonTransactional => {
                let _ = writeln!(script, "-- {}\n{sql};", case.name);
            }
        }
        checked += 1;
    }

    let directory = std::env::temp_dir().join(format!("moso-sql-live-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("a temporary directory");
    let database = directory.join("live.db");

    let output = Command::new("sqlite3")
        .arg(&database)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .as_mut()
                .expect("piped")
                .write_all(script.as_bytes())?;
            child.wait_with_output()
        });

    let output = match output {
        Ok(output) => output,
        Err(error) => {
            eprintln!("skipping the SQLite leg: could not run `sqlite3`: {error}");
            return;
        }
    };
    let _ = std::fs::remove_dir_all(&directory);

    let stderr = String::from_utf8_lossy(&output.stderr);
    // `floor`/`ceil` are OPTIONAL SQLite math functions (they need
    // `SQLITE_ENABLE_MATH_FUNCTIONS`); the `sqlite3` that ships with macOS is
    // built without them and rejects the construct at prepare time with "no such
    // function". That is the CLI lacking an optional feature, not Moso emitting
    // invalid SQL - the generated `floor(...)`/`ceil(...)` is correct and the
    // Linux leg (whose `sqlite3` has the math functions) checks it - so skip this
    // leg rather than fail it.
    if stderr.contains("no such function")
        && [
            "floor", "ceil", "ceiling", "trunc", "ln", "log", "pow", "power", "sqrt", "exp",
        ]
        .iter()
        .any(|function| stderr.contains(function))
    {
        eprintln!(
            "skipping the SQLite leg: this `sqlite3` lacks optional math functions:\n{stderr}"
        );
        return;
    }
    assert!(
        output.status.success() && stderr.is_empty(),
        "SQLite rejected generated SQL:\n{stderr}\n\
         --- the script that was sent ---\n{script}"
    );
    assert!(checked > 30, "only {checked} statements were checked");
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Renders a case, panicking with the diagnostic if the dialect refuses it.
#[track_caller]
fn render_for(dialect: &dyn Dialect, case: &Case) -> String {
    case.statement
        .build(dialect)
        .unwrap_or_else(|error| {
            panic!(
                "`{}` should render on {}, but: {error}",
                case.name,
                dialect.name()
            )
        })
        .text
}

/// Whether an executable is on `PATH`.
fn which(program: &str) -> Option<()> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
        .map(|_| ())
}
