---
title: Multi-tenancy
description: Choose between discriminator, schema-per-tenant and database-per-tenant, make an unscoped query on a tenant entity a compile error, and route tenants to their own pools.
order: 17
status: shipped
---

Moso implements three tenancy models rather than one, because "which one does your framework
support" is a question every SaaS asks and picking one for you would be picking wrong for two thirds
of readers. You choose with `DatabaseConfig::with_tenancy`, and the choice decides where the
isolation lives: in a `WHERE` clause, in a `search_path`, or in a whole separate database.

Under the first model there is one failure the framework refuses to leave to review: a query that
forgets its tenant does not error, it returns **another customer's rows**. So `#[entity(tenant =
"...")]` makes an unscoped `SELECT` a compile error, and an unscoped `INSERT`, `UPDATE` or `DELETE`
an error raised while the statement is built, before anything is sent.

> [!IMPORTANT]
> Two shapes of this page's subject area are worth setting straight up front. `Db::for_tenant`
> routes connections rather than adding the query-level predicate, so reach for `.scoped(..)` to
> discharge the tenant obligation on a query. `#[entity(rls)]` is accepted and readable from the
> descriptor as a forward-compatibility marker; emitting `ENABLE ROW LEVEL SECURITY` and
> `CREATE POLICY` from it is reserved for a later release, so write those in a migration for now.
> Migrating every tenant is fully wired: `moso db migrate --all-tenants`, over
> `moso_migrate::command::migrate_tenants`, reads its tenant list from `tenants()` in your
> `src/db.rs`.

## The three models

| Model | Isolation | Tenants it suits | What it costs |
| --- | --- | --- | --- |
| `TenancyModel::Discriminator` | a `WHERE` clause | millions | nothing, one pool |
| `TenancyModel::SchemaPerTenant { prefix }` | a `search_path` | hundreds | one pool per live tenant |
| `TenancyModel::DatabasePerTenant { url_template }` | a whole database | tens | one pool per live tenant |

Choosing:

- **Discriminator** unless you have a reason. One pool, one migration run, one backup, and every
  cross-tenant report is an ordinary query. The cost is that isolation is a predicate, which is why
  the compiler enforces it.
- **Schema per tenant** when a customer contract says their data is not in the same tables as
  anybody else's, when tenants need slightly different columns, or when you want per-tenant restore.
  PostgreSQL only. Migrations run once per schema.
- **Database per tenant** when tenants need separate backup and restore, separate residency, or
  separate hardware. Tens of tenants, not thousands, because each live one holds a pool.

Set the model on the configuration:

```rust title="src/main.rs"
use moso_orm::db::{DatabaseConfig, Db, TenancyModel};

let config = DatabaseConfig::from_url(std::env::var("DATABASE_URL")?)
    .with_tenancy(TenancyModel::schema_per_tenant("tenant_"))
    .with_max_tenant_pools(16);

let db = Db::connect(&config).await?;
```

`TenancyModel::routes_connections()` is `true` for the last two, and `as_str()` gives the name that
appears in the boot log.

`TenantId` is reachable as `moso::db::TenantId`. `TenancyModel`, `TenantRouter`, `TenantSource` and
`UrlTemplate` are one module down, in `moso_orm::db`, which is the spelling the snippets below use.

## The discriminator model

Name the column on the entity and every query over it starts owing a tenant:

```rust title="src/models.rs"
use moso::db::prelude::*;
use moso::Entity;

/// One invoice, belonging to exactly one tenant.
#[derive(Entity, Debug, Clone)]
#[entity(table = "invoices", tenant = "tenant_id")]
pub struct Invoice {
    /// The primary key.
    #[entity(pk)]
    pub id: i64,
    /// Which customer this row belongs to.
    pub tenant_id: i64,
    /// The amount, in minor units.
    pub total_cents: i64,
}
```

`Invoice::query()` is now a `Select<Invoice, NeedsTenant>` rather than a `Select<Invoice>`.
`NeedsTenant` implements no `Ready`, and `fetch_all`, `fetch_one`, `count`, `paginate` and the rest
are all gated on `J: Ready<E>`. So the query does not have those methods until you discharge the
obligation:

```rust
// `Invoice::query()` is a `Select<Invoice, NeedsTenant>`, which has no `fetch_all`.
let mine = Invoice::query()
    .scoped(TenantId::of(tenant_id))
    .fetch_all(&db)
    .await?;

// Deliberately every tenant, easy to grep for in review.
let everyones = Invoice::query().across_tenants().fetch_all(&db).await?;
```

Forget both and the compiler says so:

