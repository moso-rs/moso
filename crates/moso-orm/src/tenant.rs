//! Multi-tenancy: [`TenantId`], [`TenancyModel`] and [`TenantRouter`].
//!
//! `docs/02-data/24-transactions-pooling.md` documents three models, and this
//! module implements all three because "which one does your framework support"
//! is a question every SaaS asks and picking one for the user is picking wrong
//! for two thirds of them.
//!
//! | Model | Isolation | Tenants | What it costs |
//! | --- | --- | --- | --- |
//! | [`TenancyModel::Discriminator`] | a `WHERE` clause | millions | nothing; one pool |
//! | [`TenancyModel::SchemaPerTenant`] | a `search_path` | hundreds | one pool per live tenant |
//! | [`TenancyModel::DatabasePerTenant`] | a whole database | tens | one pool per live tenant |
//!
//! # Why the first one is enforced at compile time and the others are not
//!
//! Under the discriminator model, forgetting the scope does not fail: it
//! returns **another customer's rows**. That is the one failure in the data
//! layer that a runtime check cannot make safe, because by the time it is
//! observed the data has already been served. So `#[entity(tenant = "…")]`
//! makes `Select<Invoice, NeedsTenant>` unrunnable until `.scoped(..)` or
//! `.across_tenants()` discharges the obligation — see
//! [`Ready`](crate::Ready).
//!
//! Under the other two models the isolation is in the connection, not in the
//! statement, so there is nothing for a type to enforce: a query on a
//! tenant-routed handle *cannot* see another tenant's rows.
//!
//! This module is private; every type in it is re-exported from
//! [`crate::db`], which is where the frozen paths live.

use core::fmt;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use moso_sql::{Ident, Value};

use crate::db::Db;
use crate::db::config::DatabaseConfig;
use crate::error::{Error, Result};
use crate::sqltype::SqlType;

/// A tenant's identity, as a bound value.
///
/// Any [`SqlType`] can be a tenant key — a `Uuid`, an `i64`, a `String` — so
/// this wraps the bound form rather than fixing one.
///
/// ```
/// use moso_orm::TenantId;
///
/// let acme = TenantId::of(42_i64);
/// assert_eq!(acme.value(), &moso_sql::Value::I64(42));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct TenantId(Value);

impl TenantId {
    /// The tenant identified by `value`.
    ///
    /// ```
    /// use moso_orm::TenantId;
    ///
    /// assert_eq!(TenantId::of(1_i64), TenantId::of(1_i64));
    /// ```
    #[must_use]
    pub fn of(value: impl SqlType) -> Self {
        Self(value.into_value())
    }

    /// The tenant identified by an already-bound value.
    ///
    /// ```
    /// use moso_orm::TenantId;
    /// use moso_sql::Value;
    ///
    /// assert_eq!(TenantId::from_value(Value::I64(1)), TenantId::of(1_i64));
    /// ```
    #[must_use]
    pub const fn from_value(value: Value) -> Self {
        Self(value)
    }

