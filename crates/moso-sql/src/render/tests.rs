//! Snapshot tests for the renderer: every construct the spine promises, on
//! both dialects, with the divergences asserted rather than described.
//!
//! The expectations are written out in full. A snapshot test whose expectation
//! is computed from the code it tests proves nothing, so every string here was
//! read by a human against the two servers' grammars, and the
//! `postgres`/`sqlite` modules of `super::live` run the same statements through
//! the real parsers.

use super::*;
use crate::ddl::{IndexTarget, TableConstraint};
use crate::value::{Date, DateTime, Decimal, Interval, Json, Time, Uuid};

// ── harness ─────────────────────────────────────────────────────────────────

/// Renders for PostgreSQL, panicking with the diagnostic if it refuses.
#[track_caller]
fn pg(statement: &Statement) -> Sql {
    statement
        .build(&Postgres)
        .unwrap_or_else(|error| panic!("PostgreSQL refused the statement:\n{error}"))
}

/// Renders for SQLite, panicking with the diagnostic if it refuses.
#[track_caller]
fn lite(statement: &Statement) -> Sql {
    statement
        .build(&Sqlite)
        .unwrap_or_else(|error| panic!("SQLite refused the statement:\n{error}"))
}

/// The error PostgreSQL's renderer returns.
#[track_caller]
fn pg_err(statement: &Statement) -> Error {
    statement
        .build(&Postgres)
        .expect_err("expected PostgreSQL to refuse this statement")
}

/// The error SQLite's renderer returns.
#[track_caller]
fn lite_err(statement: &Statement) -> Error {
    statement
        .build(&Sqlite)
        .expect_err("expected SQLite to refuse this statement")
}

/// Wraps a bare expression in the smallest query that can carry one.
fn probe(expr: Expr) -> Statement {
    Select::new().select_expr(expr).into_statement()
}

/// The rendered form of a bare expression on PostgreSQL, without the
/// `SELECT ` the probe adds.
#[track_caller]
fn pg_expr(expr: Expr) -> String {
    let sql = pg(&probe(expr));
    sql.text
        .strip_prefix("SELECT ")
        .expect("the probe always starts with SELECT")
        .to_owned()
}

/// The rendered form of a bare expression on SQLite.
#[track_caller]
fn lite_expr(expr: Expr) -> String {
    let sql = lite(&probe(expr));
    sql.text
        .strip_prefix("SELECT ")
        .expect("the probe always starts with SELECT")
        .to_owned()
}

/// Asserts that both dialects render an expression the same way, allowing for
/// the one difference that is never interesting: PostgreSQL numbers its
/// placeholders and SQLite does not.
///
/// `expected` is written in PostgreSQL's spelling; the SQLite expectation is
/// derived by replacing `$1`, `$2`, … with `?`, so a divergence anywhere else
/// fails the assertion.
#[track_caller]
fn both_expr(expr: &Expr, expected: &str) {
    assert_eq!(pg_expr(expr.clone()), expected, "PostgreSQL");
    assert_eq!(lite_expr(expr.clone()), unnumber(expected), "SQLite");
}

/// Rewrites PostgreSQL's numbered placeholders into SQLite's anonymous one.
fn unnumber(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let digits = rest[at + 1..]
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(rest.len() - at - 1);
        if digits == 0 {
            out.push('$');
            rest = &rest[at + 1..];
            continue;
        }
        out.push('?');
        rest = &rest[at + 1 + digits..];
    }
    out.push_str(rest);
    out
}

/// A column of the anonymous probe table.
fn col(name: &'static str) -> Expr {
    Expr::col(Ident::from_static(name))
}

// ── values and identifiers ──────────────────────────────────────────────────

#[test]
fn a_value_becomes_a_placeholder_and_never_text() {
    let statement = probe(Expr::value("hunter2"));
    let postgres = pg(&statement);
    assert_eq!(postgres.text, "SELECT $1");
    assert_eq!(postgres.args, vec![Value::text("hunter2")]);
    assert!(!postgres.text.contains("hunter2"));

    let sqlite = lite(&statement);
    assert_eq!(sqlite.text, "SELECT ?");
    assert_eq!(sqlite.args, vec![Value::text("hunter2")]);
}

#[test]
fn postgres_numbers_its_placeholders_in_binding_order() {
    let statement = Select::from_table(TableRef::from_static("t"))
        .select_all()
        .filter(col("a").eq(Expr::value(1)))
        .filter(col("b").eq(Expr::value(2)))
        .filter(col("c").eq(Expr::value(3)))
        .into_statement();
    let sql = pg(&statement);
    assert_eq!(
        sql.text,
        r#"SELECT * FROM "t" WHERE "a" = $1 AND "b" = $2 AND "c" = $3"#
    );
    assert_eq!(
        sql.args,
        vec![Value::I32(1), Value::I32(2), Value::I32(3)],
        "the arguments come back in placeholder order"
    );
    assert_eq!(
        lite(&statement).text,
        r#"SELECT * FROM "t" WHERE "a" = ? AND "b" = ? AND "c" = ?"#
    );
}

#[test]
fn every_identifier_is_quoted_even_when_it_is_a_keyword() {
    let statement = Select::from_table(TableRef::qualified(
        Ident::from_static("select"),
        Ident::from_static("order"),
    ))
    .select_column(ColumnRef::qualified(
        Ident::from_static("order"),
        Ident::from_static("group"),
    ))
    .into_statement();
    let expected = r#"SELECT "order"."group" FROM "select"."order""#;
    assert_eq!(pg(&statement).text, expected);
    assert_eq!(lite(&statement).text, expected);
}

#[test]
fn an_identifier_longer_than_the_server_allows_is_refused_before_the_wire() {
    /// A dialect whose server truncates at 30 bytes, as Oracle's does.
    #[derive(Debug)]
    struct Narrow;

    impl Dialect for Narrow {
        fn name(&self) -> &'static str {
            "Narrow"
        }
        fn quote_ident(&self, ident: &Ident, out: &mut String) {
            out.push('"');
            out.push_str(ident.as_str());
            out.push('"');
        }
        fn placeholder(&self, index: usize, out: &mut String) {
            let _ = write!(out, ":{}", index + 1);
        }
        fn type_name(&self, _: &DataType, out: &mut String) -> Result<(), Error> {
            out.push_str("text");
            Ok(())
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::postgres()
        }
        fn build(&self, statement: StatementRef<'_>) -> Result<Sql, Error> {
            super::build(self, statement)
        }
        fn max_ident_len(&self) -> usize {
            30
        }
    }

    let long = "a".repeat(40);
    let statement = Select::from_table(TableRef::new(Ident::new(long).expect("under 63 bytes")))
        .select_all()
        .into_statement();
    let error = statement
        .build(&Narrow)
        .expect_err("40 bytes over a 30 limit");
    assert!(matches!(error, Error::Ident(_)), "{error}");
    assert!(error.to_string().contains("help:"));

    // The same statement is fine on a server whose limit is PostgreSQL's.
    assert!(statement.build(&Postgres).is_ok());
}

// ── operators and precedence ────────────────────────────────────────────────

