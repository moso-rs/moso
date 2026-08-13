//! Multipart upload: writing one object as many parts.
//!
//! Needed for anything above a few gigabytes, and useful below that because a
//! failed part is retried rather than the whole transfer. The type is a
//! *handle* rather than a builder: it holds the backend's upload id, and
//! dropping it without [`MultipartUpload::complete`] or
//! [`MultipartUpload::abort`] leaks storage the provider will bill for — which
//! is why `Drop` logs a warning naming the key.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{Deadlines, ObjectMeta, Result, StorageKey};

/// A part's ordinal. One-based, because every backend numbers parts from one.
///
/// ```no_run
/// use moso_storage::PartNumber;
///
/// let first = PartNumber::new(1)?;
/// assert_eq!(first.get(), 1);
/// # Ok::<(), moso_storage::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PartNumber(u32);

impl PartNumber {
    /// The largest part number every supported backend accepts.
    ///
    /// ```
    /// assert_eq!(moso_storage::PartNumber::MAX, 10_000);
    /// ```
    pub const MAX: u32 = 10_000;

    /// Wrap a one-based part number.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) when `number` is zero or above
    /// [`PartNumber::MAX`].
    ///
    /// ```no_run
    /// use moso_storage::PartNumber;
    ///
    /// assert!(PartNumber::new(0).is_err());
    /// ```
    pub fn new(number: u32) -> Result<Self> {
        if number == 0 {
            return Err(crate::Error::config(
                "a part number is one-based; part 0 does not exist on any supported backend",
            ));
        }
        if number > Self::MAX {
            return Err(crate::Error::config(format!(
                "part number {number} is above the {} every supported backend accepts — use \
                 larger parts",
                Self::MAX,
            )));
        }
        Ok(Self(number))
    }

    /// The number.
    ///
    /// ```no_run
    /// # use moso_storage::PartNumber;
    /// # fn f(p: PartNumber) { let _: u32 = p.get(); }
    /// ```
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A part the backend has acknowledged.
///
/// Collected and handed back to [`MultipartUpload::complete`], which is what
/// tells the backend the order to assemble them in.
///
/// ```no_run
/// use moso_storage::CompletedPart;
///
/// # fn f(p: &CompletedPart) {
/// let _ = &p.etag;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompletedPart {
    /// Which part this was.
    pub number: PartNumber,
    /// The entity tag the backend returned.
    pub etag: String,
    /// How many bytes it carried.
    pub size: u64,
}

/// An upload in progress.
///
/// ```no_run
/// use moso_storage::{MultipartUpload, PartNumber};
///
/// async fn go(mut upload: MultipartUpload, chunk: bytes::Bytes)
///     -> moso_storage::Result<()>
/// {
///     let part = upload.upload_part(PartNumber::new(1)?, chunk).await?;
///     upload.complete(vec![part]).await?;
///     Ok(())
/// }
/// ```
pub struct MultipartUpload {
    /// The key being written.
    key: StorageKey,
    /// The backend's identifier for this upload.
    upload_id: String,
    /// The backend's name, for error messages.
    backend: &'static str,
    /// The smallest non-final part this backend accepts.
    min_part_size: u64,
    /// What actually talks to the backend.
    driver: std::sync::Arc<dyn MultipartDriver>,
    /// How long each of the three operations may take.
    deadlines: Deadlines,
    /// Whether the handle was completed or aborted, so `Drop` can warn.
    settled: bool,
}