    /// The bound value.
    ///
    /// ```
    /// use moso_orm::TenantId;
    ///
    /// assert!(!TenantId::of(1_i64).value().is_null());
    /// ```
    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.0
    }

    /// The bound value, consuming the identity.
    ///
    /// ```
    /// use moso_orm::TenantId;
    /// use moso_sql::Value;
    ///
    /// assert_eq!(TenantId::of(1_i64).into_value(), Value::I64(1));
    /// ```
    #[must_use]
    pub fn into_value(self) -> Value {
        self.0
    }

    /// The tenant as a cache key: the value rendered so that two equal tenants
    /// produce the same string and two different ones never do.
    ///
    /// The kind is part of the key, so that `TenantId::of(1_i64)` and
    /// `TenantId::of("1")` do not collide on the same pool.
    ///
    /// ```
    /// use moso_orm::TenantId;
    ///
    /// assert_ne!(TenantId::of(1_i64).key(), TenantId::of(String::from("1")).key());
    /// assert_eq!(TenantId::of(7_i64).key(), TenantId::of(7_i64).key());
    /// ```
    #[must_use]
    pub fn key(&self) -> String {
        format!("{:?}:{}", self.0.kind(), self.render())
    }

    /// The tenant as it appears inside an identifier or a URL.
    ///
    /// # Errors
    ///
    /// [`Error::Configuration`] for a tenant whose rendering is not
    /// `[A-Za-z0-9_]+`. This is the check that keeps a tenant key out of the
    /// `search_path` statement it would otherwise be interpolated into: the
    /// only way a tenant name reaches SQL is as a quoted [`Ident`], and the
    /// only way it reaches this function's output is by being boring.
    ///
    /// ```
    /// use moso_orm::TenantId;
    ///
    /// assert_eq!(TenantId::of(42_i64).slug()?, "42");
    /// assert!(TenantId::of(String::from("a; drop table users")).slug().is_err());
    /// # Ok::<(), moso_orm::Error>(())
    /// ```
    pub fn slug(&self) -> Result<String> {
        let rendered = self.render();
        let acceptable = !rendered.is_empty()
            && rendered
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        if !acceptable {
            return Err(Error::Configuration {
                detail: format!(
                    "the tenant `{rendered}` cannot name a schema or a database\n  \
                     help: a routed tenant key must be letters, digits, `_` or `-`\n  \
                     note: this is the check that stops a tenant name from being interpolated \
                     into SQL"
                ),
            });
        }
        Ok(rendered.replace('-', "_"))
    }

    /// The schema this tenant's tables live in, under
    /// [`TenancyModel::SchemaPerTenant`].
    ///
    /// # Errors
    ///
    /// [`Error::Configuration`] when the tenant key is not usable as an
    /// identifier, or when `prefix` plus the key is longer than an identifier
    /// may be.
    ///
    /// ```
    /// use moso_orm::TenantId;
    ///
    /// assert_eq!(TenantId::of(7_i64).schema("tenant_")?.as_str(), "tenant_7");
    /// # Ok::<(), moso_orm::Error>(())
    /// ```
    pub fn schema(&self, prefix: &str) -> Result<Ident> {
        let name = format!("{prefix}{}", self.slug()?);
        Ident::new(&name).map_err(|error| Error::Configuration {
            detail: format!(
                "the schema name `{name}` is not usable: {error}\n  \
                 help: shorten `tenancy.prefix`, or use shorter tenant keys"
            ),
        })
    }

    /// The value without its type tag: `42`, `acme`, a UUID's hyphenated form.
    ///
    /// Only the types a tenant key is ever declared as get a plain rendering.
    /// Anything else falls back to its debug form, which is unique — so
    /// [`TenantId::key`] stays injective — and is not `[A-Za-z0-9_-]`, so
    /// [`TenantId::slug`] refuses it rather than putting it in an identifier.
    fn render(&self) -> String {
        match &self.0 {
            Value::Text(text) => text.clone(),
            Value::Uuid(uuid) => uuid.to_string(),
            Value::Null(_) => String::new(),
            Value::Bool(value) => value.to_string(),
            Value::I8(value) => value.to_string(),
            Value::I16(value) => value.to_string(),
            Value::I32(value) => value.to_string(),
            Value::I64(value) => value.to_string(),
            Value::U8(value) => value.to_string(),
            Value::U16(value) => value.to_string(),
            Value::U32(value) => value.to_string(),
            Value::U64(value) => value.to_string(),
            Value::Decimal(value) => value.to_string(),
            other => format!("{other:?}"),
        }
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// Which multi-tenancy model an application uses.
///
/// ```
/// use moso_orm::db::TenancyModel;
///
/// assert_eq!(TenancyModel::default(), TenancyModel::Discriminator);
/// assert!(!TenancyModel::Discriminator.routes_connections());
/// assert!(TenancyModel::schema_per_tenant("tenant_").routes_connections());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TenancyModel {
    /// A `tenant_id` column, and a `WHERE` clause on every statement.
    ///
    /// The default recommendation: one pool, one schema, one set of indexes,
    /// and a scope obligation the compiler enforces.
    #[default]
    Discriminator,

    /// One PostgreSQL schema per tenant, selected with `search_path`.
    ///
    /// Suits tens to hundreds of tenants. Migrations run per schema — see
    /// `moso db migrate --all-tenants`.
    SchemaPerTenant {
        /// Prepended to the tenant key to make the schema name.
        prefix: String,
    },

    /// One database per tenant.
    ///
    /// Suits few, large tenants with a contractual isolation requirement. The
    /// template is a connection URL containing `{tenant}`.
    DatabasePerTenant {
        /// The connection URL, with `{tenant}` where the key goes.
        url_template: String,
    },
}

