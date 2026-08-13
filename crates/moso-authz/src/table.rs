//! The `moso_authz_audit` table: the audit trail, stored where it can be
//! queried, exported and aged out.
//!
//! `docs/03-batteries/31-authorization.md` names the table by name and says
//! what a compliance-driven buyer asks for: *"Queryable in the admin,
//! exportable, with a retention policy."* [`TableAuditSink`] is the writer,
//! [`AuditEntry`] is the row, [`create_table_sql`] is the DDL, and
//! [`TableAuditSink::purge_expired`] plus [`TableAuditSink::spawn_purge`] are
//! the retention policy — the configured number of days and a task that
//! actually runs, rather than a note telling the reader to write one.
//!
//! # Why the row is not an `Entity`
//!
//! It should be — `moso-migrate` generates a migration by diffing entity
//! descriptors (non-negotiable N6), and an `Entity` would mean the DDL and the
//! writer came from one declaration. Implementing [`Entity`](moso_orm::Entity)
//! requires naming
//! `moso_sql::{TableRef, ValueKind, Expr}` in the associated constants, and
//! `xtask/allow/dep-edges.toml` declares `"moso-authz" = ["moso-orm"]` — no
//! `moso-sql`. Rather than widen a machine-checked architecture rule from
//! inside the crate it governs, the writer uses [`RawQuery`] with a
//! dialect-correct placeholder list, and [`AuditEntry::from_row`] decodes the
//! same columns for whoever queries the table.
//!
//! The consequence is stated rather than hidden: **the DDL here is
//! hand-written**, `create_table_sql` and the writer are kept in step by
//! [`AuditEntry::COLUMNS`] and a test that compares them, and the day the edge
//! is declared this module becomes twenty lines shorter.

use core::time::Duration;

use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use moso_orm::{Backend, Db, DecodeError, RawQuery, Row};

use crate::audit::count_dropped;
use crate::{
    ActorId, ActorKind, AuditConfig, AuditOutcome, AuditRecord, AuditSink, Scope, ScopeId,
};

/// The table an [`AuditRecord`] is stored in.
///
/// ```
/// assert_eq!(moso_authz::AUDIT_TABLE, "moso_authz_audit");
/// ```
pub const AUDIT_TABLE: &str = "moso_authz_audit";

/// One stored authorization decision.
///
/// The wire shape of [`AuditRecord`], flattened: [`Scope`] becomes its cache
/// key (`global`, `org:acme`) because that is what an operator greps for, and
/// [`AuditOutcome`] becomes `allow`/`deny` for the same reason.
///
/// ```
/// use moso_authz::AuditEntry;
///
/// assert_eq!(AuditEntry::COLUMNS.len(), 11);
/// assert_eq!(AuditEntry::COLUMNS[0], "at");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuditEntry {
    /// When the decision was made.
    pub at: DateTime<Utc>,
    /// Who was acting.
    pub actor: String,
    /// What kind of thing was acting: `user`, `api_key`, `service`, `job`.
    pub actor_kind: String,
    /// Where, as [`Scope::as_key`] renders it.
    pub scope: String,
    /// What they tried to do.
    pub action: String,
    /// What they tried to do it to, as `Name#id`.
    pub resource: Option<String>,
    /// `allow` or `deny`.
    pub outcome: String,
    /// The reason the decision carried, bounded at
    /// [`AuditRecord::REASON_MAX`].
    pub reason: String,
    /// The correlation id, so an entry joins to the request's logs.
    pub request_id: Option<String>,
    /// The caller's address, as the trusted-proxy configuration resolved it.
    pub ip: Option<String>,
    /// The matched route pattern, never the raw path.
    pub route: Option<String>,
}