/// What a backend has to be able to do to support multipart.
///
/// Separate from [`Storage`](crate::Storage) because a multipart upload is a
/// *session*: it carries the backend's upload id, and the three operations are
/// only meaningful against that id. Dyn-compatible for the usual reason —
/// [`MultipartUpload`] is one concrete type whatever produced it.
///
/// ```no_run
/// use moso_storage::multipart::MultipartDriver;
///
/// fn takes(driver: &dyn MultipartDriver) {
///     let _ = driver;
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot drive a multipart upload",
    label = "not a multipart driver",
    note = "a multipart driver implements `upload_part`, `complete` and `abort` against an upload \
            id the backend issued",
    note = "help: implement it alongside `Storage::multipart_start`, or leave \
            `StorageCapabilities::multipart` false and let the default `Error::Unsupported` stand"
)]
pub trait MultipartDriver: Send + Sync + 'static {
    /// Upload one part, returning what the backend acknowledged.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) for a transient
    /// failure; the same part number may simply be retried.
    fn upload_part<'a>(
        &'a self,
        upload_id: &'a str,
        key: &'a StorageKey,
        number: PartNumber,
        body: Bytes,
    ) -> moso_core::BoxFuture<'a, Result<CompletedPart>>;

    /// Assemble the parts into the finished object.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`](crate::Error::Refused) when a part is missing or too
    /// small.
    fn complete<'a>(
        &'a self,
        upload_id: &'a str,
        key: &'a StorageKey,
        parts: Vec<CompletedPart>,
    ) -> moso_core::BoxFuture<'a, Result<ObjectMeta>>;

    /// Abandon the upload, freeing whatever the backend is holding.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    fn abort<'a>(
        &'a self,
        upload_id: &'a str,
        key: &'a StorageKey,
    ) -> moso_core::BoxFuture<'a, Result<()>>;
}

impl MultipartUpload {
    /// Build a handle for an upload a backend has already started.
    ///
    /// Called from [`Storage::multipart_start`](crate::Storage::multipart_start).
    /// It is public because a third-party backend has to be able to return one.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_storage::{multipart::MultipartDriver, MultipartUpload, StorageKey};
    /// # fn f(key: StorageKey, driver: Arc<dyn MultipartDriver>) -> MultipartUpload {
    /// MultipartUpload::new(key, "upload-id-from-the-backend", "s3", 5 * 1024 * 1024, driver)
    /// # }
    /// ```
    #[must_use]
    pub fn new(
        key: StorageKey,
        upload_id: impl Into<String>,
        backend: &'static str,
        min_part_size: u64,
        driver: std::sync::Arc<dyn MultipartDriver>,
    ) -> Self {
        Self {
            key,
            upload_id: upload_id.into(),
            backend,
            min_part_size,
            driver,
            deadlines: Deadlines::NONE,
            settled: false,
        }
    }

    /// Bound each of the three operations by `deadlines`.
    ///
    /// Set by [`TimedStorage`](crate::TimedStorage) when it forwards
    /// `multipart_start`, because the session outlives the call that opened it
    /// and a deadline that stopped at the handle would bound nothing.
    ///
    /// All three are **unary**: a part is bytes already in memory, so there is
    /// no progress signal to restart a stall deadline from, and
    /// [`Deadlines::operation`] has to be long enough for the largest part a
    /// caller sends. Chunk to fit it, or raise it.
    ///
    /// ```no_run
    /// # use moso_storage::{Deadlines, MultipartUpload};
    /// # use std::time::Duration;
    /// # fn f(u: MultipartUpload) -> MultipartUpload {
    /// u.with_deadlines(Deadlines::uniform(Duration::from_secs(120)))
    /// # }
    /// ```
    #[must_use]
    pub fn with_deadlines(mut self, deadlines: Deadlines) -> Self {
        self.deadlines = deadlines;
        self
    }

    /// What bounds the three operations.
    ///
    /// ```no_run
    /// # use moso_storage::{Deadlines, MultipartUpload};
    /// # fn f(u: &MultipartUpload) { let _: Deadlines = u.deadlines(); }
    /// ```
    #[must_use]
    pub fn deadlines(&self) -> Deadlines {
        self.deadlines
    }

