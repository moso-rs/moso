//! A directory on this machine.

use std::ops::Range;
use std::path::{Path, PathBuf};

use moso_core::BoxFuture;
use moso_schema::Url;

use crate::{
    ByteStream, Listing, ObjectMeta, PutOpts, Result, Storage, StorageCapabilities, StorageKey,
};

/// A directory on this machine.
///
/// For development and for single-node deployments that genuinely do not need
/// object storage. Keys are joined onto the root **after** validation, and the
/// join is re-checked against the canonicalised root, so a key cannot escape it
/// even if [`StorageKey`]'s rules were somehow bypassed.
///
/// ```
/// use moso_storage::backend::LocalStorage;
///
/// let storage = LocalStorage::new("var/uploads");
/// assert_eq!(moso_storage::Storage::name(&storage), "local");
/// ```
#[derive(Debug)]
pub struct LocalStorage {
    /// The directory objects live under.
    root: PathBuf,
    /// The URL prefix [`Storage::signed_url`](crate::Storage::signed_url)
    /// builds against, when the development serve route is mounted.
    public_base: Option<String>,
    /// The key used to sign development URLs, so they expire like real ones.
    signing_key: Option<moso_core::config::SecretBytes>,
}

/// The suffix of the sidecar file that holds an object's metadata.
///
/// A filesystem has no notion of a content type or a user metadata pair, and
/// the alternative — extended attributes — is not portable and is lost by most
/// copy tools.
const SIDECAR: &str = ".moso-meta.json";

/// How many objects one `list` page carries.
const PAGE: usize = 1000;

