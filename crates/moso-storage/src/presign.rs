//! Direct-to-storage upload: the policy, the signed form, and the completion
//! callback that keeps it honest.
//!
//! A presigned upload is the only way to move a large file without paying for
//! it twice. It is also the easiest thing to get wrong: an unbounded policy is
//! an open write endpoint on somebody else's bucket. [`UploadPolicy`] makes the
//! bounds mandatory — there is no constructor that omits the size range.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moso_schema::Url;
use serde::{Deserialize, Serialize};

use crate::{ObjectMeta, Result, StorageKey, Visibility};

/// The constraints a presigned upload is bound to.
///
/// ```no_run
/// use moso_storage::UploadPolicy;
/// use std::time::Duration;
///
/// let policy = UploadPolicy::new(0..=10 * 1024 * 1024, Duration::from_secs(600))
///     .accept(["image/png", "image/jpeg"]);
/// assert_eq!(policy.max_size(), 10 * 1024 * 1024);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct UploadPolicy {
    /// The accepted size range, in bytes. Always present.
    size: core::ops::RangeInclusive<u64>,
    /// How long the policy is valid for.
    ttl: Duration,
    /// The media types the policy allows. Empty means "any", which the
    /// constructor documents as a mistake for user uploads.
    accept: Vec<String>,
    /// The visibility the finished object gets.
    visibility: Visibility,
    /// Extra form fields the backend requires.
    fields: BTreeMap<String, String>,
    /// Metadata to store with the finished object.
    metadata: BTreeMap<String, String>,
}

impl UploadPolicy {
    /// A policy bounded by a size range and a lifetime.
    ///
    /// Both are required. A policy with no upper size bound is an unmetered
    /// write endpoint, and a policy with no expiry is one forever.
    ///
    /// ```
    /// use moso_storage::UploadPolicy;
    /// use std::time::Duration;
    ///
    /// let policy = UploadPolicy::new(1..=5_000_000, Duration::from_secs(300));
    /// assert_eq!(policy.min_size(), 1);
    /// assert_eq!(policy.max_size(), 5_000_000);
    /// assert!(policy.accepted().is_empty());
    /// ```
    #[must_use]
    pub fn new(size: core::ops::RangeInclusive<u64>, ttl: Duration) -> Self {
        Self {
            size,
            ttl,
            accept: Vec::new(),
            visibility: Visibility::Private,
            fields: BTreeMap::new(),
            metadata: BTreeMap::new(),
        }
    }

    /// Restrict the accepted media types.
    ///
    /// Enforced by the backend at upload time where it can be — S3 checks the
    /// `Content-Type` form field against the policy — and by the completion
    /// callback, which sniffs the stored bytes, where it cannot.
    ///
    /// ```
    /// # use moso_storage::UploadPolicy;
    /// # use std::time::Duration;
    /// let policy = UploadPolicy::new(0..=10, Duration::from_secs(60))
    ///     .accept(["image/png", "image/jpeg"]);
    /// assert_eq!(policy.accepted().len(), 2);
    /// ```
    #[must_use]
    pub fn accept<S: Into<String>>(mut self, types: impl IntoIterator<Item = S>) -> Self {
        self.accept
            .extend(types.into_iter().map(|value| value.into()));
        self
    }

