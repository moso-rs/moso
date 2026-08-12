//! The [`Storage`] trait, and what a backend says it can do.
//!
//! Dyn-compatible (decision D4): an application injects `Inject<dyn Storage>`
//! and never names the backend, so the same handler writes to a directory in
//! `moso dev`, to a map in a test and to S3 in production.

use std::ops::Range;
use std::time::Duration;

use moso_core::BoxFuture;
use moso_schema::Url;

use crate::{
    ByteStream, Listing, MultipartUpload, ObjectMeta, PresignedPost, PutOpts, Result, ServedObject,
    StorageKey, UploadPolicy,
};

/// What a backend can and cannot do.
///
/// Sixteen of [`Storage`]'s methods have defaults that fail with
/// [`Error::Unsupported`](crate::Error::Unsupported). Declaring capabilities is
/// how the memory backend can be an honest test double — same semantics for
/// everything it claims — while saying plainly that it cannot presign.
///
/// ```
/// use moso_storage::StorageCapabilities;
///
/// let caps = StorageCapabilities::minimal();
/// assert!(!caps.presigned_upload);
/// assert!(caps.ranges);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StorageCapabilities {
    /// Whether [`Storage::get_range`] is served without reading the whole object.
    pub ranges: bool,
    /// Whether [`Storage::signed_url`] produces a time-limited URL.
    pub signed_urls: bool,
    /// Whether [`Storage::presigned_upload`] lets a client upload directly.
    pub presigned_upload: bool,
    /// Whether [`Storage::multipart_start`] works.
    pub multipart: bool,
    /// The smallest non-final multipart part, in bytes. Zero when `multipart`
    /// is false.
    pub min_part_size: u64,
    /// Whether [`Storage::copy`] happens server-side rather than by streaming
    /// through this process.
    pub server_side_copy: bool,
    /// Whether [`PutOpts::if_absent`] is atomic rather than a `head`-then-`put`.
    pub conditional_writes: bool,
    /// Whether objects can be made publicly readable.
    pub public_objects: bool,
    /// Whether the backend stores and returns arbitrary metadata pairs.
    pub metadata: bool,
    /// The largest single object, in bytes.
    pub max_object_size: u64,
    /// Whether a listing can be delimited into common prefixes.
    pub delimited_listing: bool,
}

impl StorageCapabilities {
    /// The conservative set: read, write, delete, list, ranges. Nothing else.
    ///
    /// A new backend starts here and turns things on as it implements them, so
    /// an unimplemented feature is an honest `false` rather than a surprise at
    /// the first large upload.
    ///
    /// ```
    /// use moso_storage::StorageCapabilities;
    ///
    /// assert_eq!(StorageCapabilities::minimal().min_part_size, 0);
    /// ```
    #[must_use]
    pub const fn minimal() -> Self {
        Self {
            ranges: true,
            signed_urls: false,
            presigned_upload: false,
            multipart: false,
            min_part_size: 0,
            server_side_copy: false,
            conditional_writes: false,
            public_objects: false,
            metadata: false,
            max_object_size: u64::MAX,
            delimited_listing: false,
        }
    }
}

impl Default for StorageCapabilities {
    fn default() -> Self {
        Self::minimal()
    }
}