impl AuditEntry {
    /// The columns, in the order [`AuditEntry::from_row`] reads them and
    /// [`TableAuditSink`] writes them.
    ///
    /// The `id` column is absent on purpose: it is the database's to assign,
    /// and nothing here reads it.
    ///
    /// ```
    /// use moso_authz::AuditEntry;
    ///
    /// assert!(AuditEntry::COLUMNS.contains(&"request_id"));
    /// ```
    pub const COLUMNS: &'static [&'static str] = &[
        "at",
        "actor",
        "actor_kind",
        "scope",
        "action",
        "resource",
        "outcome",
        "reason",
        "request_id",
        "ip",
        "route",
    ];

    /// The `SELECT` list, so a caller's query reads the columns this decodes.
    ///
    /// ```
    /// use moso_authz::AuditEntry;
    ///
    /// assert!(AuditEntry::select_list().starts_with("at, actor,"));
    /// ```
    #[must_use]
    pub fn select_list() -> String {
        Self::COLUMNS.join(", ")
    }

    /// Decode one row, positionally, in [`AuditEntry::COLUMNS`] order.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] naming the column and both types.
    ///
    /// ```no_run
    /// # use moso_authz::AuditEntry;
    /// # fn f(row: &moso_orm::Row) -> Result<AuditEntry, moso_orm::DecodeError> {
    /// AuditEntry::from_row(row)
    /// # }
    /// ```
    pub fn from_row(row: &Row) -> Result<Self, DecodeError> {
        Ok(Self {
            at: row.get_timestamp(0)?,
            actor: row.get_string(1)?,
            actor_kind: row.get_string(2)?,
            scope: row.get_string(3)?,
            action: row.get_string(4)?,
            resource: optional(row, 5)?,
            outcome: row.get_string(6)?,
            reason: row.get_string(7)?,
            request_id: optional(row, 8)?,
            ip: optional(row, 9)?,
            route: optional(row, 10)?,
        })
    }

    /// The row this record becomes.
    ///
    /// ```no_run
    /// # use moso_authz::{AuditEntry, AuditRecord};
    /// # fn f(record: AuditRecord) { let _ = AuditEntry::of(&record); }
    /// ```
    #[must_use]
    pub fn of(record: &AuditRecord) -> Self {
        Self {
            at: record.at,
            actor: record.actor.as_str().to_owned(),
            actor_kind: record.actor_kind.as_str().to_owned(),
            scope: record.scope.as_key(),
            action: record.action.clone(),
            resource: record.resource.clone(),
            outcome: if record.outcome.is_deny() {
                "deny"
            } else {
                "allow"
            }
            .to_owned(),
            reason: record.reason.clone(),
            request_id: record.request_id.clone(),
            ip: record.ip.clone(),
            route: record.route.clone(),
        }
    }

    /// The [`AuditRecord`] this row records, as far as it can be recovered.
    ///
    /// The scope comes back as [`Scope::Custom`] for anything the key does not
    /// name exactly — an audit trail records what happened, not a
    /// serialisation of the type system.
    ///
    /// ```no_run
    /// # use moso_authz::AuditEntry;
    /// # fn f(entry: &AuditEntry) { let _ = entry.to_record(); }
    /// ```
    #[must_use]
    pub fn to_record(&self) -> AuditRecord {
        let mut record = AuditRecord::new(
            ActorId::new(self.actor.clone()),
            parse_kind(&self.actor_kind),
            parse_scope(&self.scope),
            self.action.clone(),
            self.reason.clone(),
            if self.outcome == "deny" {
                AuditOutcome::Deny
            } else {
                AuditOutcome::Allow
            },
        );
        record.at = self.at;
        record.resource = self.resource.clone();
        record.request_id = self.request_id.clone();
        record.ip = self.ip.clone();
        record.route = self.route.clone();
        record
    }
}

/// A nullable text column.
fn optional(row: &Row, index: usize) -> Result<Option<String>, DecodeError> {
    if row.is_null(index)? {
        return Ok(None);
    }
    row.get_string(index).map(Some)
}

/// The reverse of [`ActorKind::as_str`].
fn parse_kind(kind: &str) -> ActorKind {
    match kind {
        "user" => ActorKind::User,
        "api_key" => ActorKind::ApiKey,
        "service" => ActorKind::Service,
        "job" => ActorKind::Job,
        _ => ActorKind::Anonymous,
    }
}

/// The reverse of [`Scope::as_key`], as far as a key can be reversed.
fn parse_scope(key: &str) -> Scope {
    match key.split_once(':') {
        Some(("org", id)) => Scope::Org(ScopeId::new(id)),
        Some(("project", id)) => Scope::Project(ScopeId::new(id)),
        Some((kind, id)) => Scope::Custom {
            kind: kind.to_owned(),
            id: ScopeId::new(id),
        },
        None => Scope::Global,
    }
}