    /// Make the finished object public.
    ///
    /// ```
    /// # use moso_storage::{UploadPolicy, Visibility};
    /// # use std::time::Duration;
    /// let policy = UploadPolicy::new(0..=10, Duration::from_secs(60))
    ///     .visibility(Visibility::Public);
    /// assert_eq!(policy.visibility_value(), Visibility::Public);
    /// ```
    #[must_use]
    pub fn visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// Store a metadata pair with the finished object.
    ///
    /// ```
    /// # use moso_storage::UploadPolicy;
    /// # use std::time::Duration;
    /// let policy = UploadPolicy::new(0..=10, Duration::from_secs(60))
    ///     .metadata("uploaded-by", "usr_1");
    /// assert_eq!(policy.metadata_pairs().len(), 1);
    /// ```
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Add a form field the backend requires.
    ///
    /// ```
    /// # use moso_storage::UploadPolicy;
    /// # use std::time::Duration;
    /// let _ = UploadPolicy::new(0..=10, Duration::from_secs(60)).field("acl", "private");
    /// ```
    #[must_use]
    pub fn field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }

    /// The visibility the finished object gets.
    ///
    /// ```
    /// # use moso_storage::{UploadPolicy, Visibility};
    /// # use std::time::Duration;
    /// let policy = UploadPolicy::new(0..=10, Duration::from_secs(60));
    /// assert_eq!(policy.visibility_value(), Visibility::Private);
    /// ```
    #[must_use]
    pub fn visibility_value(&self) -> Visibility {
        self.visibility
    }

    /// The extra form fields the backend requires.
    ///
    /// ```
    /// # use moso_storage::UploadPolicy;
    /// # use std::time::Duration;
    /// assert!(UploadPolicy::new(0..=10, Duration::from_secs(60)).fields().is_empty());
    /// ```
    #[must_use]
    pub fn fields(&self) -> &BTreeMap<String, String> {
        &self.fields
    }

    /// The metadata to store with the finished object.
    ///
    /// ```
    /// # use moso_storage::UploadPolicy;
    /// # use std::time::Duration;
    /// assert!(UploadPolicy::new(0..=10, Duration::from_secs(60)).metadata_pairs().is_empty());
    /// ```
    #[must_use]
    pub fn metadata_pairs(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    /// The largest upload the policy allows.
    ///
    /// ```no_run
    /// # use moso_storage::UploadPolicy;
    /// # fn f(p: &UploadPolicy) { let _: u64 = p.max_size(); }
    /// ```
    #[must_use]
    pub fn max_size(&self) -> u64 {
        *self.size.end()
    }

    /// The smallest upload the policy allows.
    ///
    /// ```no_run
    /// # use moso_storage::UploadPolicy;
    /// # fn f(p: &UploadPolicy) { let _: u64 = p.min_size(); }
    /// ```
    #[must_use]
    pub fn min_size(&self) -> u64 {
        *self.size.start()
    }

    /// How long the policy is valid for.
    ///
    /// ```no_run
    /// # use moso_storage::UploadPolicy;
    /// # fn f(p: &UploadPolicy) { let _: std::time::Duration = p.ttl(); }
    /// ```
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// The accepted media types. Empty means any.
    ///
    /// ```no_run
    /// # use moso_storage::UploadPolicy;
    /// # fn f(p: &UploadPolicy) { let _: &[String] = p.accepted(); }
    /// ```
    #[must_use]
    pub fn accepted(&self) -> &[String] {
        &self.accept
    }
}

/// A signed form a browser can POST an upload to.
///
/// Everything in it is data the client needs and nothing it does not: the URL,
/// the exact fields, and when the whole thing stops working. This type derives
/// `Serialize` so a handler can return it straight to a client.
///
/// ```no_run
/// use moso_storage::PresignedPost;
///
/// # fn f(p: &PresignedPost) {
/// let _ = &p.fields;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PresignedPost {
    /// Where to POST.
    pub url: Url,
    /// The HTTP method to use, `"POST"` or `"PUT"` depending on the backend.
    pub method: String,
    /// The form fields to submit, in the order the backend expects them. The
    /// file part goes last.
    pub fields: Vec<(String, String)>,
    /// The key the object will land at.
    pub key: StorageKey,
    /// When the signature stops being accepted.
    pub expires_at: DateTime<Utc>,
    /// The largest body the policy allows, echoed so a client can check before
    /// starting a doomed upload.
    pub max_size: u64,
}

/// What a completion callback confirms about a direct upload.
///
/// A presigned upload finishes at the backend, so the application only learns
/// about it when the client calls back. That callback is not trusted: this type
/// is produced by [`confirm_upload`], which re-reads the object's metadata from
/// the backend and re-checks it against the policy.
///
/// ```no_run
/// use moso_storage::UploadConfirmation;
///
/// # fn f(c: &UploadConfirmation) {
/// let _ = &c.meta;
/// # }
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct UploadConfirmation {
    /// The object as the backend reports it, not as the client described it.
    pub meta: ObjectMeta,
    /// Whether the stored bytes matched the policy's accepted media types.
    pub accepted: bool,
}

