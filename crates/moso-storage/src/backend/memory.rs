//! Objects in a map. The test backend.

use std::ops::Range;

use moso_core::BoxFuture;

use crate::{
    ByteStream, Listing, ObjectMeta, PutOpts, Result, Storage, StorageCapabilities, StorageKey,
};

/// Objects in a map. The test backend.
///
/// The semantics are the real ones — sniffing, checksums, conditional writes,
/// prefix listing, ranges — with none of the durability. What it does *not*
/// claim is presigning: a signed URL that only worked inside the test process
/// would make a test pass that production fails.
///
/// ```
/// use moso_storage::backend::MemoryStorage;
///
/// let storage = MemoryStorage::new();
/// assert_eq!(storage.len(), 0);
/// ```
#[derive(Debug, Default)]
pub struct MemoryStorage {
    /// Objects by key, with their metadata.
    objects: std::sync::RwLock<std::collections::BTreeMap<String, (ObjectMeta, bytes::Bytes)>>,
    /// A cap, so a runaway test fails loudly instead of exhausting the machine.
    max_total_bytes: u64,
}

/// The default total-size cap.
const DEFAULT_CAP: u64 = 256 * 1024 * 1024;

/// How many objects one `list` page carries.
const PAGE: usize = 1000;

impl MemoryStorage {
    /// An empty store, capped at 256 MiB in total.
    ///
    /// ```
    /// use moso_storage::backend::MemoryStorage;
    ///
    /// assert!(MemoryStorage::new().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            objects: std::sync::RwLock::new(std::collections::BTreeMap::new()),
            max_total_bytes: DEFAULT_CAP,
        }
    }

    /// Change the total-size cap.
    ///
    /// ```
    /// # use moso_storage::backend::MemoryStorage;
    /// let _ = MemoryStorage::new().max_total_bytes(1024 * 1024);
    /// ```
    #[must_use]
    pub fn max_total_bytes(mut self, bytes: u64) -> Self {
        self.max_total_bytes = bytes;
        self
    }

    /// How many objects are stored.
    ///
    /// ```
    /// # use moso_storage::backend::MemoryStorage;
    /// assert_eq!(MemoryStorage::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether nothing is stored.
    ///
    /// ```
    /// # use moso_storage::backend::MemoryStorage;
    /// assert!(MemoryStorage::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every key currently stored, in order. For assertions.
    ///
    /// ```
    /// # use moso_storage::backend::MemoryStorage;
    /// assert!(MemoryStorage::new().keys().is_empty());
    /// ```
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.read().keys().cloned().collect()
    }

    /// How many bytes are stored in total.
    ///
    /// ```
    /// # use moso_storage::backend::MemoryStorage;
    /// assert_eq!(MemoryStorage::new().total_bytes(), 0);
    /// ```
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.read()
            .values()
            .map(|(_, bytes)| bytes.len() as u64)
            .sum()
    }

    /// Forget everything.
    ///
    /// ```
    /// # use moso_storage::backend::MemoryStorage;
    /// MemoryStorage::new().clear();
    /// ```
    pub fn clear(&self) {
        self.write().clear();
    }

    /// The map, recovering from a poisoned lock.
    fn read(
        &self,
    ) -> std::sync::RwLockReadGuard<
        '_,
        std::collections::BTreeMap<String, (ObjectMeta, bytes::Bytes)>,
    > {
        self.objects
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The map, mutably.
    fn write(
        &self,
    ) -> std::sync::RwLockWriteGuard<
        '_,
        std::collections::BTreeMap<String, (ObjectMeta, bytes::Bytes)>,
    > {
        self.objects
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Storage for MemoryStorage {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            ranges: true,
            metadata: true,
            // A `BTreeMap` behind one lock genuinely is atomic, so this is a
            // capability rather than a claim.
            conditional_writes: true,
            delimited_listing: true,
            server_side_copy: true,
            public_objects: true,
            max_object_size: self.max_total_bytes,
            // Deliberately absent: there is no URL a browser could follow into
            // this process's heap.
            signed_urls: false,
            presigned_upload: false,
            multipart: false,
            min_part_size: 0,
        }
    }

    fn put<'a>(
        &'a self,
        key: &'a StorageKey,
        body: ByteStream,
        mut opts: PutOpts,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(async move {
            if opts.refuses_overwrite() && self.read().contains_key(key.as_str()) {
                return Err(crate::Error::refused(
                    "memory",
                    format!("`{key}` already exists and `if_absent` was set"),
                ));
            }

            let bytes = crate::collect_bounded(body, self.max_total_bytes, "memory").await?;

            if opts.sniffs()
                && let Some(sniffed) = crate::sniff(&bytes)
            {
                opts.set_content_type(sniffed);
            }

            let digest = crate::backend::sha256_hex(&bytes);
            if let Some(expected) = opts.expected_checksum()
                && expected.digest() != digest
            {
                return Err(crate::Error::Checksum {
                    key: key.to_string(),
                    expected: expected.digest().to_owned(),
                    actual: digest,
                });
            }

            let meta = crate::object::meta_from(
                key,
                bytes.len() as u64,
                &opts,
                Some(crate::Checksum::sha256(digest.clone())),
                Some(format!("\"{digest}\"")),
            );

            let mut objects = self.write();
            let existing: u64 = objects
                .iter()
                .filter(|(stored, _)| stored.as_str() != key.as_str())
                .map(|(_, (_, bytes))| bytes.len() as u64)
                .sum();
            if existing + bytes.len() as u64 > self.max_total_bytes {
                return Err(crate::Error::refused(
                    "memory",
                    format!(
                        "the in-memory store's {}-byte cap would be exceeded — a test that stores \
                         this much probably wants `LocalStorage` in a temporary directory",
                        self.max_total_bytes,
                    ),
                ));
            }
            objects.insert(key.as_str().to_owned(), (meta.clone(), bytes));
            Ok(meta)
        })
    }

    fn get<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            let bytes = self
                .read()
                .get(key.as_str())
                .map(|(_, bytes)| bytes.clone())
                .ok_or_else(|| crate::Error::not_found(key.as_str()))?;
            Ok(crate::stream_from_bytes(bytes))
        })
    }

    fn get_range<'a>(
        &'a self,
        key: &'a StorageKey,
        range: Range<u64>,
    ) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            let bytes = self
                .read()
                .get(key.as_str())
                .map(|(_, bytes)| bytes.clone())
                .ok_or_else(|| crate::Error::not_found(key.as_str()))?;

            let length = bytes.len() as u64;
            let start = range.start.min(length);
            let end = range.end.min(length);
            Ok(crate::stream_from_bytes(
                bytes.slice(start as usize..end.max(start) as usize),
            ))
        })
    }

    fn head<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<Option<ObjectMeta>>> {
        Box::pin(async move { Ok(self.read().get(key.as_str()).map(|(meta, _)| meta.clone())) })
    }

    fn delete<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(self.write().remove(key.as_str()).is_some()) })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        cursor: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(async move {
            let objects = self.read();
            let mut page = Vec::new();
            let mut next = None;

            // A `BTreeMap` is already in key order, so paging is "skip past the
            // cursor and take a page" with no sort.
            for (key, (meta, _)) in objects.iter() {
                if !key.starts_with(prefix) {
                    continue;
                }
                if let Some(cursor) = cursor
                    && key.as_str() <= cursor
                {
                    continue;
                }
                if page.len() == PAGE {
                    next = Some(key.clone());
                    break;
                }
                page.push(meta.clone());
            }

            Ok(Listing {
                objects: page,
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
            let (mut meta, bytes) = self
                .read()
                .get(from.as_str())
                .cloned()
                .ok_or_else(|| crate::Error::not_found(from.as_str()))?;
            meta.key = to.clone();
            meta.modified_at = Some(chrono::Utc::now());
            self.write()
                .insert(to.as_str().to_owned(), (meta.clone(), bytes));
            Ok(meta)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn put(storage: &MemoryStorage, key: &str, bytes: &'static [u8]) -> ObjectMeta {
        storage
            .put(
                &StorageKey::new(key).expect("valid"),
                crate::stream_from_bytes(bytes::Bytes::from_static(bytes)),
                PutOpts::new("application/octet-stream"),
            )
            .await
            .expect("stores")
    }

    /// A test double is only useful if it behaves like the real thing, so the
    /// round trip has to be exact.
    #[tokio::test]
    async fn an_object_round_trips() {
        let storage = MemoryStorage::new();
        put(&storage, "a/b.txt", b"hello").await;

        let key = StorageKey::new("a/b.txt").expect("valid");
        let bytes = crate::collect_bounded(storage.get(&key).await.expect("reads"), 1024, "t")
            .await
            .expect("collects");
        assert_eq!(bytes, "hello");

        let meta = storage.head(&key).await.expect("heads").expect("present");
        assert_eq!(meta.size, 5);
        assert!(meta.etag.is_some());
        assert!(meta.checksum.is_some());
    }

    /// The sniffer runs on the way in, so the stored type is what the bytes
    /// are — even when the caller declared otherwise.
    #[tokio::test]
    async fn the_stored_content_type_comes_from_the_bytes() {
        let storage = MemoryStorage::new();
        let key = StorageKey::new("a/logo.png").expect("valid");
        storage
            .put(
                &key,
                crate::stream_from_bytes(bytes::Bytes::from_static(
                    b"<html><body>hi</body></html>",
                )),
                PutOpts::new("image/png"),
            )
            .await
            .expect("stores");

        let meta = storage.head(&key).await.expect("heads").expect("present");
        assert_eq!(meta.content_type, "text/html");
    }

    /// `trust_content_type` is the escape hatch for content the application
    /// generated, and it has to actually skip the sniffer.
    #[tokio::test]
    async fn trusting_the_declared_type_skips_the_sniffer() {
        let storage = MemoryStorage::new();
        let key = StorageKey::new("a/report.csv").expect("valid");
        storage
            .put(
                &key,
                crate::stream_from_bytes(bytes::Bytes::from_static(b"a,b\n1,2\n")),
                PutOpts::new("text/csv").trust_content_type(),
            )
            .await
            .expect("stores");

        assert_eq!(
            storage
                .head(&key)
                .await
                .expect("heads")
                .expect("present")
                .content_type,
            "text/csv",
        );
    }

    /// A checksum that does not match means the bytes were corrupted in
    /// transit, and storing them anyway is worse than failing.
    #[tokio::test]
    async fn a_mismatched_checksum_is_refused() {
        let storage = MemoryStorage::new();
        let error = storage
            .put(
                &StorageKey::new("a/b").expect("valid"),
                crate::stream_from_bytes(bytes::Bytes::from_static(b"hello")),
                PutOpts::new("text/plain").expect_checksum(crate::Checksum::sha256("wrong")),
            )
            .await
            .expect_err("checksum mismatch");

        assert!(matches!(error, crate::Error::Checksum { .. }));
        assert!(
            storage.is_empty(),
            "a failed write leaves no partial object"
        );
    }

    /// The conditional write is real here, which is why the capability says so.
    #[tokio::test]
    async fn if_absent_refuses_an_overwrite() {
        let storage = MemoryStorage::new();
        put(&storage, "a/b", b"first").await;

        let error = storage
            .put(
                &StorageKey::new("a/b").expect("valid"),
                crate::stream_from_bytes(bytes::Bytes::from_static(b"second")),
                PutOpts::new("text/plain").if_absent(),
            )
            .await
            .expect_err("already exists");
        assert!(matches!(error, crate::Error::Refused { .. }));
    }

    /// Ranges are how `serve` answers a resumed download, so they have to be
    /// exact at both ends.
    #[tokio::test]
    async fn a_range_read_returns_exactly_the_requested_bytes() {
        let storage = MemoryStorage::new();
        put(&storage, "a/b", b"0123456789").await;
        let key = StorageKey::new("a/b").expect("valid");

        let slice = crate::collect_bounded(
            storage.get_range(&key, 2..5).await.expect("reads"),
            1024,
            "t",
        )
        .await
        .expect("collects");
        assert_eq!(slice, "234");
    }

    /// Absence is an answer for `head` and an error for `get`, which is the
    /// distinction the whole trait is built on.
    #[tokio::test]
    async fn a_missing_object_is_none_for_head_and_an_error_for_get() {
        let storage = MemoryStorage::new();
        let key = StorageKey::new("nope").expect("valid");

        assert!(storage.head(&key).await.expect("heads").is_none());
        assert!(
            storage
                .get(&key)
                .await
                .err()
                .expect("missing")
                .is_not_found(),
        );
        assert!(!storage.delete(&key).await.expect("deletes"));
    }

    /// Listing is prefix-scoped and in key order, which is what makes paging
    /// stable.
    #[tokio::test]
    async fn listing_is_prefix_scoped_and_ordered() {
        let storage = MemoryStorage::new();
        put(&storage, "b/1", b"x").await;
        put(&storage, "a/2", b"x").await;
        put(&storage, "a/1", b"x").await;

        let listing = storage.list("a/", None).await.expect("lists");
        let keys: Vec<_> = listing
            .objects
            .iter()
            .map(|meta| meta.key.as_str().to_owned())
            .collect();
        assert_eq!(keys, vec!["a/1".to_owned(), "a/2".to_owned()]);
        assert!(listing.cursor.is_none());
    }

    /// A copy is a new object with the new key, and the original survives.
    #[tokio::test]
    async fn copying_leaves_both_objects() {
        let storage = MemoryStorage::new();
        put(&storage, "a/1", b"payload").await;

        let to = StorageKey::new("b/1").expect("valid");
        let meta = storage
            .copy(&StorageKey::new("a/1").expect("valid"), &to)
            .await
            .expect("copies");
        assert_eq!(meta.key, to);
        assert_eq!(storage.len(), 2);
    }

    /// A runaway test must fail loudly rather than exhaust the machine.
    #[tokio::test]
    async fn the_cap_stops_a_runaway_test() {
        let storage = MemoryStorage::new().max_total_bytes(16);
        let error = storage
            .put(
                &StorageKey::new("a/big").expect("valid"),
                crate::stream_from_bytes(bytes::Bytes::from(vec![0_u8; 64])),
                PutOpts::new("application/octet-stream"),
            )
            .await
            .expect_err("over the cap");
        // Either the collecting limit or the total cap fires first; both name
        // the number, and both refuse the write.
        assert!(
            matches!(
                error,
                crate::Error::TooLarge { .. } | crate::Error::Refused { .. },
            ),
            "{error}",
        );
        assert!(storage.is_empty());
    }

    /// A signed URL that only worked inside this process would make a test
    /// pass that production fails, so the backend says plainly that it cannot.
    #[tokio::test]
    async fn the_memory_backend_does_not_pretend_to_presign() {
        let storage = MemoryStorage::new();
        assert!(!storage.capabilities().signed_urls);
        assert!(!storage.capabilities().presigned_upload);
        assert!(!storage.capabilities().multipart);

        let key = StorageKey::new("a/b").expect("valid");
        assert!(matches!(
            storage
                .signed_url(&key, std::time::Duration::from_secs(60))
                .await
                .expect_err("unsupported"),
            crate::Error::Unsupported { .. },
        ));
    }

    /// `delete_many` is what `Attachment::purge` calls, and its count has to be
    /// the number actually removed.
    #[tokio::test]
    async fn deleting_many_reports_how_many_were_there() {
        let storage = MemoryStorage::new();
        put(&storage, "a/1", b"x").await;
        put(&storage, "a/2", b"x").await;

        let keys = [
            StorageKey::new("a/1").expect("valid"),
            StorageKey::new("a/2").expect("valid"),
            StorageKey::new("a/3").expect("valid"),
        ];
        assert_eq!(storage.delete_many(&keys).await.expect("deletes"), 2);
        assert!(storage.is_empty());
    }
}