impl TenancyModel {
    /// One schema per tenant, named `prefix` + the tenant key.
    ///
    /// ```
    /// use moso_orm::db::TenancyModel;
    ///
    /// let model = TenancyModel::schema_per_tenant("tenant_");
    /// assert!(model.routes_connections());
    /// ```
    #[must_use]
    pub fn schema_per_tenant(prefix: impl Into<String>) -> Self {
        Self::SchemaPerTenant {
            prefix: prefix.into(),
        }
    }

    /// One database per tenant, at `url_template` with `{tenant}` substituted.
    ///
    /// ```
    /// use moso_orm::db::TenancyModel;
    ///
    /// let model = TenancyModel::database_per_tenant("postgres://h/app_{tenant}");
    /// assert!(model.routes_connections());
    /// ```
    #[must_use]
    pub fn database_per_tenant(url_template: impl Into<String>) -> Self {
        Self::DatabasePerTenant {
            url_template: url_template.into(),
        }
    }

    /// Whether a tenant gets its own connection rather than its own `WHERE`
    /// clause.
    ///
    /// ```
    /// use moso_orm::db::TenancyModel;
    ///
    /// assert!(!TenancyModel::Discriminator.routes_connections());
    /// ```
    #[must_use]
    pub const fn routes_connections(&self) -> bool {
        !matches!(self, Self::Discriminator)
    }

    /// The name this model goes by in the boot log and in `moso doctor`.
    ///
    /// ```
    /// use moso_orm::db::TenancyModel;
    ///
    /// assert_eq!(TenancyModel::Discriminator.as_str(), "discriminator");
    /// ```
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Discriminator => "discriminator",
            Self::SchemaPerTenant { .. } => "schema-per-tenant",
            Self::DatabasePerTenant { .. } => "database-per-tenant",
        }
    }

    /// The connection URL for `tenant`, under
    /// [`TenancyModel::DatabasePerTenant`].
    ///
    /// # Errors
    ///
    /// [`Error::Configuration`] for a template with no `{tenant}` placeholder,
    /// or a tenant key that is not usable in a URL.
    pub(crate) fn tenant_url(&self, tenant: &TenantId) -> Result<String> {
        let Self::DatabasePerTenant { url_template } = self else {
            return Err(Error::Configuration {
                detail: format!(
                    "`{}` does not route a tenant to its own database",
                    self.as_str()
                ),
            });
        };
        if !url_template.contains("{tenant}") {
            return Err(Error::Configuration {
                detail: String::from(
                    "the database-per-tenant URL template has no `{tenant}` placeholder, so \
                     every tenant would share one database\n  \
                     help: `postgres://host/app_{tenant}`",
                ),
            });
        }
        Ok(url_template.replace("{tenant}", &tenant.slug()?))
    }
}

