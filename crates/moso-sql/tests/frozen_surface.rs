//! The signatures in this crate are frozen: a dozen work packages are written
//! against them. These tests fail if one moves.
//!
//! Nothing here calls a dialect's renderer, so the file stays green while the
//! bodies are being filled in and turns into the harness the snapshot tests
//! hang off afterwards.

use moso_sql::ddl::{
    AlterTable, AlterTableAction, ColumnSpec, CreateIndex, CreateTable, Ddl, DropTable, ForeignKey,
    IndexMethod, IndexTarget, ReferentialAction, TableConstraint,
};
use moso_sql::{
    Aggregate, AggregateFunc, Array, Assignment, BinOp, Bindable, Capabilities, ColumnRef, Cte,
    DataType, Delete, Dialect, Expr, FromItem, Function, Ident, Insert, Join, JoinKind, JsonOp,
    Lock, LockStrength, OnConflict, Order, OrderTerm, Postgres, RawExpr, RawStatement, Returning,
    Select, SelectItem, Sqlite, Statement, StatementKind, TableRef, TextQuery, Update, Value,
    ValueKind, WindowExpr, WindowFunc, WindowSpec,
};

/// Non-negotiable N1: `Select` stays `Select` through any chain.
///
/// The assertion is a type identity, not a shape guess: if a combinator ever
/// returns `Select<Filtered<Ordered<..>>>` this stops compiling.
#[test]
fn the_select_builder_is_shape_stable_through_twelve_combinators() {
    fn same_type<T>(_: &T, _: &T) {}

    let plain = Select::from_table(TableRef::from_static("users"));
    let chained = Select::from_table(TableRef::from_static("users"))
        .select_all()
        .distinct()
        .inner_join(
            FromItem::table(TableRef::from_static("posts")),
            Expr::value(true),
        )
        .filter(Expr::col(Ident::from_static("is_admin")).eq(Expr::value(true)))
        .filter_opt(None)
        .filter_if(true, || Expr::col(Ident::from_static("x")).is_not_null())
        .when(true, |query| query.limit(10))
        .apply(|query| query.offset(20))
        .group_by(Expr::col(Ident::from_static("id")))
        .having(Expr::value(true))
        .order_by(OrderTerm::desc(Expr::col(Ident::from_static("created_at"))))
        .lock(Lock::new(LockStrength::Update).skip_locked());

    same_type(&plain, &chained);

    // N1 also promises no user-visible type longer than 80 characters. The
    // name a diagnostic would print for the whole builder is this short.
    assert!(std::any::type_name::<Select>().len() < 80);
}

