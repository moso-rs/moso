//! Backend choice as configuration.
//!
//! Which store a process writes to is a deployment decision. An application
//! builds one [`StorageConfig`] from its typed config and calls
//! [`StorageConfig::build`]; nothing on the request path names a backend.

use std::time::Duration;

use moso_core::config::SecretString;

use crate::{Deadlines, Result, Storage, TimedStorage};

/// Which backend a process writes to.
///
/// ```
/// use moso_storage::StorageBackendKind;
///
/// assert_eq!(StorageBackendKind::default(), StorageBackendKind::Local);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum StorageBackendKind {
    /// A directory on this machine. The development default.
    #[default]
    Local,
    /// A map in this process. Tests.
    Memory,
    /// S3, or anything that speaks its API.
    S3,
    /// Google Cloud Storage.
    Gcs,
    /// Azure Blob Storage.
    Azure,
}

impl StorageBackendKind {
    /// Parse the value of a `STORAGE_BACKEND` variable.
    ///
    /// ```
    /// use moso_storage::StorageBackendKind;
    ///
    /// assert_eq!(StorageBackendKind::parse("s3"), Some(StorageBackendKind::S3));
    /// assert_eq!(StorageBackendKind::parse("  S3 "), Some(StorageBackendKind::S3));
    /// assert_eq!(StorageBackendKind::parse("floppy"), None);
    /// ```
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" => Some(Self::Local),
            "memory" => Some(Self::Memory),
            // Every S3-compatible service is the S3 backend with an endpoint,
            // and an operator who writes the vendor's name should get it.
            "s3" | "r2" | "minio" | "b2" | "backblaze" | "wasabi" | "tigris" => Some(Self::S3),
            "gcs" | "google" => Some(Self::Gcs),
            "azure" | "blob" => Some(Self::Azure),
            _ => None,
        }
    }

    /// Every name [`parse`](StorageBackendKind::parse) accepts, in order.
    ///
    /// What an unknown-backend error suggests from, and what `.env.example`
    /// lists next to `STORAGE_BACKEND`.
    ///
    /// ```
    /// use moso_storage::StorageBackendKind;
    ///
    /// assert!(StorageBackendKind::NAMES.contains(&"minio"));
    /// ```
    pub const NAMES: &'static [&'static str] = &[
        "local", "memory", "s3", "r2", "minio", "b2", "wasabi", "tigris", "gcs", "azure",
    ];

    /// The cargo feature that has to be on for this backend to build.
    ///
    /// Named in the error, because "unknown backend `s3`" when the real
    /// problem is a feature flag is the least helpful message in the crate.
    ///
    /// ```
    /// use moso_storage::StorageBackendKind;
    ///
    /// assert_eq!(StorageBackendKind::S3.feature(), "s3");
    /// ```
    #[must_use]
    pub const fn feature(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Memory => "memory",
            Self::S3 => "s3",
            Self::Gcs => "gcs",
            Self::Azure => "azure",
        }
    }

    /// Whether this backend needs a bucket and credentials.
    ///
    /// ```
    /// use moso_storage::StorageBackendKind;
    ///
    /// assert!(StorageBackendKind::S3.needs_bucket());
    /// assert!(!StorageBackendKind::Local.needs_bucket());
    /// ```
    #[must_use]
    pub const fn needs_bucket(self) -> bool {
        !self.is_local()
    }

    /// The name this parses from.
    ///
    /// ```
    /// use moso_storage::StorageBackendKind;
    ///
    /// assert_eq!(StorageBackendKind::S3.as_str(), "s3");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Memory => "memory",
            Self::S3 => "s3",
            Self::Gcs => "gcs",
            Self::Azure => "azure",
        }
    }

    /// Whether this backend needs no external service.
    ///
    /// The boot log warns when a production profile picks one, because it means
    /// uploads do not survive a redeploy.
    ///
    /// ```
    /// use moso_storage::StorageBackendKind;
    ///
    /// assert!(StorageBackendKind::Memory.is_local());
    /// ```
    #[must_use]
    pub const fn is_local(self) -> bool {
        matches!(self, Self::Local | Self::Memory)
    }
}