impl fmt::Display for TenancyModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a per-tenant [`DatabaseConfig`] comes from.
///
/// Implemented by [`UrlTemplate`] for the common case, and by the application
/// when tenant connection strings live in a control-plane table.
///
/// Deliberately **synchronous**: producing a configuration is a string
/// operation, and opening the pool — the part that does I/O — is
/// [`Db::connect`]'s job. That keeps the trait dyn-compatible without a boxed
/// future (decision D4).
///
/// ```
/// use moso_orm::{DatabaseConfig, TenantId};
/// use moso_orm::db::TenantSource;
///
/// /// Every tenant on its own database, named from a control-plane map.
/// pub struct FromMap(std::collections::HashMap<String, String>);
///
/// impl TenantSource for FromMap {
///     fn config(&self, tenant: &TenantId) -> moso_orm::Result<DatabaseConfig> {
///         let url = self.0.get(&tenant.key()).ok_or_else(|| {
///             moso_orm::Error::Configuration { detail: format!("no database for {tenant}") }
///         })?;
///         Ok(DatabaseConfig::from_url(url.clone()))
///     }
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot say which database a tenant lives in",
    label = "not a tenant source",
    note = "implement `fn config(&self, tenant: &TenantId) -> Result<DatabaseConfig>`",
    note = "help: for one URL with a placeholder, use `UrlTemplate::new(\"postgres://h/app_<tenant>\", base)`",
    note = "help: `TenantRouter::new(32, {Self}::new())`"
)]
pub trait TenantSource: Send + Sync + 'static {
    /// The configuration for `tenant`'s pool.
    ///
    /// # Errors
    ///
    /// Whatever the lookup fails with; [`Error::Configuration`] for an unknown
    /// tenant is the usual answer.
    fn config(&self, tenant: &TenantId) -> Result<DatabaseConfig>;
}

/// A [`TenantSource`] that substitutes the tenant key into one URL.
///
/// ```
/// use moso_orm::{DatabaseConfig, TenantId};
/// use moso_orm::db::{TenantSource, UrlTemplate};
///
/// let base = DatabaseConfig::from_url("postgres://ignored");
/// let source = UrlTemplate::new("postgres://h/app_{tenant}", base);
///
/// let config = source.config(&TenantId::of(7_i64))?;
/// assert_eq!(config.url.expose(), "postgres://h/app_7");
/// # Ok::<(), moso_orm::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct UrlTemplate {
    template: String,
    base: DatabaseConfig,
}

impl UrlTemplate {
    /// A template with `{tenant}` where the key goes, and the pool settings
    /// every tenant's pool inherits.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    /// use moso_orm::db::UrlTemplate;
    ///
    /// let source = UrlTemplate::new(
    ///     "postgres://h/app_{tenant}",
    ///     DatabaseConfig::from_url("postgres://h/app").with_max_connections(2),
    /// );
    /// assert_eq!(source.template(), "postgres://h/app_{tenant}");
    /// ```
    #[must_use]
    pub fn new(template: impl Into<String>, base: DatabaseConfig) -> Self {
        Self {
            template: template.into(),
            base,
        }
    }

    /// The template, as given.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    /// use moso_orm::db::UrlTemplate;
    ///
    /// let source = UrlTemplate::new("postgres://h/{tenant}", DatabaseConfig::from_url("x"));
    /// assert!(source.template().contains("{tenant}"));
    /// ```
    #[must_use]
    pub fn template(&self) -> &str {
        &self.template
    }
}

impl TenantSource for UrlTemplate {
    fn config(&self, tenant: &TenantId) -> Result<DatabaseConfig> {
        let model = TenancyModel::database_per_tenant(self.template.clone());
        let url = model.tenant_url(tenant)?;
        let mut config = self.base.clone();
        config.url = moso_core::config::SecretString::new(url);
        config.replicas = Vec::new();
        config.application_name = format!("{} (tenant {tenant})", self.base.application_name);
        Ok(config)
    }
}