/// Every construct the spine promises can be *built*. Rendering is the
/// dialects' job; this is the shape check that the AST has a place for each.
#[test]
fn every_promised_construct_has_a_representation() {
    let users = TableRef::from_static("users");
    let posts = TableRef::from_static("posts");

    // Joins, including lateral.
    let lateral = FromItem::lateral(
        Select::from_table(posts.clone()).select_all().limit(3),
        Ident::from_static("recent"),
    );
    let query = Select::from_table(users.clone())
        .select_all()
        .join(Join::new(JoinKind::Left, lateral, Expr::value(true)))
        .join(Join::new(
            JoinKind::Full,
            FromItem::table(posts.clone()),
            Expr::value(true),
        ));
    assert_eq!(query.joins().len(), 2);

    // A WHERE tree with AND/OR/NOT.
    let tree = !(Expr::col(Ident::from_static("a")).is_null()
        & (Expr::col(Ident::from_static("b")).gt(Expr::value(1))
            | Expr::col(Ident::from_static("c")).ilike(Expr::value("x%"))));
    assert!(matches!(tree, Expr::Unary { .. }));

    // ORDER BY with NULLS placement, LIMIT/OFFSET, GROUP BY + HAVING.
    let ordered = Select::from_table(users.clone())
        .select_expr(Aggregate::count_star().into_expr())
        .group_by(Expr::col(Ident::from_static("author_id")))
        .having(Aggregate::count_star().into_expr().gt(Expr::value(1)))
        .order_by(OrderTerm::desc(Expr::col(Ident::from_static("at"))).nulls_last())
        .limit(10)
        .offset(20);
    assert_eq!(ordered.order_terms()[0].order(), Order::Desc);

    // Window functions — the ROW_NUMBER OVER PARTITION BY that makes a
    // per-parent LIMIT one statement (non-negotiable N3).
    let ranked = WindowExpr::new(
        WindowFunc::RowNumber,
        [],
        WindowSpec::new()
            .partition_by(Expr::col(Ident::from_static("author_id")))
            .order_by(OrderTerm::desc(Expr::col(Ident::from_static("created_at")))),
    );
    assert!(matches!(ranked.into_expr(), Expr::Window(_)));

    // CTEs.
    let with_cte = Select::from_table(TableRef::from_static("recent"))
        .select_all()
        .with(Cte::new(Ident::from_static("recent"), ordered.clone()).materialized(false))
        .recursive(false);
    assert_eq!(with_cte.ctes().len(), 1);

    // Set operations.
    assert_eq!(
        Select::from_table(users.clone())
            .union_all(Select::from_table(posts.clone()))
            .set_operations()
            .len(),
        1
    );

    // INSERT ... RETURNING, ON CONFLICT DO UPDATE and DO NOTHING.
    let upsert = Insert::into_table(users.clone())
        .columns([Ident::from_static("email"), Ident::from_static("name")])
        .values([Expr::value("a@example.com"), Expr::value("Ada")])
        .on_conflict(
            OnConflict::columns([Ident::from_static("email")])
                .target_where(Expr::col(Ident::from_static("deleted_at")).is_null())
                .do_update_columns([Ident::from_static("name")])
                .update_where(Expr::value(true)),
        )
        .returning(Returning::All);
    assert_eq!(upsert.bind_count(), 2);
    assert!(upsert.conflict().is_some());
    assert!(
        Insert::into_table(users.clone())
            .on_conflict(OnConflict::any().do_nothing())
            .conflict()
            .is_some()
    );

    // UPDATE ... SET ... RETURNING, and DELETE ... RETURNING.
    let update = Update::table(users.clone())
        .set_with(Ident::from_static("login_count"), |current| {
            current.plus(Expr::value(1))
        })
        .set_assignment(Assignment::set(Ident::from_static("name"), "Ada"))
        .from(FromItem::table(posts.clone()))
        .filter(Expr::col(Ident::from_static("id")).eq(Expr::value(1)))
        .returning(Returning::columns([ColumnRef::from_static("id")]));
    assert_eq!(update.assignments().len(), 2);
    assert!(update.has_filter());

    let delete = Delete::from_table(users.clone())
        .using(FromItem::table(posts.clone()))
        .filter(Expr::value(true))
        .returning(Returning::All);
    assert!(delete.has_filter());

    // FOR UPDATE / SHARE with SKIP LOCKED and NOWAIT.
    for lock in [
        Lock::new(LockStrength::Update).skip_locked(),
        Lock::new(LockStrength::Share).nowait(),
        Lock::new(LockStrength::NoKeyUpdate).of(users.clone()),
        Lock::new(LockStrength::KeyShare),
    ] {
        assert!(
            Select::from_table(users.clone())
                .lock(lock)
                .lock_mode()
                .is_some()
        );
    }

    // EXISTS / IN / ANY subqueries and scalar subqueries.
    let subquery = Select::from_table(posts.clone()).select_column(ColumnRef::from_static("id"));
    assert!(matches!(
        Expr::exists(subquery.clone()),
        Expr::Exists { .. }
    ));
    assert!(matches!(
        Expr::col(Ident::from_static("id")).in_subquery(subquery.clone()),
        Expr::InSubquery { .. }
    ));
    assert!(matches!(
        Expr::col(Ident::from_static("id")).any(BinOp::Eq, Expr::value(Array::of([1_i64, 2]))),
        Expr::Quantified { .. }
    ));
    assert!(matches!(Expr::scalar(subquery), Expr::Scalar(_)));

    // jsonb operators.
    for op in [
        JsonOp::Get,
        JsonOp::GetText,
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
        let expr = Expr::col(Ident::from_static("prefs")).json(op, Expr::value("theme"));
        assert!(matches!(expr, Expr::Json { .. }));
    }

    // Full text, CAST and COALESCE.
    assert!(matches!(
        Expr::text_match(
            Expr::col(Ident::from_static("body")),
            TextQuery::Websearch("rust orm".to_owned()),
            Some(Ident::from_static("english")),
        ),
        Expr::Binary { .. }
    ));
    assert!(matches!(
        Expr::value(1).cast(DataType::Text),
        Expr::Cast { .. }
    ));
    assert!(matches!(
        Function::Coalesce(vec![Expr::null(), Expr::value(0)]).into_expr(),
        Expr::Function(_)
    ));
}