/// Everything a process needs to store objects.
///
/// ```no_run
/// use moso_storage::{StorageBackendKind, StorageConfig};
///
/// let config = StorageConfig::new(StorageBackendKind::S3)
///     .bucket("uploads")
///     .region("eu-central-1");
/// config.validate()?;
/// # Ok::<(), moso_storage::Error>(())
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct StorageConfig {
    /// Which backend to build.
    pub backend: StorageBackendKind,
    /// The bucket or container, for the cloud backends.
    pub bucket: Option<String>,
    /// The region, for the backends that have one.
    pub region: Option<String>,
    /// The access key id or account name.
    pub access_key: Option<String>,
    /// The secret. Redacted in every `Debug` and log.
    pub secret_key: Option<SecretString>,
    /// A custom endpoint, for an S3-compatible service.
    pub endpoint: Option<String>,
    /// A key prefix applied to every operation.
    pub prefix: Option<String>,
    /// The directory the local backend writes to.
    pub root: Option<std::path::PathBuf>,
    /// The URL prefix the local backend serves from.
    pub public_base: Option<String>,
    /// How long a signed URL lasts by default.
    pub url_ttl: Duration,
    /// How long a call that answers once may take before it is abandoned.
    ///
    /// [`Deadlines::operation`]: `head`, `delete`, `list`, `copy`,
    /// `signed_url`, `presigned_upload`, `multipart_start`, `probe` and each
    /// multipart part. **Not** `put`, `get` or `get_range` — a whole-operation
    /// deadline around a streaming transfer kills healthy gibibytes, and those
    /// are bounded by [`stall_timeout`](StorageConfig::stall_timeout) instead.
    pub timeout: Duration,
    /// How long a transfer may move no bytes before it is abandoned.
    ///
    /// [`Deadlines::idle`]: it restarts on every chunk, so a slow download
    /// finishes and a socket that went quiet does not hold a connection, a task
    /// and a buffer open until the process restarts.
    pub stall_timeout: Duration,
}

impl StorageConfig {
    /// A configuration with the documented defaults.
    ///
    /// ```
    /// use moso_storage::{StorageBackendKind, StorageConfig};
    ///
    /// let config = StorageConfig::new(StorageBackendKind::Local);
    /// assert_eq!(config.url_ttl, std::time::Duration::from_secs(300));
    /// assert_eq!(config.timeout, std::time::Duration::from_secs(30));
    /// assert_eq!(config.stall_timeout, std::time::Duration::from_secs(30));
    /// assert_eq!(config.root.as_deref(), Some(std::path::Path::new("var/uploads")));
    /// ```
    #[must_use]
    pub fn new(backend: StorageBackendKind) -> Self {
        Self {
            backend,
            bucket: None,
            region: None,
            access_key: None,
            secret_key: None,
            endpoint: None,
            prefix: None,
            root: Some(std::path::PathBuf::from(DEFAULT_ROOT)),
            public_base: None,
            url_ttl: DEFAULT_URL_TTL,
            timeout: DEFAULT_TIMEOUT,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
        }
    }

    /// Set the bucket or container.
    ///
    /// ```
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// let config = StorageConfig::new(StorageBackendKind::S3).bucket("uploads");
    /// assert_eq!(config.bucket.as_deref(), Some("uploads"));
    /// ```
    #[must_use]
    pub fn bucket(mut self, bucket: impl Into<String>) -> Self {
        self.bucket = Some(bucket.into());
        self
    }

    /// Set the region.
    ///
    /// ```
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// let _ = StorageConfig::new(StorageBackendKind::S3).region("eu-central-1");
    /// ```
    #[must_use]
    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Set the credentials.
    ///
    /// ```
    /// # use moso_core::config::SecretString;
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// let _ = StorageConfig::new(StorageBackendKind::S3)
    ///     .credentials("AKIA…", SecretString::new("secret"));
    /// ```
    #[must_use]
    pub fn credentials(mut self, access_key: impl Into<String>, secret_key: SecretString) -> Self {
        self.access_key = Some(access_key.into());
        self.secret_key = Some(secret_key);
        self
    }