```text
error[E0277]: `Invoice` is tenant-scoped and this query has no tenant
   = note: a query that forgets its tenant reads another customer's rows, so the compiler insists on one
   = note: help: name the tenant: `Invoice::query().scoped(tenant)`
   = note: help: or, deliberately across every tenant: `Invoice::query().across_tenants()`
```

`.across_tenants()` is long to type on purpose. It is the spelling that shows up in a code review.

`Select::check_tenant()` returns the same obligation as a `Result` for code that builds a statement
dynamically and wants to check before running.

### Writes carry the same obligation

`Insert`, `Update` and `Delete` are enforced at build time rather than in the type, because their
builders are not parameterised by a readiness marker. The check runs in `to_statement()`, so nothing
reaches the server:

```rust
// `.scoped(..)` assigns the tenant column, overriding whatever the
// `NewInvoice` carried.
Invoice::insert(row)
    .scoped(TenantId::of(tenant_id))
    .execute(&tx)
    .await?;

// Adds the tenant predicate to the `WHERE` clause.
Invoice::update_all()
    .filter(Invoice::ID.eq(id))
    .set(Invoice::TOTAL_CENTS, 0)
    .scoped(TenantId::of(tenant_id))
    .execute(&tx)
    .await?;

Invoice::delete_all()
    .filter(Invoice::ID.eq(id))
    .scoped(TenantId::of(tenant_id))
    .execute(&tx)
    .await?;
```

The three checks are not identical, and the difference matters:

- **`Update` and `Delete`** demand a decision. Omit both `.scoped(..)` and `.across_tenants()` and
  the build fails with `Error::TenantMissing { entity }`. There is no way to run one by accident.
- **`Insert`** asks a narrower question: is the tenant column being written at all? Since the
  generated `NewInvoice` has a `tenant_id` field like any other column, an insert built from a
  complete `New` struct already satisfies the check. `.scoped(..)` is the alternative to threading
  the tenant through the struct, and it overrides the column rather than adding a second one. There
  is no `.across_tenants()` on an insert, because a row lands in exactly one tenant.

`Error::TenantMissing` maps to a `500`, deliberately: a missing tenant scope is your bug, never the
client's.

> [!NOTE]
> The two tenant handles have distinct jobs. `db.for_tenant(tenant)` stores the tenant on the handle
> and `db.tenant()` reads it back; that handle-level tenant is what routes connections under the
> schema-per-tenant and database-per-tenant models. Under the discriminator model the predicate comes
> from `.scoped(tenant)` on the query, so reach for `.scoped(..)` there rather than `for_tenant`.

### What tenant scoping does not cover

- **Preloads carry no tenant predicate.** A `.with(Invoice::LINES)` filters on the foreign key and
  nothing else. In practice the foreign key constrains the rows to the right tenant, but that is the
  key's guarantee, not the framework's. If a child table is reachable from more than one parent,
  check it.
- **Raw SQL is not rewritten.** `RawQuery` and the `sql!` macro send what you wrote.
- **A joined entity is scoped by its own predicate or not at all.** Joining a tenant-scoped entity
  into a query over an untenanted one does not add a predicate for the joined side.
- **Passing a tenant to an untenanted entity is harmless.** `tenant_predicate` returns `None` for an
  entity with no tenant column, so a generic helper that always calls `.scoped(..)` still works.

## `TenantId`

Any `SqlType` can be a tenant key, so `TenantId` wraps the bound value rather than fixing a type.

| Method | Returns | Notes |
| --- | --- | --- |
| `TenantId::of(value)` | `TenantId` | `42_i64`, `"acme"`, a UUID |
| `from_value(v)` / `value()` / `into_value()` | `Value` | the bound form |
| `key()` | `String` | `"<kind>:<rendering>"`, injective across types |
| `slug()` | `Result<String>` | letters, digits, `_` and `-` only |
| `schema(prefix)` | `Result<Ident>` | the validated schema name |

`key()` includes the value's kind, so `TenantId::of(1_i64)` and `TenantId::of("1")` never collide on
the same pool.

`slug()` is an **allowlist**, not a denylist. A space is enough to be refused, and a key like
`public"; drop schema public cascade; --` is an `Error::Configuration` with a `help:` line:

```rust
assert_eq!(TenantId::of(42_i64).slug()?, "42");
assert!(TenantId::of(String::from("a; drop table users")).slug().is_err());
```

This is the check that keeps a tenant key out of the statement it would otherwise be interpolated
into. The only way a tenant name reaches SQL is as a quoted `Ident`, and the only way it reaches
that point is by being boring.

## Schema per tenant

`db.for_tenant(tenant)` returns a handle whose pool runs `set search_path to "<schema>", public` on
every connect. The schema name is `TenantId::schema(prefix)`, a validated identifier, so nothing
tenant-derived is ever interpolated unquoted.