impl LocalStorage {
    /// Store objects under `root`, creating it on first write.
    ///
    /// ```
    /// use moso_storage::backend::LocalStorage;
    ///
    /// let _ = LocalStorage::new("var/uploads");
    /// ```
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            public_base: None,
            signing_key: None,
        }
    }

    /// Serve objects under `base`, signing URLs with `key`.
    ///
    /// Without this the backend cannot produce a URL at all, and
    /// [`StorageCapabilities::signed_urls`] reports `false` — which is honest,
    /// and which makes the difference between development and production
    /// visible instead of surprising.
    ///
    /// ```
    /// # use moso_core::config::SecretBytes;
    /// # use moso_storage::backend::LocalStorage;
    /// let storage = LocalStorage::new("var/uploads")
    ///     .served_at("/_storage", SecretBytes::new(vec![0_u8; 32]));
    /// assert!(moso_storage::Storage::capabilities(&storage).signed_urls);
    /// ```
    #[must_use]
    pub fn served_at(
        mut self,
        base: impl Into<String>,
        key: moso_core::config::SecretBytes,
    ) -> Self {
        self.public_base = Some(base.into().trim_end_matches('/').to_owned());
        self.signing_key = Some(key);
        self
    }

    /// The directory objects live under.
    ///
    /// ```
    /// # use moso_storage::backend::LocalStorage;
    /// let _: &std::path::Path = LocalStorage::new("x").root();
    /// ```
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The routes that serve this directory in development.
    ///
    /// Every response carries the same sandbox and caching headers
    /// [`serve`](crate::serve()) sets, and the signature is checked before the
    /// file is opened.
    ///
    /// Returns an empty router when [`served_at`](LocalStorage::served_at) was
    /// never called: without a signing key there is nothing to check, and an
    /// unauthenticated route over a directory is a file server nobody asked
    /// for.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_core::config::SecretBytes;
    /// # use moso_storage::backend::LocalStorage;
    /// let storage = Arc::new(
    ///     LocalStorage::new("var/uploads").served_at("/_storage", SecretBytes::new(vec![7; 32])),
    /// );
    /// assert_eq!(LocalStorage::routes(storage).len(), 0, "one axum mount, no Moso routes");
    /// ```
    #[must_use]
    pub fn routes(storage: std::sync::Arc<Self>) -> moso_core::Router {
        let Some(base) = storage.public_base.clone() else {
            return moso_core::Router::new();
        };
        // `mount_axum` because the handler captures the backend it serves, and
        // because a development file route has no business in the OpenAPI
        // document.
        let axum_routes = axum::Router::new()
            .route("/{*key}", axum::routing::get(serve_route))
            .with_state(storage);

        // The prefix has to outlive the router; a configured base is read once
        // at boot and never changes.
        let leaked: &'static str = Box::leak(base.into_boxed_str());
        moso_core::Router::new().mount_axum(leaked, axum_routes)
    }

    /// The absolute path of a key, refusing anything outside the root.
    ///
    /// [`StorageKey`] already forbids `..`, an absolute key and a backslash.
    /// This is the second check, against the *joined* path, so that a bug in
    /// the first one is contained rather than exploited.
    fn path_of(&self, key: &StorageKey) -> Result<PathBuf> {
        let joined = self.root.join(key.as_str());

        // `..` cannot appear — the key type forbids it — but a symlink inside
        // the root can still point out of it. Compare the canonicalised parent
        // rather than the file, which may not exist yet.
        let parent = joined.parent().unwrap_or(&self.root);
        if let (Ok(root), Ok(parent)) = (self.root.canonicalize(), parent.canonicalize())
            && !parent.starts_with(&root)
        {
            return Err(crate::Error::key(
                key.as_str(),
                "the resolved path is outside the storage root, which means something in the \
                 path is a symlink pointing out of it",
            ));
        }
        Ok(joined)
    }

    /// The sidecar path for a key.
    fn sidecar_of(&self, key: &StorageKey) -> Result<PathBuf> {
        let mut path = self.path_of(key)?.into_os_string();
        path.push(SIDECAR);
        Ok(PathBuf::from(path))
    }

    /// Read a key's metadata, falling back to what the filesystem knows.
    async fn read_meta(&self, key: &StorageKey) -> Result<Option<ObjectMeta>> {
        let path = self.path_of(key)?;
        let Ok(stat) = tokio::fs::metadata(&path).await else {
            return Ok(None);
        };
        if !stat.is_file() {
            return Ok(None);
        }

        if let Ok(text) = tokio::fs::read_to_string(self.sidecar_of(key)?).await
            && let Ok(meta) = serde_json::from_str::<ObjectMeta>(&text)
        {
            return Ok(Some(meta));
        }

        // No sidecar: the file was put there by something other than this
        // backend. Report what can be known rather than refusing it.
        Ok(Some(ObjectMeta {
            key: key.clone(),
            size: stat.len(),
            content_type: "application/octet-stream".to_owned(),
            etag: None,
            modified_at: stat
                .modified()
                .ok()
                .map(chrono::DateTime::<chrono::Utc>::from),
            checksum: None,
            metadata: std::collections::BTreeMap::new(),
            cache_control: None,
            content_disposition: None,
            public: false,
        }))
    }

    /// Sign `key` for `expiry`, as the development serve route checks it.
    fn sign(&self, key: &StorageKey, expiry: i64) -> Option<String> {
        let signing = self.signing_key.as_ref()?;
        let tag = ring::hmac::sign(
            &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, signing.expose()),
            format!("{}\n{expiry}", key.as_str()).as_bytes(),
        );
        Some(crate::backend::hex(tag.as_ref()))
    }
}