    /// Point at a custom S3-compatible endpoint.
    ///
    /// ```
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// let _ = StorageConfig::new(StorageBackendKind::S3).endpoint("http://127.0.0.1:9000");
    /// ```
    #[must_use]
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Prefix every key, so one bucket can host several applications.
    ///
    /// ```
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// let _ = StorageConfig::new(StorageBackendKind::S3).prefix("shop");
    /// ```
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Set the directory the local backend writes to.
    ///
    /// ```
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// let _ = StorageConfig::new(StorageBackendKind::Local).root("var/uploads");
    /// ```
    #[must_use]
    pub fn root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Serve the local backend from `base`, signed with `key`.
    ///
    /// Without both, the local backend reports `signed_urls: false` — which is
    /// honest, and which makes the difference between development and
    /// production visible instead of surprising.
    ///
    /// ```
    /// # use moso_core::config::SecretString;
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// let _ = StorageConfig::new(StorageBackendKind::Local)
    ///     .served_at("/_storage", SecretString::new("a-32-byte-development-signing-key"));
    /// ```
    #[must_use]
    pub fn served_at(mut self, base: impl Into<String>, key: SecretString) -> Self {
        self.public_base = Some(base.into());
        self.secret_key = Some(key);
        self
    }

    /// How long a signed URL lasts. Default five minutes.
    ///
    /// ```
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// let _ = StorageConfig::new(StorageBackendKind::S3).url_ttl(std::time::Duration::from_secs(60));
    /// ```
    #[must_use]
    pub fn url_ttl(mut self, ttl: Duration) -> Self {
        self.url_ttl = ttl;
        self
    }

    /// How long a call that answers once may take. Default thirty seconds.
    ///
    /// ```
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// let config = StorageConfig::new(StorageBackendKind::S3)
    ///     .timeout(std::time::Duration::from_secs(5));
    /// assert_eq!(config.timeout, std::time::Duration::from_secs(5));
    /// ```
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// How long a transfer may move no bytes. Default thirty seconds.
    ///
    /// Worth raising well above [`timeout`](StorageConfig::timeout) when the
    /// store is far away: a `head` across an ocean still answers in under a
    /// second, while a transfer over the same link deserves a much longer quiet
    /// period before it is called dead.
    ///
    /// ```
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// let config = StorageConfig::new(StorageBackendKind::S3)
    ///     .stall_timeout(std::time::Duration::from_secs(120));
    /// assert_eq!(config.stall_timeout, std::time::Duration::from_secs(120));
    /// ```
    #[must_use]
    pub fn stall_timeout(mut self, timeout: Duration) -> Self {
        self.stall_timeout = timeout;
        self
    }

    /// The deadlines this configuration describes.
    ///
    /// What [`build`](StorageConfig::build) wraps the backend in. Read it to
    /// apply the same policy to a backend built by hand — the development
    /// `LocalStorage` that `LocalStorage::routes` needs, for instance.
    ///
    /// ```
    /// use moso_storage::{StorageBackendKind, StorageConfig};
    ///
    /// let config = StorageConfig::new(StorageBackendKind::Local);
    /// assert_eq!(config.deadlines().operation(), Some(config.timeout));
    /// assert_eq!(config.deadlines().idle(), Some(config.stall_timeout));
    /// ```
    #[must_use]
    pub const fn deadlines(&self) -> Deadlines {
        Deadlines::new(self.timeout, self.stall_timeout)
    }