    /// The backend this upload belongs to.
    ///
    /// ```no_run
    /// # use moso_storage::MultipartUpload;
    /// # fn f(u: &MultipartUpload) { let _: &str = u.backend(); }
    /// ```
    #[must_use]
    pub fn backend(&self) -> &'static str {
        self.backend
    }

    /// The key being written.
    ///
    /// ```no_run
    /// # use moso_storage::{MultipartUpload, StorageKey};
    /// # fn f(u: &MultipartUpload) { let _: &StorageKey = u.key(); }
    /// ```
    #[must_use]
    pub fn key(&self) -> &StorageKey {
        &self.key
    }

    /// The backend's identifier for this upload, for resuming after a restart.
    ///
    /// ```no_run
    /// # use moso_storage::MultipartUpload;
    /// # fn f(u: &MultipartUpload) { let _: &str = u.upload_id(); }
    /// ```
    #[must_use]
    pub fn upload_id(&self) -> &str {
        &self.upload_id
    }

    /// The smallest non-final part this backend accepts, in bytes.
    ///
    /// A caller that chunks smaller than this gets an error from the backend at
    /// completion time rather than at upload time, which is a miserable way to
    /// find out. Read it and chunk accordingly.
    ///
    /// ```no_run
    /// # use moso_storage::MultipartUpload;
    /// # fn f(u: &MultipartUpload) { let _: u64 = u.min_part_size(); }
    /// ```
    #[must_use]
    pub fn min_part_size(&self) -> u64 {
        self.min_part_size
    }

    /// Upload one part.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) for a transient
    /// failure — the same part number may simply be retried, which is the
    /// point of multipart.
    ///
    /// ```no_run
    /// # use moso_storage::{CompletedPart, MultipartUpload, PartNumber};
    /// # async fn f(u: &MultipartUpload, n: PartNumber, b: bytes::Bytes)
    /// #     -> moso_storage::Result<CompletedPart> { u.upload_part(n, b).await }
    /// ```
    pub async fn upload_part(&self, number: PartNumber, body: Bytes) -> Result<CompletedPart> {
        self.deadlines
            .unary(
                self.backend,
                "upload_part",
                self.driver
                    .upload_part(&self.upload_id, &self.key, number, body),
            )
            .await
    }

    /// Assemble the parts into the finished object.
    ///
    /// Consumes the handle, which is how the type system stops a completed
    /// upload from being completed twice.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`](crate::Error::Refused) when a part is missing, out of
    /// order, or below the backend's minimum size.
    ///
    /// ```no_run
    /// # use moso_storage::{CompletedPart, MultipartUpload, ObjectMeta};
    /// # async fn f(u: MultipartUpload, p: Vec<CompletedPart>)
    /// #     -> moso_storage::Result<ObjectMeta> { u.complete(p).await }
    /// ```
    pub async fn complete(mut self, mut parts: Vec<CompletedPart>) -> Result<ObjectMeta> {
        // The backend assembles in the order it is given, and a caller that
        // uploaded parts concurrently collected them in completion order.
        // Sorting here means "concurrent upload" is not also "corrupt object".
        parts.sort_by_key(|part| part.number);

        // Every non-final part has to reach the backend's minimum, and finding
        // that out from the backend at completion time is a miserable way to
        // lose a multi-gigabyte transfer.
        if let Some(short) = parts
            .split_last()
            .and_then(|(_, rest)| rest.iter().find(|part| part.size < self.min_part_size))
        {
            self.settled = true;
            let _ = self.driver.abort(&self.upload_id, &self.key).await;
            return Err(crate::Error::refused(
                self.backend,
                format!(
                    "part {} is {} bytes, below the {}-byte minimum this backend requires for \
                     every part but the last — read `MultipartUpload::min_part_size` and chunk \
                     accordingly",
                    short.number.get(),
                    short.size,
                    self.min_part_size,
                ),
            ));
        }

        self.settled = true;
        self.deadlines
            .unary(
                self.backend,
                "multipart_complete",
                self.driver.complete(&self.upload_id, &self.key, parts),
            )
            .await
    }

    /// Abandon the upload, freeing whatever the backend is holding.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable). An abort that fails
    /// leaves storage the provider bills for, so the error is worth logging
    /// rather than swallowing.
    ///
    /// ```no_run
    /// # use moso_storage::MultipartUpload;
    /// # async fn f(u: MultipartUpload) -> moso_storage::Result<()> { u.abort().await }
    /// ```
    pub async fn abort(mut self) -> Result<()> {
        self.settled = true;
        self.deadlines
            .unary(
                self.backend,
                "multipart_abort",
                self.driver.abort(&self.upload_id, &self.key),
            )
            .await
    }
}

impl core::fmt::Debug for MultipartUpload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MultipartUpload")
            .field("key", &self.key)
            .field("upload_id", &self.upload_id)
            .field("backend", &self.backend)
            .field("min_part_size", &self.min_part_size)
            .field("deadlines", &self.deadlines)
            .finish_non_exhaustive()
    }
}