/// Object storage. The one trait an application depends on.
///
/// ```no_run
/// use moso_storage::{PutOpts, ServedObject, Storage, StorageKey};
///
/// async fn save(storage: &dyn Storage, key: &StorageKey, body: moso_storage::ByteStream)
///     -> moso_storage::Result<()>
/// {
///     storage.put(key, body, PutOpts::new("image/png")).await?;
///     Ok(())
/// }
///
/// async fn download(storage: &dyn Storage, key: &StorageKey)
///     -> moso_storage::Result<ServedObject>
/// {
///     storage.serve(key).await
/// }
/// ```
///
/// # The rule about large files
///
/// Bytes travelling through the application are bytes the application pays for
/// twice, in latency and in memory. For anything above a few megabytes the
/// documented path is [`presigned_upload`](Storage::presigned_upload): the
/// client uploads straight to the backend and calls back, and this process
/// validates and records the object without ever seeing it.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not object storage",
    label = "not a storage backend",
    note = "a storage backend is `Send + Sync + 'static` and implements `name`, `capabilities`, \
            `put`, `get`, `head`, `delete`, `list` and `copy`",
    note = "help: use a shipped backend — `LocalStorage` in development, `MemoryStorage` in \
            tests, `S3Storage` for S3, R2, MinIO, Backblaze, Wasabi or Tigris",
    note = "help: to write your own, `impl Storage for {Self}` and start from \
            `StorageCapabilities::minimal()`; the optional methods already fail honestly"
)]
pub trait Storage: Send + Sync + 'static {
    /// The backend's name, for logs, metrics and error messages.
    ///
    /// A short lowercase word: `"s3"`, `"local"`, `"memory"`.
    fn name(&self) -> &'static str;

    /// What this backend supports.
    fn capabilities(&self) -> StorageCapabilities;

    /// Write an object, streaming.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`](crate::Error::TooLarge) at the first byte past the
    /// backend's limit, [`Error::Checksum`](crate::Error::Checksum) when
    /// [`PutOpts::expect_checksum`] was set and did not match, and
    /// [`Error::Unavailable`](crate::Error::Unavailable) for anything
    /// transient. A failed write leaves no partial object.
    fn put<'a>(
        &'a self,
        key: &'a StorageKey,
        body: ByteStream,
        opts: PutOpts,
    ) -> BoxFuture<'a, Result<ObjectMeta>>;

    /// Read an object, streaming.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`](crate::Error::NotFound) when there is nothing at
    /// `key`.
    fn get<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<ByteStream>>;

    /// Read part of an object.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`](crate::Error::NotFound), or
    /// [`Error::Unsupported`](crate::Error::Unsupported) when
    /// [`StorageCapabilities::ranges`] is false.
    fn get_range<'a>(
        &'a self,
        key: &'a StorageKey,
        range: Range<u64>,
    ) -> BoxFuture<'a, Result<ByteStream>>;

    /// Read an object's metadata without its bytes.
    ///
    /// `Ok(None)` for an absent object — this is the one method where absence
    /// is not an error, because "does it exist" is the question being asked.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) and friends.
    fn head<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<Option<ObjectMeta>>>;

    /// Delete an object. `Ok(false)` when there was nothing to delete.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`](crate::Error::Refused) without permission.
    fn delete<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<bool>>;

    /// List objects under a prefix, one page at a time.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) and friends.
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        cursor: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>>;

    /// Copy an object, server-side when
    /// [`StorageCapabilities::server_side_copy`] is true and by streaming
    /// through this process when it is not.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`](crate::Error::NotFound) when `from` is absent.
    fn copy<'a>(
        &'a self,
        from: &'a StorageKey,
        to: &'a StorageKey,
    ) -> BoxFuture<'a, Result<ObjectMeta>>;

    /// Delete many objects, using the backend's bulk API when there is one.
    ///
    /// Returns how many were deleted. Partial failure is reported as an error
    /// naming the first key that failed; keys before it were deleted.
    ///
    /// # Errors
    ///
    /// As [`delete`](Storage::delete).
    fn delete_many<'a>(&'a self, keys: &'a [StorageKey]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let mut deleted = 0_u64;
            for key in keys {
                // The count is what was actually there, so a caller can tell
                // "deleted three" from "there were three".
                if self.delete(key).await? {
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
    }

    /// Stream the object back over HTTP, with `Range` and `ETag` handled.
    ///
    /// The method form of [`serve`](crate::serve()), which delegates to it, so
    /// a handler holding an `Inject<dyn Storage>` writes `storage.serve(&key)`
    /// and a free function that already has both writes `serve(storage, &key)`.
    /// The default body is the whole implementation: a `head` for the metadata
    /// — which decides the status, and which a 304 must answer without opening
    /// a body — then a `get`.
    ///
    /// Override it only on a backend that produces metadata and bytes in one
    /// operation; the default costs two.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`](crate::Error::NotFound) when there is nothing at
    /// `key`, and whatever the backend reports for the read.
    fn serve<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<ServedObject>> {
        Box::pin(async move {
            let meta = self
                .head(key)
                .await?
                .ok_or_else(|| crate::Error::not_found(key.as_str()))?;
            let body = self.get(key).await?;
            Ok(crate::serve::from_parts(key.clone(), meta, body))
        })
    }

    /// A time-limited URL a browser can follow to download the object.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) when
    /// [`StorageCapabilities::signed_urls`] is false.
    fn signed_url<'a>(&'a self, key: &'a StorageKey, ttl: Duration) -> BoxFuture<'a, Result<Url>> {
        let _ = (key, ttl);
        Box::pin(async move { Err(crate::Error::unsupported(self.name(), "signed_url")) })
    }

    /// A policy a browser can POST to, uploading straight to the backend.
    ///
    /// The bytes never traverse the application. The returned
    /// [`PresignedPost`] carries the exact fields the form must submit, and the
    /// policy binds the key, the size range and the content type — so a client
    /// cannot upload a 4 GiB executable to a key it chose.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) when
    /// [`StorageCapabilities::presigned_upload`] is false.
    fn presigned_upload<'a>(
        &'a self,
        key: &'a StorageKey,
        policy: UploadPolicy,
    ) -> BoxFuture<'a, Result<PresignedPost>> {
        let _ = (key, policy);
        Box::pin(async move { Err(crate::Error::unsupported(self.name(), "presigned_upload")) })
    }

    /// Begin a multipart upload.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) when
    /// [`StorageCapabilities::multipart`] is false.
    fn multipart_start<'a>(
        &'a self,
        key: &'a StorageKey,
        opts: PutOpts,
    ) -> BoxFuture<'a, Result<MultipartUpload>> {
        let _ = (key, opts);
        Box::pin(async move { Err(crate::Error::unsupported(self.name(), "multipart_start")) })
    }

    /// A readiness probe: can this backend reach its store right now?
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) when it cannot.
    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        // A backend with no remote is always reachable. Every shipped backend
        // that has one overrides this; leaving the default in place would be
        // the dishonest choice.
        Box::pin(async { Ok(()) })
    }
}