    /// Check for contradictions before anything tries to connect.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) naming the field and the fix: a
    /// cloud backend with no bucket or no credentials, a zero URL TTL, a local
    /// backend with a root that is not a directory.
    ///
    /// ```
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// // A cloud backend with nowhere to write is caught at boot, not at the
    /// // first upload.
    /// assert!(StorageConfig::new(StorageBackendKind::S3).validate().is_err());
    /// assert!(StorageConfig::new(StorageBackendKind::Local).validate().is_ok());
    /// ```
    pub fn validate(&self) -> Result<()> {
        if self.backend.needs_bucket() && self.bucket.is_none() {
            return Err(crate::Error::config(format!(
                "`storage.bucket` is required for the `{}` backend — set `STORAGE_BUCKET`, or \
                 choose `STORAGE_BACKEND=local` for local development",
                self.backend.as_str(),
            )));
        }
        if self.backend == StorageBackendKind::S3
            && (self.access_key.is_none() || self.secret_key.is_none())
        {
            return Err(crate::Error::config(
                "`storage.access_key` and `storage.secret_key` are both required for the `s3` \
                 backend — set `STORAGE_ACCESS_KEY` and `STORAGE_SECRET_KEY`",
            ));
        }
        if matches!(
            self.backend,
            StorageBackendKind::Gcs | StorageBackendKind::Azure
        ) && self.secret_key.is_none()
        {
            return Err(crate::Error::config(format!(
                "`storage.secret_key` is required for the `{}` backend — set \
                 `STORAGE_SECRET_KEY` to the account key, or to `metadata` on GCP to use \
                 workload identity",
                self.backend.as_str(),
            )));
        }
        if self.backend == StorageBackendKind::Azure && self.access_key.is_none() {
            return Err(crate::Error::config(
                "`storage.access_key` is the Azure storage account name and is required — set \
                 `STORAGE_ACCESS_KEY`",
            ));
        }
        if self.backend == StorageBackendKind::Local && self.root.is_none() {
            return Err(crate::Error::config(
                "`storage.root` is required for the `local` backend — set `STORAGE_ROOT` to a \
                 writable path such as `var/uploads`",
            ));
        }
        if let Some(root) = &self.root
            && root.exists()
            && !root.is_dir()
        {
            return Err(crate::Error::config(format!(
                "`storage.root` points at `{}`, which exists and is not a directory",
                root.display(),
            )));
        }
        if self.url_ttl.is_zero() {
            return Err(crate::Error::config(
                "`storage.url_ttl` is zero, so every signed URL expires before it is used — set \
                 it to a few minutes, e.g. `STORAGE_URL_TTL=5m`",
            ));
        }
        if self.timeout.is_zero() {
            return Err(crate::Error::config(
                "`storage.timeout` is zero, which abandons every operation before it starts — \
                 set it to a few seconds, e.g. `STORAGE_TIMEOUT=30s`",
            ));
        }
        if self.stall_timeout.is_zero() {
            return Err(crate::Error::config(
                "`storage.stall_timeout` is zero, which abandons every upload and download before \
                 its first byte — set it to at least a few seconds, e.g. \
                 `STORAGE_STALL_TIMEOUT=30s`",
            ));
        }
        Ok(())
    }

    /// Warnings an operator should see at boot, but which are not failures.
    ///
    /// ```
    /// # use moso_storage::{StorageBackendKind, StorageConfig};
    /// let config = StorageConfig::new(StorageBackendKind::Memory);
    /// assert_eq!(config.warnings(true).len(), 1);
    /// assert!(config.warnings(false).is_empty());
    /// ```
    #[must_use]
    pub fn warnings(&self, production: bool) -> Vec<String> {
        let mut warnings = Vec::new();
        if !production {
            return warnings;
        }
        if self.backend.is_local() {
            warnings.push(format!(
                "storage: this profile writes to the `{}` backend, so uploads do not survive a \
                 redeploy and are not shared between instances — set `STORAGE_BACKEND` to `s3`, \
                 `gcs` or `azure`",
                self.backend.as_str(),
            ));
        }
        if self.public_base.is_some() && self.backend == StorageBackendKind::Local {
            warnings.push(
                "storage: the development file route is mounted in a production profile; user \
                 content should be served from a separate origin"
                    .to_owned(),
            );
        }
        warnings
    }