impl Storage for LocalStorage {
    fn name(&self) -> &'static str {
        "local"
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            ranges: true,
            metadata: true,
            delimited_listing: true,
            server_side_copy: true,
            // A rename is atomic on every filesystem this runs on, but the
            // "does it exist" check before it is not, so this stays false.
            conditional_writes: false,
            // Honest: signing only works when a route exists to check it.
            signed_urls: self.signing_key.is_some(),
            presigned_upload: false,
            multipart: false,
            min_part_size: 0,
            public_objects: false,
            max_object_size: u64::MAX,
        }
    }

    fn put<'a>(
        &'a self,
        key: &'a StorageKey,
        mut body: ByteStream,
        mut opts: PutOpts,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(async move {
            use futures_util::StreamExt as _;
            use tokio::io::AsyncWriteExt as _;

            let path = self.path_of(key)?;
            if opts.refuses_overwrite() && tokio::fs::try_exists(&path).await.unwrap_or(false) {
                return Err(crate::Error::refused(
                    "local",
                    format!("`{key}` already exists and `if_absent` was set"),
                ));
            }

            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|error| {
                    io_error(
                        "local",
                        &format!("could not create `{}`", parent.display()),
                        error,
                    )
                })?;
            }

            // A temporary file and a rename, so a failed write leaves nothing
            // and a concurrent reader never sees a half-written object.
            let temporary = path.with_extension(format!("moso-partial-{}", std::process::id(),));
            let mut file = tokio::fs::File::create(&temporary).await.map_err(|error| {
                io_error(
                    "local",
                    &format!("could not create `{}`", temporary.display()),
                    error,
                )
            })?;

            let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
            let mut size = 0_u64;
            let mut prefix = bytes::BytesMut::new();

            let outcome = async {
                while let Some(chunk) = body.next().await {
                    let chunk = chunk?;
                    if prefix.len() < crate::upload::SNIFF_BYTES {
                        let wanted = crate::upload::SNIFF_BYTES - prefix.len();
                        prefix.extend_from_slice(&chunk[..wanted.min(chunk.len())]);
                    }
                    hasher.update(&chunk);
                    size += chunk.len() as u64;
                    file.write_all(&chunk)
                        .await
                        .map_err(|error| io_error("local", "the write failed", error))?;
                }
                file.flush()
                    .await
                    .map_err(|error| io_error("local", "the flush failed", error))?;
                Ok::<(), crate::Error>(())
            }
            .await;

            // Whatever happened, the partial file must not survive.
            if let Err(error) = outcome {
                drop(file);
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(error);
            }
            drop(file);

            let digest = crate::backend::hex(hasher.finish().as_ref());
            if let Some(expected) = opts.expected_checksum()
                && expected.digest() != digest
            {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(crate::Error::Checksum {
                    key: key.to_string(),
                    expected: expected.digest().to_owned(),
                    actual: digest,
                });
            }

            if opts.sniffs()
                && let Some(sniffed) = crate::sniff(&prefix)
            {
                opts.set_content_type(sniffed);
            }

            tokio::fs::rename(&temporary, &path)
                .await
                .map_err(|error| {
                    io_error(
                        "local",
                        &format!("could not publish `{}`", path.display()),
                        error,
                    )
                })?;

            let meta = crate::object::meta_from(
                key,
                size,
                &opts,
                Some(crate::Checksum::sha256(digest.clone())),
                Some(format!("\"{digest}\"")),
            );

            // A best-effort sidecar: an object whose metadata could not be
            // written is still an object, and `read_meta` degrades to the
            // filesystem's own view.
            if let Ok(json) = serde_json::to_vec(&meta) {
                let _ = tokio::fs::write(self.sidecar_of(key)?, json).await;
            }
            Ok(meta)
        })
    }

    fn get<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            let path = self.path_of(key)?;
            let file = tokio::fs::File::open(&path)
                .await
                .map_err(|_| crate::Error::not_found(key.as_str()))?;
            Ok(read_stream(file, None))
        })
    }

    fn get_range<'a>(
        &'a self,
        key: &'a StorageKey,
        range: Range<u64>,
    ) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            use tokio::io::AsyncSeekExt as _;

            let path = self.path_of(key)?;
            let mut file = tokio::fs::File::open(&path)
                .await
                .map_err(|_| crate::Error::not_found(key.as_str()))?;
            file.seek(std::io::SeekFrom::Start(range.start))
                .await
                .map_err(|error| io_error("local", "the seek failed", error))?;
            Ok(read_stream(
                file,
                Some(range.end.saturating_sub(range.start)),
            ))
        })
    }

    fn head<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<Option<ObjectMeta>>> {
        Box::pin(async move { self.read_meta(key).await })
    }

    fn delete<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let path = self.path_of(key)?;
            let removed = tokio::fs::remove_file(&path).await.is_ok();
            let _ = tokio::fs::remove_file(self.sidecar_of(key)?).await;
            Ok(removed)
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        cursor: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            // Collected and sorted rather than streamed: a directory walk has
            // no order, and a listing whose order changes between pages cannot
            // be paged at all.
            let mut keys = Vec::new();
            walk(&self.root, &self.root, &mut keys).await?;
            keys.sort();

            let mut objects = Vec::new();
            let mut next = None;
            for key in keys {
                if !key.starts_with(prefix) {
                    continue;
                }
                if let Some(cursor) = cursor
                    && key.as_str() <= cursor
                {
                    continue;
                }
                if objects.len() == PAGE {
                    next = Some(key);
                    break;
                }
                let Ok(parsed) = StorageKey::new(key) else {
                    continue;
                };
                if let Some(meta) = self.read_meta(&parsed).await? {
                    objects.push(meta);
                }
            }

            Ok(Listing {
                objects,
                prefixes: Vec::new(),
                cursor: next,
            })
        })
    }

    fn copy<'a>(
        &'a self,
        from: &'a StorageKey,
        to: &'a StorageKey,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(async move {
            let source = self.path_of(from)?;
            let target = self.path_of(to)?;
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|error| {
                    io_error("local", "could not create the target directory", error)
                })?;
            }
            tokio::fs::copy(&source, &target)
                .await
                .map_err(|_| crate::Error::not_found(from.as_str()))?;

            let mut meta = self
                .read_meta(from)
                .await?
                .ok_or_else(|| crate::Error::not_found(from.as_str()))?;
            meta.key = to.clone();
            meta.modified_at = Some(chrono::Utc::now());
            if let Ok(json) = serde_json::to_vec(&meta) {
                let _ = tokio::fs::write(self.sidecar_of(to)?, json).await;
            }
            Ok(meta)
        })
    }

    fn signed_url<'a>(
        &'a self,
        key: &'a StorageKey,
        ttl: std::time::Duration,
    ) -> BoxFuture<'a, Result<Url>> {
        Box::pin(async move {
            let (Some(base), Some(_)) = (self.public_base.as_deref(), self.signing_key.as_ref())
            else {
                return Err(crate::Error::unsupported("local", "signed_url"));
            };

            let expiry = chrono::Utc::now().timestamp() + ttl.as_secs() as i64;
            let signature = self
                .sign(key, expiry)
                .ok_or_else(|| crate::Error::unsupported("local", "signed_url"))?;

            // A relative URL, because this backend does not know the origin it
            // is served from and guessing one would produce links that work in
            // development and break behind a proxy.
            Url::parse_http(&format!(
                "http://localhost{base}/{}?expires={expiry}&signature={signature}",
                key.as_str(),
            ))
            .map_err(|error| crate::Error::config(error.message().to_owned()))
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            tokio::fs::create_dir_all(&self.root)
                .await
                .map_err(|error| {
                    io_error(
                        "local",
                        &format!("the storage root `{}` is not usable", self.root.display()),
                        error,
                    )
                })
        })
    }
}