/// The audit sink that writes to `moso_authz_audit`.
///
/// ```no_run
/// use moso_authz::TableAuditSink;
///
/// # async fn f(db: moso_orm::Db) -> moso_orm::Result<()> {
/// let sink = TableAuditSink::new(db);
/// sink.create_table().await?;
/// # Ok(())
/// # }
/// ```
///
/// # Why a failed write is logged and not returned
///
/// [`AuditSink::record`] returns nothing, deliberately: the request has already
/// been decided by the time an entry is written, and turning "the audit table
/// is full" into a 500 on every endpoint is a worse outcome than a logged write
/// failure. A deployment that needs the opposite — no request without an audit
/// row — writes the row itself inside its own transaction, which is what
/// [`AuditEntry::COLUMNS`] and [`insert_sql`] are public for.
#[derive(Clone, Debug)]
pub struct TableAuditSink {
    /// Where the rows go.
    db: Db,
}

impl TableAuditSink {
    /// A sink writing to `moso_authz_audit` on `db`.
    ///
    /// ```no_run
    /// # use moso_authz::TableAuditSink;
    /// # fn f(db: moso_orm::Db) { let _ = TableAuditSink::new(db); }
    /// ```
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// The handle this sink writes through.
    ///
    /// ```no_run
    /// # use moso_authz::TableAuditSink;
    /// # fn f(s: &TableAuditSink) { let _: &moso_orm::Db = s.db(); }
    /// ```
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Create the table and its two indexes, if they are not there.
    ///
    /// For an application with no migration runner. One with a runner should
    /// take the statements from [`create_table_sql`] into a migration instead,
    /// so the schema change is reviewable (non-negotiable N6).
    ///
    /// # Errors
    ///
    /// Anything the database reports.
    ///
    /// ```no_run
    /// # use moso_authz::TableAuditSink;
    /// # async fn f(sink: &TableAuditSink) -> moso_orm::Result<()> {
    /// sink.create_table().await
    /// # }
    /// ```
    pub async fn create_table(&self) -> moso_orm::Result<()> {
        for statement in create_table_sql(self.db.backend()) {
            RawQuery::new(statement).execute(&self.db).await?;
        }
        Ok(())
    }

    /// Delete every entry older than `cutoff`, returning how many went.
    ///
    /// The explicit form, for a caller that has its own idea of a cutoff.
    /// [`purge_expired`](TableAuditSink::purge_expired) computes one from
    /// [`AuditConfig::retention_days`](crate::AuditConfig::retention_days), and
    /// [`spawn_purge`](TableAuditSink::spawn_purge) runs that on a timer.
    ///
    /// # Errors
    ///
    /// Anything the database reports.
    ///
    /// ```no_run
    /// # use chrono::{Duration, Utc};
    /// # use moso_authz::TableAuditSink;
    /// # async fn f(sink: &TableAuditSink) -> moso_orm::Result<u64> {
    /// sink.purge(Utc::now() - Duration::days(365)).await
    /// # }
    /// ```
    pub async fn purge(&self, cutoff: DateTime<Utc>) -> moso_orm::Result<u64> {
        let placeholder = placeholders(self.db.backend(), 1);
        RawQuery::new(format!(
            "delete from {AUDIT_TABLE} where at < {placeholder}"
        ))
        .bind(cutoff)
        .execute(&self.db)
        .await
    }

    /// Delete every entry outside the configured retention window.
    ///
    /// [`purge`](TableAuditSink::purge) with the cutoff computed from
    /// [`AuditConfig::retention_days`], so the configured number and the
    /// statement cannot disagree. A retention of zero means "keep forever" and
    /// deletes nothing.
    ///
    /// # Errors
    ///
    /// Anything the database reports.
    ///
    /// ```no_run
    /// # use moso_authz::{AuditConfig, TableAuditSink};
    /// # async fn f(sink: &TableAuditSink) -> moso_orm::Result<u64> {
    /// sink.purge_expired(&AuditConfig::default()).await
    /// # }
    /// ```
    pub async fn purge_expired(&self, config: &AuditConfig) -> moso_orm::Result<u64> {
        match config.retention_cutoff(Utc::now()) {
            Some(cutoff) => self.purge(cutoff).await,
            None => Ok(0),
        }
    }