/// A shared backend is a backend.
///
/// Two things need the *same* store at once and cannot both own it: the
/// development file route takes an `Arc<LocalStorage>`, and
/// [`TimedStorage`](crate::TimedStorage) wraps a value. Without this impl an
/// application would have to build the backend twice to have both, so it is
/// here rather than left as a papercut.
///
/// `?Sized` on purpose: it makes `Arc<dyn Storage>` a `Storage` too, which is
/// what lets a layer wrap a backend it was handed through the provider map.
///
/// Every method is forwarded explicitly, including the ones with defaults. A
/// forwarding impl that let the defaults stand would silently bypass an
/// override on the backend underneath, which is the one bug this shape has.
///
/// ```
/// use std::sync::Arc;
/// use moso_storage::{Storage, backend::MemoryStorage};
///
/// let shared = Arc::new(MemoryStorage::new());
/// assert_eq!(Storage::name(&shared), "memory");
/// ```
#[diagnostic::do_not_recommend]
impl<S: Storage + ?Sized> Storage for std::sync::Arc<S> {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn capabilities(&self) -> StorageCapabilities {
        (**self).capabilities()
    }

    fn put<'a>(
        &'a self,
        key: &'a StorageKey,
        body: ByteStream,
        opts: PutOpts,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        (**self).put(key, body, opts)
    }

    fn get<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<ByteStream>> {
        (**self).get(key)
    }

    fn get_range<'a>(
        &'a self,
        key: &'a StorageKey,
        range: Range<u64>,
    ) -> BoxFuture<'a, Result<ByteStream>> {
        (**self).get_range(key, range)
    }

    fn head<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<Option<ObjectMeta>>> {
        (**self).head(key)
    }

    fn delete<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<bool>> {
        (**self).delete(key)
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        cursor: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        (**self).list(prefix, cursor)
    }

    fn copy<'a>(
        &'a self,
        from: &'a StorageKey,
        to: &'a StorageKey,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        (**self).copy(from, to)
    }

    fn delete_many<'a>(&'a self, keys: &'a [StorageKey]) -> BoxFuture<'a, Result<u64>> {
        (**self).delete_many(keys)
    }

    fn serve<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<ServedObject>> {
        (**self).serve(key)
    }

    fn signed_url<'a>(&'a self, key: &'a StorageKey, ttl: Duration) -> BoxFuture<'a, Result<Url>> {
        (**self).signed_url(key, ttl)
    }

    fn presigned_upload<'a>(
        &'a self,
        key: &'a StorageKey,
        policy: UploadPolicy,
    ) -> BoxFuture<'a, Result<PresignedPost>> {
        (**self).presigned_upload(key, policy)
    }

    fn multipart_start<'a>(
        &'a self,
        key: &'a StorageKey,
        opts: PutOpts,
    ) -> BoxFuture<'a, Result<MultipartUpload>> {
        (**self).multipart_start(key, opts)
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        (**self).probe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store with one object in it.
    async fn stored() -> (crate::backend::MemoryStorage, StorageKey) {
        let storage = crate::backend::MemoryStorage::new();
        let key = StorageKey::new("reports/annual.pdf").expect("valid");
        storage
            .put(
                &key,
                crate::stream_from_bytes(bytes::Bytes::from_static(b"%PDF-1.7 payload")),
                PutOpts::new("application/pdf").trust_content_type(),
            )
            .await
            .expect("stores");
        (storage, key)
    }

    /// Two spellings of one operation, and the free function delegates — so a
    /// backend that overrides the method changes both, and they cannot drift.
    #[tokio::test]
    async fn the_method_and_the_free_function_are_the_same_operation() {
        let (storage, key) = stored().await;

        let method = storage.serve(&key).await.expect("serves");
        let free = crate::serve(&storage, &key).await.expect("serves");

        assert_eq!(method.meta().size, free.meta().size);
        assert_eq!(method.meta().content_type, free.meta().content_type);
        assert_eq!(method.status(), free.status());
        assert_eq!(method.status(), http::StatusCode::OK);
    }

    /// The method is dispatchable on `dyn Storage`, which is the whole point:
    /// a handler holds an `Inject<dyn Storage>` and never names a backend.
    #[tokio::test]
    async fn the_method_works_through_the_trait_object() {
        let (storage, key) = stored().await;
        let erased: &dyn Storage = &storage;

        assert_eq!(erased.serve(&key).await.expect("serves").meta().size, 16);
    }

    /// The development file route needs `Arc<LocalStorage>` and `TimedStorage`
    /// needs a value; without a shared backend being a backend an application
    /// would have to build the store twice to have both.
    #[tokio::test]
    async fn a_shared_backend_is_a_backend() {
        let (storage, key) = stored().await;
        let shared = std::sync::Arc::new(storage);

        assert_eq!(Storage::name(&shared), "memory");
        assert!(!Storage::capabilities(&shared).signed_urls);
        assert_eq!(
            Storage::head(&shared, &key)
                .await
                .expect("heads")
                .expect("present")
                .size,
            16,
        );

        // And it composes with the wrapper, which is the point.
        let timed = crate::TimedStorage::new(
            std::sync::Arc::clone(&shared),
            crate::Deadlines::uniform(Duration::from_secs(5)),
        );
        assert_eq!(timed.name(), "memory");
        assert_eq!(timed.serve(&key).await.expect("serves").meta().size, 16);
    }

    /// Nothing there is a 404 and not an empty 200, whichever spelling asked.
    #[tokio::test]
    async fn serving_something_that_is_not_there_is_not_found() {
        let storage = crate::backend::MemoryStorage::new();
        let key = StorageKey::new("reports/missing.pdf").expect("valid");

        assert!(
            storage
                .serve(&key)
                .await
                .expect_err("nothing there")
                .is_not_found(),
        );
        assert!(
            crate::serve(&storage, &key)
                .await
                .expect_err("nothing there")
                .is_not_found(),
        );
    }
}