    /// Build the storage this configuration describes.
    ///
    /// The backend comes back wrapped in [`TimedStorage`], so
    /// [`deadlines`](StorageConfig::deadlines) are enforced without an
    /// application doing anything. The wrapper is transparent —
    /// [`name`](Storage::name) and [`capabilities`](Storage::capabilities) are
    /// still the real backend's.
    ///
    /// # Errors
    ///
    /// Everything [`validate`](StorageConfig::validate) reports, plus
    /// [`Error::Config`](crate::Error::Config) when the chosen backend's cargo
    /// feature is off — with the feature name in the message.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_storage::{Storage, StorageBackendKind, StorageConfig};
    /// let storage: Arc<dyn Storage> = StorageConfig::new(StorageBackendKind::Memory).build()?;
    /// assert_eq!(storage.name(), "memory");
    /// # Ok::<(), moso_storage::Error>(())
    /// ```
    pub fn build(&self) -> Result<std::sync::Arc<dyn Storage>> {
        self.validate()?;
        let deadlines = self.deadlines();

        /// The message a backend whose feature is off produces.
        ///
        /// Unreachable when every backend feature is on, which is what the
        /// `cfg_attr` says; with any of them off it is the only arm that runs.
        #[cfg_attr(
            all(
                feature = "local",
                feature = "memory",
                feature = "s3",
                feature = "gcs",
                feature = "azure"
            ),
            expect(
                dead_code,
                reason = "every backend is compiled in, so no arm reaches it"
            )
        )]
        fn disabled(kind: StorageBackendKind) -> crate::Error {
            crate::Error::config(format!(
                "the `{}` storage backend needs the `{}` cargo feature — add \
                 `moso-storage = {{ features = [\"{}\"] }}` to your `Cargo.toml`",
                kind.as_str(),
                kind.feature(),
                kind.feature(),
            ))
        }

        /// A required field, with the environment variable in the message.
        fn required<'a, T>(value: Option<&'a T>, field: &str) -> Result<&'a T> {
            value.ok_or_else(|| {
                crate::Error::config(format!(
                    "`storage.{field}` is required — set `STORAGE_{}`",
                    field.to_ascii_uppercase(),
                ))
            })
        }

        match self.backend {
            StorageBackendKind::Memory => {
                #[cfg(feature = "memory")]
                {
                    Ok(std::sync::Arc::new(TimedStorage::new(
                        crate::backend::MemoryStorage::new(),
                        deadlines,
                    )))
                }
                #[cfg(not(feature = "memory"))]
                Err(disabled(StorageBackendKind::Memory))
            }
            StorageBackendKind::Local => {
                #[cfg(feature = "local")]
                {
                    let root = required(self.root.as_ref(), "root")?;
                    let mut storage = crate::backend::LocalStorage::new(root.clone());
                    if let (Some(base), Some(key)) = (&self.public_base, &self.secret_key) {
                        storage = storage.served_at(
                            base.clone(),
                            moso_core::config::SecretBytes::new(key.expose().as_bytes().to_vec()),
                        );
                    }
                    Ok(std::sync::Arc::new(TimedStorage::new(storage, deadlines)))
                }
                #[cfg(not(feature = "local"))]
                Err(disabled(StorageBackendKind::Local))
            }
            StorageBackendKind::S3 => {
                #[cfg(feature = "s3")]
                {
                    let mut storage = crate::backend::S3Storage::new(
                        required(self.bucket.as_ref(), "bucket")?.clone(),
                        self.region
                            .clone()
                            .unwrap_or_else(|| "us-east-1".to_owned()),
                        required(self.access_key.as_ref(), "access_key")?.clone(),
                        required(self.secret_key.as_ref(), "secret_key")?.clone(),
                    );
                    if let Some(endpoint) = &self.endpoint {
                        storage = storage.endpoint(endpoint.clone());
                    }
                    if let Some(prefix) = &self.prefix {
                        storage = storage.prefix(prefix.clone());
                    }
                    Ok(std::sync::Arc::new(TimedStorage::new(storage, deadlines)))
                }
                #[cfg(not(feature = "s3"))]
                Err(disabled(StorageBackendKind::S3))
            }
            StorageBackendKind::Gcs => {
                #[cfg(feature = "gcs")]
                {
                    let mut storage = crate::backend::GcsStorage::new(
                        required(self.bucket.as_ref(), "bucket")?.clone(),
                        required(self.secret_key.as_ref(), "secret_key")?.clone(),
                    )?;
                    if let Some(prefix) = &self.prefix {
                        storage = storage.prefix(prefix.clone());
                    }
                    Ok(std::sync::Arc::new(TimedStorage::new(storage, deadlines)))
                }
                #[cfg(not(feature = "gcs"))]
                Err(disabled(StorageBackendKind::Gcs))
            }
            StorageBackendKind::Azure => {
                #[cfg(feature = "azure")]
                {
                    let mut storage = crate::backend::AzureStorage::new(
                        required(self.access_key.as_ref(), "access_key")?.clone(),
                        required(self.bucket.as_ref(), "bucket")?.clone(),
                        required(self.secret_key.as_ref(), "secret_key")?.clone(),
                    );
                    if let Some(prefix) = &self.prefix {
                        storage = storage.prefix(prefix.clone());
                    }
                    Ok(std::sync::Arc::new(TimedStorage::new(storage, deadlines)))
                }
                #[cfg(not(feature = "azure"))]
                Err(disabled(StorageBackendKind::Azure))
            }
        }
    }

    /// The readiness probe for this backend.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_storage::{Storage, StorageBackendKind, StorageConfig, StorageHealthCheck};
    /// let storage = StorageConfig::new(StorageBackendKind::Memory).build()?;
    /// let _: StorageHealthCheck = StorageConfig::health_check(storage);
    /// # Ok::<(), moso_storage::Error>(())
    /// ```
    #[must_use]
    pub fn health_check(storage: std::sync::Arc<dyn Storage>) -> StorageHealthCheck {
        StorageHealthCheck::new(storage)
    }
}