    /// Run [`purge_expired`](TableAuditSink::purge_expired) every `interval`,
    /// until the returned handle is dropped.
    ///
    /// The runnable retention policy: one line at boot instead of a scheduled
    /// job an application has to remember to write.
    ///
    /// ```text
    /// let purge = sink.spawn_purge(audit, Duration::from_secs(60 * 60 * 6));
    ///
    /// App::new(config)
    ///     .provide_dyn::<dyn AuditSink>(sink)
    ///     .lifespan(move |_| async move { Ok(purge) })
    /// ```
    ///
    /// The first purge runs one interval in, not at boot: a deploy loop that
    /// restarted every minute would otherwise run a full-table delete every
    /// minute. A failed purge is logged and retried on the next tick, because a
    /// database that is briefly unreachable must not switch retention off until
    /// somebody notices.
    ///
    /// # Panics
    ///
    /// If there is no Tokio runtime, because the purge is a spawned task.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_authz::{AuditConfig, TableAuditSink};
    ///
    /// # tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap()
    /// #     .block_on(async {
    /// # async fn run(sink: TableAuditSink) {
    /// let task = sink.spawn_purge(AuditConfig::default(), Duration::from_secs(3_600));
    /// assert!(!task.is_finished());
    /// task.stop();
    /// # }
    /// # let _ = run;
    /// # });
    /// ```
    #[must_use]
    pub fn spawn_purge(&self, config: AuditConfig, interval: Duration) -> PurgeTask {
        let sink = self.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // The first tick completes immediately; skipping it is what keeps a
            // crash-looping deploy from purging on every start.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match sink.purge_expired(&config).await {
                    Ok(removed) if removed > 0 => tracing::info!(
                        target: crate::audit::AUDIT_TARGET,
                        removed,
                        retention_days = config.retention_days,
                        "aged out authorization audit entries"
                    ),
                    Ok(_) => {}
                    Err(error) => tracing::error!(
                        target: crate::audit::AUDIT_TARGET,
                        error = %error,
                        "the authorization audit retention purge failed; it runs again at the \
                         next interval"
                    ),
                }
            }
        });
        PurgeTask { handle }
    }

    /// Write one entry, without going through the sink's error handling.
    ///
    /// What an application calls when it wants the audit row inside its own
    /// transaction, so that a request either records its decision or does not
    /// happen at all.
    ///
    /// # Errors
    ///
    /// Anything the database reports.
    ///
    /// ```no_run
    /// # use moso_authz::{AuditRecord, TableAuditSink};
    /// # async fn f(sink: &TableAuditSink, entry: AuditRecord) -> moso_orm::Result<()> {
    /// sink.write(entry).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn write(&self, entry: AuditRecord) -> moso_orm::Result<()> {
        let row = AuditEntry::of(&entry);
        RawQuery::new(insert_sql(self.db.backend()))
            .bind(row.at)
            .bind(row.actor)
            .bind(row.actor_kind)
            .bind(row.scope)
            .bind(row.action)
            .bind(row.resource)
            .bind(row.outcome)
            .bind(row.reason)
            .bind(row.request_id)
            .bind(row.ip)
            .bind(row.route)
            .execute(&self.db)
            .await?;
        Ok(())
    }
}

