//! `sea-query` as a differential oracle, and the ADR-0005 reversal record.
//!
//! This is the *only* module in the crate that may name `sea_query`; the whole
//! public surface above is Moso's, and `xtask check-sealed` proves it.
//!
//! # What happened to "delegate to `sea-query`"
//!
//! ADR-0005 says `moso-sql`'s implementation "initially delegates to
//! `sea-query`". When the renderer was written, two things turned out to be
//! true at once, and they are recorded here rather than in a commit message:
//!
//! 1. **`sea-query`'s statement AST covers roughly the DML core and little of
//!    the rest.** [`UNREPRESENTABLE`] lists, by name, the constructs the frozen
//!    `moso-sql` surface promises that have no representation in it. Four of
//!    them — `CREATE INDEX CONCURRENTLY`, `ADD CONSTRAINT … NOT VALID`,
//!    `VALIDATE CONSTRAINT` and `ADD … USING INDEX` — are named in
//!    `docs/02-data/23-migrations.md` as the difference between a schema change
//!    that takes a lock for a millisecond and one that takes the site down.
//! 2. **`sea_query::Value` cannot round-trip two of Moso's own scalars.**
//!    [`Decimal`](crate::Decimal) carries an `i128` mantissa where
//!    `rust_decimal` carries 96 bits, and [`Timestamp`](crate::Timestamp)
//!    carries an `i64` second count where `chrono` carries a narrower range.
//!    Delegating would have meant either truncating a `numeric` on the way to
//!    the server — silently, which is the exact class of bug this crate exists
//!    to prevent — or carrying a second, parallel value path anyway.
//!
//! So the grammar walk is Moso's (`crate::render`), and `sea-query` earns its
//! place in the dependency list a different way: **it is the oracle.** For every
//! construct it *does* cover, the tests below build the same statement twice —
//! once through Moso, once through `sea-query` — and compare the text byte for
//! byte. A mature engine agreeing with the renderer on the common path is a
//! stronger correctness argument than delegation would have been, because
//! delegation proves only that the translation compiled.
//!
//! This is exactly the "reversal signal" ADR-0005 asks for, recorded before
//! anyone argues about it. The ADR's reversal criteria are met on the first
//! bullet: a required construct cannot be expressed. The consequence the ADR
//! predicted still holds and is the reason nothing else changes — no user-
//! visible type moved, and the engine can still be swapped in a patch release.

use sea_query::{
    Alias, Expr as SeaExpr, ExprTrait, Order as SeaOrder, PostgresQueryBuilder, Query,
    SqliteQueryBuilder,
};

use crate::{
    Aggregate, ColumnRef, Delete, Expr, FromItem, Ident, Insert, Postgres, Returning, Select,
    SelectItem, Sqlite, Statement, TableRef, Update,
};

/// The constructs the frozen `moso-sql` surface promises that `sea-query`'s
/// statement AST has no representation for.
///
/// Kept as data rather than as prose so that the day someone re-evaluates the
/// engine, the list is a checklist rather than an archaeology exercise.
pub(crate) const UNREPRESENTABLE: &[&str] = &[
    // expressions
    "IS DISTINCT FROM / IS NOT DISTINCT FROM",
    "the jsonb operators ?, ?|, ?&, #>, #>>, #-",
    "FILTER (WHERE …) on an aggregate",
    "an aggregate's internal ORDER BY",
    "EXCLUDE on a window frame",
    "to_tsvector / websearch_to_tsquery / ts_rank / ts_headline",
    // queries
    "LATERAL subqueries",
    "a VALUES list as a FROM item",
    "WITH ORDINALITY",
    "FROM ONLY",
    // DDL
    "CREATE INDEX CONCURRENTLY",
    "partial indexes (CREATE INDEX … WHERE …)",
    "covering indexes (INCLUDE (…))",
    "index operator classes",
    "ADD CONSTRAINT … NOT VALID",
    "VALIDATE CONSTRAINT",
    "ADD PRIMARY KEY / UNIQUE … USING INDEX",
    "EXCLUDE USING … constraints",
    "PARTITION BY / ATTACH PARTITION / DETACH PARTITION",
    "TRUNCATE",
    "CREATE SCHEMA / DROP SCHEMA",
    "COMMENT ON",
    "ALTER TYPE … ADD VALUE IF NOT EXISTS",
];

/// Renders a Moso statement for PostgreSQL.
#[track_caller]
fn moso_pg(statement: &Statement) -> String {
    statement.build(&Postgres).expect("renders").text
}