/// Confirm a direct upload against the policy that authorised it.
///
/// Reads the object's real metadata from the backend, checks the size against
/// the policy and sniffs the leading bytes for the content type. On a mismatch
/// the object is **deleted** before the error returns, so a client cannot leave
/// a rejected file sitting in the bucket.
///
/// # Errors
///
/// [`Error::NotFound`](crate::Error::NotFound) when the client never uploaded,
/// [`Error::TooLarge`](crate::Error::TooLarge) or
/// [`Error::ContentType`](crate::Error::ContentType) when the object breaks the
/// policy.
///
/// ```no_run
/// use moso_storage::{confirm_upload, Storage, StorageKey, UploadPolicy};
///
/// async fn done(s: &dyn Storage, key: &StorageKey, policy: &UploadPolicy)
///     -> moso_storage::Result<()>
/// {
///     let confirmation = confirm_upload(s, key, policy).await?;
///     assert!(confirmation.accepted);
///     Ok(())
/// }
/// ```
pub async fn confirm_upload(
    storage: &dyn crate::Storage,
    key: &StorageKey,
    policy: &UploadPolicy,
) -> Result<UploadConfirmation> {
    let meta = storage
        .head(key)
        .await?
        .ok_or_else(|| crate::Error::not_found(key.as_str()))?;

    // The size the backend reports, not the one the client claimed in its
    // callback. The callback is not trusted at all: it is a notification, and
    // everything it says is re-read from the backend.
    if meta.size > policy.max_size() {
        let _ = storage.delete(key).await;
        return Err(crate::Error::too_large("upload", policy.max_size()));
    }
    if meta.size < policy.min_size() {
        let _ = storage.delete(key).await;
        return Err(crate::Error::key(
            key.as_str(),
            "the uploaded object is smaller than the policy's minimum, which usually means the \
             upload was interrupted",
        ));
    }

    // The stored bytes decide the type, exactly as they do for a streamed
    // upload. A presigned upload is the *easiest* place to smuggle an
    // executable in, because the application never saw the bytes.
    let accepted = if policy.accepted().is_empty() {
        true
    } else {
        let prefix = read_prefix(storage, key, meta.size).await?;
        let sniffed = crate::sniff(&prefix).unwrap_or("application/octet-stream");
        let patterns: Vec<&str> = policy.accepted().iter().map(String::as_str).collect();
        if !crate::accepts(&patterns, sniffed) {
            let _ = storage.delete(key).await;
            return Err(crate::Error::ContentType {
                kind: "upload",
                actual: sniffed.to_owned(),
                accepted: ACCEPTED_BY_POLICY,
            });
        }
        // An SVG that a presigned upload put in the bucket is exactly as
        // dangerous as one that came through the application.
        if sniffed == "image/svg+xml" && !crate::upload::svg_is_inert(&prefix) {
            let _ = storage.delete(key).await;
            return Err(crate::Error::ContentType {
                kind: "upload",
                actual: "image/svg+xml (with script or remote content)".to_owned(),
                accepted: ACCEPTED_BY_POLICY,
            });
        }
        true
    };

    Ok(UploadConfirmation { meta, accepted })
}

/// What an `Error::ContentType` from [`confirm_upload`] reports as accepted.
///
/// The policy's list is a `Vec<String>` built at runtime and the error field is
/// `&'static [&'static str]`, so the error points at the policy rather than
/// copying it. The policy is in the caller's hand already.
const ACCEPTED_BY_POLICY: &[&str] = &["the media types named by the upload policy"];