impl Drop for MultipartUpload {
    /// Warn about an upload nobody finished.
    ///
    /// A multipart upload that is neither completed nor aborted leaves parts
    /// the provider bills for, indefinitely, and nothing ever lists them. The
    /// warning names the key and the upload id, which is exactly what an
    /// `abort-multipart-upload` needs.
    fn drop(&mut self) {
        if !self.settled {
            tracing::warn!(
                target: "moso::storage",
                backend = self.backend,
                key = %self.key,
                upload_id = %self.upload_id,
                "a multipart upload was dropped without `complete` or `abort`; the parts already \
                 uploaded will be billed until something aborts it",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver that records what it was asked to do.
    #[derive(Debug, Default)]
    struct Recorder {
        parts: std::sync::Mutex<Vec<PartNumber>>,
        completed: std::sync::Mutex<Option<Vec<PartNumber>>>,
        aborted: std::sync::atomic::AtomicBool,
    }

    impl MultipartDriver for Recorder {
        fn upload_part<'a>(
            &'a self,
            _: &'a str,
            _: &'a StorageKey,
            number: PartNumber,
            body: Bytes,
        ) -> moso_core::BoxFuture<'a, Result<CompletedPart>> {
            Box::pin(async move {
                self.parts.lock().expect("not poisoned").push(number);
                Ok(CompletedPart {
                    number,
                    etag: format!("etag-{}", number.get()),
                    size: body.len() as u64,
                })
            })
        }

        fn complete<'a>(
            &'a self,
            _: &'a str,
            key: &'a StorageKey,
            parts: Vec<CompletedPart>,
        ) -> moso_core::BoxFuture<'a, Result<ObjectMeta>> {
            Box::pin(async move {
                *self.completed.lock().expect("not poisoned") =
                    Some(parts.iter().map(|part| part.number).collect());
                Ok(crate::ObjectMeta {
                    key: key.clone(),
                    size: parts.iter().map(|part| part.size).sum(),
                    content_type: "application/octet-stream".to_owned(),
                    etag: None,
                    modified_at: None,
                    checksum: None,
                    metadata: std::collections::BTreeMap::new(),
                    cache_control: None,
                    content_disposition: None,
                    public: false,
                })
            })
        }