/// Renders a Moso statement for SQLite.
#[track_caller]
fn moso_sqlite(statement: &Statement) -> String {
    statement.build(&Sqlite).expect("renders").text
}

/// A Moso column of an unqualified name.
fn col(name: &'static str) -> Expr {
    Expr::col(Ident::from_static(name))
}

#[test]
fn the_engine_is_present_and_parameterises_both_dialects() {
    let mut query = Query::select();
    query
        .expr(SeaExpr::col(Alias::new("id")))
        .from(Alias::new("users"))
        .and_where(SeaExpr::col(Alias::new("is_admin")).eq(true));

    let (postgres, postgres_values) = query.build(PostgresQueryBuilder);
    assert!(postgres.contains("$1"), "{postgres}");
    assert_eq!(postgres_values.0.len(), 1);

    let (sqlite, sqlite_values) = query.build(SqliteQueryBuilder);
    assert!(sqlite.contains('?'), "{sqlite}");
    assert_eq!(sqlite_values.0.len(), 1);
}

#[test]
fn moso_and_the_engine_agree_on_a_filtered_select() {
    let mut oracle = Query::select();
    oracle
        .column(Alias::new("id"))
        .column(Alias::new("email"))
        .from(Alias::new("users"))
        .and_where(SeaExpr::col(Alias::new("is_admin")).eq(true))
        .and_where(SeaExpr::col(Alias::new("age")).gte(18_i32));

    let moso = Select::from_table(TableRef::from_static("users"))
        .select_column(ColumnRef::from_static("id"))
        .select_column(ColumnRef::from_static("email"))
        .filter(col("is_admin").eq(Expr::value(true)))
        .filter(col("age").ge(Expr::value(18_i32)))
        .into_statement();

    assert_eq!(moso_pg(&moso), oracle.build(PostgresQueryBuilder).0);
    assert_eq!(moso_sqlite(&moso), oracle.build(SqliteQueryBuilder).0);
}

#[test]
fn moso_and_the_engine_agree_on_a_join() {
    let mut oracle = Query::select();
    oracle
        .expr(SeaExpr::col(Alias::new("id")))
        .from(Alias::new("posts"))
        .inner_join(
            Alias::new("users"),
            SeaExpr::col((Alias::new("posts"), Alias::new("author_id")))
                .equals((Alias::new("users"), Alias::new("id"))),
        );

    let moso = Select::from_table(TableRef::from_static("posts"))
        .select_column(ColumnRef::from_static("id"))
        .inner_join(
            FromItem::table(TableRef::from_static("users")),
            Expr::column(TableRef::from_static("posts").column(Ident::from_static("author_id")))
                .eq(Expr::column(
                    TableRef::from_static("users").column(Ident::from_static("id")),
                )),
        )
        .into_statement();

    assert_eq!(moso_pg(&moso), oracle.build(PostgresQueryBuilder).0);
    assert_eq!(moso_sqlite(&moso), oracle.build(SqliteQueryBuilder).0);
}

#[test]
fn moso_and_the_engine_agree_on_the_predicate_vocabulary() {
    let cases: Vec<(sea_query::Expr, Expr)> = vec![
        (SeaExpr::col(Alias::new("a")).is_null(), col("a").is_null()),
        (
            SeaExpr::col(Alias::new("a")).is_not_null(),
            col("a").is_not_null(),
        ),
        (
            SeaExpr::col(Alias::new("a")).between(1_i32, 9_i32),
            col("a").between(Expr::value(1_i32), Expr::value(9_i32)),
        ),
        (
            SeaExpr::col(Alias::new("a")).not_between(1_i32, 9_i32),
            col("a").not_between(Expr::value(1_i32), Expr::value(9_i32)),
        ),
        (
            SeaExpr::col(Alias::new("a")).is_in([1_i32, 2, 3]),
            col("a").in_list([Expr::value(1_i32), Expr::value(2_i32), Expr::value(3_i32)]),
        ),
        (
            SeaExpr::col(Alias::new("a")).is_not_in([1_i32]),
            col("a").not_in_list([Expr::value(1_i32)]),
        ),
        (
            SeaExpr::col(Alias::new("a")).like("x%"),
            col("a").like(Expr::value("x%")),
        ),
        (
            SeaExpr::col(Alias::new("a")).not_like("x%"),
            col("a").not_like(Expr::value("x%")),
        ),
        (
            SeaExpr::col(Alias::new("a")).ne(1_i32),
            col("a").ne(Expr::value(1_i32)),
        ),
        (
            SeaExpr::col(Alias::new("a")).lt(1_i32),
            col("a").lt(Expr::value(1_i32)),
        ),
        (
            SeaExpr::col(Alias::new("a")).lte(1_i32),
            col("a").le(Expr::value(1_i32)),
        ),
        (
            SeaExpr::col(Alias::new("a")).gt(1_i32),
            col("a").gt(Expr::value(1_i32)),
        ),
        (
            SeaExpr::col(Alias::new("a")).gte(1_i32),
            col("a").ge(Expr::value(1_i32)),
        ),
        (
            SeaExpr::col(Alias::new("a"))
                .eq(1_i32)
                .and(SeaExpr::col(Alias::new("b")).eq(2_i32)),
            col("a").eq(Expr::value(1_i32)) & col("b").eq(Expr::value(2_i32)),
        ),
    ];

    for (index, (sea, moso)) in cases.into_iter().enumerate() {
        let mut oracle = Query::select();
        oracle
            .expr(SeaExpr::col(Alias::new("id")))
            .from(Alias::new("t"))
            .and_where(sea);
        let statement = Select::from_table(TableRef::from_static("t"))
            .select_column(ColumnRef::from_static("id"))
            .filter(moso)
            .into_statement();
        assert_eq!(
            moso_pg(&statement),
            oracle.build(PostgresQueryBuilder).0,
            "case {index}"
        );
        assert_eq!(
            moso_sqlite(&statement),
            oracle.build(SqliteQueryBuilder).0,
            "case {index}"
        );
    }
}