#[test]
fn the_comparison_operators_render() {
    let cases = [
        (col("a").eq(Expr::value(1)), r#""a" = $1"#),
        (col("a").ne(Expr::value(1)), r#""a" <> $1"#),
        (col("a").lt(Expr::value(1)), r#""a" < $1"#),
        (col("a").le(Expr::value(1)), r#""a" <= $1"#),
        (col("a").gt(Expr::value(1)), r#""a" > $1"#),
        (col("a").ge(Expr::value(1)), r#""a" >= $1"#),
    ];
    for (expr, expected) in cases {
        assert_eq!(pg_expr(expr), expected);
    }
}

#[test]
fn precedence_drops_the_parentheses_a_reader_would_not_write() {
    // `a + b * c` — multiplication binds tighter, so no parentheses.
    both_expr(&(col("a") + col("b") * col("c")), r#""a" + "b" * "c""#);
    // `(a + b) * c` — the tree says otherwise, so the parentheses stay.
    both_expr(&((col("a") + col("b")) * col("c")), r#"("a" + "b") * "c""#);
    // Left associativity is visible: `a - b - c` stays flat …
    both_expr(&(col("a") - col("b") - col("c")), r#""a" - "b" - "c""#);
    // … and the right-nested form keeps its meaning.
    both_expr(&(col("a") - (col("b") - col("c"))), r#""a" - ("b" - "c")"#);
}

#[test]
fn an_or_inside_an_and_keeps_its_parentheses() {
    // The bug this prevents: "admin AND (active OR trial)" silently becoming
    // "(admin AND active) OR trial", which returns every trial account.
    let expr = col("is_admin")
        .eq(Expr::value(true))
        .and(col("active").eq(Expr::value(true)) | col("trial").eq(Expr::value(true)));
    assert_eq!(
        pg_expr(expr),
        r#""is_admin" = $1 AND ("active" = $2 OR "trial" = $3)"#
    );
}

#[test]
fn a_filter_list_is_and_ed_and_each_element_is_protected() {
    let statement = Select::from_table(TableRef::from_static("t"))
        .select_all()
        .filter(col("a").eq(Expr::value(1)) | col("b").eq(Expr::value(2)))
        .filter(col("c").is_not_null())
        .into_statement();
    assert_eq!(
        pg(&statement).text,
        r#"SELECT * FROM "t" WHERE ("a" = $1 OR "b" = $2) AND "c" IS NOT NULL"#
    );
}

#[test]
fn not_and_the_unary_operators_render() {
    both_expr(&!col("ok"), r#"NOT "ok""#);
    both_expr(&col("n").unary(UnOp::Neg), r#"-"n""#);
    both_expr(&col("n").unary(UnOp::BitNot), r#"~"n""#);
    both_expr(
        &!(col("a").is_null() & col("b").is_null()),
        r#"NOT ("a" IS NULL AND "b" IS NULL)"#,
    );
}

#[test]
fn the_arithmetic_and_bitwise_operators_render() {
    both_expr(&col("a").plus(Expr::value(1)), r#""a" + $1"#);
    both_expr(&col("a").minus(Expr::value(1)), r#""a" - $1"#);
    both_expr(&col("a").times(Expr::value(2)), r#""a" * $1"#);
    both_expr(&col("a").over(Expr::value(2)), r#""a" / $1"#);
    both_expr(&col("a").modulo(Expr::value(2)), r#""a" % $1"#);
    both_expr(&col("a").concat(col("b")), r#""a" || "b""#);
    both_expr(&col("a").binary(BinOp::BitAnd, col("b")), r#""a" & "b""#);
    both_expr(&col("a").binary(BinOp::BitOr, col("b")), r#""a" | "b""#);
    both_expr(
        &col("a").binary(BinOp::ShiftLeft, Expr::value(2)),
        r#""a" << $1"#,
    );
    both_expr(
        &col("a").binary(BinOp::ShiftRight, Expr::value(2)),
        r#""a" >> $1"#,
    );

    // The two PostgreSQL-only ones.
    assert_eq!(
        pg_expr(col("a").binary(BinOp::Exp, col("b"))),
        r#""a" ^ "b""#
    );
    assert_eq!(
        pg_expr(col("a").binary(BinOp::BitXor, col("b"))),
        r#""a" # "b""#
    );
    for op in [BinOp::Exp, BinOp::BitXor] {
        let error = lite_err(&probe(col("a").binary(op, col("b"))));
        assert!(error.is_dialect_gap(), "{error}");
        assert!(error.to_string().contains("help:"), "{error}");
    }
}

#[test]
fn is_distinct_from_renders_on_both_dialects() {
    both_expr(
        &col("a").is_distinct_from(Expr::null()),
        r#""a" IS DISTINCT FROM $1"#,
    );
    both_expr(
        &col("a").is_not_distinct_from(col("b")),
        r#""a" IS NOT DISTINCT FROM "b""#,
    );
}

#[test]
fn the_regex_operators_are_postgres_only() {
    let cases = [
        (BinOp::Regex, r#""a" ~ $1"#),
        (BinOp::RegexCaseInsensitive, r#""a" ~* $1"#),
        (BinOp::NotRegex, r#""a" !~ $1"#),
        (BinOp::NotRegexCaseInsensitive, r#""a" !~* $1"#),
    ];
    for (op, expected) in cases {
        let expr = col("a").binary(op, Expr::value("^x"));
        assert_eq!(pg_expr(expr.clone()), expected);
        let error = lite_err(&probe(expr));
        assert!(error.is_dialect_gap(), "{error}");
        assert!(error.to_string().contains("REGEXP"), "{error}");
    }
}

// ── predicates ──────────────────────────────────────────────────────────────

#[test]
fn null_tests_render() {
    both_expr(&col("x").is_null(), r#""x" IS NULL"#);
    both_expr(&col("x").is_not_null(), r#""x" IS NOT NULL"#);
}

#[test]
fn between_protects_its_bounds_from_the_grammatical_and() {
    both_expr(
        &col("n").between(Expr::value(1), Expr::value(9)),
        r#""n" BETWEEN $1 AND $2"#,
    );
    both_expr(
        &col("n").not_between(Expr::value(1), Expr::value(9)),
        r#""n" NOT BETWEEN $1 AND $2"#,
    );
    // A boolean bound would otherwise be swallowed by the `AND` that separates
    // them.
    both_expr(
        &col("n").between(col("a").eq(col("b")), Expr::value(9)),
        r#""n" BETWEEN ("a" = "b") AND $1"#,
    );
}

#[test]
fn like_renders_with_its_escape_character() {
    both_expr(&col("name").like(Expr::value("a%")), r#""name" LIKE $1"#);
    both_expr(
        &col("name").not_like(Expr::value("a%")),
        r#""name" NOT LIKE $1"#,
    );
    both_expr(
        &col("path").like(Expr::value(r"100\%")).escape('\\'),
        r#""path" LIKE $1 ESCAPE '\'"#,
    );
    // The escape character is a value, so a quote in it cannot end the literal.
    both_expr(
        &col("path").like(Expr::value("x")).escape('\''),
        r#""path" LIKE $1 ESCAPE ''''"#,
    );
}

/// The documented SQLite divergence (ADR-0010, `20-orm-overview.md`): no
/// `ILIKE`, so both sides are lowered.
#[test]
fn ilike_is_postgres_native_and_lowered_on_sqlite() {
    let expr = col("name").ilike(Expr::value("a%"));
    assert_eq!(pg_expr(expr.clone()), r#""name" ILIKE $1"#);
    assert_eq!(lite_expr(expr), r#"lower("name") LIKE lower(?)"#);

    let negated = col("name").not_ilike(Expr::value("a%"));
    assert_eq!(pg_expr(negated.clone()), r#""name" NOT ILIKE $1"#);
    assert_eq!(lite_expr(negated), r#"lower("name") NOT LIKE lower(?)"#);

    // The escape survives the lowering.
    let escaped = col("name").ilike(Expr::value("a%")).escape('!');
    assert_eq!(
        lite_expr(escaped),
        r#"lower("name") LIKE lower(?) ESCAPE '!'"#
    );
}

#[test]
fn an_empty_in_list_is_a_constant_rather_than_a_syntax_error() {
    // `IN ()` does not parse anywhere, and an empty list can never match.
    both_expr(&col("id").in_list([]), "FALSE");
    both_expr(&col("id").not_in_list([]), "TRUE");
    both_expr(
        &col("id").in_list([Expr::value(1), Expr::value(2)]),
        r#""id" IN ($1, $2)"#,
    );
    both_expr(
        &col("id").not_in_list([Expr::value(1)]),
        r#""id" NOT IN ($1)"#,
    );
}

#[test]
fn subquery_predicates_render() {
    let bans = Select::from_table(TableRef::from_static("bans"))
        .select_column(ColumnRef::from_static("user_id"));
    both_expr(
        &col("id").in_subquery(bans.clone()),
        r#""id" IN (SELECT "user_id" FROM "bans")"#,
    );
    both_expr(
        &col("id").not_in_subquery(bans.clone()),
        r#""id" NOT IN (SELECT "user_id" FROM "bans")"#,
    );
    both_expr(
        &Expr::exists(bans.clone()),
        r#"EXISTS (SELECT "user_id" FROM "bans")"#,
    );
    both_expr(
        &Expr::not_exists(bans.clone()),
        r#"NOT EXISTS (SELECT "user_id" FROM "bans")"#,
    );
    both_expr(
        &Expr::scalar(bans).eq(Expr::value(1)),
        r#"(SELECT "user_id" FROM "bans") = $1"#,
    );
}

#[test]
fn a_quantified_comparison_is_one_array_parameter_and_postgres_only() {
    let expr = col("id").any(BinOp::Eq, Expr::value(Array::of([1_i64, 2, 3])));
    let sql = pg(&probe(expr.clone()));
    assert_eq!(sql.text, r#"SELECT "id" = ANY ($1)"#);
    assert_eq!(
        sql.args.len(),
        1,
        "one array parameter, not one per element"
    );

    assert_eq!(
        pg_expr(col("n").all(BinOp::Gt, Expr::value(Array::of([1_i64])))),
        r#""n" > ALL ($1)"#
    );

    let subquery =
        Select::from_table(TableRef::from_static("t")).select_column(ColumnRef::from_static("id"));
    assert_eq!(
        pg_expr(col("id").any(BinOp::Eq, Expr::scalar(subquery))),
        r#""id" = ANY (SELECT "id" FROM "t")"#
    );

    let error = lite_err(&probe(expr));
    assert!(error.is_dialect_gap(), "{error}");
    assert!(error.to_string().contains("is_in"), "{error}");
}

// ── json ────────────────────────────────────────────────────────────────────

#[test]
fn the_jsonb_operators_render_on_postgres() {
    let cases = [
        (JsonOp::Get, r#""prefs" -> $1"#),
        (JsonOp::GetText, r#""prefs" ->> $1"#),
        (JsonOp::GetPath, r#""prefs" #> $1"#),
        (JsonOp::GetPathText, r#""prefs" #>> $1"#),
        (JsonOp::Contains, r#""prefs" @> $1"#),
        (JsonOp::ContainedBy, r#""prefs" <@ $1"#),
        (JsonOp::HasKey, r#""prefs" ? $1"#),
        (JsonOp::HasAnyKey, r#""prefs" ?| $1"#),
        (JsonOp::HasAllKeys, r#""prefs" ?& $1"#),
        (JsonOp::Concat, r#""prefs" || $1"#),
        (JsonOp::Remove, r#""prefs" - $1"#),
        (JsonOp::RemovePath, r#""prefs" #- $1"#),
    ];
    for (op, expected) in cases {
        assert_eq!(
            pg_expr(col("prefs").json(op, Expr::value("theme"))),
            expected,
            "{op:?}"
        );
    }
}

#[test]
fn sqlite_lowers_the_two_json_accessors_and_refuses_the_rest() {
    // SQLite's `->` and `->>` are `json_extract` with PostgreSQL's
    // key-or-index abbreviation, so the two accessors mean the same thing.
    assert_eq!(
        lite_expr(col("prefs").json(JsonOp::Get, Expr::value("theme"))),
        r#""prefs" -> ?"#
    );
    assert_eq!(
        lite_expr(col("prefs").json(JsonOp::GetText, Expr::value("theme"))),
        r#""prefs" ->> ?"#
    );
    for op in [
        JsonOp::GetPath,
        JsonOp::GetPathText,
        JsonOp::Contains,
        JsonOp::ContainedBy,
        JsonOp::HasKey,
        JsonOp::HasAnyKey,
        JsonOp::HasAllKeys,
        JsonOp::Concat,
        JsonOp::Remove,
        JsonOp::RemovePath,
    ] {
        let error = lite_err(&probe(col("prefs").json(op, Expr::value("theme"))));
        assert!(error.is_dialect_gap(), "{op:?}: {error}");
        assert!(error.to_string().contains("help:"), "{op:?}: {error}");
    }
}

// ── functions ───────────────────────────────────────────────────────────────

#[test]
fn the_portable_scalar_functions_render_the_same_on_both() {
    both_expr(
        &Function::Coalesce(vec![col("nickname"), col("name")]).into_expr(),
        r#"coalesce("nickname", "name")"#,
    );
    both_expr(
        &Function::NullIf(Box::new(col("a")), Box::new(Expr::value(0))).into_expr(),
        r#"nullif("a", $1)"#,
    );
    both_expr(
        &Function::Abs(Box::new(col("n"))).into_expr(),
        r#"abs("n")"#,
    );
    both_expr(
        &Function::Floor(Box::new(col("n"))).into_expr(),
        r#"floor("n")"#,
    );
    both_expr(
        &Function::Ceil(Box::new(col("n"))).into_expr(),
        r#"ceil("n")"#,
    );
    both_expr(
        &Function::Lower(Box::new(col("s"))).into_expr(),
        r#"lower("s")"#,
    );
    both_expr(
        &Function::Upper(Box::new(col("s"))).into_expr(),
        r#"upper("s")"#,
    );
    both_expr(
        &Function::Length(Box::new(col("s"))).into_expr(),
        r#"length("s")"#,
    );
    both_expr(
        &Function::Round {
            operand: Box::new(col("n")),
            decimals: Some(Box::new(Expr::value(2))),
        }
        .into_expr(),
        r#"round("n", $1)"#,
    );
    both_expr(
        &Function::Replace {
            operand: Box::new(col("s")),
            from: Box::new(Expr::value("a")),
            to: Box::new(Expr::value("b")),
        }
        .into_expr(),
        r#"replace("s", $1, $2)"#,
    );
    both_expr(&Function::CurrentDate.into_expr(), "CURRENT_DATE");
    both_expr(&Function::CurrentTime.into_expr(), "CURRENT_TIME");
    both_expr(&Function::CurrentTimestamp.into_expr(), "CURRENT_TIMESTAMP");
    both_expr(&Function::Random.into_expr(), "random()");
    both_expr(
        &Function::custom(Ident::from_static("gen_random_uuid"), []).into_expr(),
        r#""gen_random_uuid"()"#,
    );
}

#[test]
fn now_greatest_trim_and_substring_take_each_dialects_spelling() {
    let now = Function::Now.into_expr();
    assert_eq!(pg_expr(now.clone()), "now()");
    assert_eq!(lite_expr(now), "CURRENT_TIMESTAMP");

    let greatest = Function::Greatest(vec![col("a"), col("b")]).into_expr();
    assert_eq!(pg_expr(greatest.clone()), r#"greatest("a", "b")"#);
    assert_eq!(lite_expr(greatest), r#"max("a", "b")"#);

    let least = Function::Least(vec![col("a"), col("b")]).into_expr();
    assert_eq!(pg_expr(least.clone()), r#"least("a", "b")"#);
    assert_eq!(lite_expr(least), r#"min("a", "b")"#);

    // One argument would be SQLite's *aggregate* `max`, so it collapses.
    let single = Function::Greatest(vec![col("a")]).into_expr();
    assert_eq!(pg_expr(single.clone()), r#"greatest("a")"#);
    assert_eq!(lite_expr(single), r#""a""#);

    let trim = Function::Trim {
        operand: Box::new(col("s")),
        mode: TrimMode::Leading,
        characters: Some(Box::new(Expr::value("0"))),
    }
    .into_expr();
    assert_eq!(pg_expr(trim.clone()), r#"trim(LEADING $1 FROM "s")"#);
    assert_eq!(lite_expr(trim), r#"ltrim("s", ?)"#);

    let both = Function::Trim {
        operand: Box::new(col("s")),
        mode: TrimMode::Both,
        characters: None,
    }
    .into_expr();
    assert_eq!(pg_expr(both.clone()), r#"trim(BOTH FROM "s")"#);
    assert_eq!(lite_expr(both), r#"trim("s")"#);

    let substring = Function::Substring {
        operand: Box::new(col("s")),
        from: Some(Box::new(Expr::value(2))),
        length: Some(Box::new(Expr::value(3))),
    }
    .into_expr();
    assert_eq!(
        pg_expr(substring.clone()),
        r#"substring("s" FROM $1 FOR $2)"#
    );
    assert_eq!(lite_expr(substring), r#"substr("s", ?, ?)"#);

    // SQLite's `substr` has no "from the beginning" form, so the standard's
    // implied `1` becomes explicit rather than silently dropped.
    let prefix = Function::Substring {
        operand: Box::new(col("s")),
        from: None,
        length: Some(Box::new(Expr::value(3))),
    }
    .into_expr();
    assert_eq!(pg_expr(prefix.clone()), r#"substring("s" FOR $1)"#);
    assert_eq!(lite_expr(prefix), r#"substr("s", 1, ?)"#);
}

#[test]
fn concat_renders_with_its_separator() {
    both_expr(
        &Function::Concat(vec![col("a"), col("b")]).into_expr(),
        r#"concat("a", "b")"#,
    );
    both_expr(
        &Function::ConcatWs {
            separator: Box::new(Expr::value(" ")),
            items: vec![col("first"), col("last")],
        }
        .into_expr(),
        r#"concat_ws($1, "first", "last")"#,
    );
}

#[test]
fn full_text_search_binds_the_search_box_and_is_postgres_only() {
    let expr = Expr::text_match(
        col("body"),
        TextQuery::Websearch("rust orm".to_owned()),
        Some(Ident::from_static("english")),
    );
    let sql = pg(&probe(expr.clone()));
    assert_eq!(
        sql.text,
        r#"SELECT to_tsvector('english', "body") @@ websearch_to_tsquery('english', $1)"#
    );
    assert_eq!(sql.args, vec![Value::text("rust orm")]);
    assert!(
        !sql.text.contains("rust orm"),
        "the query text is a parameter, never syntax"
    );

    // A materialised `tsvector` column skips the `to_tsvector`.
    assert_eq!(
        pg_expr(Expr::text_match_vector(
            col("search"),
            TextQuery::Plain("rust".to_owned()),
            None,
        )),
        r#""search" @@ plainto_tsquery($1)"#
    );
    for (query, function) in [
        (TextQuery::Plain(String::new()), "plainto_tsquery"),
        (TextQuery::Phrase(String::new()), "phraseto_tsquery"),
        (TextQuery::Websearch(String::new()), "websearch_to_tsquery"),
        (TextQuery::Tsquery(String::new()), "to_tsquery"),
    ] {
        let rendered = pg_expr(Expr::text_match_vector(col("v"), query, None));
        assert!(rendered.contains(function), "{rendered}");
    }

    // Ranking and highlighting.
    assert_eq!(
        pg_expr(
            Function::TsRank {
                vector: Box::new(col("search")),
                query: Box::new(col("q")),
                normalization: Some(2),
            }
            .into_expr()
        ),
        r#"ts_rank("search", "q", $1)"#
    );
    assert_eq!(
        pg_expr(
            Function::TsHeadline {
                config: Some(Ident::from_static("english")),
                document: Box::new(col("body")),
                query: Box::new(col("q")),
                options: Some("MaxWords=20".to_owned()),
            }
            .into_expr()
        ),
        r#"ts_headline('english', "body", "q", $1)"#
    );

    let error = lite_err(&probe(expr));
    assert!(error.is_dialect_gap(), "{error}");
    assert!(error.to_string().contains("FTS5"), "{error}");
}

#[test]
fn case_renders_in_both_of_its_forms() {
    both_expr(
        &Case::new()
            .when(col("score").ge(Expr::value(90)), Expr::value("a"))
            .when(col("score").ge(Expr::value(80)), Expr::value("b"))
            .otherwise(Expr::value("c"))
            .into_expr(),
        r#"CASE WHEN "score" >= $1 THEN $2 WHEN "score" >= $3 THEN $4 ELSE $5 END"#,
    );
    both_expr(
        &Case::on(col("status"))
            .when(Expr::value(1), Expr::value("new"))
            .into_expr(),
        r#"CASE "status" WHEN $1 THEN $2 END"#,
    );
    let error = pg_err(&probe(Case::new().into_expr()));
    assert!(matches!(error, Error::InvalidClause { .. }), "{error}");
}

#[test]
fn cast_and_the_tuple_and_array_constructors_render() {
    both_expr(&col("n").cast(DataType::Text), r#"CAST("n" AS text)"#);
    assert_eq!(
        pg_expr(col("n").cast(DataType::Timestamp {
            with_time_zone: true
        })),
        r#"CAST("n" AS timestamptz)"#
    );
    assert_eq!(
        lite_expr(col("n").cast(DataType::Timestamp {
            with_time_zone: true
        })),
        r#"CAST("n" AS text)"#
    );
    both_expr(
        &Expr::tuple([col("a"), col("b")]).eq(Expr::tuple([Expr::value(1), Expr::value(2)])),
        r#"("a", "b") = ($1, $2)"#,
    );
    assert_eq!(
        pg_expr(Expr::array([Expr::value(1), Expr::value(2)])),
        "ARRAY[$1, $2]"
    );
    let error = lite_err(&probe(Expr::array([Expr::value(1)])));
    assert!(error.is_dialect_gap(), "{error}");
    // An empty constructor has no element type the server can infer.
    let error = pg_err(&probe(Expr::array([])));
    assert!(matches!(error, Error::InvalidClause { .. }), "{error}");
}

#[test]
fn explicit_parentheses_survive() {
    both_expr(&col("a").nested(), r#"("a")"#);
}

// ── aggregates ──────────────────────────────────────────────────────────────

#[test]
fn aggregates_render_with_distinct_filter_and_their_own_ordering() {
    both_expr(&Aggregate::count_star().into_expr(), "count(*)");
    both_expr(
        &Aggregate::new(AggregateFunc::Count, [col("id")])
            .distinct()
            .into_expr(),
        r#"count(DISTINCT "id")"#,
    );
    both_expr(
        &Aggregate::count_star().filter(col("published")).into_expr(),
        r#"count(*) FILTER (WHERE "published")"#,
    );
    both_expr(
        &Aggregate::count_star()
            .filter(col("a"))
            .filter(col("b"))
            .into_expr(),
        r#"count(*) FILTER (WHERE "a" AND "b")"#,
    );
    for (func, name) in [
        (AggregateFunc::Sum, "sum"),
        (AggregateFunc::Avg, "avg"),
        (AggregateFunc::Min, "min"),
        (AggregateFunc::Max, "max"),
    ] {
        both_expr(
            &Aggregate::new(func, [col("n")]).into_expr(),
            &format!(r#"{name}("n")"#),
        );
    }
    both_expr(
        &Aggregate::new(
            AggregateFunc::Custom(Ident::from_static("mode")),
            [col("n")],
        )
        .into_expr(),
        r#""mode"("n")"#,
    );
}

#[test]
fn the_aggregates_that_differ_take_each_dialects_name_or_say_no() {
    let string_agg = Aggregate::new(AggregateFunc::StringAgg, [col("name"), Expr::value(",")])
        .order_by(OrderTerm::asc(col("name")))
        .into_expr();
    assert_eq!(
        pg_expr(string_agg.clone()),
        r#"string_agg("name", $1 ORDER BY "name" ASC)"#
    );
    assert_eq!(
        lite_expr(string_agg),
        r#"group_concat("name", ? ORDER BY "name" ASC)"#
    );

    let json_agg = Aggregate::new(AggregateFunc::JsonAgg, [col("row")]).into_expr();
    assert_eq!(pg_expr(json_agg.clone()), r#"json_agg("row")"#);
    assert_eq!(lite_expr(json_agg), r#"json_group_array("row")"#);

    let jsonb_agg = Aggregate::new(AggregateFunc::JsonbAgg, [col("row")]).into_expr();
    assert_eq!(pg_expr(jsonb_agg.clone()), r#"jsonb_agg("row")"#);
    assert_eq!(lite_expr(jsonb_agg), r#"json_group_array("row")"#);

    let object_agg = Aggregate::new(AggregateFunc::JsonObjectAgg, [col("k"), col("v")]).into_expr();
    assert_eq!(pg_expr(object_agg.clone()), r#"json_object_agg("k", "v")"#);
    assert_eq!(lite_expr(object_agg), r#"json_group_object("k", "v")"#);

    for (func, needle) in [
        (AggregateFunc::ArrayAgg, "json_group_array"),
        (AggregateFunc::BoolAnd, "min(flag)"),
        (AggregateFunc::BoolOr, "max(flag)"),
        (AggregateFunc::StdDev, "extension-functions"),
        (AggregateFunc::Variance, "extension-functions"),
    ] {
        let expr = Aggregate::new(func.clone(), [col("n")]).into_expr();
        assert!(pg(&probe(expr.clone())).text.len() > "SELECT ".len());
        let error = lite_err(&probe(expr));
        assert!(error.is_dialect_gap(), "{func:?}: {error}");
        assert!(error.to_string().contains(needle), "{func:?}: {error}");
    }
}

// ── window functions ────────────────────────────────────────────────────────

#[test]
fn the_row_number_that_makes_a_preload_one_statement_renders() {
    // Non-negotiable N3: the first `n` children of every parent in one query.
    let ranked = WindowExpr::new(
        WindowFunc::RowNumber,
        [],
        WindowSpec::new()
            .partition_by(col("author_id"))
            .order_by(OrderTerm::desc(col("created_at"))),
    )
    .into_expr();
    both_expr(
        &ranked,
        r#"row_number() OVER (PARTITION BY "author_id" ORDER BY "created_at" DESC)"#,
    );
}

#[test]
fn every_window_function_renders() {
    let cases = [
        (WindowFunc::Rank, "rank()"),
        (WindowFunc::DenseRank, "dense_rank()"),
        (WindowFunc::PercentRank, "percent_rank()"),
        (WindowFunc::CumeDist, "cume_dist()"),
        (WindowFunc::FirstValue, "first_value()"),
        (WindowFunc::LastValue, "last_value()"),
    ];
    for (func, name) in cases {
        both_expr(
            &WindowExpr::new(func, [], WindowSpec::new()).into_expr(),
            &format!("{name} OVER ()"),
        );
    }
    both_expr(
        &WindowExpr::new(WindowFunc::Ntile, [Expr::value(4)], WindowSpec::new()).into_expr(),
        "ntile($1) OVER ()",
    );
    both_expr(
        &WindowExpr::new(
            WindowFunc::Lag,
            [col("n"), Expr::value(1)],
            WindowSpec::new(),
        )
        .into_expr(),
        r#"lag("n", $1) OVER ()"#,
    );
    both_expr(
        &WindowExpr::new(WindowFunc::Lead, [col("n")], WindowSpec::new()).into_expr(),
        r#"lead("n") OVER ()"#,
    );
    both_expr(
        &WindowExpr::new(
            WindowFunc::NthValue,
            [col("n"), Expr::value(2)],
            WindowSpec::new(),
        )
        .into_expr(),
        r#"nth_value("n", $1) OVER ()"#,
    );
    both_expr(
        &WindowExpr::new(
            WindowFunc::Custom(Ident::from_static("my_rank")),
            [],
            WindowSpec::new(),
        )
        .into_expr(),
        r#""my_rank"() OVER ()"#,
    );
    // An ordinary aggregate used as a window function keeps its FILTER, and
    // the FILTER comes before the OVER.
    both_expr(
        &WindowExpr::new(
            WindowFunc::Aggregate(Box::new(Aggregate::count_star().filter(col("ok")))),
            [],
            WindowSpec::new().partition_by(col("g")),
        )
        .into_expr(),
        r#"count(*) FILTER (WHERE "ok") OVER (PARTITION BY "g")"#,
    );
    // A window declared once in the query's WINDOW clause.
    both_expr(
        &WindowExpr::over_named(WindowFunc::Rank, [], Ident::from_static("w")).into_expr(),
        r#"rank() OVER "w""#,
    );
}

#[test]
fn window_frames_render_including_the_advanced_ones() {
    let running_total = WindowExpr::new(
        WindowFunc::Aggregate(Box::new(Aggregate::new(AggregateFunc::Sum, [col("n")]))),
        [],
        WindowSpec::new().order_by(OrderTerm::asc(col("at"))).frame(
            Frame::new(FrameUnits::Rows, FrameBound::UnboundedPreceding).to(FrameBound::CurrentRow),
        ),
    )
    .into_expr();
    both_expr(
        &running_total,
        r#"sum("n") OVER (ORDER BY "at" ASC ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)"#,
    );

    both_expr(
        &WindowExpr::new(
            WindowFunc::Rank,
            [],
            WindowSpec::new().frame(Frame::new(FrameUnits::Rows, FrameBound::Preceding(3))),
        )
        .into_expr(),
        "rank() OVER (ROWS 3 PRECEDING)",
    );
    both_expr(
        &WindowExpr::new(
            WindowFunc::Rank,
            [],
            WindowSpec::new().frame(
                Frame::new(FrameUnits::Groups, FrameBound::Preceding(1))
                    .to(FrameBound::Following(1))
                    .exclude(FrameExclusion::Ties),
            ),
        )
        .into_expr(),
        "rank() OVER (GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES)",
    );
    both_expr(
        &WindowExpr::new(
            WindowFunc::Rank,
            [],
            WindowSpec::new().frame(
                Frame::new(FrameUnits::Range, FrameBound::UnboundedPreceding)
                    .to(FrameBound::UnboundedFollowing),
            ),
        )
        .into_expr(),
        "rank() OVER (RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)",
    );

    // A frame whose bounds are the wrong way round is a build-time error, not a
    // database one.
    let backwards = WindowExpr::new(
        WindowFunc::Rank,
        [],
        WindowSpec::new().frame(
            Frame::new(FrameUnits::Rows, FrameBound::UnboundedFollowing).to(FrameBound::CurrentRow),
        ),
    )
    .into_expr();
    let error = pg_err(&probe(backwards));
    assert!(matches!(error, Error::InvalidClause { .. }), "{error}");
}

#[test]
fn a_named_window_is_declared_once() {
    let statement = Select::from_table(TableRef::from_static("t"))
        .select_expr(
            WindowExpr::over_named(WindowFunc::Rank, [], Ident::from_static("w")).into_expr(),
        )
        .window(
            Ident::from_static("w"),
            WindowSpec::new().order_by(OrderTerm::asc(col("n"))),
        )
        .into_statement();
    let expected = r#"SELECT rank() OVER "w" FROM "t" WINDOW "w" AS (ORDER BY "n" ASC)"#;
    assert_eq!(pg(&statement).text, expected);
    assert_eq!(lite(&statement).text, expected);
}

// ── SELECT clauses ──────────────────────────────────────────────────────────

#[test]
fn a_select_with_no_projection_names_the_missing_call() {
    let error = pg_err(&Select::from_table(TableRef::from_static("t")).into_statement());
    assert!(matches!(error, Error::Incomplete { .. }), "{error}");
    assert!(error.to_string().contains("select_all"), "{error}");
}

#[test]
fn projections_render() {
    let statement = Select::from_table(TableRef::from_static("t"))
        .select_items([
            SelectItem::All,
            SelectItem::AllFrom(Ident::from_static("t")),
            SelectItem::column(ColumnRef::from_static("id")),
            SelectItem::aliased(Expr::value(1), Ident::from_static("one")),
        ])
        .into_statement();
    let expected = r#"SELECT *, "t".*, "id", $1 AS "one" FROM "t""#;
    assert_eq!(pg(&statement).text, expected);
    assert_eq!(
        lite(&statement).text,
        expected.replace("$1", "?"),
        "SQLite spells the placeholder differently and nothing else"
    );
}

#[test]
fn distinct_renders_and_distinct_on_is_postgres_only() {
    let plain = Select::from_table(TableRef::from_static("t"))
        .select_all()
        .distinct()
        .into_statement();
    let expected = r#"SELECT DISTINCT * FROM "t""#;
    assert_eq!(pg(&plain).text, expected);
    assert_eq!(lite(&plain).text, expected);

    let on = Select::from_table(TableRef::from_static("posts"))
        .select_all()
        .distinct_on([col("author_id")])
        .order_by(OrderTerm::asc(col("author_id")))
        .order_by(OrderTerm::desc(col("created_at")))
        .into_statement();
    assert_eq!(
        pg(&on).text,
        r#"SELECT DISTINCT ON ("author_id") * FROM "posts" ORDER BY "author_id" ASC, "created_at" DESC"#
    );
    let error = lite_err(&on);
    assert!(error.is_dialect_gap(), "{error}");
    assert!(error.to_string().contains("row_number"), "{error}");
}

#[test]
fn every_join_kind_renders() {
    let source = FromItem::table(TableRef::from_static("posts"));
    let on = Expr::column(TableRef::from_static("posts").column(Ident::from_static("author_id")))
        .eq(Expr::column(
            TableRef::from_static("users").column(Ident::from_static("id")),
        ));
    let cases = [
        (JoinKind::Inner, "INNER JOIN"),
        (JoinKind::Left, "LEFT JOIN"),
        (JoinKind::Right, "RIGHT JOIN"),
        (JoinKind::Full, "FULL JOIN"),
    ];
    for (kind, keyword) in cases {
        let statement = Select::from_table(TableRef::from_static("users"))
            .select_all()
            .join(Join::new(kind, source.clone(), on.clone()))
            .into_statement();
        let expected = format!(
            r#"SELECT * FROM "users" {keyword} "posts" ON "posts"."author_id" = "users"."id""#
        );
        assert_eq!(pg(&statement).text, expected);
        assert_eq!(lite(&statement).text, expected);
    }

    let cross = Select::from_table(TableRef::from_static("a"))
        .select_all()
        .cross_join(FromItem::table(TableRef::from_static("b")))
        .into_statement();
    assert_eq!(pg(&cross).text, r#"SELECT * FROM "a" CROSS JOIN "b""#);

    let using = Select::from_table(TableRef::from_static("a"))
        .select_all()
        .join(Join::using(
            JoinKind::Inner,
            FromItem::table(TableRef::from_static("b")),
            [Ident::from_static("id")],
        ))
        .into_statement();
    assert_eq!(
        pg(&using).text,
        r#"SELECT * FROM "a" INNER JOIN "b" USING ("id")"#
    );

    // A `USING` with no columns is a build-time error rather than a syntax one.
    let empty_using = Select::from_table(TableRef::from_static("a"))
        .select_all()
        .join(Join::using(
            JoinKind::Inner,
            FromItem::table(TableRef::from_static("b")),
            [],
        ))
        .into_statement();
    let error = pg_err(&empty_using);
    assert!(matches!(error, Error::Incomplete { .. }), "{error}");
}

#[test]
fn from_items_render_and_lateral_is_postgres_only() {
    let aliased = Select::new()
        .select_all()
        .from(FromItem::table_as(
            TableRef::from_static("users"),
            Ident::from_static("u"),
        ))
        .into_statement();
    assert_eq!(pg(&aliased).text, r#"SELECT * FROM "users" AS "u""#);

    let subquery = Select::new()
        .select_all()
        .from(FromItem::subquery(
            Select::from_table(TableRef::from_static("t")).select_all(),
            Ident::from_static("s"),
        ))
        .into_statement();
    assert_eq!(
        pg(&subquery).text,
        r#"SELECT * FROM (SELECT * FROM "t") AS "s""#
    );

    let lateral = Select::from_table(TableRef::from_static("users"))
        .select_all()
        .join(Join::new(
            JoinKind::Left,
            FromItem::lateral(
                Select::from_table(TableRef::from_static("posts"))
                    .select_all()
                    .limit(3),
                Ident::from_static("recent"),
            ),
            Expr::value(true),
        ))
        .into_statement();
    assert_eq!(
        pg(&lateral).text,
        r#"SELECT * FROM "users" LEFT JOIN LATERAL (SELECT * FROM "posts" LIMIT 3) AS "recent" ON $1"#
    );
    let error = lite_err(&lateral);
    assert!(error.is_dialect_gap(), "{error}");

    let only = Select::new()
        .select_all()
        .from(FromItem::only(TableRef::from_static("events")))
        .into_statement();
    assert_eq!(pg(&only).text, r#"SELECT * FROM ONLY "events""#);
    assert!(lite_err(&only).is_dialect_gap());

    let function = Select::new()
        .select_all()
        .from(FromItem::function(
            Function::custom(
                Ident::from_static("unnest"),
                [Expr::value(Array::of([1_i32]))],
            ),
            Some(Ident::from_static("u")),
        ))
        .into_statement();
    assert_eq!(pg(&function).text, r#"SELECT * FROM "unnest"($1) AS "u""#);
}

#[test]
fn a_values_list_is_a_table_on_postgres_and_needs_a_cte_on_sqlite() {
    let item = FromItem::values(
        [
            vec![Expr::value(1), Expr::value("a")],
            vec![Expr::value(2), Expr::value("b")],
        ],
        Ident::from_static("v"),
        [Ident::from_static("id"), Ident::from_static("name")],
    );
    let statement = Select::new().select_all().from(item).into_statement();
    let sql = pg(&statement);
    assert_eq!(
        sql.text,
        r#"SELECT * FROM (VALUES ($1, $2), ($3, $4)) AS "v"("id", "name")"#
    );
    assert_eq!(sql.args.len(), 4);
    let error = lite_err(&statement);
    assert!(error.is_dialect_gap(), "{error}");
    assert!(error.to_string().contains("CTE"), "{error}");

    // Without column names it is portable.
    let anonymous = Select::new()
        .select_all()
        .from(FromItem::values(
            [vec![Expr::value(1)]],
            Ident::from_static("v"),
            [],
        ))
        .into_statement();
    assert_eq!(
        lite(&anonymous).text,
        r#"SELECT * FROM (VALUES (?)) AS "v""#
    );

    // A ragged VALUES list is a build-time arity error.
    let ragged = Select::new()
        .select_all()
        .from(FromItem::values(
            [vec![Expr::value(1)], vec![Expr::value(1), Expr::value(2)]],
            Ident::from_static("v"),
            [],
        ))
        .into_statement();
    assert!(matches!(pg_err(&ragged), Error::RowArity { row: 1, .. }));
}

#[test]
fn group_by_having_order_limit_and_offset_render() {
    let statement = Select::from_table(TableRef::from_static("posts"))
        .select_expr(Aggregate::count_star().into_expr())
        .group_by(col("author_id"))
        .having(Aggregate::count_star().into_expr().gt(Expr::value(1)))
        .having(col("author_id").is_not_null())
        .order_by(OrderTerm::desc(col("at")).nulls_last())
        .limit(10)
        .offset(20)
        .into_statement();
    let expected = concat!(
        r#"SELECT count(*) FROM "posts" GROUP BY "author_id" "#,
        r#"HAVING count(*) > $1 AND "author_id" IS NOT NULL "#,
        r#"ORDER BY "at" DESC NULLS LAST LIMIT 10 OFFSET 20"#
    );
    assert_eq!(pg(&statement).text, expected);
    assert_eq!(lite(&statement).text, expected.replace("$1", "?"));
}

#[test]
fn sqlite_needs_a_limit_before_an_offset() {
    let statement = Select::from_table(TableRef::from_static("t"))
        .select_all()
        .offset(20)
        .into_statement();
    assert_eq!(pg(&statement).text, r#"SELECT * FROM "t" OFFSET 20"#);
    assert_eq!(
        lite(&statement).text,
        r#"SELECT * FROM "t" LIMIT -1 OFFSET 20"#,
        "SQLite's grammar has no bare OFFSET"
    );
}

#[test]
fn nulls_placement_renders_on_both_dialects() {
    let statement = Select::from_table(TableRef::from_static("t"))
        .select_all()
        .order_by(OrderTerm::asc(col("a")).nulls_first())
        .order_by(OrderTerm::desc(col("b")).nulls_last())
        .into_statement();
    let expected = r#"SELECT * FROM "t" ORDER BY "a" ASC NULLS FIRST, "b" DESC NULLS LAST"#;
    assert_eq!(pg(&statement).text, expected);
    assert_eq!(lite(&statement).text, expected);
}

#[test]
fn row_locks_are_postgres_only_and_refuse_the_combinations_the_server_does() {
    let cases = [
        (Lock::new(LockStrength::Update), "FOR UPDATE"),
        (Lock::new(LockStrength::NoKeyUpdate), "FOR NO KEY UPDATE"),
        (Lock::new(LockStrength::Share), "FOR SHARE"),
        (Lock::new(LockStrength::KeyShare), "FOR KEY SHARE"),
    ];
    for (lock, keyword) in cases {
        let statement = Select::from_table(TableRef::from_static("jobs"))
            .select_all()
            .lock(lock)
            .into_statement();
        assert_eq!(
            pg(&statement).text,
            format!(r#"SELECT * FROM "jobs" {keyword}"#)
        );
        assert!(lite_err(&statement).is_dialect_gap());
    }

    // The job-queue idiom.
    let claim = Select::from_table(TableRef::from_static("jobs"))
        .select_all()
        .filter(col("state").eq(Expr::value("ready")))
        .order_by(OrderTerm::asc(col("run_at")))
        .limit(1)
        .lock(
            Lock::new(LockStrength::Update)
                .of(TableRef::from_static("jobs"))
                .skip_locked(),
        )
        .into_statement();
    assert_eq!(
        pg(&claim).text,
        concat!(
            r#"SELECT * FROM "jobs" WHERE "state" = $1 ORDER BY "run_at" ASC LIMIT 1 "#,
            r#"FOR UPDATE OF "jobs" SKIP LOCKED"#
        )
    );

    assert_eq!(
        pg(&Select::from_table(TableRef::from_static("t"))
            .select_all()
            .lock(Lock::new(LockStrength::Update).nowait())
            .into_statement())
        .text,
        r#"SELECT * FROM "t" FOR UPDATE NOWAIT"#
    );

    // A lock over a grouped query has no single row to lock.
    let grouped = Select::from_table(TableRef::from_static("t"))
        .select_all()
        .group_by(col("a"))
        .lock(Lock::new(LockStrength::Update))
        .into_statement();
    let error = pg_err(&grouped);
    assert!(matches!(error, Error::InvalidClause { .. }), "{error}");
    assert!(error.to_string().contains("help:"), "{error}");
}

#[test]
fn set_operations_render_and_sqlite_refuses_the_two_it_lacks() {
    let left = Select::from_table(TableRef::from_static("a")).select_all();
    let right = Select::from_table(TableRef::from_static("b")).select_all();
    let cases = [
        (SetOp::Union, "UNION"),
        (SetOp::UnionAll, "UNION ALL"),
        (SetOp::Intersect, "INTERSECT"),
        (SetOp::Except, "EXCEPT"),
    ];
    for (op, keyword) in cases {
        let statement = left.clone().set_op(op, right.clone()).into_statement();
        let expected = format!(r#"SELECT * FROM "a" {keyword} SELECT * FROM "b""#);
        assert_eq!(pg(&statement).text, expected);
        assert_eq!(lite(&statement).text, expected);
    }
    for op in [SetOp::IntersectAll, SetOp::ExceptAll] {
        let statement = left.clone().set_op(op, right.clone()).into_statement();
        assert!(pg(&statement).text.contains("ALL"));
        assert!(lite_err(&statement).is_dialect_gap());
    }

    // Ordering and limiting belong to the compound query, not to a branch.
    let ordered = left
        .clone()
        .union(right.clone().limit(1))
        .order_by(OrderTerm::asc(col("id")))
        .limit(10)
        .into_statement();
    let error = pg_err(&ordered);
    assert!(matches!(error, Error::InvalidClause { .. }), "{error}");

    let whole = left
        .union_all(right)
        .order_by(OrderTerm::asc(col("id")))
        .limit(10)
        .into_statement();
    assert_eq!(
        pg(&whole).text,
        r#"SELECT * FROM "a" UNION ALL SELECT * FROM "b" ORDER BY "id" ASC LIMIT 10"#
    );
}

#[test]
fn common_table_expressions_render() {
    let recent = Cte::new(
        Ident::from_static("recent"),
        Select::from_table(TableRef::from_static("posts"))
            .select_all()
            .limit(100),
    )
    .columns([Ident::from_static("id"), Ident::from_static("title")])
    .materialized(false);
    let statement = Select::from_table(TableRef::from_static("recent"))
        .select_all()
        .with(recent)
        .into_statement();
    let expected = concat!(
        r#"WITH "recent"("id", "title") AS NOT MATERIALIZED "#,
        r#"(SELECT * FROM "posts" LIMIT 100) SELECT * FROM "recent""#
    );
    assert_eq!(pg(&statement).text, expected);
    assert_eq!(lite(&statement).text, expected);

    let recursive = Select::from_table(TableRef::from_static("tree"))
        .select_all()
        .with(Cte::new(
            Ident::from_static("tree"),
            Select::from_table(TableRef::from_static("nodes")).select_all(),
        ))
        .recursive(true)
        .into_statement();
    assert!(pg(&recursive).text.starts_with("WITH RECURSIVE "));
    assert!(lite(&recursive).text.starts_with("WITH RECURSIVE "));
}

#[test]
fn a_data_modifying_cte_is_postgres_only() {
    let moved = Cte::from_statement(
        Ident::from_static("deleted"),
        Delete::from_table(TableRef::from_static("stale"))
            .filter(col("at").lt(Expr::value(0_i64)))
            .returning(Returning::All)
            .into_statement(),
    );
    let statement = Insert::into_table(TableRef::from_static("archive"))
        .columns([Ident::from_static("id")])
        .from_select(
            Select::from_table(TableRef::from_static("deleted"))
                .select_column(ColumnRef::from_static("id")),
        )
        .with(moved)
        .into_statement();
    assert_eq!(
        pg(&statement).text,
        concat!(
            r#"WITH "deleted" AS (DELETE FROM "stale" WHERE "at" < $1 RETURNING *) "#,
            r#"INSERT INTO "archive" ("id") SELECT "id" FROM "deleted""#
        )
    );
    let error = lite_err(&statement);
    assert!(error.is_dialect_gap(), "{error}");
}

// ── INSERT ──────────────────────────────────────────────────────────────────

#[test]
fn a_multi_row_insert_is_one_statement() {
    let statement = Insert::into_table(TableRef::from_static("users"))
        .columns([Ident::from_static("email"), Ident::from_static("name")])
        .values([Expr::value("a@example.com"), Expr::value("Ada")])
        .values([Expr::value("g@example.com"), Expr::value("Grace")])
        .into_statement();
    let sql = pg(&statement);
    assert_eq!(
        sql.text,
        r#"INSERT INTO "users" ("email", "name") VALUES ($1, $2), ($3, $4)"#
    );
    assert_eq!(sql.args.len(), 4);
    assert_eq!(
        lite(&statement).text,
        r#"INSERT INTO "users" ("email", "name") VALUES (?, ?), (?, ?)"#
    );
}

#[test]
fn a_row_that_does_not_match_the_column_list_names_the_row() {
    let statement = Insert::into_table(TableRef::from_static("t"))
        .columns([Ident::from_static("a"), Ident::from_static("b")])
        .values([Expr::value(1), Expr::value(2)])
        .values([Expr::value(3)])
        .into_statement();
    let error = pg_err(&statement);
    assert!(
        matches!(
            error,
            Error::RowArity {
                row: 1,
                expected: 2,
                found: 1
            }
        ),
        "{error}"
    );
    assert!(error.to_string().contains("help:"));
}

#[test]
fn an_insert_with_nothing_to_insert_names_the_missing_call() {
    let error = pg_err(&Insert::into_table(TableRef::from_static("t")).into_statement());
    assert!(matches!(error, Error::Incomplete { .. }), "{error}");
    assert!(error.to_string().contains("default_values"), "{error}");
}

#[test]
fn insert_default_values_and_insert_select_render() {
    assert_eq!(
        pg(&Insert::into_table(TableRef::from_static("t"))
            .default_values()
            .into_statement())
        .text,
        r#"INSERT INTO "t" DEFAULT VALUES"#
    );
    assert_eq!(
        pg(&Insert::into_table(TableRef::from_static("archive"))
            .columns([Ident::from_static("id")])
            .from_select(
                Select::from_table(TableRef::from_static("t"))
                    .select_column(ColumnRef::from_static("id"))
            )
            .into_statement())
        .text,
        r#"INSERT INTO "archive" ("id") SELECT "id" FROM "t""#
    );
    // `DEFAULT` in a value list.
    assert_eq!(
        pg(&Insert::into_table(TableRef::from_static("t"))
            .columns([Ident::from_static("a"), Ident::from_static("b")])
            .values([Expr::value(1), Expr::Default])
            .into_statement())
        .text,
        r#"INSERT INTO "t" ("a", "b") VALUES ($1, DEFAULT)"#
    );
}

#[test]
fn on_conflict_renders_in_every_form() {
    let ignore = Insert::into_table(TableRef::from_static("users"))
        .columns([Ident::from_static("email")])
        .values([Expr::value("a@example.com")])
        .on_conflict(OnConflict::columns([Ident::from_static("email")]).do_nothing())
        .into_statement();
    let expected = r#"INSERT INTO "users" ("email") VALUES ($1) ON CONFLICT ("email") DO NOTHING"#;
    assert_eq!(pg(&ignore).text, expected);
    assert_eq!(lite(&ignore).text, expected.replace("$1", "?"));

    let any = Insert::into_table(TableRef::from_static("users"))
        .default_values()
        .on_conflict(OnConflict::any().do_nothing())
        .into_statement();
    assert_eq!(
        pg(&any).text,
        r#"INSERT INTO "users" DEFAULT VALUES ON CONFLICT DO NOTHING"#
    );

    let upsert = Insert::into_table(TableRef::from_static("users"))
        .columns([Ident::from_static("email"), Ident::from_static("name")])
        .values([Expr::value("a@example.com"), Expr::value("Ada")])
        .on_conflict(
            OnConflict::columns([Ident::from_static("email")])
                .target_where(col("deleted_at").is_null())
                .do_update_columns([Ident::from_static("name")])
                .update_where(col("users").ne(Expr::excluded(Ident::from_static("name")))),
        )
        .returning(Returning::All)
        .into_statement();
    assert_eq!(
        pg(&upsert).text,
        concat!(
            r#"INSERT INTO "users" ("email", "name") VALUES ($1, $2) "#,
            r#"ON CONFLICT ("email") WHERE "deleted_at" IS NULL "#,
            r#"DO UPDATE SET "name" = "excluded"."name" "#,
            r#"WHERE "users" <> "excluded"."name" RETURNING *"#
        )
    );

    let named = Insert::into_table(TableRef::from_static("users"))
        .default_values()
        .on_conflict(
            OnConflict::constraint(Ident::from_static("users_email_key"))
                .do_update_columns([Ident::from_static("name")]),
        )
        .into_statement();
    assert_eq!(
        pg(&named).text,
        concat!(
            r#"INSERT INTO "users" DEFAULT VALUES "#,
            r#"ON CONFLICT ON CONSTRAINT "users_email_key" DO UPDATE SET "name" = "excluded"."name""#
        )
    );
    assert!(lite_err(&named).is_dialect_gap());

    // `DO UPDATE` with no target is a server error; it is caught here instead.
    let targetless = Insert::into_table(TableRef::from_static("t"))
        .default_values()
        .on_conflict(OnConflict::any().do_update_columns([Ident::from_static("a")]))
        .into_statement();
    let error = pg_err(&targetless);
    assert!(matches!(error, Error::InvalidClause { .. }), "{error}");
}

#[test]
fn returning_renders_in_both_forms() {
    let all = Insert::into_table(TableRef::from_static("t"))
        .default_values()
        .returning(Returning::All)
        .into_statement();
    assert_eq!(
        pg(&all).text,
        r#"INSERT INTO "t" DEFAULT VALUES RETURNING *"#
    );
    let columns = Update::table(TableRef::from_static("t"))
        .set(Ident::from_static("a"), Expr::value(1))
        .filter(col("id").eq(Expr::value(2)))
        .returning(Returning::columns([
            ColumnRef::from_static("id"),
            ColumnRef::from_static("a"),
        ]))
        .into_statement();
    let expected = r#"UPDATE "t" SET "a" = $1 WHERE "id" = $2 RETURNING "id", "a""#;
    assert_eq!(pg(&columns).text, expected);
    assert_eq!(
        lite(&columns).text,
        expected.replace("$1", "?").replace("$2", "?")
    );

    let empty = Insert::into_table(TableRef::from_static("t"))
        .default_values()
        .returning(Returning::items([]))
        .into_statement();
    assert!(matches!(pg_err(&empty), Error::Incomplete { .. }));
}

// ── UPDATE and DELETE ───────────────────────────────────────────────────────

#[test]
fn update_renders_with_from_and_the_atomic_increment_idiom() {
    let statement = Update::table(TableRef::from_static("users"))
        .alias(Ident::from_static("u"))
        .set_with(Ident::from_static("login_count"), |current| {
            current.plus(Expr::value(1))
        })
        .set(Ident::from_static("name"), Expr::value("Ada"))
        .from(FromItem::table(TableRef::from_static("audit")))
        .filter(col("id").eq(Expr::value(1)))
        .into_statement();
    let expected = concat!(
        r#"UPDATE "users" AS "u" SET "login_count" = "login_count" + $1, "name" = $2 "#,
        r#"FROM "audit" WHERE "id" = $3"#
    );
    assert_eq!(pg(&statement).text, expected);
    assert!(lite(&statement).text.contains(r#"FROM "audit""#));
}

#[test]
fn an_update_with_nothing_to_set_names_the_missing_call() {
    let error = pg_err(&Update::table(TableRef::from_static("t")).into_statement());
    assert!(matches!(error, Error::Incomplete { .. }), "{error}");
    assert!(error.to_string().contains(".set("), "{error}");
}

#[test]
fn delete_renders_and_using_is_postgres_only() {
    let plain = Delete::from_table(TableRef::from_static("sessions"))
        .filter(col("expires_at").lt(Expr::value(0_i64)))
        .returning(Returning::All)
        .into_statement();
    let expected = r#"DELETE FROM "sessions" WHERE "expires_at" < $1 RETURNING *"#;
    assert_eq!(pg(&plain).text, expected);
    assert_eq!(lite(&plain).text, expected.replace("$1", "?"));

    let using = Delete::from_table(TableRef::from_static("a"))
        .alias(Ident::from_static("x"))
        .using(FromItem::table(TableRef::from_static("b")))
        .filter(Expr::value(true))
        .into_statement();
    assert_eq!(
        pg(&using).text,
        r#"DELETE FROM "a" AS "x" USING "b" WHERE $1"#
    );
    let error = lite_err(&using);
    assert!(error.is_dialect_gap(), "{error}");
    assert!(error.to_string().contains("IN (SELECT"), "{error}");
}

// ── raw SQL ─────────────────────────────────────────────────────────────────

#[test]
fn a_raw_fragment_renumbers_its_placeholders_and_never_interpolates() {
    let statement = Select::from_table(TableRef::from_static("t"))
        .select_all()
        .filter(col("id").eq(Expr::value(7)))
        .filter(Expr::raw(
            RawExpr::new("created_at > now() - ?::interval").bind("1 day"),
        ))
        .into_statement();
    let sql = pg(&statement);
    assert_eq!(
        sql.text,
        r#"SELECT * FROM "t" WHERE "id" = $1 AND (created_at > now() - $2::interval)"#
    );
    assert_eq!(sql.args, vec![Value::I32(7), Value::text("1 day")]);
    assert!(!sql.text.contains("1 day"));

    assert_eq!(
        lite(&statement).text,
        r#"SELECT * FROM "t" WHERE "id" = ? AND (created_at > now() - ?::interval)"#
    );
}

#[test]
fn a_doubled_question_mark_is_a_literal_one() {
    let statement = RawStatement::new("select ?? , ?").bind(1).into_statement();
    assert_eq!(pg(&statement).text, "select ? , $1");
    assert_eq!(lite(&statement).text, "select ? , ?");
}

#[test]
fn a_raw_fragment_with_the_wrong_number_of_values_is_caught_at_build_time() {
    let statement = RawStatement::new("select * from t where a = ? and b = ?")
        .bind(1)
        .into_statement();
    let error = pg_err(&statement);
    assert!(
        matches!(
            error,
            Error::RawArity {
                expected: 2,
                found: 1,
                ..
            }
        ),
        "{error}"
    );
    assert!(error.to_string().contains("help:"));
}

#[test]
fn a_raw_fragment_keeps_its_multibyte_text_intact() {
    let statement = RawStatement::new("select 'héllo → wörld', ?")
        .bind(1)
        .into_statement();
    assert_eq!(pg(&statement).text, "select 'héllo → wörld', $1");
}

// ── DDL ─────────────────────────────────────────────────────────────────────

/// The `users` table the DDL tests build on.
fn users_table() -> CreateTable {
    CreateTable::new(TableRef::from_static("users"))
        .if_not_exists()
        .column(ColumnSpec::new(Ident::from_static("id"), DataType::Uuid).primary_key())
        .column(
            ColumnSpec::new(Ident::from_static("email"), DataType::Text)
                .not_null()
                .unique(),
        )
        .column(
            ColumnSpec::new(
                Ident::from_static("created_at"),
                DataType::Timestamp {
                    with_time_zone: true,
                },
            )
            .not_null()
            .default(Expr::Function(Function::Now)),
        )
}

#[test]
fn create_table_renders_on_both_dialects() {
    let statement = Ddl::CreateTable(users_table()).into_statement();
    assert_eq!(
        pg(&statement).text,
        concat!(
            r#"CREATE TABLE IF NOT EXISTS "users" ("#,
            r#""id" uuid PRIMARY KEY, "#,
            r#""email" text NOT NULL UNIQUE, "#,
            r#""created_at" timestamptz NOT NULL DEFAULT (now()))"#
        )
    );
    assert_eq!(
        lite(&statement).text,
        concat!(
            r#"CREATE TABLE IF NOT EXISTS "users" ("#,
            r#""id" text PRIMARY KEY, "#,
            r#""email" text NOT NULL UNIQUE, "#,
            r#""created_at" text NOT NULL DEFAULT (CURRENT_TIMESTAMP))"#
        )
    );
    assert!(
        pg(&statement).args.is_empty(),
        "DDL binds no parameters: the catalogue stores the text"
    );
}

#[test]
fn a_ddl_default_is_a_literal_rather_than_a_placeholder() {
    let statement = Ddl::CreateTable(
        CreateTable::new(TableRef::from_static("t"))
            .column(
                ColumnSpec::new(Ident::from_static("flag"), DataType::Boolean)
                    .default(Expr::value(false)),
            )
            .column(
                ColumnSpec::new(Ident::from_static("n"), DataType::Integer)
                    .default(Expr::value(42_i32)),
            )
            .column(
                ColumnSpec::new(Ident::from_static("label"), DataType::Text)
                    .default(Expr::value("it's fine")),
            )
            .column(
                ColumnSpec::new(Ident::from_static("ratio"), DataType::DoublePrecision)
                    .default(Expr::value(1.0_f64)),
            )
            .column(
                ColumnSpec::new(Ident::from_static("blob"), DataType::Bytea)
                    .default(Expr::value(vec![0xDE_u8, 0xAD])),
            ),
    )
    .into_statement();
    let sql = pg(&statement);
    assert!(sql.args.is_empty());
    assert!(
        sql.text.contains(r#""flag" boolean DEFAULT FALSE"#),
        "{}",
        sql.text
    );
    assert!(
        sql.text.contains(r#""n" integer DEFAULT 42"#),
        "{}",
        sql.text
    );
    assert!(
        sql.text.contains(r#""label" text DEFAULT 'it''s fine'"#),
        "a quote in a literal is doubled, not backslash-escaped: {}",
        sql.text
    );
    assert!(
        sql.text.contains(r#""ratio" double precision DEFAULT 1.0"#),
        "{}",
        sql.text
    );
    assert!(
        sql.text.contains(r#""blob" bytea DEFAULT '\xDEAD'"#),
        "{}",
        sql.text
    );
    let sqlite = lite(&statement);
    assert!(
        sqlite.text.contains(r#""blob" blob DEFAULT X'DEAD'"#),
        "{}",
        sqlite.text
    );
}

#[test]
fn the_column_modifiers_render() {
    let statement = Ddl::CreateTable(
        CreateTable::new(TableRef::from_static("docs"))
            .column(
                ColumnSpec::new(Ident::from_static("id"), DataType::BigInt)
                    .not_null()
                    .identity(Identity::Always),
            )
            .column(
                ColumnSpec::new(Ident::from_static("title"), DataType::Text)
                    .collate(Ident::from_static("C"))
                    .check(
                        Expr::Function(Function::Length(Box::new(col("title")))).gt(Expr::value(0)),
                    ),
            )
            .column(
                ColumnSpec::new(Ident::from_static("search"), DataType::TsVector).generated(
                    Generated::stored(Expr::Function(Function::ToTsVector {
                        config: Some(Ident::from_static("english")),
                        document: Box::new(col("title")),
                    })),
                ),
            )
            .column(
                ColumnSpec::new(Ident::from_static("author_id"), DataType::Uuid).references(
                    ForeignKey::new(
                        Some(Ident::from_static("docs_author_fk")),
                        [Ident::from_static("author_id")],
                        TableRef::from_static("users"),
                        [Ident::from_static("id")],
                    )
                    .on_delete(ReferentialAction::Cascade)
                    .on_update(ReferentialAction::Restrict)
                    .deferrable(true),
                ),
            ),
    )
    .into_statement();
    let text = pg(&statement).text;
    assert!(
        text.contains(r#""id" bigint GENERATED ALWAYS AS IDENTITY NOT NULL"#),
        "{text}"
    );
    assert!(
        text.contains(r#""title" text COLLATE "C" CHECK (length("title") > 0)"#),
        "{text}"
    );
    assert!(
        text.contains(
            r#""search" tsvector GENERATED ALWAYS AS (to_tsvector('english', "title")) STORED"#
        ),
        "{text}"
    );
    assert!(
        text.contains(concat!(
            r#""author_id" uuid CONSTRAINT "docs_author_fk" REFERENCES "users" ("id") "#,
            r#"ON DELETE CASCADE ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED"#
        )),
        "{text}"
    );
}

#[test]
fn an_inline_foreign_key_that_is_not_about_its_own_column_is_refused() {
    let statement = Ddl::CreateTable(CreateTable::new(TableRef::from_static("t")).column(
        ColumnSpec::new(Ident::from_static("a"), DataType::Uuid).references(ForeignKey::new(
            None,
            [Ident::from_static("b")],
            TableRef::from_static("u"),
            [Ident::from_static("id")],
        )),
    ))
    .into_statement();
    let error = pg_err(&statement);
    assert!(matches!(error, Error::InvalidClause { .. }), "{error}");
    assert!(error.to_string().contains("TableConstraint"), "{error}");
}

#[test]
fn table_constraints_render() {
    let statement = Ddl::CreateTable(
        CreateTable::new(TableRef::from_static("orders"))
            .column(ColumnSpec::new(
                Ident::from_static("order_id"),
                DataType::BigInt,
            ))
            .column(ColumnSpec::new(
                Ident::from_static("line_no"),
                DataType::Integer,
            ))
            .constraint(TableConstraint::primary_key(
                Some(Ident::from_static("orders_pkey")),
                [
                    Ident::from_static("order_id"),
                    Ident::from_static("line_no"),
                ],
            ))
            .constraint(TableConstraint::Unique {
                name: None,
                columns: vec![Ident::from_static("order_id")],
                nulls_not_distinct: true,
            })
            .constraint(TableConstraint::check(
                Some(Ident::from_static("orders_line_no_check")),
                col("line_no").gt(Expr::value(0)),
            ))
            .constraint(TableConstraint::ForeignKey(ForeignKey::new(
                None,
                [Ident::from_static("order_id")],
                TableRef::from_static("order_headers"),
                [Ident::from_static("id")],
            ))),
    )
    .into_statement();
    let text = pg(&statement).text;
    assert!(
        text.contains(r#"CONSTRAINT "orders_pkey" PRIMARY KEY ("order_id", "line_no")"#),
        "{text}"
    );
    assert!(
        text.contains(r#"UNIQUE NULLS NOT DISTINCT ("order_id")"#),
        "{text}"
    );
    assert!(
        text.contains(r#"CONSTRAINT "orders_line_no_check" CHECK ("line_no" > 0)"#),
        "{text}"
    );
    assert!(
        text.contains(r#"FOREIGN KEY ("order_id") REFERENCES "order_headers" ("id")"#),
        "{text}"
    );
    // `NULLS NOT DISTINCT` is the one part SQLite has no answer for.
    assert!(lite_err(&statement).is_dialect_gap());
}

#[test]
fn an_exclusion_constraint_renders_on_postgres_only() {
    let statement = Ddl::CreateTable(
        CreateTable::new(TableRef::from_static("bookings"))
            .column(ColumnSpec::new(
                Ident::from_static("room"),
                DataType::Integer,
            ))
            .constraint(TableConstraint::Exclude {
                name: Some(Ident::from_static("bookings_no_overlap")),
                method: Some(Ident::from_static("gist")),
                elements: vec![
                    (col("room"), Ident::from_static("=")),
                    (col("during"), Ident::from_static("&&")),
                ],
                predicate: Some(col("cancelled_at").is_null()),
            }),
    )
    .into_statement();
    assert!(
        pg(&statement).text.contains(concat!(
            r#"CONSTRAINT "bookings_no_overlap" EXCLUDE USING "gist" "#,
            r#"("room" WITH =, "during" WITH &&) WHERE ("cancelled_at" IS NULL)"#
        )),
        "{}",
        pg(&statement).text
    );
    assert!(lite_err(&statement).is_dialect_gap());
}

#[test]
fn a_table_comment_becomes_a_following_statement_on_postgres() {
    let statement = Ddl::CreateTable(
        CreateTable::new(TableRef::from_static("users"))
            .comment("Everyone who can sign in.")
            .column(
                ColumnSpec::new(Ident::from_static("email"), DataType::Text)
                    .comment("The handle they sign in with."),
            ),
    )
    .into_statement();
    assert_eq!(
        pg(&statement).text,
        concat!(
            "CREATE TABLE \"users\" (\"email\" text);\n",
            "COMMENT ON TABLE \"users\" IS 'Everyone who can sign in.';\n",
            "COMMENT ON COLUMN \"users\".\"email\" IS 'The handle they sign in with.'"
        )
    );
    // SQLite has no comment catalogue. A comment carries no semantics, so it is
    // dropped rather than refused — the one place in the crate where a clause
    // silently disappears, and it is asserted here so it stays deliberate.
    assert_eq!(
        lite(&statement).text,
        r#"CREATE TABLE "users" ("email" text)"#
    );
}

#[test]
fn partitioning_renders_on_postgres_only() {
    let statement = Ddl::CreateTable(
        CreateTable::new(TableRef::from_static("events"))
            .column(ColumnSpec::new(
                Ident::from_static("created_at"),
                DataType::Timestamp {
                    with_time_zone: true,
                },
            ))
            .partition_by(Partitioning::new(
                PartitionStrategy::Range,
                [Ident::from_static("created_at")],
            )),
    )
    .into_statement();
    assert!(
        pg(&statement)
            .text
            .ends_with(r#"PARTITION BY RANGE ("created_at")"#),
        "{}",
        pg(&statement).text
    );
    assert!(lite_err(&statement).is_dialect_gap());
    for strategy in [
        PartitionStrategy::List,
        PartitionStrategy::Hash,
        PartitionStrategy::Range,
    ] {
        let statement = Ddl::CreateTable(
            CreateTable::new(TableRef::from_static("t"))
                .column(ColumnSpec::new(Ident::from_static("a"), DataType::Integer))
                .partition_by(Partitioning::new(strategy, [Ident::from_static("a")])),
        )
        .into_statement();
        assert!(pg(&statement).text.contains("PARTITION BY"));
    }
}

#[test]
fn a_temporary_or_unlogged_table_renders() {
    let temporary = Ddl::CreateTable(
        CreateTable::new(TableRef::from_static("scratch"))
            .temporary()
            .column(ColumnSpec::new(Ident::from_static("a"), DataType::Integer)),
    )
    .into_statement();
    assert!(pg(&temporary).text.starts_with("CREATE TEMPORARY TABLE "));
    assert!(lite(&temporary).text.starts_with("CREATE TEMPORARY TABLE "));

    let unlogged = Ddl::CreateTable(
        CreateTable::new(TableRef::from_static("scratch"))
            .unlogged()
            .column(ColumnSpec::new(Ident::from_static("a"), DataType::Integer)),
    )
    .into_statement();
    assert!(pg(&unlogged).text.starts_with("CREATE UNLOGGED TABLE "));
    assert!(lite_err(&unlogged).is_dialect_gap());
}

#[test]
fn alter_table_groups_its_actions_on_postgres_and_splits_them_on_sqlite() {
    let alter = AlterTable::new(TableRef::from_static("users"))
        .add_column(ColumnSpec::new(
            Ident::from_static("locale"),
            DataType::Text,
        ))
        .add_column(ColumnSpec::new(
            Ident::from_static("timezone"),
            DataType::Text,
        ));
    let statement = Ddl::AlterTable(alter).into_statement();
    assert_eq!(
        pg(&statement).text,
        r#"ALTER TABLE "users" ADD COLUMN "locale" text, ADD COLUMN "timezone" text"#,
        "one statement means one lock"
    );
    assert_eq!(
        lite(&statement).text,
        concat!(
            "ALTER TABLE \"users\" ADD COLUMN \"locale\" text;\n",
            "ALTER TABLE \"users\" ADD COLUMN \"timezone\" text"
        ),
        "SQLite takes exactly one action per ALTER TABLE"
    );
}

/// PostgreSQL's `RENAME`, `SET SCHEMA` and `ATTACH`/`DETACH PARTITION` are
/// separate statement *forms*, not entries in the comma-separated action list.
///
/// Mixing them answers `syntax error at or near "RENAME"`, pointing at the
/// keyword rather than at the mistake. This test exists because the live
/// PostgreSQL leg found exactly that — a snapshot alone would have been happy.
#[test]
fn a_rename_is_cut_out_of_the_action_list_into_its_own_statement() {
    let statement = Ddl::AlterTable(
        AlterTable::new(TableRef::from_static("t"))
            .add_column(ColumnSpec::new(Ident::from_static("added"), DataType::Text))
            .drop_column(Ident::from_static("gone"))
            .action(AlterTableAction::RenameColumn {
                from: Ident::from_static("b"),
                to: Ident::from_static("renamed"),
            })
            .action(AlterTableAction::SetNotNull(Ident::from_static("added")))
            .action(AlterTableAction::DropDefault(Ident::from_static("added")))
            .action(AlterTableAction::SetSchema(Ident::from_static("archive"))),
    )
    .into_statement();
    assert_eq!(
        pg(&statement).text,
        concat!(
            "ALTER TABLE \"t\" ADD COLUMN \"added\" text, DROP COLUMN \"gone\";\n",
            "ALTER TABLE \"t\" RENAME COLUMN \"b\" TO \"renamed\";\n",
            "ALTER TABLE \"t\" ALTER COLUMN \"added\" SET NOT NULL, ",
            "ALTER COLUMN \"added\" DROP DEFAULT;\n",
            "ALTER TABLE \"t\" SET SCHEMA \"archive\""
        ),
        "the list-able runs stay grouped so the lock is taken once each"
    );
}

#[test]
fn the_zero_downtime_alter_table_actions_render() {
    let statement = Ddl::AlterTable(
        AlterTable::new(TableRef::from_static("users"))
            .action(AlterTableAction::AddConstraint(
                TableConstraint::ForeignKey(
                    ForeignKey::new(
                        Some(Ident::from_static("fk")),
                        [Ident::from_static("author_id")],
                        TableRef::from_static("users"),
                        [Ident::from_static("id")],
                    )
                    .not_valid(),
                ),
            ))
            .action(AlterTableAction::ValidateConstraint(Ident::from_static(
                "fk",
            )))
            .action(AlterTableAction::AddUniqueUsingIndex {
                name: Some(Ident::from_static("users_email_key")),
                index: Ident::from_static("idx_users_email"),
            })
            .action(AlterTableAction::AddPrimaryKeyUsingIndex {
                name: None,
                index: Ident::from_static("idx_users_pkey"),
            }),
    )
    .into_statement();
    assert_eq!(
        pg(&statement).text,
        concat!(
            r#"ALTER TABLE "users" "#,
            r#"ADD CONSTRAINT "fk" FOREIGN KEY ("author_id") REFERENCES "users" ("id") NOT VALID, "#,
            r#"VALIDATE CONSTRAINT "fk", "#,
            r#"ADD CONSTRAINT "users_email_key" UNIQUE USING INDEX "idx_users_email", "#,
            r#"ADD PRIMARY KEY USING INDEX "idx_users_pkey""#
        )
    );
    assert!(lite_err(&statement).is_dialect_gap());
}

#[test]
fn the_remaining_alter_table_actions_render() {
    let cases: [(AlterTableAction, &str); 9] = [
        (
            AlterTableAction::DropColumn {
                name: Ident::from_static("c"),
                if_exists: true,
                cascade: true,
            },
            r#"DROP COLUMN IF EXISTS "c" CASCADE"#,
        ),
        (
            AlterTableAction::RenameColumn {
                from: Ident::from_static("a"),
                to: Ident::from_static("b"),
            },
            r#"RENAME COLUMN "a" TO "b""#,
        ),
        (
            AlterTableAction::AlterColumnType {
                name: Ident::from_static("id"),
                data_type: DataType::Text,
                using: Some(col("id").cast(DataType::Text)),
                lossy: true,
            },
            r#"ALTER COLUMN "id" TYPE text USING CAST("id" AS text)"#,
        ),
        (
            AlterTableAction::SetNotNull(Ident::from_static("c")),
            r#"ALTER COLUMN "c" SET NOT NULL"#,
        ),
        (
            AlterTableAction::DropNotNull(Ident::from_static("c")),
            r#"ALTER COLUMN "c" DROP NOT NULL"#,
        ),
        (
            AlterTableAction::SetDefault {
                name: Ident::from_static("c"),
                value: Expr::value(0),
            },
            r#"ALTER COLUMN "c" SET DEFAULT 0"#,
        ),
        (
            AlterTableAction::DropDefault(Ident::from_static("c")),
            r#"ALTER COLUMN "c" DROP DEFAULT"#,
        ),
        (
            AlterTableAction::DropConstraint {
                name: Ident::from_static("k"),
                if_exists: true,
                cascade: false,
            },
            r#"DROP CONSTRAINT IF EXISTS "k""#,
        ),
        (
            AlterTableAction::RenameConstraint {
                from: Ident::from_static("a"),
                to: Ident::from_static("b"),
            },
            r#"RENAME CONSTRAINT "a" TO "b""#,
        ),
    ];
    for (action, expected) in cases {
        let statement = Ddl::AlterTable(AlterTable::new(TableRef::from_static("t")).action(action))
            .into_statement();
        assert_eq!(
            pg(&statement).text,
            format!(r#"ALTER TABLE "t" {expected}"#)
        );
    }

    // Partitions.
    let attach = Ddl::AlterTable(AlterTable::new(TableRef::from_static("events")).action(
        AlterTableAction::AttachPartition {
            partition: TableRef::from_static("events_2026_07"),
            bounds: "FOR VALUES FROM ('2026-07-01') TO ('2026-08-01')".to_owned(),
        },
    ))
    .into_statement();
    assert_eq!(
        pg(&attach).text,
        concat!(
            r#"ALTER TABLE "events" ATTACH PARTITION "events_2026_07" "#,
            "FOR VALUES FROM ('2026-07-01') TO ('2026-08-01')"
        )
    );
    let detach = Ddl::AlterTable(AlterTable::new(TableRef::from_static("events")).action(
        AlterTableAction::DetachPartition {
            partition: TableRef::from_static("events_2026_07"),
            concurrently: true,
        },
    ))
    .into_statement();
    assert_eq!(
        pg(&detach).text,
        r#"ALTER TABLE "events" DETACH PARTITION "events_2026_07" CONCURRENTLY"#
    );

    let set_schema = Ddl::AlterTable(
        AlterTable::new(TableRef::from_static("t"))
            .action(AlterTableAction::SetSchema(Ident::from_static("archive"))),
    )
    .into_statement();
    assert_eq!(
        pg(&set_schema).text,
        r#"ALTER TABLE "t" SET SCHEMA "archive""#
    );
}

/// SQLite's `ALTER TABLE` can rename, add and drop a column and nothing else.
/// Everything else points at the table-rebuild recipe, which `moso-migrate`
/// owns because it is the only layer that holds the whole target schema.
#[test]
fn sqlite_points_at_the_table_rebuild_for_what_alter_table_cannot_do() {
    let unsupported = [
        AlterTableAction::AlterColumnType {
            name: Ident::from_static("c"),
            data_type: DataType::BigInt,
            using: None,
            lossy: false,
        },
        AlterTableAction::SetNotNull(Ident::from_static("c")),
        AlterTableAction::DropNotNull(Ident::from_static("c")),
        AlterTableAction::SetDefault {
            name: Ident::from_static("c"),
            value: Expr::value(0),
        },
        AlterTableAction::DropDefault(Ident::from_static("c")),
        AlterTableAction::AddConstraint(TableConstraint::check(None, Expr::value(true))),
        AlterTableAction::DropConstraint {
            name: Ident::from_static("k"),
            if_exists: false,
            cascade: false,
        },
        AlterTableAction::ValidateConstraint(Ident::from_static("k")),
        AlterTableAction::RenameConstraint {
            from: Ident::from_static("a"),
            to: Ident::from_static("b"),
        },
        AlterTableAction::AddUniqueUsingIndex {
            name: None,
            index: Ident::from_static("i"),
        },
        AlterTableAction::SetSchema(Ident::from_static("s")),
    ];
    for action in unsupported {
        let statement =
            Ddl::AlterTable(AlterTable::new(TableRef::from_static("t")).action(action.clone()))
                .into_statement();
        let error = lite_err(&statement);
        assert!(error.is_dialect_gap(), "{action:?}: {error}");
        assert!(error.to_string().contains("help:"), "{action:?}: {error}");
    }

    // The three it can do.
    for (action, expected) in [
        (
            AlterTableAction::AddColumn {
                column: Box::new(ColumnSpec::new(Ident::from_static("c"), DataType::Text)),
                if_not_exists: false,
            },
            r#"ADD COLUMN "c" text"#,
        ),
        (
            AlterTableAction::DropColumn {
                name: Ident::from_static("c"),
                if_exists: false,
                cascade: false,
            },
            r#"DROP COLUMN "c""#,
        ),
        (
            AlterTableAction::RenameColumn {
                from: Ident::from_static("a"),
                to: Ident::from_static("b"),
            },
            r#"RENAME COLUMN "a" TO "b""#,
        ),
    ] {
        let statement = Ddl::AlterTable(AlterTable::new(TableRef::from_static("t")).action(action))
            .into_statement();
        assert_eq!(
            lite(&statement).text,
            format!(r#"ALTER TABLE "t" {expected}"#)
        );
    }

    // And the recipe is named where a reader will look for it.
    let statement = Ddl::AlterTable(AlterTable::new(TableRef::from_static("t")).action(
        AlterTableAction::AlterColumnType {
            name: Ident::from_static("c"),
            data_type: DataType::BigInt,
            using: None,
            lossy: false,
        },
    ))
    .into_statement();
    let message = lite_err(&statement).to_string();
    assert!(message.contains("rebuild the table"), "{message}");
    assert!(message.contains("23-migrations.md"), "{message}");
}

/// SQLite appends a column without rewriting the rows, which rules out
/// anything that would have to be checked against — or filled in for — the rows
/// already there.
///
/// Every one of these is a server error at *migration* time, on the customer's
/// database. They are caught here instead, with the recipe.
#[test]
fn sqlite_refuses_the_four_add_column_shapes_that_would_fail_at_migration_time() {
    /// An `ALTER TABLE t ADD COLUMN` of the given column.
    fn adding(column: ColumnSpec) -> Statement {
        Ddl::AlterTable(AlterTable::new(TableRef::from_static("t")).add_column(column))
            .into_statement()
    }
    let text = || ColumnSpec::new(Ident::from_static("c"), DataType::Text);

    let refused = [
        // `Cannot add a UNIQUE column`
        (text().unique(), "UNIQUE"),
        // `Cannot add a PRIMARY KEY column`
        (text().primary_key(), "PRIMARY KEY"),
        // `Cannot add a NOT NULL column with default value NULL`
        (text().not_null(), "NOT NULL"),
        (text().not_null().default(Expr::null()), "NOT NULL"),
        // `Cannot add a column with non-constant default`
        (
            text().default(Expr::Function(Function::Now)),
            "DEFAULT <expression>",
        ),
        // `cannot add a STORED column`
        (
            ColumnSpec::new(Ident::from_static("g"), DataType::Integer)
                .generated(Generated::stored(col("a"))),
            "STORED",
        ),
    ];
    for (column, needle) in refused {
        let statement = adding(column);
        // PostgreSQL takes every one of them.
        assert!(
            pg(&statement)
                .text
                .starts_with(r#"ALTER TABLE "t" ADD COLUMN"#)
        );
        let error = lite_err(&statement);
        assert!(error.is_dialect_gap(), "{needle}: {error}");
        assert!(error.to_string().contains(needle), "{needle}: {error}");
        assert!(error.to_string().contains("help:"), "{needle}: {error}");
    }

    // The shapes SQLite does take.
    let accepted = [
        text(),
        text().not_null().default(Expr::value("x")),
        text().default(Expr::value(1_i32)),
        text().check(col("c").is_not_null()),
        text().collate(Ident::from_static("NOCASE")),
        ColumnSpec::new(Ident::from_static("g"), DataType::Integer)
            .generated(Generated::virtual_(col("a"))),
    ];
    for column in accepted {
        assert!(
            lite(&adding(column)).text.contains("ADD COLUMN"),
            "SQLite takes this shape"
        );
    }
}

/// SQLite parses `NULLS LAST` in an `ORDER BY` and answers
/// `unsupported use of NULLS LAST` in a `CREATE INDEX`, which is why the check
/// is its own rather than the `nulls_ordering` capability's.
#[test]
fn a_nulls_placement_on_an_index_is_postgres_only() {
    let statement = Ddl::CreateIndex(CreateIndex::new(
        Ident::from_static("i"),
        TableRef::from_static("t"),
        [IndexTarget::column(Ident::from_static("c"))
            .order(Order::Desc)
            .nulls(Nulls::Last)],
    ))
    .into_statement();
    assert_eq!(
        pg(&statement).text,
        r#"CREATE INDEX "i" ON "t" ("c" DESC NULLS LAST)"#
    );
    let error = lite_err(&statement);
    assert!(error.is_dialect_gap(), "{error}");
    assert!(error.to_string().contains("coalesce"), "{error}");

    // Without the placement it is portable, and `ORDER BY` still takes one.
    let portable = Ddl::CreateIndex(CreateIndex::new(
        Ident::from_static("i"),
        TableRef::from_static("t"),
        [IndexTarget::column(Ident::from_static("c")).order(Order::Desc)],
    ))
    .into_statement();
    assert_eq!(
        lite(&portable).text,
        r#"CREATE INDEX "i" ON "t" ("c" DESC)"#
    );
}

#[test]
fn drop_and_rename_table_render() {
    let drop = Ddl::DropTable(
        DropTable::new([TableRef::from_static("a"), TableRef::from_static("b")])
            .if_exists()
            .cascade(),
    )
    .into_statement();
    assert_eq!(pg(&drop).text, r#"DROP TABLE IF EXISTS "a", "b" CASCADE"#);
    assert!(lite_err(&drop).is_dialect_gap(), "SQLite has no CASCADE");

    let plain = Ddl::DropTable(
        DropTable::new([TableRef::from_static("a"), TableRef::from_static("b")]).if_exists(),
    )
    .into_statement();
    assert_eq!(
        lite(&plain).text,
        "DROP TABLE IF EXISTS \"a\";\nDROP TABLE IF EXISTS \"b\""
    );

    let rename = Ddl::RenameTable(RenameTable::new(
        TableRef::from_static("user"),
        Ident::from_static("users"),
    ))
    .into_statement();
    let expected = r#"ALTER TABLE "user" RENAME TO "users""#;
    assert_eq!(pg(&rename).text, expected);
    assert_eq!(lite(&rename).text, expected);
}

#[test]
fn truncate_has_an_exact_sqlite_equivalent() {
    let plain = Ddl::Truncate(Truncate::new([
        TableRef::from_static("events"),
        TableRef::from_static("audit"),
    ]))
    .into_statement();
    assert_eq!(pg(&plain).text, r#"TRUNCATE TABLE "events", "audit""#);
    assert_eq!(
        lite(&plain).text,
        "DELETE FROM \"events\";\nDELETE FROM \"audit\"",
        "SQLite's optimiser turns an unfiltered DELETE into the same truncate"
    );

    // `RESTART IDENTITY` would need `DELETE FROM sqlite_sequence`, and that
    // table does not exist until the database has held an `AUTOINCREMENT`
    // column — so the statement would work on some databases and fail on
    // others. It is refused instead.
    let restart =
        Ddl::Truncate(Truncate::new([TableRef::from_static("events")]).restart_identity())
            .into_statement();
    assert_eq!(
        pg(&restart).text,
        r#"TRUNCATE TABLE "events" RESTART IDENTITY"#
    );
    let error = lite_err(&restart);
    assert!(error.is_dialect_gap(), "{error}");
    assert!(error.to_string().contains("sqlite_sequence"), "{error}");

    let cascade =
        Ddl::Truncate(Truncate::new([TableRef::from_static("t")]).cascade()).into_statement();
    assert_eq!(pg(&cascade).text, r#"TRUNCATE TABLE "t" CASCADE"#);
    assert!(lite_err(&cascade).is_dialect_gap());
}

#[test]
fn the_zero_downtime_index_renders() {
    let index = CreateIndex::new(
        Ident::from_static("idx_users_email_active"),
        TableRef::from_static("users"),
        [IndexTarget::column(Ident::from_static("email"))
            .collate(Ident::from_static("C"))
            .operator_class(Ident::from_static("text_pattern_ops"))
            .order(Order::Desc)
            .nulls(Nulls::Last)],
    )
    .unique()
    .concurrently()
    .if_not_exists()
    .using(IndexMethod::BTree)
    .include([Ident::from_static("name")])
    .nulls_not_distinct()
    .where_(col("deleted_at").is_null());
    let statement = Ddl::CreateIndex(index).into_statement();
    assert_eq!(
        pg(&statement).text,
        concat!(
            r#"CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS "idx_users_email_active" "#,
            r#"ON "users" USING btree "#,
            r#"("email" COLLATE "C" "text_pattern_ops" DESC NULLS LAST) "#,
            r#"INCLUDE ("name") NULLS NOT DISTINCT WHERE "deleted_at" IS NULL"#
        )
    );
    assert!(lite_err(&statement).is_dialect_gap());
}

#[test]
fn a_portable_index_renders_on_both_dialects() {
    let statement = Ddl::CreateIndex(
        CreateIndex::new(
            Ident::from_static("idx_posts_author"),
            TableRef::from_static("posts"),
            [
                IndexTarget::column(Ident::from_static("author_id")),
                IndexTarget::expr(Expr::Function(Function::Lower(Box::new(col("title")))))
                    .order(Order::Desc),
            ],
        )
        .where_(col("deleted_at").is_null()),
    )
    .into_statement();
    let expected = concat!(
        r#"CREATE INDEX "idx_posts_author" ON "posts" "#,
        r#"("author_id", (lower("title")) DESC) WHERE "deleted_at" IS NULL"#
    );
    assert_eq!(pg(&statement).text, expected);
    assert_eq!(lite(&statement).text, expected);
}

#[test]
fn every_index_method_renders() {
    for (method, name) in [
        (IndexMethod::BTree, "btree"),
        (IndexMethod::Hash, "hash"),
        (IndexMethod::Gin, "gin"),
        (IndexMethod::Gist, "gist"),
        (IndexMethod::SpGist, "spgist"),
        (IndexMethod::Brin, "brin"),
    ] {
        let statement = Ddl::CreateIndex(
            CreateIndex::new(
                Ident::from_static("i"),
                TableRef::from_static("t"),
                [IndexTarget::column(Ident::from_static("c"))],
            )
            .using(method),
        )
        .into_statement();
        assert!(pg(&statement).text.contains(&format!("USING {name}")));
    }
    let custom = Ddl::CreateIndex(
        CreateIndex::new(
            Ident::from_static("i"),
            TableRef::from_static("t"),
            [IndexTarget::column(Ident::from_static("c"))],
        )
        .using(IndexMethod::Custom(Ident::from_static("hnsw"))),
    )
    .into_statement();
    assert!(pg(&custom).text.contains(r#"USING "hnsw""#));
}

#[test]
fn drop_and_rename_index_render() {
    let drop = Ddl::DropIndex(
        DropIndex::new(Ident::from_static("idx"))
            .in_schema(Ident::from_static("public"))
            .concurrently()
            .if_exists()
            .cascade(),
    )
    .into_statement();
    assert_eq!(
        pg(&drop).text,
        r#"DROP INDEX CONCURRENTLY IF EXISTS "public"."idx" CASCADE"#
    );
    assert!(lite_err(&drop).is_dialect_gap());

    let plain =
        Ddl::DropIndex(DropIndex::new(Ident::from_static("idx")).if_exists()).into_statement();
    assert_eq!(lite(&plain).text, r#"DROP INDEX IF EXISTS "idx""#);

    let rename = Ddl::RenameIndex(RenameIndex::new(
        Ident::from_static("idx_new"),
        Ident::from_static("idx"),
    ))
    .into_statement();
    assert_eq!(pg(&rename).text, r#"ALTER INDEX "idx_new" RENAME TO "idx""#);
    assert!(lite_err(&rename).is_dialect_gap());
}

#[test]
fn a_sqlite_index_qualifies_the_index_rather_than_the_table() {
    let statement = Ddl::CreateIndex(CreateIndex::new(
        Ident::from_static("idx"),
        TableRef::qualified(Ident::from_static("aux"), Ident::from_static("t")),
        [IndexTarget::column(Ident::from_static("c"))],
    ))
    .into_statement();
    assert_eq!(
        pg(&statement).text,
        r#"CREATE INDEX "idx" ON "aux"."t" ("c")"#
    );
    assert_eq!(
        lite(&statement).text,
        r#"CREATE INDEX "aux"."idx" ON "t" ("c")"#
    );
}

#[test]
fn enum_types_render_on_postgres_only() {
    let create = Ddl::CreateType(CreateType::new(
        TypeRef::from_static("order_status"),
        TypeBody::enumeration(["pending", "paid", "it's shipped"]),
    ))
    .into_statement();
    assert_eq!(
        pg(&create).text,
        r#"CREATE TYPE "order_status" AS ENUM ('pending', 'paid', 'it''s shipped')"#
    );
    let error = lite_err(&create);
    assert!(error.is_dialect_gap(), "{error}");
    assert!(error.to_string().contains("CHECK"), "{error}");

    let add = Ddl::AlterType(AlterType::new(
        TypeRef::from_static("order_status"),
        AlterTypeAction::add_value("refunded"),
    ))
    .into_statement();
    assert_eq!(
        pg(&add).text,
        r#"ALTER TYPE "order_status" ADD VALUE IF NOT EXISTS 'refunded'"#
    );

    let placed = Ddl::AlterType(AlterType::new(
        TypeRef::from_static("order_status"),
        AlterTypeAction::AddValue {
            value: "queued".to_owned(),
            before: Some("paid".to_owned()),
            after: None,
            if_not_exists: false,
        },
    ))
    .into_statement();
    assert_eq!(
        pg(&placed).text,
        r#"ALTER TYPE "order_status" ADD VALUE 'queued' BEFORE 'paid'"#
    );

    let both = Ddl::AlterType(AlterType::new(
        TypeRef::from_static("t"),
        AlterTypeAction::AddValue {
            value: "x".to_owned(),
            before: Some("a".to_owned()),
            after: Some("b".to_owned()),
            if_not_exists: false,
        },
    ))
    .into_statement();
    assert!(matches!(pg_err(&both), Error::InvalidClause { .. }));

    let rename_value = Ddl::AlterType(AlterType::new(
        TypeRef::from_static("t"),
        AlterTypeAction::RenameValue {
            from: "paid".to_owned(),
            to: "settled".to_owned(),
        },
    ))
    .into_statement();
    assert_eq!(
        pg(&rename_value).text,
        r#"ALTER TYPE "t" RENAME VALUE 'paid' TO 'settled'"#
    );

    let rename = Ddl::AlterType(AlterType::new(
        TypeRef::from_static("t"),
        AlterTypeAction::Rename(Ident::from_static("u")),
    ))
    .into_statement();
    assert_eq!(pg(&rename).text, r#"ALTER TYPE "t" RENAME TO "u""#);

    let set_schema = Ddl::AlterType(AlterType::new(
        TypeRef::from_static("t"),
        AlterTypeAction::SetSchema(Ident::from_static("s")),
    ))
    .into_statement();
    assert_eq!(pg(&set_schema).text, r#"ALTER TYPE "t" SET SCHEMA "s""#);

    let drop = Ddl::DropType(
        DropType::new(TypeRef::qualified(
            Ident::from_static("shop"),
            Ident::from_static("mood"),
        ))
        .if_exists()
        .cascade(),
    )
    .into_statement();
    assert_eq!(
        pg(&drop).text,
        r#"DROP TYPE IF EXISTS "shop"."mood" CASCADE"#
    );
    assert!(lite_err(&drop).is_dialect_gap());
}

#[test]
fn schemas_extensions_and_comments_render_on_postgres_only() {
    let create = Ddl::CreateSchema(
        CreateSchema::new(Ident::from_static("billing"))
            .if_not_exists()
            .authorization(Ident::from_static("app")),
    )
    .into_statement();
    assert_eq!(
        pg(&create).text,
        r#"CREATE SCHEMA IF NOT EXISTS "billing" AUTHORIZATION "app""#
    );
    assert!(lite_err(&create).is_dialect_gap());

    let drop = Ddl::DropSchema(
        DropSchema::new(Ident::from_static("billing"))
            .if_exists()
            .cascade(),
    )
    .into_statement();
    assert_eq!(pg(&drop).text, r#"DROP SCHEMA IF EXISTS "billing" CASCADE"#);
    assert!(lite_err(&drop).is_dialect_gap());

    let extension = Ddl::CreateExtension(
        CreateExtension::new(Ident::from_static("pg_trgm"))
            .if_not_exists()
            .schema(Ident::from_static("public"))
            .version("1.6"),
    )
    .into_statement();
    assert_eq!(
        pg(&extension).text,
        r#"CREATE EXTENSION IF NOT EXISTS "pg_trgm" SCHEMA "public" VERSION '1.6'"#
    );
    assert!(lite_err(&extension).is_dialect_gap());

    let targets = [
        (
            CommentTarget::Table(TableRef::from_static("users")),
            r#"TABLE "users""#,
        ),
        (
            CommentTarget::Column {
                table: TableRef::from_static("users"),
                column: Ident::from_static("email"),
            },
            r#"COLUMN "users"."email""#,
        ),
        (
            CommentTarget::Index(Ident::from_static("idx")),
            r#"INDEX "idx""#,
        ),
        (
            CommentTarget::Type(TypeRef::from_static("mood")),
            r#"TYPE "mood""#,
        ),
    ];
    for (target, rendered) in targets {
        let statement =
            Ddl::Comment(CommentOn::new(target, Some("hello".to_owned()))).into_statement();
        assert_eq!(
            pg(&statement).text,
            format!("COMMENT ON {rendered} IS 'hello'")
        );
        assert!(lite_err(&statement).is_dialect_gap());
    }

    let removed = Ddl::Comment(CommentOn::new(
        CommentTarget::Table(TableRef::from_static("t")),
        None,
    ))
    .into_statement();
    assert_eq!(pg(&removed).text, r#"COMMENT ON TABLE "t" IS NULL"#);
}

#[test]
fn raw_ddl_still_binds_its_parameters() {
    let statement =
        Ddl::Raw(RawStatement::new("set local statement_timeout = ?").bind("5s")).into_statement();
    let sql = pg(&statement);
    assert_eq!(sql.text, "set local statement_timeout = $1");
    assert_eq!(sql.args, vec![Value::text("5s")]);
}

// ── literals ────────────────────────────────────────────────────────────────

#[test]
fn every_value_kind_has_a_ddl_literal() {
    /// Renders `value` as the DEFAULT of a throwaway column.
    #[track_caller]
    fn default_of(value: Value) -> String {
        let statement = Ddl::CreateTable(CreateTable::new(TableRef::from_static("t")).column(
            ColumnSpec::new(Ident::from_static("c"), DataType::Text).default(Expr::bound(value)),
        ))
        .into_statement();
        let text = pg(&statement).text;
        text.trim_start_matches(r#"CREATE TABLE "t" ("c" text DEFAULT "#)
            .trim_end_matches(')')
            .to_owned()
    }

    assert_eq!(default_of(Value::null(ValueKind::Text)), "NULL");
    assert_eq!(default_of(Value::Bool(true)), "TRUE");
    assert_eq!(default_of(Value::I8(-1)), "-1");
    assert_eq!(default_of(Value::I16(2)), "2");
    assert_eq!(default_of(Value::I32(3)), "3");
    assert_eq!(default_of(Value::I64(4)), "4");
    assert_eq!(default_of(Value::U8(5)), "5");
    assert_eq!(default_of(Value::U16(6)), "6");
    assert_eq!(default_of(Value::U32(7)), "7");
    assert_eq!(default_of(Value::U64(8)), "8");
    assert_eq!(default_of(Value::F32(1.5)), "1.5");
    assert_eq!(default_of(Value::F64(-0.25)), "-0.25");
    assert_eq!(
        default_of(Value::Decimal(Decimal::new(1999, 2).expect("in range"))),
        "19.99"
    );
    assert_eq!(default_of(Value::text("hi")), "'hi'");
    assert_eq!(default_of(Value::bytes([0x01, 0xFF])), r"'\x01FF'");
    assert_eq!(
        default_of(Value::Uuid(Uuid::NIL)),
        "'00000000-0000-0000-0000-000000000000'"
    );
    assert_eq!(
        default_of(Value::Json(Json::parse(r#"{"a": 1}"#).expect("valid"))),
        r#"'{"a":1}'"#
    );
    assert_eq!(
        default_of(Value::Date(Date::new(2026, 7, 30).expect("valid"))),
        "'2026-07-30'"
    );
    assert_eq!(
        default_of(Value::Time(Time::new(9, 5, 0, 0).expect("valid"))),
        "'09:05:00'"
    );
    assert_eq!(
        default_of(Value::DateTime(DateTime::new(
            Date::new(2026, 7, 30).expect("valid"),
            Time::new(9, 5, 0, 0).expect("valid"),
        ))),
        "'2026-07-30 09:05:00'"
    );
    assert_eq!(
        default_of(Value::Interval(Interval::from_days(14))),
        "'14 days'"
    );
    assert_eq!(
        default_of(Value::Array(Array::of([1_i32, 2]))),
        "ARRAY[1, 2]"
    );
    assert_eq!(
        default_of(Value::Array(Array::empty(ValueKind::Text))),
        "ARRAY[]::text[]"
    );
}

#[test]
fn a_timestamp_literal_reconstructs_the_calendar() {
    let cases = [
        (0_i64, 0_u32, "1970-01-01 00:00:00+00"),
        (-1, 0, "1969-12-31 23:59:59+00"),
        (1_769_000_000, 0, "2026-01-21 12:53:20+00"),
        (951_782_400, 0, "2000-02-29 00:00:00+00"),
        (1_709_164_800, 0, "2024-02-29 00:00:00+00"),
        (0, 123_456_789, "1970-01-01 00:00:00.123456789+00"),
    ];
    for (seconds, nanos, expected) in cases {
        let timestamp = Timestamp::new(seconds, nanos).expect("valid");
        assert_eq!(format_timestamp(timestamp), expected, "{seconds}");
    }
}

#[test]
fn a_non_finite_float_literal_is_quoted_on_postgres_and_refused_on_sqlite() {
    let statement = Ddl::CreateTable(
        CreateTable::new(TableRef::from_static("t")).column(
            ColumnSpec::new(Ident::from_static("c"), DataType::DoublePrecision)
                .default(Expr::value(f64::NAN)),
        ),
    )
    .into_statement();
    assert!(pg(&statement).text.contains("'NaN'"));
    assert!(lite_err(&statement).is_dialect_gap());

    for (value, text) in [
        (f64::INFINITY, "'Infinity'"),
        (f64::NEG_INFINITY, "'-Infinity'"),
    ] {
        let statement = Ddl::CreateTable(
            CreateTable::new(TableRef::from_static("t")).column(
                ColumnSpec::new(Ident::from_static("c"), DataType::DoublePrecision)
                    .default(Expr::value(value)),
            ),
        )
        .into_statement();
        assert!(
            pg(&statement).text.contains(text),
            "{}",
            pg(&statement).text
        );
    }
}

// ── budget and safety ───────────────────────────────────────────────────────

#[test]
fn a_statement_over_the_parameter_budget_suggests_a_chunk_size() {
    let mut insert = Insert::into_table(TableRef::from_static("t")).columns([
        Ident::from_static("a"),
        Ident::from_static("b"),
        Ident::from_static("c"),
    ]);
    // 30_000 rows × 3 columns = 90_000 parameters, over both limits.
    for _ in 0..30_000 {
        insert = insert.values([Expr::value(1), Expr::value(2), Expr::value(3)]);
    }
    let statement = insert.into_statement();

    let error = pg_err(&statement);
    match error {
        Error::TooManyParameters {
            dialect,
            limit,
            found,
            suggested,
        } => {
            assert_eq!(dialect, "PostgreSQL");
            assert_eq!(limit, Postgres::MAX_BIND_PARAMS);
            assert_eq!(found, 90_000);
            assert_eq!(suggested, 65_535 / 3);
            assert!(suggested * 3 <= limit, "the suggestion must actually fit");
        }
        other => panic!("expected a budget error, got {other}"),
    }
    assert!(pg_err(&statement).to_string().contains("chunks"));

    let error = lite_err(&statement);
    assert!(matches!(
        error,
        Error::TooManyParameters {
            limit: Sqlite::MAX_BIND_PARAMS,
            ..
        }
    ));
}

#[test]
fn a_value_that_looks_like_sql_stays_a_value() {
    let statement = Select::from_table(TableRef::from_static("users"))
        .select_all()
        .filter(col("email").eq(Expr::value("'; drop table users; --")))
        .into_statement();
    let sql = pg(&statement);
    assert_eq!(
        sql.text, r#"SELECT * FROM "users" WHERE "email" = $1"#,
        "a value is never text"
    );
    assert!(!sql.text.contains("drop"));
    assert_eq!(sql.args, vec![Value::text("'; drop table users; --")]);
}

#[test]
fn a_ddl_literal_that_looks_like_sql_is_quoted_rather_than_executed() {
    let statement = Ddl::CreateType(CreateType::new(
        TypeRef::from_static("t"),
        TypeBody::enumeration(["a', 'b"]),
    ))
    .into_statement();
    assert_eq!(
        pg(&statement).text,
        r#"CREATE TYPE "t" AS ENUM ('a'', ''b')"#,
        "the embedded quote is doubled, so the label stays one label"
    );
}

#[test]
fn a_read_only_statement_is_the_only_one_that_may_reach_a_replica() {
    assert!(Select::new().select_all().into_statement().is_read_only());
    for statement in [
        Insert::into_table(TableRef::from_static("t")).into_statement(),
        Update::table(TableRef::from_static("t")).into_statement(),
        Delete::from_table(TableRef::from_static("t")).into_statement(),
        RawStatement::new("select 1").into_statement(),
    ] {
        assert!(!statement.is_read_only());
    }
}

#[test]
fn the_two_dialects_agree_on_the_shape_of_an_ordinary_query() {
    // The one query an application runs most: a filtered, joined, ordered,
    // paginated read. Everything but the placeholder spelling is identical, and
    // that is the promise `20-orm-overview.md` makes about SQLite.
    let statement = Select::from_table(TableRef::from_static("posts"))
        .select_items([
            SelectItem::column(ColumnRef::qualified(
                Ident::from_static("posts"),
                Ident::from_static("id"),
            )),
            SelectItem::aliased(
                Expr::column(ColumnRef::qualified(
                    Ident::from_static("users"),
                    Ident::from_static("name"),
                )),
                Ident::from_static("author"),
            ),
        ])
        .inner_join(
            FromItem::table(TableRef::from_static("users")),
            Expr::column(ColumnRef::qualified(
                Ident::from_static("posts"),
                Ident::from_static("author_id"),
            ))
            .eq(Expr::column(ColumnRef::qualified(
                Ident::from_static("users"),
                Ident::from_static("id"),
            ))),
        )
        .filter(col("published").eq(Expr::value(true)))
        .filter(col("created_at").gt(Expr::value(0_i64)))
        .order_by(OrderTerm::desc(col("created_at")).nulls_last())
        .limit(20)
        .into_statement();

    let postgres = pg(&statement);
    let sqlite = lite(&statement);
    assert_eq!(
        postgres.text.replace("$1", "?").replace("$2", "?"),
        sqlite.text
    );
    assert_eq!(postgres.args, sqlite.args);
    assert_eq!(postgres.args.len(), 2);
}