impl AuditSink for TableAuditSink {
    fn record<'a>(&'a self, entry: AuditRecord) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Err(error) = self.write(entry).await {
                // Logged and counted, not returned. See the type's
                // documentation for why the request is not failed, and
                // `audit_dropped` for why a log line alone is not enough.
                count_dropped(1);
                tracing::error!(
                    target: crate::audit::AUDIT_TARGET,
                    error = %error,
                    metric = crate::audit::DROPPED_METRIC,
                    "an authorization audit entry could not be written; the decision it \
                     records still stands"
                );
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// A running periodic purge, stopped by dropping it.
///
/// [`TableAuditSink::spawn_purge`] returns one. It is a handle and not a
/// [`Drop`]-time action: ageing rows out is not something to attempt during a
/// shutdown drain, so stopping is all this does.
///
/// ```
/// # use moso_authz::table::PurgeTask;
/// # fn f(task: PurgeTask) {
/// // Held for the life of the application; dropping it stops the timer.
/// drop(task);
/// # }
/// ```
#[derive(Debug)]
pub struct PurgeTask {
    /// The timer, until it is stopped.
    handle: tokio::task::JoinHandle<()>,
}

impl PurgeTask {
    /// Whether the task has stopped on its own.
    ///
    /// It only does so if the runtime is shutting down; a purge that fails logs
    /// and waits for the next tick, because a database that is briefly
    /// unreachable must not disable retention until the next deploy.
    ///
    /// ```
    /// # use moso_authz::table::PurgeTask;
    /// # fn f(task: &PurgeTask) { let _: bool = task.is_finished(); }
    /// ```
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Stop the timer now.
    ///
    /// The same thing dropping it does, spelled out for a call site where the
    /// drop would otherwise be invisible.
    ///
    /// ```
    /// # use moso_authz::table::PurgeTask;
    /// # fn f(task: PurgeTask) { task.stop(); }
    /// ```
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for PurgeTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// The `INSERT` the writer binds against, for the dialect in hand.
///
/// Public so an application can write the row inside its own transaction with
/// exactly the statement this crate would have used.
///
/// ```
/// use moso_authz::table::insert_sql;
///
/// assert!(insert_sql(moso_orm::Backend::Postgres).contains("$11"));
/// assert!(insert_sql(moso_orm::Backend::Sqlite).contains('?'));
/// ```
#[must_use]
pub fn insert_sql(backend: Backend) -> String {
    let columns = AuditEntry::COLUMNS.join(", ");
    let values = (1..=AuditEntry::COLUMNS.len())
        .map(|position| placeholders(backend, position))
        .collect::<Vec<_>>()
        .join(", ");
    format!("insert into {AUDIT_TABLE} ({columns}) values ({values})")
}

/// One bind placeholder, numbered for the dialect that numbers them.
fn placeholders(backend: Backend, position: usize) -> String {
    match backend {
        Backend::Postgres => format!("${position}"),
        _ => "?".to_owned(),
    }
}

/// The `CREATE TABLE` and indexes for `moso_authz_audit`.
///
/// The index on `(actor, at)` is the query the admin runs; the one on `at` is
/// the query retention runs.
///
/// ```
/// use moso_authz::table::create_table_sql;
///
/// assert!(create_table_sql(moso_orm::Backend::Postgres)[0].contains("bigserial"));
/// assert!(create_table_sql(moso_orm::Backend::Sqlite)[0].contains("integer primary key"));
/// ```
#[must_use]
pub fn create_table_sql(backend: Backend) -> Vec<String> {
    let (key, timestamp) = match backend {
        Backend::Postgres => ("id bigserial primary key", "timestamptz"),
        _ => ("id integer primary key autoincrement", "text"),
    };
    vec![
        format!(
            "create table if not exists {AUDIT_TABLE} (\n  \
                 {key},\n  \
                 at {timestamp} not null,\n  \
                 actor text not null,\n  \
                 actor_kind text not null,\n  \
                 scope text not null,\n  \
                 action text not null,\n  \
                 resource text,\n  \
                 outcome text not null,\n  \
                 reason text not null,\n  \
                 request_id text,\n  \
                 ip text,\n  \
                 route text\n\
             )"
        ),
        format!("create index if not exists {AUDIT_TABLE}_actor_at on {AUDIT_TABLE} (actor, at)"),
        format!("create index if not exists {AUDIT_TABLE}_at on {AUDIT_TABLE} (at)"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stored row as an entity, so a test can read the table back.
    ///
    /// Declared here and not in the module proper for the reason the module
    /// header gives: an `Entity` needs `moso-sql`, which is a *dev*-dependency
    /// of this crate. That is exactly enough to prove `AuditEntry::from_row`
    /// decodes what the writer wrote, in the order it wrote it.
    struct StoredEntry(AuditEntry);

    impl moso_orm::Entity for StoredEntry {
        type Pk = i64;

        const TABLE: moso_sql::TableRef = moso_sql::TableRef::from_static(AUDIT_TABLE);
        const COLUMNS: &'static [moso_orm::ColumnDef] = &[
            moso_orm::ColumnDef::new("at", moso_sql::ValueKind::Timestamp),
            moso_orm::ColumnDef::new("actor", moso_sql::ValueKind::Text),
            moso_orm::ColumnDef::new("actor_kind", moso_sql::ValueKind::Text),
            moso_orm::ColumnDef::new("scope", moso_sql::ValueKind::Text),
            moso_orm::ColumnDef::new("action", moso_sql::ValueKind::Text),
            moso_orm::ColumnDef::new("resource", moso_sql::ValueKind::Text),
            moso_orm::ColumnDef::new("outcome", moso_sql::ValueKind::Text),
            moso_orm::ColumnDef::new("reason", moso_sql::ValueKind::Text),
            moso_orm::ColumnDef::new("request_id", moso_sql::ValueKind::Text),
            moso_orm::ColumnDef::new("ip", moso_sql::ValueKind::Text),
            moso_orm::ColumnDef::new("route", moso_sql::ValueKind::Text),
        ];
        const NAME: &'static str = "AuditEntry";

        fn pk(&self) -> i64 {
            0
        }

        fn from_row(row: &Row) -> Result<Self, DecodeError> {
            AuditEntry::from_row(row).map(StoredEntry)
        }

        fn descriptor() -> &'static moso_orm::descriptor::EntityDescriptor {
            static DESCRIPTOR: std::sync::OnceLock<moso_orm::descriptor::EntityDescriptor> =
                std::sync::OnceLock::new();
            DESCRIPTOR.get_or_init(|| {
                moso_orm::descriptor::EntityDescriptor::builder("AuditEntry", Self::TABLE).build()
            })
        }
    }

    /// The entity's column list *is* the decoder's column list, which is the
    /// invariant `from_row` reads positionally against.
    #[test]
    fn the_reader_entity_declares_the_decoder_s_columns_in_order() {
        let declared: Vec<&str> = <StoredEntry as moso_orm::Entity>::COLUMNS
            .iter()
            .map(moso_orm::ColumnDef::name)
            .collect();
        assert_eq!(declared, AuditEntry::COLUMNS);
    }

    fn record() -> AuditRecord {
        AuditRecord::deny(
            ActorId::new("usr_1"),
            ActorKind::ApiKey,
            Scope::Org(ScopeId::new("acme")),
            "posts.publish",
            "not the author and not an admin",
        )
        .with_resource("Post", "456")
        .with_request(
            "01JABCDEF",
            Some("/posts/{id}/publish"),
            Some("203.0.113.7"),
        )
    }

    /// The DDL, the insert and the decoder all read from `COLUMNS`, so a column
    /// added to one is added to all three. This is what stands in for the
    /// entity descriptor the crate cannot declare.
    #[test]
    fn the_ddl_the_writer_and_the_decoder_agree_on_the_columns() {
        let ddl = &create_table_sql(Backend::Postgres)[0];
        for column in AuditEntry::COLUMNS {
            assert!(
                ddl.contains(&format!("\n  {column} ")),
                "`{column}` is not in the DDL"
            );
        }

        let insert = insert_sql(Backend::Postgres);
        for column in AuditEntry::COLUMNS {
            assert!(insert.contains(column), "`{column}` is not written");
        }
        assert!(
            insert.contains(&format!("${}", AuditEntry::COLUMNS.len())),
            "one placeholder per column: {insert}",
        );
        assert!(
            !insert.contains(" id,"),
            "the key is the database's: {insert}"
        );
    }

    #[test]
    fn the_two_dialects_number_their_placeholders_differently() {
        assert!(insert_sql(Backend::Postgres).contains("$1, $2"));
        assert!(insert_sql(Backend::Sqlite).contains("?, ?"));
        assert_eq!(
            insert_sql(Backend::Sqlite).matches('?').count(),
            AuditEntry::COLUMNS.len(),
        );
    }

    #[test]
    fn a_record_round_trips_through_the_row() {
        let original = record();
        let recovered = AuditEntry::of(&original).to_record();

        assert_eq!(recovered.actor, original.actor);
        assert_eq!(recovered.actor_kind, original.actor_kind);
        assert_eq!(recovered.scope, original.scope);
        assert_eq!(recovered.action, original.action);
        assert_eq!(recovered.resource, original.resource);
        assert_eq!(recovered.outcome, original.outcome);
        assert_eq!(recovered.reason, original.reason);
        assert_eq!(recovered.request_id, original.request_id);
        assert_eq!(recovered.ip, original.ip);
        assert_eq!(recovered.route, original.route);
        assert_eq!(recovered.at, original.at);
    }

    #[test]
    fn a_scope_key_round_trips_through_the_column() {
        for scope in [
            Scope::Global,
            Scope::Org(ScopeId::new("acme")),
            Scope::Project(ScopeId::new("apollo")),
            Scope::Custom {
                kind: "team".to_owned(),
                id: ScopeId::new("core"),
            },
        ] {
            assert_eq!(parse_scope(&scope.as_key()), scope);
        }
    }

    #[test]
    fn an_actor_kind_round_trips_through_the_column() {
        for kind in [
            ActorKind::Anonymous,
            ActorKind::User,
            ActorKind::ApiKey,
            ActorKind::Service,
            ActorKind::Job,
        ] {
            assert_eq!(parse_kind(kind.as_str()), kind);
        }
    }

    #[test]
    fn the_ddl_names_the_indexes_the_admin_and_retention_need() {
        for backend in [Backend::Postgres, Backend::Sqlite] {
            let statements = create_table_sql(backend);
            assert_eq!(statements.len(), 3);
            assert!(statements[1].contains("(actor, at)"), "{}", statements[1]);
            assert!(statements[2].contains("(at)"), "{}", statements[2]);
        }
    }

    #[test]
    fn the_select_list_is_the_decoder_s_order() {
        assert_eq!(AuditEntry::select_list(), AuditEntry::COLUMNS.join(", "));
    }

    // ── against a real database ───────────────────────────────────────────

    /// Written, counted and aged out. SQLite always; PostgreSQL when
    /// `DATABASE_URL` is set, and skipped with a message when it is not.
    async fn the_trail_is_written_and_aged_out(db: &Db) {
        let sink = TableAuditSink::new(db.clone());
        sink.create_table().await.expect("the table is created");
        RawQuery::new(format!("delete from {AUDIT_TABLE}"))
            .execute(db)
            .await
            .expect("a clean table");

        sink.record(record()).await;
        sink.record(AuditRecord::allow(
            ActorId::new("usr_2"),
            ActorKind::User,
            Scope::Global,
            "posts.read",
            "viewer",
        ))
        .await;

        // Read back through the decoder: the writer bound eleven values and the
        // reader takes eleven columns, positionally, and they have to line up.
        let stored: Vec<StoredEntry> = moso_orm::Select::<StoredEntry>::new()
            .filter(moso_orm::Column::<StoredEntry, String>::new("actor").eq("usr_1".to_owned()))
            .fetch_all(db)
            .await
            .expect("the query runs");

        assert_eq!(stored.len(), 1);
        let entry = &stored[0].0;
        assert_eq!(entry.actor, "usr_1");
        assert_eq!(entry.actor_kind, "api_key");
        assert_eq!(entry.scope, "org:acme");
        assert_eq!(entry.action, "posts.publish");
        assert_eq!(entry.resource.as_deref(), Some("Post#456"));
        assert_eq!(entry.outcome, "deny");
        assert_eq!(entry.reason, "not the author and not an admin");
        assert_eq!(entry.request_id.as_deref(), Some("01JABCDEF"));
        assert_eq!(entry.ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(entry.route.as_deref(), Some("/posts/{id}/publish"));

        // The other row has no resource, so the nullable columns decode as
        // `None` rather than as the empty string.
        let anonymous: Vec<StoredEntry> = moso_orm::Select::<StoredEntry>::new()
            .filter(moso_orm::Column::<StoredEntry, String>::new("actor").eq("usr_2".to_owned()))
            .fetch_all(db)
            .await
            .expect("the query runs");
        assert_eq!(anonymous.len(), 1);
        assert_eq!(anonymous[0].0.resource, None);
        assert_eq!(anonymous[0].0.outcome, "allow");

        // Retention removes what is older than the cutoff, and nothing else,
        // and reports how many rows went — which is also how many were written.
        assert_eq!(
            sink.purge(Utc::now() - chrono::Duration::days(1))
                .await
                .expect("purge"),
            0,
            "nothing is a day old yet",
        );
        assert_eq!(
            sink.purge(Utc::now() + chrono::Duration::days(1))
                .await
                .expect("purge"),
            2,
            "both entries were written, and both were old enough to go",
        );
        assert_eq!(
            sink.purge(Utc::now() + chrono::Duration::days(1))
                .await
                .expect("purge"),
            0,
            "the table is empty now",
        );
    }

    /// The configured retention window, applied. `retention_days` used to be a
    /// number nothing read; this is the statement it now produces.
    async fn the_configured_window_is_the_one_that_is_applied(db: &Db) {
        let sink = TableAuditSink::new(db.clone());
        sink.create_table().await.expect("the table is created");
        RawQuery::new(format!("delete from {AUDIT_TABLE}"))
            .execute(db)
            .await
            .expect("a clean table");

        let mut ancient = record();
        ancient.at = Utc::now() - chrono::Duration::days(400);
        sink.write(ancient).await.expect("the old entry is written");
        sink.write(record()).await.expect("and a fresh one");

        let forever = AuditConfig {
            retention_days: 0,
            ..AuditConfig::default()
        };
        assert_eq!(
            sink.purge_expired(&forever).await.expect("purge"),
            0,
            "zero days means keep forever, not delete everything",
        );

        assert_eq!(
            sink.purge_expired(&AuditConfig::default())
                .await
                .expect("purge"),
            1,
            "the default keeps a year, so only the 400-day-old entry goes",
        );
        assert_eq!(
            sink.purge_expired(&AuditConfig::default())
                .await
                .expect("purge"),
            0,
            "and running it again removes nothing",
        );
        assert_eq!(count_rows(db).await, 1, "the fresh entry is still there");
    }

    /// How many entries the table holds.
    async fn count_rows(db: &Db) -> usize {
        moso_orm::Select::<StoredEntry>::new()
            .unlimited()
            .fetch_all(db)
            .await
            .expect("the query runs")
            .len()
    }

    /// Both claims on one connection: the fixture is a table with a fixed name,
    /// so two tests that created it concurrently would race on the same server.
    async fn the_table_claims(db: &Db) {
        the_trail_is_written_and_aged_out(db).await;
        the_configured_window_is_the_one_that_is_applied(db).await;
    }

    #[tokio::test]
    async fn the_audit_table_works_on_sqlite() {
        let db = Db::connect_url("sqlite://:memory:")
            .await
            .expect("an in-memory SQLite database");
        the_table_claims(&db).await;
    }

    /// The runnable retention policy: a task, not a note telling the reader to
    /// write a scheduled job. On SQLite only — every in-memory database is its
    /// own, so a timer running against one cannot race another test's table.
    #[tokio::test]
    async fn the_periodic_purge_ages_entries_out_without_a_scheduled_job() {
        let db = Db::connect_url("sqlite://:memory:")
            .await
            .expect("an in-memory SQLite database");
        let sink = TableAuditSink::new(db.clone());
        sink.create_table().await.expect("the table is created");

        let mut ancient = record();
        ancient.at = Utc::now() - chrono::Duration::days(400);
        sink.write(ancient).await.expect("the old entry is written");
        sink.write(record()).await.expect("and a fresh one");
        assert_eq!(count_rows(&db).await, 2);

        let task = sink.spawn_purge(AuditConfig::default(), Duration::from_millis(20));

        // The first tick is skipped, so this also proves the task survives it.
        let mut remaining = count_rows(&db).await;
        for _ in 0..100 {
            if remaining == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            remaining = count_rows(&db).await;
        }

        assert_eq!(remaining, 1, "the timer aged the old entry out");
        assert!(
            !task.is_finished(),
            "and it is still running for the next one"
        );
        task.stop();
    }

    #[tokio::test]
    async fn the_audit_table_works_on_postgres() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skipping: DATABASE_URL is not set. Start the test server with \
                 `scripts/test-db.sh` and re-run to exercise the PostgreSQL path."
            );
            return;
        };
        if url.is_empty() {
            return;
        }
        let db = Db::connect_url(&url).await.expect("the test server");
        the_table_claims(&db).await;
        RawQuery::new(format!("drop table if exists {AUDIT_TABLE}"))
            .execute(&db)
            .await
            .expect("the fixture table is dropped");
    }
}