#[test]
fn moso_and_the_engine_agree_on_grouping_and_ordering() {
    let mut oracle = Query::select();
    oracle
        .expr(SeaExpr::col(Alias::new("author_id")))
        .from(Alias::new("posts"))
        .group_by_col(Alias::new("author_id"))
        .and_having(SeaExpr::col(Alias::new("author_id")).is_not_null())
        .order_by(Alias::new("author_id"), SeaOrder::Desc);

    let moso = Select::from_table(TableRef::from_static("posts"))
        .select_column(ColumnRef::from_static("author_id"))
        .group_by(col("author_id"))
        .having(col("author_id").is_not_null())
        .order_by(crate::OrderTerm::desc(col("author_id")))
        .into_statement();

    assert_eq!(moso_pg(&moso), oracle.build(PostgresQueryBuilder).0);
    assert_eq!(moso_sqlite(&moso), oracle.build(SqliteQueryBuilder).0);
}

#[test]
fn moso_and_the_engine_agree_on_distinct() {
    let mut oracle = Query::select();
    oracle
        .distinct()
        .expr(SeaExpr::col(Alias::new("id")))
        .from(Alias::new("a"));

    let moso = Select::from_table(TableRef::from_static("a"))
        .select_column(ColumnRef::from_static("id"))
        .distinct()
        .into_statement();

    assert_eq!(moso_pg(&moso), oracle.build(PostgresQueryBuilder).0);
    assert_eq!(moso_sqlite(&moso), oracle.build(SqliteQueryBuilder).0);
}

/// A set operation, where the two agree exactly on SQLite and differ only in
/// punctuation on PostgreSQL.
///
/// `sea-query` writes `… UNION ALL (SELECT …)` on PostgreSQL and
/// `… UNION ALL SELECT …` on SQLite, because a parenthesised query is not a
/// `select-core` and SQLite's compound select is defined over `select-core`s —
/// `sqlite3` answers `near "(": syntax error` for the parenthesised form. Moso
/// emits the unparenthesised form on both, which both servers accept, and
/// pushes the one case a branch would need parentheses for — its own
/// `ORDER BY` or `LIMIT` — back to the caller as
/// [`Error::InvalidClause`](crate::Error::InvalidClause) with the fix.
#[test]
fn moso_and_the_engine_agree_on_a_set_operation_modulo_punctuation() {
    let mut branch = Query::select();
    branch
        .expr(SeaExpr::col(Alias::new("id")))
        .from(Alias::new("b"));
    let mut oracle = Query::select();
    oracle
        .expr(SeaExpr::col(Alias::new("id")))
        .from(Alias::new("a"))
        .union(sea_query::UnionType::All, branch);

    let moso = Select::from_table(TableRef::from_static("a"))
        .select_column(ColumnRef::from_static("id"))
        .union_all(
            Select::from_table(TableRef::from_static("b"))
                .select_column(ColumnRef::from_static("id")),
        )
        .into_statement();

    // Byte for byte on SQLite, where only one form parses.
    assert_eq!(moso_sqlite(&moso), oracle.build(SqliteQueryBuilder).0);
    assert_eq!(
        moso_sqlite(&moso),
        r#"SELECT "id" FROM "a" UNION ALL SELECT "id" FROM "b""#
    );

    // On PostgreSQL both parse; the engine keeps the parentheses and Moso does
    // not, so the comparison is modulo them.
    let engine_pg = oracle.build(PostgresQueryBuilder).0;
    assert!(engine_pg.contains("ALL (SELECT"), "{engine_pg}");
    assert_eq!(
        moso_pg(&moso),
        engine_pg
            .replace("ALL (SELECT", "ALL SELECT")
            .replace(')', "")
    );
}