```rust
let config = DatabaseConfig::from_url(url)
    .with_tenancy(TenancyModel::schema_per_tenant("tenant_"));
let db = Db::connect(&config).await?;

// `TenantId::of(7_i64).schema("tenant_")` is `tenant_7`.
let acme = db.for_tenant(TenantId::of(7_i64));
let ledger = Ledger::query().fetch_all(&acme).await?;
```

`Ledger` here has **no** `#[entity(tenant = "...")]`, and that is the point: under this model the
isolation is in the connection, so there is no discriminator column and nothing for a type to
enforce. A query on a tenant-routed handle cannot see another tenant's rows. Mixing the two, by
declaring a tenant column on an entity you are also isolating by schema, means paying the obligation
twice for no extra safety.

This model is PostgreSQL only. On SQLite it is `Error::Unsupported { feature: "schema-per-tenant",
backend: Sqlite }`.

Each live tenant holds a pool, capped by `database.max_tenant_pools` (default 32) and evicted
least-recently-used first.

## Database per tenant

The URL template carries a `{tenant}` placeholder:

```rust
let config = DatabaseConfig::from_url("postgres://h/app")
    .with_tenancy(TenancyModel::database_per_tenant("postgres://h/app_{tenant}"));
```

`db.for_tenant(TenantId::of(7_i64))` then draws from a lazily-opened pool against
`postgres://h/app_7`. The same substitution is available directly as a `TenantSource`:

```rust
use moso_orm::{DatabaseConfig, TenantId};
use moso_orm::db::{TenantSource, UrlTemplate};

let base = DatabaseConfig::from_url("postgres://ignored");
let source = UrlTemplate::new("postgres://h/app_{tenant}", base);

let config = source.config(&TenantId::of(7_i64))?;
assert_eq!(config.url.expose(), "postgres://h/app_7");
```

A derived tenant configuration inherits the base settings but drops the replicas and appends the
tenant to the application name, so `pg_stat_activity` says which tenant a connection belongs to.

### Routing from a control plane

When the connection strings live in a table rather than a template, implement `TenantSource`:

```rust title="src/tenancy.rs"
use moso_orm::{DatabaseConfig, TenantId};
use moso_orm::db::TenantSource;

/// Every tenant on its own database, named from a control-plane map.
pub struct FromMap(std::collections::HashMap<String, String>);

impl TenantSource for FromMap {
    fn config(&self, tenant: &TenantId) -> moso_orm::Result<DatabaseConfig> {
        let url = self.0.get(&tenant.key()).ok_or_else(|| {
            moso_orm::Error::Configuration { detail: format!("no database for {tenant}") }
        })?;
        Ok(DatabaseConfig::from_url(url.clone()))
    }
}
```

The trait is synchronous on purpose: producing a configuration is a string operation, and opening
the pool is `Db::connect`'s job. That keeps it dyn-compatible without a boxed future. If your source
of truth is itself a database, read it into a map at boot or refresh it on a schedule.

Hand it to a `TenantRouter`, which caches the pools:

```rust
use moso_orm::db::{TenantRouter, UrlTemplate};

let router = TenantRouter::new(
    8,
    UrlTemplate::new(
        "postgres://h/app_{tenant}",
        DatabaseConfig::from_url("postgres://h/app"),
    ),
);

let acme = router.db(&TenantId::of(1_i64)).await?;
assert_eq!(router.len(), 1);
```

| Method | What it does |
| --- | --- |
| `new(capacity, source)` | at most `capacity` pools stay open; 0 is raised to 1 |
| `db(&tenant)` | the tenant's handle, opening a pool if there is not one |
| `len()` / `capacity()` / `is_empty()` | occupancy |
| `evictions()` | how many pools have been closed to make room, worth exporting |
| `close()` | closes every pool, idempotent |

Register the router as a provider and reach it from a handler with `Inject<TenantRouter>`. Two tasks
that ask for the same new tenant at once are handled: whichever pool landed first is kept, the loser
is closed rather than leaked.

Eviction is least-recently-used, over a linear scan of a small map. If `evictions()` climbs steadily
your capacity is below your working set, and every eviction costs the next request a connect.

## An unroutable tenant is refused, not guessed

`Db::for_tenant` cannot fail by signature, which forces the design: a tenant that cannot be routed
produces a **poisoned** handle that refuses every statement with the reason and reports itself down.
It never produces one that quietly reads the untenanted rows.