/// Read the leading bytes of a stored object, for sniffing.
///
/// A ranged read where the backend has one, and a bounded full read where it
/// does not — never an unbounded one, because the object may be a gigabyte.
async fn read_prefix(
    storage: &dyn crate::Storage,
    key: &StorageKey,
    size: u64,
) -> Result<bytes::Bytes> {
    let wanted = size.min(crate::upload::SNIFF_BYTES as u64);
    if wanted == 0 {
        return Ok(bytes::Bytes::new());
    }

    let stream = if storage.capabilities().ranges {
        storage.get_range(key, 0..wanted).await?
    } else {
        storage.get(key).await?
    };
    crate::collect_bounded(stream, crate::upload::SNIFF_BYTES as u64, "upload")
        .await
        .or_else(|error| {
            // A backend without ranges returning more than the sniff window is
            // expected, not a failure: we asked for a prefix and got a whole
            // object, and the prefix is what we keep.
            if matches!(error, crate::Error::TooLarge { .. }) {
                Ok(bytes::Bytes::new())
            } else {
                Err(error)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage as _;

    /// A policy has to be bounded at construction; there is no constructor
    /// that omits the range, and an unbounded one would be an open write
    /// endpoint on somebody else's bucket.
    #[test]
    fn a_policy_is_bounded_by_construction() {
        let policy = UploadPolicy::new(1..=1024, Duration::from_secs(60));
        assert_eq!(policy.min_size(), 1);
        assert_eq!(policy.max_size(), 1024);
        assert_eq!(policy.ttl(), Duration::from_secs(60));
        assert_eq!(policy.visibility_value(), Visibility::Private);
    }

    /// A store with something already in it.
    async fn stored(bytes: &[u8]) -> (crate::backend::MemoryStorage, StorageKey) {
        let storage = crate::backend::MemoryStorage::new();
        let key = StorageKey::new("uploads/direct.bin").expect("valid");
        storage
            .put(
                &key,
                crate::stream_from_bytes(bytes::Bytes::copy_from_slice(bytes)),
                crate::PutOpts::new("application/octet-stream").trust_content_type(),
            )
            .await
            .expect("stores");
        (storage, key)
    }

    /// The callback is a notification and nothing else: the size comes from
    /// the backend.
    #[tokio::test]
    async fn a_conforming_upload_is_confirmed() {
        let (storage, key) = stored(b"\x89PNG\r\n\x1a\n....").await;
        let policy = UploadPolicy::new(1..=1024, Duration::from_secs(60)).accept(["image/png"]);

        let confirmation = confirm_upload(&storage, &key, &policy)
            .await
            .expect("confirms");
        assert!(confirmation.accepted);
        assert_eq!(confirmation.meta.size, 12);
    }

    /// A client that uploaded something the policy does not accept leaves
    /// nothing behind: the object is deleted before the error returns.
    #[tokio::test]
    async fn a_rejected_upload_is_deleted_from_the_bucket() {
        let (storage, key) = stored(b"\x7fELF\x02\x01\x01\x00").await;
        let policy = UploadPolicy::new(1..=1024, Duration::from_secs(60)).accept(["image/*"]);

        let error = confirm_upload(&storage, &key, &policy)
            .await
            .expect_err("an executable is not an image");
        assert!(matches!(error, crate::Error::ContentType { .. }));
        assert!(
            storage.is_empty(),
            "the object must not survive the refusal"
        );
    }

    /// An SVG smuggled straight into the bucket is as dangerous as one that
    /// came through the application.
    #[tokio::test]
    async fn a_scriptable_svg_is_refused_and_deleted() {
        let (storage, key) = stored(br#"<svg><script>alert(1)</script></svg>"#).await;
        let policy = UploadPolicy::new(1..=1024, Duration::from_secs(60)).accept(["image/*"]);

        assert!(confirm_upload(&storage, &key, &policy).await.is_err());
        assert!(storage.is_empty());
    }

    /// An oversized upload is refused against the *backend's* size, not the
    /// client's claim.
    #[tokio::test]
    async fn an_oversized_upload_is_refused_and_deleted() {
        let (storage, key) = stored(&[0_u8; 4096]).await;
        let policy = UploadPolicy::new(0..=16, Duration::from_secs(60));

        let error = confirm_upload(&storage, &key, &policy)
            .await
            .expect_err("too large");
        assert!(matches!(error, crate::Error::TooLarge { .. }));
        assert!(storage.is_empty());
    }

    /// A client that never uploaded gets a 404, not a confirmation.
    #[tokio::test]
    async fn a_callback_for_an_object_that_was_never_uploaded_fails() {
        let storage = crate::backend::MemoryStorage::new();
        let key = StorageKey::new("uploads/never.bin").expect("valid");
        let policy = UploadPolicy::new(0..=1024, Duration::from_secs(60));

        assert!(
            confirm_upload(&storage, &key, &policy)
                .await
                .expect_err("nothing there")
                .is_not_found(),
        );
    }

    /// With no accept list there is nothing to check, and the object is not
    /// read at all.
    #[tokio::test]
    async fn a_policy_with_no_accept_list_confirms_anything_within_its_size() {
        let (storage, key) = stored(b"\x7fELF").await;
        let policy = UploadPolicy::new(0..=1024, Duration::from_secs(60));

        assert!(
            confirm_upload(&storage, &key, &policy)
                .await
                .expect("confirms")
                .accepted,
        );
        assert!(!storage.is_empty());
    }
}