/// A file as a `ByteStream`, optionally truncated to `limit` bytes.
///
/// The remaining count lives in the unfold's *state* rather than in a captured
/// variable, because a `move` closure copies a `Copy` capture and the
/// decrement would be lost on every call — which is a range read that quietly
/// returns the whole file.
fn read_stream(file: tokio::fs::File, limit: Option<u64>) -> ByteStream {
    use tokio::io::AsyncReadExt as _;

    /// 64 KiB per chunk: large enough that the syscall overhead disappears and
    /// small enough that a thousand concurrent downloads do not need a
    /// gigabyte of buffers. This is also the constant the peak-RSS acceptance
    /// criterion depends on.
    const CHUNK: usize = 64 * 1024;

    Box::pin(futures_util::stream::unfold(
        (file, limit, false),
        |(mut file, remaining, done)| async move {
            if done {
                return None;
            }
            let wanted = remaining.map_or(CHUNK, |left| CHUNK.min(left as usize));
            if wanted == 0 {
                return None;
            }

            let mut buffer = vec![0_u8; wanted];
            match file.read(&mut buffer).await {
                Ok(0) => None,
                Ok(read) => {
                    buffer.truncate(read);
                    let left = remaining.map(|left| left.saturating_sub(read as u64));
                    Some((Ok(bytes::Bytes::from(buffer)), (file, left, false)))
                }
                Err(error) => Some((
                    Err(io_error("local", "the read failed", error)),
                    (file, remaining, true),
                )),
            }
        },
    ))
}