/// Tenant → [`Db`], with pools opened on demand and a cap on how many stay
/// open.
///
/// The third tenancy model. Register one as a provider and reach it with
/// `Inject<TenantRouter>`; ask it for a handle per request.
///
/// Pools are opened lazily and closed least-recently-used-first past
/// [`TenantRouter::capacity`], because a thousand idle tenants must not hold a
/// thousand pools' worth of connections open.
///
/// ```no_run
/// use moso_orm::{DatabaseConfig, Result, TenantId};
/// use moso_orm::db::{TenantRouter, UrlTemplate};
///
/// # async fn example() -> Result<()> {
/// let router = TenantRouter::new(
///     8,
///     UrlTemplate::new(
///         "postgres://h/app_{tenant}",
///         DatabaseConfig::from_url("postgres://h/app"),
///     ),
/// );
///
/// let acme = router.db(&TenantId::of(1_i64)).await?;
/// assert_eq!(router.len(), 1);
/// # let _ = acme;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct TenantRouter {
    inner: std::sync::Arc<TenantRouterInner>,
}

/// What a [`TenantRouter`]'s clones share.
struct TenantRouterInner {
    source: Box<dyn TenantSource>,
    capacity: usize,
    pools: Mutex<TenantPools<Db>>,
    evictions: AtomicU64,
}

impl TenantRouter {
    /// A router that keeps at most `capacity` pools open, taking each tenant's
    /// configuration from `source`.
    ///
    /// A capacity of zero is raised to one: a router that may hold no pools
    /// would open and close one per request.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    /// use moso_orm::db::{TenantRouter, UrlTemplate};
    ///
    /// let router = TenantRouter::new(
    ///     4,
    ///     UrlTemplate::new("postgres://h/{tenant}", DatabaseConfig::from_url("postgres://h/a")),
    /// );
    /// assert_eq!(router.capacity(), 4);
    /// assert!(router.is_empty());
    /// ```
    #[must_use]
    pub fn new(capacity: usize, source: impl TenantSource) -> Self {
        Self {
            inner: std::sync::Arc::new(TenantRouterInner {
                source: Box::new(source),
                capacity: capacity.max(1),
                pools: Mutex::new(TenantPools::default()),
                evictions: AtomicU64::new(0),
            }),
        }
    }

    /// The handle for `tenant`, opening its pool if this is the first ask.
    ///
    /// # Errors
    ///
    /// Whatever the [`TenantSource`] or [`Db::connect`] fails with.
    ///
    /// ```no_run
    /// # use moso_orm::{Result, TenantId};
    /// # use moso_orm::db::TenantRouter;
    /// # async fn example(router: &TenantRouter) -> Result<()> {
    /// let db = router.db(&TenantId::of(1_i64)).await?;
    /// db.ping().await
    /// # }
    /// ```
    pub async fn db(&self, tenant: &TenantId) -> Result<Db> {
        let key = tenant.key();
        if let Some(existing) = self
            .inner
            .pools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .touch(&key)
        {
            return Ok(existing);
        }

        let config = self.inner.source.config(tenant)?;
        let opened = Db::connect(&config).await?;

        // Another task may have opened the same tenant while this one was
        // connecting. Keep whichever landed first, so that one tenant never has
        // two pools; the loser is closed rather than leaked.
        let (stored, evicted) = {
            let mut pools = self.inner.pools.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = pools.touch(&key) {
                (Some(existing), Some(opened.clone()))
            } else {
                let evicted = pools.insert(key, opened.clone(), self.inner.capacity);
                (None, evicted)
            }
        };
        if let Some(evicted) = evicted {
            self.inner.evictions.fetch_add(1, Ordering::Relaxed);
            evicted.close().await;
        }
        Ok(stored.unwrap_or(opened))
    }