/// Moso parenthesises a top-level `OR` filter where the engine does not.
///
/// With one filter the two mean the same thing. With two they do not, and a
/// query builder that only adds the parentheses when a second filter arrives
/// has to re-render the first — so Moso always adds them, and the cost is four
/// characters on a query nobody reads.
#[test]
fn moso_parenthesises_a_top_level_or_filter_that_the_engine_leaves_bare() {
    let mut oracle = Query::select();
    oracle
        .expr(SeaExpr::col(Alias::new("id")))
        .from(Alias::new("t"))
        .and_where(
            SeaExpr::col(Alias::new("a"))
                .eq(1_i32)
                .or(SeaExpr::col(Alias::new("b")).eq(2_i32)),
        );
    assert_eq!(
        oracle.build(PostgresQueryBuilder).0,
        r#"SELECT "id" FROM "t" WHERE "a" = $1 OR "b" = $2"#
    );

    let one = Select::from_table(TableRef::from_static("t"))
        .select_column(ColumnRef::from_static("id"))
        .filter(col("a").eq(Expr::value(1_i32)) | col("b").eq(Expr::value(2_i32)))
        .into_statement();
    assert_eq!(
        moso_pg(&one),
        r#"SELECT "id" FROM "t" WHERE ("a" = $1 OR "b" = $2)"#
    );

    // And this is why: the second filter must not swallow the first.
    let two = Select::from_table(TableRef::from_static("t"))
        .select_column(ColumnRef::from_static("id"))
        .filter(col("a").eq(Expr::value(1_i32)) | col("b").eq(Expr::value(2_i32)))
        .filter(col("c").is_not_null())
        .into_statement();
    assert_eq!(
        moso_pg(&two),
        r#"SELECT "id" FROM "t" WHERE ("a" = $1 OR "b" = $2) AND "c" IS NOT NULL"#
    );
}

#[test]
fn moso_and_the_engine_agree_on_count_star() {
    let mut oracle = Query::select();
    oracle
        .expr(SeaExpr::cust("count(*)"))
        .from(Alias::new("posts"));

    let moso = Select::from_table(TableRef::from_static("posts"))
        .select_expr(Aggregate::count_star().into_expr())
        .into_statement();

    assert_eq!(moso_pg(&moso), oracle.build(PostgresQueryBuilder).0);
}

#[test]
fn moso_and_the_engine_agree_on_an_insert() {
    let mut oracle = Query::insert();
    oracle
        .into_table(Alias::new("users"))
        .columns([Alias::new("email"), Alias::new("name")])
        .values_panic(["a@example.com".into(), "Ada".into()])
        .values_panic(["g@example.com".into(), "Grace".into()]);

    let moso = Insert::into_table(TableRef::from_static("users"))
        .columns([Ident::from_static("email"), Ident::from_static("name")])
        .values([Expr::value("a@example.com"), Expr::value("Ada")])
        .values([Expr::value("g@example.com"), Expr::value("Grace")])
        .into_statement();

    assert_eq!(moso_pg(&moso), oracle.build(PostgresQueryBuilder).0);
    assert_eq!(moso_sqlite(&moso), oracle.build(SqliteQueryBuilder).0);
}

#[test]
fn moso_and_the_engine_agree_on_an_insert_that_returns() {
    let mut oracle = Query::insert();
    oracle
        .into_table(Alias::new("users"))
        .columns([Alias::new("email")])
        .values_panic(["a@example.com".into()])
        .returning_all();

    let moso = Insert::into_table(TableRef::from_static("users"))
        .columns([Ident::from_static("email")])
        .values([Expr::value("a@example.com")])
        .returning(Returning::All)
        .into_statement();

    assert_eq!(moso_pg(&moso), oracle.build(PostgresQueryBuilder).0);
    assert_eq!(moso_sqlite(&moso), oracle.build(SqliteQueryBuilder).0);
}