/// The directory the local backend writes to when configuration does not say.
const DEFAULT_ROOT: &str = "var/uploads";

/// How long a signed URL lasts when configuration does not say.
const DEFAULT_URL_TTL: Duration = Duration::from_secs(300);

/// How long one call that answers once may take when configuration does not say.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a transfer may go quiet when configuration does not say.
///
/// The same number as [`DEFAULT_TIMEOUT`] and for the same reason — thirty
/// seconds of silence is a dead socket on any link — but a separate constant,
/// because they answer different questions and one of them will move.
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The `/readyz` probe for object storage.
///
/// Not critical by default: an application that reads its database and writes
/// uploads should stay in rotation when the bucket is briefly unreachable, and
/// fail the individual upload instead.
///
/// ```no_run
/// # use std::sync::Arc;
/// # use moso_storage::{Storage, StorageHealthCheck};
/// # fn f(s: Arc<dyn Storage>) {
/// let _ = StorageHealthCheck::new(s).critical(true);
/// # }
/// ```
#[derive(Clone)]
pub struct StorageHealthCheck {
    /// What to probe.
    storage: std::sync::Arc<dyn Storage>,
    /// Whether a failure makes the instance unready.
    critical: bool,
}

impl StorageHealthCheck {
    /// A non-critical probe of `storage`.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_storage::{Storage, StorageHealthCheck};
    /// # fn f(s: Arc<dyn Storage>) { let _ = StorageHealthCheck::new(s); }
    /// ```
    #[must_use]
    pub fn new(storage: std::sync::Arc<dyn Storage>) -> Self {
        Self {
            storage,
            critical: false,
        }
    }

    /// Make a failure take the instance out of rotation.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_storage::{Storage, StorageHealthCheck};
    /// # fn f(s: Arc<dyn Storage>) { let _ = StorageHealthCheck::new(s).critical(true); }
    /// ```
    #[must_use]
    pub fn critical(mut self, critical: bool) -> Self {
        self.critical = critical;
        self
    }
}

impl core::fmt::Debug for StorageHealthCheck {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StorageHealthCheck")
            .field("backend", &self.storage.name())
            .field("critical", &self.critical)
            .finish()
    }
}