/// Walk a directory, collecting keys relative to `root`.
fn walk<'a>(
    root: &'a Path,
    directory: &'a Path,
    into: &'a mut Vec<String>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
            // An absent root is an empty store, not a failure: the directory is
            // created on the first write.
            return Ok(());
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, into).await?;
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let Some(key) = relative.to_str() else {
                continue;
            };
            if key.ends_with(SIDECAR) || key.contains(".moso-partial-") {
                continue;
            }
            into.push(key.replace(std::path::MAIN_SEPARATOR, "/"));
        }
        Ok(())
    })
}

/// Wrap an I/O failure, keeping the source.
fn io_error(backend: &'static str, what: &str, error: std::io::Error) -> crate::Error {
    crate::Error::unavailable(backend, format!("{what}: {error}"), Some(Box::new(error)))
}

/// The development serve route.
///
/// Checks the signature and the expiry *before* the file is opened, so an
/// unsigned request never touches the disk.
async fn serve_route(
    axum::extract::State(storage): axum::extract::State<std::sync::Arc<LocalStorage>>,
    axum::extract::Path(key): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<SignedQuery>,
    headers: http::HeaderMap,
) -> axum::response::Response {
    use moso_core::IntoResponse as _;

    let refuse = |status: http::StatusCode, detail: &'static str| {
        let mut response = axum::response::Response::new(axum::body::Body::from(detail));
        *response.status_mut() = status;
        response
    };

    let Ok(key) = StorageKey::new(key) else {
        return refuse(http::StatusCode::BAD_REQUEST, "not a storage key");
    };

    if query.expires < chrono::Utc::now().timestamp() {
        return refuse(http::StatusCode::FORBIDDEN, "the link has expired");
    }
    let Some(expected) = storage.sign(&key, query.expires) else {
        return refuse(http::StatusCode::NOT_FOUND, "this backend is not served");
    };
    // Constant-time through a digest compare: a signature checked with `==`
    // leaks its bytes through timing.
    if !crate::backend::digest_eq(expected.as_bytes(), query.signature.as_bytes()) {
        return refuse(http::StatusCode::FORBIDDEN, "the signature did not verify");
    }

    match crate::serve(storage.as_ref(), &key).await {
        Ok(object) => object.evaluate(&headers).into_response(),
        Err(error) => moso_core::Error::from(error).into_response(),
    }
}