    /// How many pools are open.
    ///
    /// ```
    /// # use moso_orm::DatabaseConfig;
    /// # use moso_orm::db::{TenantRouter, UrlTemplate};
    /// # let router = TenantRouter::new(4, UrlTemplate::new("postgres://h/{tenant}", DatabaseConfig::from_url("x")));
    /// assert_eq!(router.len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .pools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Whether no pool is open.
    ///
    /// ```
    /// # use moso_orm::DatabaseConfig;
    /// # use moso_orm::db::{TenantRouter, UrlTemplate};
    /// # let router = TenantRouter::new(4, UrlTemplate::new("postgres://h/{tenant}", DatabaseConfig::from_url("x")));
    /// assert!(router.is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most pools that stay open at once.
    ///
    /// ```
    /// # use moso_orm::DatabaseConfig;
    /// # use moso_orm::db::{TenantRouter, UrlTemplate};
    /// # let router = TenantRouter::new(4, UrlTemplate::new("postgres://h/{tenant}", DatabaseConfig::from_url("x")));
    /// assert_eq!(router.capacity(), 4);
    /// ```
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// How many pools have been closed to stay under the cap.
    ///
    /// A number that climbs steadily means the cap is below the working set and
    /// every request is paying for a fresh pool.
    ///
    /// ```
    /// # use moso_orm::DatabaseConfig;
    /// # use moso_orm::db::{TenantRouter, UrlTemplate};
    /// # let router = TenantRouter::new(4, UrlTemplate::new("postgres://h/{tenant}", DatabaseConfig::from_url("x")));
    /// assert_eq!(router.evictions(), 0);
    /// ```
    #[must_use]
    pub fn evictions(&self) -> u64 {
        self.inner.evictions.load(Ordering::Relaxed)
    }

    /// Closes every pool. Called by the application's shutdown lifespan.
    ///
    /// ```no_run
    /// # use moso_orm::db::TenantRouter;
    /// # async fn shutdown(router: &TenantRouter) {
    /// router.close().await;
    /// # }
    /// ```
    pub async fn close(&self) {
        let open = {
            let mut pools = self.inner.pools.lock().unwrap_or_else(|e| e.into_inner());
            pools.drain()
        };
        for db in open {
            db.close().await;
        }
    }
}

impl fmt::Debug for TenantRouter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantRouter")
            .field("open", &self.len())
            .field("capacity", &self.inner.capacity)
            .finish_non_exhaustive()
    }
}

/// A least-recently-used map of tenant key to pool.
///
/// A linear scan rather than an intrusive list: the cap is tens, not millions,
/// and a `Vec` scan of thirty-two entries is faster than the pointer chasing a
/// real LRU would do — as well as being code somebody can read.
pub(crate) struct TenantPools<T> {
    entries: HashMap<String, (T, u64)>,
    clock: u64,
}

impl<T> Default for TenantPools<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
        }
    }
}

impl<T: Clone> TenantPools<T> {
    /// The pool for `key`, marked as just used.
    pub(crate) fn touch(&mut self, key: &str) -> Option<T> {
        self.clock += 1;
        let clock = self.clock;
        let (value, used) = self.entries.get_mut(key)?;
        *used = clock;
        Some(value.clone())
    }

    /// Stores `value` under `key`, returning the pool evicted to stay under
    /// `capacity`.
    pub(crate) fn insert(&mut self, key: String, value: T, capacity: usize) -> Option<T> {
        self.clock += 1;
        self.entries.insert(key, (value, self.clock));
        if self.entries.len() <= capacity {
            return None;
        }
        let oldest = self
            .entries
            .iter()
            .min_by_key(|(_, (_, used))| *used)
            .map(|(key, _)| key.clone())?;
        self.entries.remove(&oldest).map(|(value, _)| value)
    }

    /// Empties the map, handing back every pool so the caller can close them.
    pub(crate) fn drain(&mut self) -> Vec<T> {
        self.entries.drain().map(|(_, (value, _))| value).collect()
    }