impl moso_core::HealthCheck for StorageHealthCheck {
    fn check<'a>(
        &'a self,
        resolver: &'a moso_core::Resolver,
    ) -> moso_core::BoxFuture<'a, moso_core::health::HealthStatus> {
        let _ = resolver;
        Box::pin(async move {
            match self.storage.probe().await {
                Ok(()) => moso_core::health::HealthStatus::Up,
                // Degraded rather than down when the check is not critical: an
                // application that reads its database and writes uploads should
                // stay in rotation when the bucket is briefly unreachable, and
                // fail the individual upload instead.
                Err(error) if !self.critical => {
                    moso_core::health::HealthStatus::Degraded(error.to_string())
                }
                Err(error) => moso_core::health::HealthStatus::Down(error.to_string()),
            }
        })
    }

    fn critical(&self) -> bool {
        self.critical
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every combination the boot report should catch, caught.
    #[test]
    fn validation_catches_the_contradictions_it_is_for() {
        assert!(
            StorageConfig::new(StorageBackendKind::S3)
                .validate()
                .is_err()
        );
        assert!(
            StorageConfig::new(StorageBackendKind::S3)
                .bucket("uploads")
                .validate()
                .is_err(),
            "a bucket with no credentials",
        );
        assert!(
            StorageConfig::new(StorageBackendKind::S3)
                .bucket("uploads")
                .credentials("AK", SecretString::new("SK"))
                .validate()
                .is_ok(),
        );

        assert!(
            StorageConfig::new(StorageBackendKind::Local)
                .url_ttl(Duration::ZERO)
                .validate()
                .is_err(),
        );
    }

    /// The message names the environment variable, because that is what the
    /// operator reading it can change.
    #[test]
    fn a_validation_failure_names_the_variable_and_the_fix() {
        let error = StorageConfig::new(StorageBackendKind::S3)
            .validate()
            .expect_err("no bucket");
        let text = error.to_string();
        assert!(text.contains("STORAGE_BUCKET"), "{text}");
        assert!(text.contains("STORAGE_BACKEND=local"), "{text}");
    }

    /// A production profile that would lose every upload on redeploy says so.
    #[test]
    fn a_production_profile_warns_about_a_local_backend() {
        let warnings = StorageConfig::new(StorageBackendKind::Local).warnings(true);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("do not survive a redeploy"))
        );
        assert!(
            StorageConfig::new(StorageBackendKind::Local)
                .warnings(false)
                .is_empty()
        );
    }

    /// Every name in `NAMES` parses, every kind has a feature, and the
    /// vendor aliases all land on S3.
    #[test]
    fn every_documented_backend_name_parses() {
        for name in StorageBackendKind::NAMES {
            assert!(
                StorageBackendKind::parse(name).is_some(),
                "`{name}` is documented and does not parse",
            );
        }
        for alias in ["r2", "minio", "b2", "wasabi", "tigris"] {
            assert_eq!(
                StorageBackendKind::parse(alias),
                Some(StorageBackendKind::S3),
                "`{alias}` is S3-compatible",
            );
        }
        assert_eq!(StorageBackendKind::parse("floppy"), None);
    }

    /// The memory backend builds with no configuration at all, which is what
    /// makes a test harness cheap.
    #[test]
    fn the_memory_backend_builds_from_nothing() {
        let storage = StorageConfig::new(StorageBackendKind::Memory)
            .build()
            .expect("builds");
        assert_eq!(storage.name(), "memory");
    }

    /// The local backend reports honest capabilities: no signing until a route
    /// exists to check one.
    #[test]
    fn the_local_backend_signs_only_when_it_is_served() {
        let plain = StorageConfig::new(StorageBackendKind::Local)
            .root(std::env::temp_dir())
            .build()
            .expect("builds");
        assert!(!plain.capabilities().signed_urls);

        let served = StorageConfig::new(StorageBackendKind::Local)
            .root(std::env::temp_dir())
            .served_at("/_storage", SecretString::new("a-development-signing-key"))
            .build()
            .expect("builds");
        assert!(served.capabilities().signed_urls);
    }

    /// A backend whose cargo feature is off names the feature rather than
    /// failing with something unactionable.
    #[test]
    fn a_backend_behind_a_disabled_feature_names_the_feature() {
        let config = StorageConfig::new(StorageBackendKind::S3)
            .bucket("uploads")
            .credentials("AK", SecretString::new("SK"));

        match config.build() {
            Ok(storage) => assert_eq!(storage.name(), "s3"),
            Err(error) => assert!(error.to_string().contains("cargo feature"), "{error}"),
        }
    }

    /// A non-critical probe degrades rather than taking the instance out of
    /// rotation, and a critical one does the opposite.
    #[tokio::test]
    async fn the_probe_degrades_unless_it_is_critical() {
        use moso_core::HealthCheck as _;

        /// A backend that is never reachable.
        #[derive(Debug)]
        struct Down;

        impl Storage for Down {
            fn name(&self) -> &'static str {
                "down"
            }
            fn capabilities(&self) -> crate::StorageCapabilities {
                crate::StorageCapabilities::minimal()
            }
            fn put<'a>(
                &'a self,
                _: &'a crate::StorageKey,
                _: crate::ByteStream,
                _: crate::PutOpts,
            ) -> moso_core::BoxFuture<'a, Result<crate::ObjectMeta>> {
                Box::pin(async { Err(crate::Error::unavailable("down", "always", None)) })
            }
            fn get<'a>(
                &'a self,
                _: &'a crate::StorageKey,
            ) -> moso_core::BoxFuture<'a, Result<crate::ByteStream>> {
                Box::pin(async { Err(crate::Error::unavailable("down", "always", None)) })
            }
            fn get_range<'a>(
                &'a self,
                _: &'a crate::StorageKey,
                _: std::ops::Range<u64>,
            ) -> moso_core::BoxFuture<'a, Result<crate::ByteStream>> {
                Box::pin(async { Err(crate::Error::unavailable("down", "always", None)) })
            }
            fn head<'a>(
                &'a self,
                _: &'a crate::StorageKey,
            ) -> moso_core::BoxFuture<'a, Result<Option<crate::ObjectMeta>>> {
                Box::pin(async { Err(crate::Error::unavailable("down", "always", None)) })
            }
            fn delete<'a>(
                &'a self,
                _: &'a crate::StorageKey,
            ) -> moso_core::BoxFuture<'a, Result<bool>> {
                Box::pin(async { Err(crate::Error::unavailable("down", "always", None)) })
            }
            fn list<'a>(
                &'a self,
                _: &'a str,
                _: Option<&'a str>,
            ) -> moso_core::BoxFuture<'a, Result<crate::Listing>> {
                Box::pin(async { Err(crate::Error::unavailable("down", "always", None)) })
            }
            fn copy<'a>(
                &'a self,
                _: &'a crate::StorageKey,
                _: &'a crate::StorageKey,
            ) -> moso_core::BoxFuture<'a, Result<crate::ObjectMeta>> {
                Box::pin(async { Err(crate::Error::unavailable("down", "always", None)) })
            }
            fn probe(&self) -> moso_core::BoxFuture<'_, Result<()>> {
                Box::pin(async { Err(crate::Error::unavailable("down", "always", None)) })
            }
        }

        let resolver = moso_core::Resolver::new(std::sync::Arc::default());
        let storage: std::sync::Arc<dyn Storage> = std::sync::Arc::new(Down);

        let lenient = StorageHealthCheck::new(storage.clone());
        assert!(!moso_core::HealthCheck::critical(&lenient));
        assert!(matches!(
            lenient.check(&resolver).await,
            moso_core::health::HealthStatus::Degraded(_),
        ));

        let strict = StorageHealthCheck::new(storage).critical(true);
        assert!(moso_core::HealthCheck::critical(&strict));
        assert!(matches!(
            strict.check(&resolver).await,
            moso_core::health::HealthStatus::Down(_),
        ));
    }
}