        fn abort<'a>(
            &'a self,
            _: &'a str,
            _: &'a StorageKey,
        ) -> moso_core::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.aborted
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            })
        }
    }

    fn upload(driver: std::sync::Arc<Recorder>, min_part_size: u64) -> MultipartUpload {
        MultipartUpload::new(
            StorageKey::new("uploads/big.bin").expect("valid"),
            "upload-1",
            "test",
            min_part_size,
            driver,
        )
    }

    /// Every backend numbers parts from one, and a caller that passes zero has
    /// made a mistake worth catching before the request.
    #[test]
    fn a_part_number_is_one_based_and_bounded() {
        assert!(PartNumber::new(0).is_err());
        assert!(PartNumber::new(PartNumber::MAX + 1).is_err());
        assert_eq!(PartNumber::new(1).expect("valid").get(), 1);
        assert_eq!(
            PartNumber::new(PartNumber::MAX).expect("valid").get(),
            PartNumber::MAX,
        );
    }

    /// A caller that uploaded parts concurrently collected them in completion
    /// order; assembling in that order would corrupt the object.
    #[tokio::test]
    async fn parts_are_assembled_in_number_order_however_they_arrived() {
        let driver = std::sync::Arc::new(Recorder::default());
        let handle = upload(driver.clone(), 0);

        let three = handle
            .upload_part(PartNumber::new(3).expect("valid"), Bytes::from_static(b"c"))
            .await
            .expect("uploads");
        let one = handle
            .upload_part(PartNumber::new(1).expect("valid"), Bytes::from_static(b"a"))
            .await
            .expect("uploads");
        let two = handle
            .upload_part(PartNumber::new(2).expect("valid"), Bytes::from_static(b"b"))
            .await
            .expect("uploads");

        handle
            .complete(vec![three, one, two])
            .await
            .expect("completes");

        let order = driver
            .completed
            .lock()
            .expect("not poisoned")
            .clone()
            .expect("completed");
        assert_eq!(
            order
                .iter()
                .copied()
                .map(PartNumber::get)
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
        );
    }

    /// Finding out at completion time that a part was too small loses the whole
    /// transfer. Catching it here also aborts, so nothing is left behind.
    #[tokio::test]
    async fn a_short_non_final_part_is_caught_before_the_backend_sees_it() {
        let driver = std::sync::Arc::new(Recorder::default());
        let handle = upload(driver.clone(), 5 * 1024 * 1024);

        let parts = vec![
            CompletedPart {
                number: PartNumber::new(1).expect("valid"),
                etag: "a".to_owned(),
                size: 1024,
            },
            CompletedPart {
                number: PartNumber::new(2).expect("valid"),
                etag: "b".to_owned(),
                size: 1024,
            },
        ];

        let error = handle.complete(parts).await.expect_err("part 1 is short");
        assert!(error.to_string().contains("below the 5242880-byte minimum"));
        assert!(driver.aborted.load(std::sync::atomic::Ordering::Relaxed));
        assert!(driver.completed.lock().expect("not poisoned").is_none());
    }

    /// The last part may be any size, which is what lets a transfer end on a
    /// short chunk.
    #[tokio::test]
    async fn the_final_part_may_be_short() {
        let driver = std::sync::Arc::new(Recorder::default());
        let handle = upload(driver.clone(), 1024);

        let parts = vec![
            CompletedPart {
                number: PartNumber::new(1).expect("valid"),
                etag: "a".to_owned(),
                size: 4096,
            },
            CompletedPart {
                number: PartNumber::new(2).expect("valid"),
                etag: "b".to_owned(),
                size: 7,
            },
        ];

        let meta = handle.complete(parts).await.expect("completes");
        assert_eq!(meta.size, 4103);
    }

    /// Aborting frees what the backend is holding, and the handle is consumed
    /// so it cannot be aborted twice.
    #[tokio::test]
    async fn aborting_releases_the_upload() {
        let driver = std::sync::Arc::new(Recorder::default());
        upload(driver.clone(), 0).abort().await.expect("aborts");
        assert!(driver.aborted.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// A session outlives the call that opened it, so the deadline has to
    /// travel with the handle — otherwise a part upload against a wedged
    /// endpoint hangs forever with nothing bounding it.
    #[tokio::test]
    async fn a_part_upload_is_bounded_by_the_handles_deadline() {
        /// A driver that never answers.
        #[derive(Debug)]
        struct Wedged;

        impl MultipartDriver for Wedged {
            fn upload_part<'a>(
                &'a self,
                _: &'a str,
                _: &'a StorageKey,
                _: PartNumber,
                _: Bytes,
            ) -> moso_core::BoxFuture<'a, Result<CompletedPart>> {
                Box::pin(async {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    Err(crate::Error::unsupported("test", "upload_part"))
                })
            }

            fn complete<'a>(
                &'a self,
                _: &'a str,
                _: &'a StorageKey,
                _: Vec<CompletedPart>,
            ) -> moso_core::BoxFuture<'a, Result<ObjectMeta>> {
                Box::pin(async { Err(crate::Error::unsupported("test", "complete")) })
            }

            fn abort<'a>(
                &'a self,
                _: &'a str,
                _: &'a StorageKey,
            ) -> moso_core::BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }

        let handle = MultipartUpload::new(
            StorageKey::new("uploads/big.bin").expect("valid"),
            "upload-1",
            "test",
            0,
            std::sync::Arc::new(Wedged),
        )
        .with_deadlines(Deadlines::uniform(std::time::Duration::from_millis(50)));

        let error = handle
            .upload_part(
                PartNumber::new(1).expect("valid"),
                Bytes::from_static(b"part"),
            )
            .await
            .expect_err("the deadline fires");
        assert!(
            matches!(
                error,
                crate::Error::Timeout {
                    operation: "upload_part",
                    ..
                },
            ),
            "{error}",
        );

        // The handle reports what it enforces, so a caller chunking a transfer
        // can size its parts against the number rather than guessing.
        assert_eq!(
            handle.deadlines().operation(),
            Some(std::time::Duration::from_millis(50)),
        );
        handle.abort().await.expect("aborts");
    }
}