/// The DDL vocabulary the migration generator needs, including the four
/// zero-downtime constructs that generic builders usually omit.
#[test]
fn the_ddl_surface_covers_the_migration_operation_table() {
    let users = TableRef::from_static("users");

    let create = CreateTable::new(users.clone())
        .if_not_exists()
        .column(ColumnSpec::new(Ident::from_static("id"), DataType::Uuid).primary_key())
        .column(
            ColumnSpec::new(Ident::from_static("email"), DataType::Text)
                .not_null()
                .unique(),
        )
        .column(
            ColumnSpec::new(Ident::from_static("author_id"), DataType::Uuid).references(
                ForeignKey::new(
                    None,
                    [Ident::from_static("author_id")],
                    users.clone(),
                    [Ident::from_static("id")],
                )
                .on_delete(ReferentialAction::Cascade),
            ),
        )
        .constraint(TableConstraint::unique(
            Some(Ident::from_static("users_email_key")),
            [Ident::from_static("email")],
        ));
    assert_eq!(create.columns().len(), 3);
    assert!(!Ddl::CreateTable(create).is_destructive());

    // Partial, concurrent, unique index with a method and an operator class.
    let index = CreateIndex::new(
        Ident::from_static("idx_users_email_active"),
        users.clone(),
        [IndexTarget::column(Ident::from_static("email"))
            .order(Order::Desc)
            .operator_class(Ident::from_static("text_pattern_ops"))],
    )
    .unique()
    .concurrently()
    .using(IndexMethod::BTree)
    .include([Ident::from_static("name")])
    .where_(Expr::col(Ident::from_static("deleted_at")).is_null());
    assert!(Ddl::CreateIndex(index).requires_no_transaction());

    // NOT VALID / VALIDATE CONSTRAINT and ADD ... USING INDEX: the four
    // statements a lock-free schema change is made of.
    let alter = AlterTable::new(users.clone())
        .action(AlterTableAction::AddConstraint(
            TableConstraint::ForeignKey(
                ForeignKey::new(
                    Some(Ident::from_static("fk")),
                    [Ident::from_static("author_id")],
                    users.clone(),
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
            index: Ident::from_static("idx_users_email_active"),
        })
        .action(AlterTableAction::AlterColumnType {
            name: Ident::from_static("id"),
            data_type: DataType::Text,
            using: Some(Expr::col(Ident::from_static("id")).cast(DataType::Text)),
            lossy: true,
        });
    assert_eq!(alter.actions().len(), 4);
    assert!(alter.is_destructive());

    assert!(Ddl::DropTable(DropTable::new([users]).if_exists().cascade()).is_destructive());
}

/// The two dialects disagree only where the databases do, and every gap comes
/// back as an error with a fix rather than as silently different SQL.
#[test]
fn the_dialects_are_honest_about_their_gaps() {
    let dialects: [&dyn Dialect; 2] = [&Postgres, &Sqlite];
    for dialect in dialects {
        assert!(!dialect.name().is_empty());
        assert!(dialect.max_bind_params() < usize::MAX);
        assert_eq!(dialect.quoted(&Ident::from_static("select")), r#""select""#);
    }

    let postgres = Capabilities::postgres();
    let sqlite = Capabilities::sqlite();
    assert!(postgres.lateral_joins && !sqlite.lateral_joins);
    assert!(postgres.enum_types && !sqlite.enum_types);

    let mut out = String::new();
    let error = Sqlite
        .type_name(&DataType::array_of(DataType::Text), &mut out)
        .expect_err("SQLite has no arrays");
    assert!(error.is_dialect_gap());
    assert!(error.to_string().contains("help:"));
}

/// Non-negotiable N8: a raw fragment binds its own parameters and never
/// interpolates them.
#[test]
fn the_raw_escape_hatch_binds_rather_than_interpolates() {
    let fragment = RawExpr::new("created_at > now() - ?::interval").bind("1 day");
    assert_eq!(fragment.placeholder_count(), 1);
    assert_eq!(fragment.args(), &[Value::text("1 day")]);
    assert!(!fragment.fragment().contains("1 day"));

    let statement = RawStatement::new("select * from users where email = ?")
        .bind("ada@example.com")
        .read_only();
    assert!(statement.is_read_only());
    assert_eq!(statement.placeholder_count(), 1);
    assert!(!statement.text().contains("ada@example.com"));
    assert_eq!(statement.into_statement().kind(), StatementKind::Raw);
}

/// A value is bound with its type, so `None` still produces a typed `NULL` and
/// a PostgreSQL placeholder can be inferred.
#[test]
fn binding_preserves_the_column_type_through_none() {
    assert_eq!(Value::bind(None::<i64>), Value::Null(ValueKind::I64));
    assert_eq!(<Option<String> as Bindable>::KIND, ValueKind::Text);
    assert_eq!(Array::of(Vec::<i32>::new()).element_kind(), ValueKind::I32);

    // Every statement kind reports itself, which is what routes a read to a
    // replica and a write to the primary.
    let kinds = [
        (Select::new().into_statement(), StatementKind::Select),
        (
            Insert::into_table(TableRef::from_static("t")).into_statement(),
            StatementKind::Insert,
        ),
        (
            Update::table(TableRef::from_static("t")).into_statement(),
            StatementKind::Update,
        ),
        (
            Delete::from_table(TableRef::from_static("t")).into_statement(),
            StatementKind::Delete,
        ),
        (
            Ddl::DropTable(DropTable::new([TableRef::from_static("t")])).into_statement(),
            StatementKind::Ddl,
        ),
        (
            RawStatement::new("vacuum").into_statement(),
            StatementKind::Raw,
        ),
    ];
    for (statement, expected) in kinds {
        assert_eq!(statement.kind(), expected);
        assert_eq!(statement.is_read_only(), expected == StatementKind::Select);
    }
}

/// A projection is a list of items, and a count query is the same query with
/// the list swapped — which is why `set_projection` exists.
#[test]
fn a_count_query_is_the_same_query_with_a_different_projection() {
    let listing = Select::from_table(TableRef::from_static("posts"))
        .select_items([
            SelectItem::column(ColumnRef::from_static("id")),
            SelectItem::aliased(Expr::value(1), Ident::from_static("one")),
        ])
        .filter(Expr::col(Ident::from_static("published")).eq(Expr::value(true)))
        .order_by(OrderTerm::asc(Expr::col(Ident::from_static("id"))))
        .limit(20);

    let counted = listing
        .clone()
        .set_projection([SelectItem::expr(
            Aggregate::new(AggregateFunc::Count, [Expr::col(Ident::from_static("id"))])
                .distinct()
                .into_expr(),
        )])
        .clear_order_by()
        .clear_limit();

    assert_eq!(listing.items().len(), 2);
    assert_eq!(counted.items().len(), 1);
    assert_eq!(counted.filters(), listing.filters());
    assert!(counted.order_terms().is_empty());
    assert_eq!(counted.limit_value(), None);
}

/// The whole public surface is Moso's. This is a reminder in code of what
/// `xtask check-sealed` enforces from rustdoc: if a signature here ever needs a
/// `use` of a foreign crate, the gate is about to go red.
#[test]
fn no_foreign_type_is_needed_to_use_the_api() {
    let statement: Statement = Select::from_table(TableRef::from_static("t"))
        .select_all()
        .into_statement();
    let borrowed = statement.borrowed();
    assert_eq!(borrowed.kind(), StatementKind::Select);
    assert!(borrowed.is_read_only());
}