#[test]
fn moso_and_the_engine_agree_on_an_update() {
    let mut oracle = Query::update();
    oracle
        .table(Alias::new("users"))
        .value(Alias::new("name"), "Ada")
        .and_where(SeaExpr::col(Alias::new("id")).eq(1_i32));

    let moso = Update::table(TableRef::from_static("users"))
        .set(Ident::from_static("name"), Expr::value("Ada"))
        .filter(col("id").eq(Expr::value(1_i32)))
        .into_statement();

    assert_eq!(moso_pg(&moso), oracle.build(PostgresQueryBuilder).0);
    assert_eq!(moso_sqlite(&moso), oracle.build(SqliteQueryBuilder).0);
}

#[test]
fn moso_and_the_engine_agree_on_a_delete() {
    let mut oracle = Query::delete();
    oracle
        .from_table(Alias::new("sessions"))
        .and_where(SeaExpr::col(Alias::new("expired")).eq(true));

    let moso = Delete::from_table(TableRef::from_static("sessions"))
        .filter(col("expired").eq(Expr::value(true)))
        .into_statement();

    assert_eq!(moso_pg(&moso), oracle.build(PostgresQueryBuilder).0);
    assert_eq!(moso_sqlite(&moso), oracle.build(SqliteQueryBuilder).0);
}

#[test]
fn moso_and_the_engine_agree_on_an_aliased_projection() {
    let mut oracle = Query::select();
    oracle
        .expr_as(SeaExpr::col(Alias::new("name")), Alias::new("author"))
        .from(Alias::new("users"));

    let moso = Select::from_table(TableRef::from_static("users"))
        .select_items([SelectItem::aliased(
            col("name"),
            Ident::from_static("author"),
        )])
        .into_statement();

    assert_eq!(moso_pg(&moso), oracle.build(PostgresQueryBuilder).0);
}

/// The one place Moso deliberately differs from the oracle on a construct they
/// both have: `LIMIT` and `OFFSET`.
///
/// `sea-query` binds them as parameters. Moso writes them as literals, because
/// a page size is not user data — it is a `u64` this crate produced, so it
/// cannot inject — and because two of the 65 535 parameters a statement may
/// bind are better spent on the `WHERE` clause. A literal also lets the planner
/// see the row count, which for a `LIMIT` is the difference between an index
/// scan and a sort.
#[test]
fn moso_writes_limit_and_offset_as_literals_where_the_engine_binds_them() {
    let mut oracle = Query::select();
    oracle
        .expr(SeaExpr::col(Alias::new("id")))
        .from(Alias::new("t"))
        .limit(10)
        .offset(20);
    let engine_text = oracle.build(PostgresQueryBuilder).0;
    assert!(engine_text.contains("LIMIT $1"), "{engine_text}");

    let moso = Select::from_table(TableRef::from_static("t"))
        .select_column(ColumnRef::from_static("id"))
        .limit(10)
        .offset(20)
        .into_statement();
    let sql = moso.build(&Postgres).expect("renders");
    assert_eq!(sql.text, r#"SELECT "id" FROM "t" LIMIT 10 OFFSET 20"#);
    assert!(
        sql.args.is_empty(),
        "the page size never costs a bind parameter"
    );
}

/// The reversal record itself: the list is non-empty, and every construct on it
/// is one Moso actually renders.
#[test]
fn the_reversal_record_is_not_empty_and_names_things_moso_renders() {
    assert!(
        UNREPRESENTABLE.len() > 20,
        "if this list ever shrinks to nothing, re-read ADR-0005: delegating again would be \
         cheaper than maintaining a walk"
    );
    for construct in UNREPRESENTABLE {
        assert!(!construct.is_empty());
    }

    // A spot check that the list is describing reality rather than a memory:
    // three of the named constructs, rendered.
    let filtered = Select::from_table(TableRef::from_static("t"))
        .select_expr(Aggregate::count_star().filter(col("published")).into_expr())
        .into_statement();
    assert!(moso_pg(&filtered).contains("FILTER (WHERE"));

    let distinct_from = Select::from_table(TableRef::from_static("t"))
        .select_all()
        .filter(col("a").is_distinct_from(Expr::null()))
        .into_statement();
    assert!(moso_pg(&distinct_from).contains("IS DISTINCT FROM"));

    let concurrent = crate::ddl::Ddl::CreateIndex(
        crate::ddl::CreateIndex::new(
            Ident::from_static("i"),
            TableRef::from_static("t"),
            [crate::ddl::IndexTarget::column(Ident::from_static("c"))],
        )
        .concurrently()
        .where_(col("deleted_at").is_null()),
    )
    .into_statement();
    let text = moso_pg(&concurrent);
    assert!(text.contains("CONCURRENTLY"), "{text}");
    assert!(text.contains("WHERE"), "{text}");
}