/// The query a signed development URL carries.
#[derive(serde::Deserialize)]
struct SignedQuery {
    /// When the link stops working, as a Unix timestamp.
    expires: i64,
    /// The HMAC over the key and the expiry.
    signature: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory nothing else is using.
    ///
    /// The counter is not decoration: the clock behind
    /// `timestamp_nanos_opt` has microsecond resolution on macOS, tests in this
    /// binary run in parallel threads, and two that started inside the same tick
    /// would share a root — so one's `Scratch` would delete the other's files
    /// mid-test. The counter makes the name unique whatever the clock says.
    fn temporary() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        std::env::temp_dir().join(format!(
            "moso-storage-{}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ))
    }

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn storage() -> (Scratch, LocalStorage) {
        let root = temporary();
        (Scratch(root.clone()), LocalStorage::new(root))
    }

    async fn put(storage: &LocalStorage, key: &str, bytes: &'static [u8]) -> ObjectMeta {
        storage
            .put(
                &StorageKey::new(key).expect("valid"),
                crate::stream_from_bytes(bytes::Bytes::from_static(bytes)),
                PutOpts::new("application/octet-stream"),
            )
            .await
            .expect("stores")
    }

    /// The round trip, including the metadata a filesystem cannot hold on its
    /// own.
    #[tokio::test]
    async fn an_object_round_trips_with_its_metadata() {
        let (_scratch, storage) = storage();
        let key = StorageKey::new("a/b/c.txt").expect("valid");
        storage
            .put(
                &key,
                crate::stream_from_bytes(bytes::Bytes::from_static(b"hello")),
                PutOpts::new("text/plain")
                    .trust_content_type()
                    .metadata("uploaded-by", "usr_1")
                    .cache_control("no-store"),
            )
            .await
            .expect("stores");

        let meta = storage.head(&key).await.expect("heads").expect("present");
        assert_eq!(meta.size, 5);
        assert_eq!(meta.content_type, "text/plain");
        assert_eq!(meta.cache_control.as_deref(), Some("no-store"));
        assert_eq!(
            meta.metadata.get("uploaded-by").map(String::as_str),
            Some("usr_1")
        );

        let bytes = crate::collect_bounded(storage.get(&key).await.expect("reads"), 1024, "t")
            .await
            .expect("collects");
        assert_eq!(bytes, "hello");
    }

    /// A failed write must leave nothing: a half-written file that a later read
    /// succeeds on is worse than no file.
    #[tokio::test]
    async fn a_failed_checksum_leaves_no_partial_file() {
        let (_scratch, storage) = storage();
        let key = StorageKey::new("a/b").expect("valid");

        assert!(
            storage
                .put(
                    &key,
                    crate::stream_from_bytes(bytes::Bytes::from_static(b"hello")),
                    PutOpts::new("text/plain").expect_checksum(crate::Checksum::sha256("wrong")),
                )
                .await
                .is_err(),
        );
        assert!(storage.head(&key).await.expect("heads").is_none());

        // And no stray temporary file either.
        let listing = storage.list("", None).await.expect("lists");
        assert!(listing.objects.is_empty());
    }

    /// The sidecar is not part of the store, or every object would appear
    /// twice in a listing.
    #[tokio::test]
    async fn the_sidecar_is_invisible_to_listing() {
        let (_scratch, storage) = storage();
        put(&storage, "a/1.txt", b"x").await;
        put(&storage, "a/2.txt", b"y").await;

        let listing = storage.list("a/", None).await.expect("lists");
        let keys: Vec<_> = listing
            .objects
            .iter()
            .map(|meta| meta.key.as_str().to_owned())
            .collect();
        assert_eq!(keys, vec!["a/1.txt".to_owned(), "a/2.txt".to_owned()]);
    }

    /// A ranged read is what makes a resumed download work.
    #[tokio::test]
    async fn a_range_read_returns_exactly_the_requested_bytes() {
        let (_scratch, storage) = storage();
        put(&storage, "a/b", b"0123456789").await;

        let key = StorageKey::new("a/b").expect("valid");
        let slice =
            crate::collect_bounded(storage.get_range(&key, 3..7).await.expect("reads"), 64, "t")
                .await
                .expect("collects");
        assert_eq!(slice, "3456");
    }

    /// Deleting takes the sidecar with it, or the next `head` reports an object
    /// that is not there.
    #[tokio::test]
    async fn deleting_removes_the_sidecar_too() {
        let (_scratch, storage) = storage();
        put(&storage, "a/b", b"x").await;
        let key = StorageKey::new("a/b").expect("valid");

        assert!(storage.delete(&key).await.expect("deletes"));
        assert!(storage.head(&key).await.expect("heads").is_none());
        assert!(!storage.delete(&key).await.expect("already gone"));
    }

    /// A backend that cannot sign says so, rather than producing a URL that
    /// does not work.
    #[tokio::test]
    async fn signing_is_unsupported_until_a_route_exists_to_check_it() {
        let (_scratch, storage) = storage();
        assert!(!storage.capabilities().signed_urls);

        let key = StorageKey::new("a/b").expect("valid");
        assert!(matches!(
            storage
                .signed_url(&key, std::time::Duration::from_secs(60))
                .await
                .expect_err("unsupported"),
            crate::Error::Unsupported { .. },
        ));
    }

    /// With a key and a route, a development URL expires like a real one and
    /// its signature covers the key.
    #[tokio::test]
    async fn a_development_url_is_signed_over_the_key_and_the_expiry() {
        let root = temporary();
        let _scratch = Scratch(root.clone());
        let storage = LocalStorage::new(root).served_at(
            "/_storage",
            moso_core::config::SecretBytes::new(vec![7_u8; 32]),
        );

        assert!(storage.capabilities().signed_urls);

        let a = StorageKey::new("a/b").expect("valid");
        let b = StorageKey::new("a/c").expect("valid");
        assert_ne!(storage.sign(&a, 100), storage.sign(&b, 100));
        assert_ne!(storage.sign(&a, 100), storage.sign(&a, 200));

        let url = storage
            .signed_url(&a, std::time::Duration::from_secs(300))
            .await
            .expect("signs");
        assert!(url.as_str().contains("/_storage/a/b?expires="));
        assert!(url.as_str().contains("&signature="));
    }

    /// Copying is server-side here — one `fs::copy` — and both objects survive.
    #[tokio::test]
    async fn copying_leaves_both_objects_with_metadata() {
        let (_scratch, storage) = storage();
        put(&storage, "a/1", b"payload").await;

        let to = StorageKey::new("b/1").expect("valid");
        let meta = storage
            .copy(&StorageKey::new("a/1").expect("valid"), &to)
            .await
            .expect("copies");
        assert_eq!(meta.key, to);
        assert_eq!(
            storage
                .head(&to)
                .await
                .expect("heads")
                .expect("present")
                .size,
            7
        );
        assert!(
            storage
                .head(&StorageKey::new("a/1").expect("valid"))
                .await
                .expect("heads")
                .is_some()
        );
    }

    /// The whole reason the local backend is not just `fs::write`: a key must
    /// not be able to reach outside the root.
    #[tokio::test]
    async fn a_key_cannot_escape_the_root() {
        let (_scratch, storage) = storage();
        // `StorageKey` refuses the traversal before the backend ever sees it,
        // which is the first of the two checks.
        assert!(StorageKey::new("../escaped").is_err());
        assert!(StorageKey::new("a/../../escaped").is_err());

        // And a legal key resolves inside the root, which is the second.
        let key = StorageKey::new("a/b/c").expect("valid");
        let path = storage.path_of(&key).expect("resolves");
        assert!(path.starts_with(storage.root()));
    }

    /// An empty store lists as empty rather than failing, because the root is
    /// created lazily on the first write.
    #[tokio::test]
    async fn an_absent_root_lists_as_empty() {
        let (_scratch, storage) = storage();
        assert!(
            storage
                .list("", None)
                .await
                .expect("lists")
                .objects
                .is_empty()
        );
    }

    /// The readiness probe creates the root, which is what makes a first
    /// deployment work without a manual `mkdir`.
    #[tokio::test]
    async fn the_probe_creates_the_root() {
        let (_scratch, storage) = storage();
        storage.probe().await.expect("probes");
        assert!(storage.root().is_dir());
    }
}
