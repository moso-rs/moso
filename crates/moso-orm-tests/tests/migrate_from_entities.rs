//! Non-negotiable N6, end to end: `#[derive(Entity)]` → a reviewable migration
//! → a real database → the same entities reading rows back.
//!
//! `moso-migrate`'s own suite proves the generator against hand-built
//! [`Schema`]s. What it cannot prove — because it must not depend on the
//! facade — is that the descriptors a *derive* produces are the ones the
//! generator can build a schema from. That is the seam this file covers, and it
//! is the seam where the two crates would drift apart silently.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use moso::db::prelude::*;
use moso::{Entity, migration};
use moso_migrate::Schema;
use moso_migrate::file::split_statements;
use moso_migrate::generator::Generator;
use moso_migrate::rename::DropAndAdd;
use moso_migrate::schema::Column;

/// A customer, with every column shape the generator has to render.
#[derive(Entity, Debug, Clone)]
#[entity(table = "mg_customers")]
pub struct Customer {
    /// The primary key.
    #[entity(pk)]
    pub id: i64,
    /// Login identity; one row per address.
    #[entity(unique)]
    pub email: String,
    /// Display name, which may be absent.
    pub nickname: Option<String>,
    /// Lifetime spend, in minor units.
    pub spend: i64,
    /// Everything they ordered.
    #[entity(has_many = Order, fk = "customer_id")]
    pub orders: Related<Vec<Order>>,
}

/// One order, by one customer.
#[derive(Entity, Debug, Clone)]
#[entity(table = "mg_orders")]
pub struct Order {
    /// The primary key.
    #[entity(pk)]
    pub id: i64,
    /// Whose order this is.
    #[entity(index)]
    pub customer_id: i64,
    /// What it cost, in minor units.
    pub total: i64,
    /// Who placed it.
    #[entity(belongs_to = Customer, fk = "customer_id")]
    pub customer: Related<Customer>,
}

/// A hand-written migration, to prove the attribute compiles and registers.
///
/// It is deliberately a no-op: what is under test here is that
/// `#[migration]` produces something that implements the runner's trait, not
/// what the body does.
#[migration(version = "20260730120000", name = "noop")]
pub struct Noop;

/// A scratch directory no other test in this binary can be holding.
///
/// Keyed by a counter as well as the process, because `#[test]`s run on
/// separate threads of one process and two of them ask for the SQLite backend:
/// with only the backend and the pid in the name they shared a directory, and
/// whichever finished first deleted the one the other was still listing.
fn scratch_directory(backend: Backend) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "moso-mg-{backend}-{}-{ordinal}",
        std::process::id()
    ))
}

/// Renders a fresh migration for the two entities above.
fn generated(backend: Backend) -> (String, Schema) {
    let directory = scratch_directory(backend);
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");

    let generator = Generator::new(&directory, backend);
    let descriptors = [
        <Customer as Entity>::descriptor(),
        <Order as Entity>::descriptor(),
    ];
    let generated = generator
        .make_migration(&descriptors, Some("initial"), &DropAndAdd)
        .expect("the descriptors build a schema")
        .expect("an empty database differs from two entities");
    let schema = Schema::from_entities(descriptors).expect("builds");
    let _ = std::fs::remove_dir_all(&directory);
    (generated.migration().to_owned(), schema)
}

#[test]
fn n6_the_schema_is_built_from_the_derived_descriptors() {
    let (_, schema) = generated(Backend::Postgres);

    let customers = schema
        .table("mg_customers")
        .expect("the derive's table name reaches the schema");
    assert_eq!(
        customers
            .columns()
            .iter()
            .map(Column::name)
            .collect::<Vec<_>>(),
        ["id", "email", "nickname", "spend"],
        "relations are not columns; everything else is, in declaration order"
    );
    assert!(
        customers
            .columns()
            .iter()
            .any(|column| column.name() == "nickname" && column.is_nullable()),
        "`Option<String>` reaches the generator as a nullable column"
    );

    let orders = schema.table("mg_orders").expect("the second table");
    assert!(
        orders.foreign_keys().len() == 1,
        "the `belongs_to` reaches the generator as a foreign key"
    );
}

#[test]
fn n6_the_migration_is_reviewable_sql_on_both_dialects() {
    for backend in [Backend::Postgres, Backend::Sqlite] {
        let (migration, _) = generated(backend);
        assert!(
            migration.contains("-- +migrate up"),
            "a migration is a reviewable file, not an opaque blob: {migration}"
        );
        assert!(migration.contains("mg_customers"), "{migration}");
        assert!(migration.contains("mg_orders"), "{migration}");
        assert!(
            migration.contains("-- +migrate down"),
            "and it is reversible: {migration}"
        );
    }
}

#[tokio::test]
async fn the_generated_migration_produces_a_schema_the_entities_can_use() {
    let path = std::env::temp_dir().join(format!("moso-mg-apply-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let db = Db::connect_url(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .expect("an on-disk SQLite database always opens");

    let outcome = async {
        let (migration, _) = generated(Backend::Sqlite);
        let up = migration
            .split("-- +migrate down")
            .next()
            .expect("the up half is before the down marker");
        for statement in split_statements(up) {
            RawQuery::new(statement).execute(&db).await?;
        }

        // The tables the *generator* wrote, read by the *derive*'s `from_row`.
        Customer::insert(NewCustomer {
            id: 1,
            email: "ada@example.test".to_owned(),
            nickname: None,
            spend: 0,
        })
        .execute(&db)
        .await?;
        Order::insert(NewOrder {
            id: 1,
            customer_id: 1,
            total: 4_200,
        })
        .execute(&db)
        .await?;

        let orders = Order::query().with(Order::CUSTOMER).fetch_all(&db).await?;
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].total, 4_200);
        assert_eq!(orders[0].customer()?.email, "ada@example.test");

        let customer = Customer::find(1).fetch_one(&db).await?;
        assert_eq!(customer.nickname, None, "a NULL column reads back as None");
        Ok::<(), Error>(())
    }
    .await;

    db.close().await;
    let _ = std::fs::remove_file(&path);
    outcome.expect("the generated schema and the derived entities agree");
}