```rust
let config = DatabaseConfig::from_url("sqlite://:memory:")
    .with_tenancy(TenancyModel::schema_per_tenant("t_"));
let db = Db::connect(&config).await.expect("open");

// A key with a space cannot name a schema, so the handle is poisoned
// rather than quietly reading the untenanted rows.
let hostile = db.for_tenant(TenantId::of(String::from("two words")));
let error = hostile
    .ping()
    .await
    .expect_err("a poisoned handle runs nothing");
assert!(error.to_string().contains("help:"), "{error}");
assert!(
    hostile.health().await.is_down(),
    "and it reports itself unhealthy rather than pretending"
);
```

`Db::try_for_tenant(tenant)` is the same thing with the error at the call site, for code that would
rather branch than propagate a poisoned handle.

## Tenancy and authorisation

Tenant scoping is not authorisation, and the two run in a fixed order. Tenant scoping answers "whose
data is this"; authorisation answers "may this actor see it". Applying an authorisation filter to a
query that has not been tenant-scoped means filtering across every tenant's rows.

Moso makes that ordering a compile error rather than a review comment. `ScopedPolicy::scope_query`
takes a `Select<R>`, which is the shape a query has **after** the tenant obligation is discharged.
A `Select<Invoice, NeedsTenant>` does not fit:

```rust
let visible = Invoice::query()
    .scoped(tenant)                       // first: whose rows
    .authorized_for::<Read>(&actor)       // then: which of them this actor may see
    .paginate(cursor, 50)
    .fetch(&db)
    .await?;
```

Both contribute `WHERE` clauses to the same builder, so the shape does not change and pagination
counts stay correct. See [policies and query scoping](./policies.md) and
[permissions and roles](./permissions.md).

## Operating a routed deployment

- **Connection budget.** Under the routed models the number of open connections is
  `max_connections` times the number of live tenants, capped by `max_tenant_pools`. Multiply that by
  the number of processes before you size the server, exactly as in
  [transactions and pooling](./transactions.md).
- **Shutdown.** `Db::close()` closes replicas and every per-tenant pool, idempotently.
  `TenantRouter::close()` does the same for a router.
- **Health.** A poisoned tenant handle reports `Down`. A lagging replica reports `Degraded`. See
  [health and shutdown](./health-and-shutdown.md).
- **Migrations.** `moso db migrate --all-tenants` migrates every tenant your `src/db.rs` lists,
  reports each one, and exits non-zero naming the ones that failed. The list is yours to build
  (Moso does not know where you keep it), and `tenants()` in `src/db.rs` is where it goes:

  ```rust
  use moso_migrate::command::{migrate_tenants, TenantTarget};

  let tenants = [
      TenantTarget::schema("acme", url, TenantId::of(7_i64).schema("tenant_")),
      TenantTarget::database("globex", "postgres://localhost/globex"),
  ];
  let report = migrate_tenants("migrations", &tenants, &options, &|_runner| {}).await?;
  ```

  It migrates each tenant in turn (creating a missing schema and setting `search_path`, so each
  tenant gets its own `moso_migrations`), records a tenant that fails rather than stopping, and
  answers `is_clean()`, which is what the CLI turns into the exit code. Make it part of deployment,
  because a tenant whose schema is behind is a tenant whose requests fail. See
  [migrating every tenant](./migrations.md#migrating-every-tenant).

## Failure modes

| What you did | What happens |
| --- | --- |
| `fetch_all` on a tenant-scoped query with no tenant | compile error naming both fixes |
| `execute` on a tenant-scoped `Update` or `Delete` with neither `.scoped(..)` nor `.across_tenants()` | `Error::TenantMissing`, mapped to `500`. Nothing is sent |
| Built an `Insert` that does not write the tenant column at all | the same `Error::TenantMissing` |
| `db.for_tenant(..)` and expected the predicate | no predicate is added. Use `.scoped(..)` |
| A tenant key with a space, a quote or a semicolon under a routed model | `Error::Configuration` with a `help:` line, and a poisoned handle from `for_tenant` |
| Schema per tenant on SQLite | `Error::Unsupported` |
| More live tenants than `max_tenant_pools` | least-recently-used pools close; `evictions()` counts it |
| Two requests open the same new tenant at once | one pool is kept, the other closed. No leak |
| Relied on `#[entity(rls)]` for isolation | nothing is emitted. It is descriptor metadata only |

## See also

- [Transactions and pooling](./transactions.md) for pool sizing, replicas and health.
- [Relations](./relations.md) for the builder these scopes attach to.
- [Policies and query scoping](./policies.md) for `authorized_for` and `ScopedPolicy`.
- [Migrations](./migrations.md) for running a migrator per schema or per database.
- [Security](./security.md) for the rest of the defaults, and for why the unscoped query is a
  compile error and the unjoined column is not.