    /// How many pools are held.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenant_id_accepts_any_column_type() {
        assert_eq!(TenantId::of(1_i64).value(), &Value::I64(1));
        assert_eq!(
            TenantId::of(String::from("acme")).value(),
            &Value::text("acme")
        );
    }

    #[test]
    fn the_key_separates_tenants_that_render_the_same() {
        assert_ne!(
            TenantId::of(1_i64).key(),
            TenantId::of(String::from("1")).key()
        );
        assert_eq!(TenantId::of(1_i64).key(), TenantId::of(1_i64).key());
    }

    #[test]
    fn a_routed_tenant_key_cannot_carry_sql() {
        let hostile = TenantId::of(String::from("public\"; drop schema public cascade; --"));
        let error = hostile.slug().expect_err("that is not an identifier");
        assert!(error.to_string().contains("help:"), "{error}");

        // A space is enough to be refused: the check is an allowlist.
        assert!(TenantId::of(String::from("two words")).slug().is_err());
        // And the boring cases work.
        assert_eq!(TenantId::of(42_i64).slug().expect("digits"), "42");
        assert_eq!(
            TenantId::of(String::from("acme_eu")).slug().expect("word"),
            "acme_eu"
        );
    }

    #[test]
    fn a_schema_name_is_a_validated_identifier() {
        let schema = TenantId::of(7_i64).schema("tenant_").expect("valid");
        assert_eq!(schema.as_str(), "tenant_7");

        let too_long = TenantId::of("x".repeat(80));
        assert!(too_long.schema("tenant_").is_err());
    }

    #[test]
    fn the_url_template_needs_its_placeholder() {
        let model = TenancyModel::database_per_tenant("postgres://h/app");
        let error = model
            .tenant_url(&TenantId::of(1_i64))
            .expect_err("no placeholder");
        assert!(error.to_string().contains("{tenant}"), "{error}");

        let model = TenancyModel::database_per_tenant("postgres://h/app_{tenant}");
        assert_eq!(
            model.tenant_url(&TenantId::of(1_i64)).expect("substituted"),
            "postgres://h/app_1"
        );
    }

    #[test]
    fn only_the_discriminator_model_shares_a_connection() {
        assert!(!TenancyModel::Discriminator.routes_connections());
        assert!(TenancyModel::schema_per_tenant("t_").routes_connections());
        assert!(TenancyModel::database_per_tenant("u{tenant}").routes_connections());
        assert_eq!(TenancyModel::default(), TenancyModel::Discriminator);
    }

    #[test]
    fn the_url_template_source_inherits_the_pool_settings() {
        let base = DatabaseConfig::from_url("postgres://h/app")
            .with_max_connections(3)
            .with_application_name("shop");
        let source = UrlTemplate::new("postgres://h/app_{tenant}", base);

        let config = source
            .config(&TenantId::of(String::from("acme")))
            .expect("a boring key");
        assert_eq!(config.url.expose(), "postgres://h/app_acme");
        assert_eq!(config.max_connections, 3);
        assert!(config.application_name.contains("acme"));
        assert!(config.replicas.is_empty());
    }

    #[test]
    fn the_lru_evicts_the_least_recently_used_and_nothing_else() {
        // The policy is tested over `u32` rather than `Db`, because opening a
        // pool needs a server and the eviction order does not.
        let mut pools = TenantPools::<u32>::default();
        assert!(pools.touch("a").is_none());

        assert!(pools.insert("a".into(), 1, 2).is_none());
        assert!(pools.insert("b".into(), 2, 2).is_none());
        assert_eq!(pools.len(), 2);

        // Using `a` makes `b` the least recently used …
        assert_eq!(pools.touch("a"), Some(1));
        assert_eq!(
            pools.insert("c".into(), 3, 2),
            Some(2),
            "the untouched entry is the one that goes"
        );
        assert_eq!(pools.len(), 2);
        assert_eq!(pools.touch("a"), Some(1));
        assert_eq!(pools.touch("c"), Some(3));
        assert!(pools.touch("b").is_none());

        let drained = pools.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(pools.len(), 0);
    }

    #[test]
    fn a_capacity_of_zero_still_holds_one_pool() {
        let router = TenantRouter::new(
            0,
            UrlTemplate::new(
                "postgres://h/{tenant}",
                DatabaseConfig::from_url("postgres://h/a"),
            ),
        );
        assert_eq!(router.capacity(), 1, "a router that holds nothing thrashes");
        assert!(router.is_empty());
        assert_eq!(router.evictions(), 0);
    }
}
