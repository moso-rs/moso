#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = "Moso's object-storage battery: streamed uploads, presigning, and typed attachments."]
//!
//! Storing a file is easy. Storing a file *safely* is a list of things nobody
//! remembers until one of them is an incident: sniff the content type instead of
//! believing the client, enforce the size limit at the first byte instead of
//! after buffering, strip the GPS coordinates out of the photograph, refuse the
//! SVG, sandbox what you serve, and never let bytes larger than a few megabytes
//! travel through the application at all.
//!
//! ```no_run
//! use moso_storage::{PutOpts, Storage, StorageKey};
//!
//! async fn save(storage: &dyn Storage, body: moso_storage::ByteStream)
//!     -> moso_storage::Result<()>
//! {
//!     let key = StorageKey::from_segments(["avatars", "usr_123", "original.png"])?;
//!     storage.put(&key, body, PutOpts::new("image/png")).await?;
//!     Ok(())
//! }
//! ```
//!
//! # The map
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`mod@storage`] | [`Storage`], [`StorageCapabilities`] |
//! | [`mod@key`] | [`StorageKey`] — the only way to name an object |
//! | [`mod@object`] | [`ObjectMeta`], [`Listing`], [`PutOpts`], [`ByteStream`], [`Checksum`], [`collect_bounded()`] |
//! | [`mod@presign`] | [`UploadPolicy`], [`PresignedPost`], [`confirm_upload`] |
//! | [`mod@multipart`] | [`MultipartUpload`], [`PartNumber`], [`CompletedPart`], [`MultipartDriver`](multipart::MultipartDriver) |
//! | [`mod@attachment`] | [`Attachment`], [`AttachmentKind`], [`Variant`], [`VariantSpec`], [`Rendition`] |
//! | [`mod@upload`] | [`Upload`], [`sniff()`], [`accepts()`], [`sanitise_filename()`], [`strip_metadata`](upload::strip_metadata), [`svg_is_inert`](upload::svg_is_inert) |
//! | [`mod@serve`] | [`serve()`], [`from_parts`](serve::from_parts), [`ServedObject`] — `Range`, `ETag`, and the sandbox |
//! | [`mod@deadline`] | [`Deadlines`], [`TimedStorage`] — the two deadlines, and the wrapper that enforces them |
//! | [`mod@backend`] | every shipped [`Storage`] |
//! | [`mod@config`] | [`StorageConfig`], [`StorageHealthCheck`] |
//! | [`mod@error`] | [`Error`], and what each variant becomes over HTTP |
//!
//! # Three decisions worth knowing before reading the code
//!
//! **The content type comes from the bytes.** [`sniff()`] reads the leading
//! [`SNIFF_BYTES`](upload::SNIFF_BYTES) and decides; the client's
//! `Content-Type` and the filename's extension are hints and nothing more. A
//! `.png` that is really an HTML document is stored XSS on any origin that
//! serves user content, and no amount of validating the *declared* type
//! prevents it.
//!
//! **Nothing buffers.** [`ByteStream`] is the currency of every read and write.
//! The acceptance criterion is a 1 GiB upload under 20 MiB of peak RSS, and the
//! only way to meet it is for no layer to be allowed to collect. It is also why
//! there are **two** deadlines rather than one: a whole-operation limit around a
//! streaming transfer would kill a healthy gibibyte, so `put`, `get` and
//! `get_range` are bounded by a stall deadline that restarts on every chunk, and
//! only the calls that answer once are bounded end to end. See [`mod@deadline`].
//!
//! **This crate does not depend on `moso-orm`, and does not need to.**
//! [`Attachment`] is a plain `Serialize + Deserialize` descriptor, so
//! `#[entity(attachment(..))]` stores it through `moso_orm::Json` without an
//! edge in either direction (`xtask/allow/dep-edges.toml`: `storage -> []`).
//! A stateless service can store files without compiling a database layer.
//!
//! # Cargo features
//!
//! | Feature | Default | What it adds |
//! | --- | --- | --- |
//! | `local` | yes | `backend::LocalStorage`, and the development serve route |
//! | `memory` | yes | `backend::MemoryStorage`, the test double |
//! | `s3` | no | `backend::S3Storage` — S3, R2, MinIO, Backblaze, Wasabi, Tigris |
//! | `gcs` | no | `backend::GcsStorage` |
//! | `azure` | no | `backend::AzureStorage` |
//!
//! Code spans rather than links for the feature-gated names: a link to a type
//! that only exists under a cargo feature is a broken link in every build that
//! does not turn it on, and `rustdoc::broken_intra_doc_links` is `deny` across
//! this workspace.

pub mod attachment;
pub mod backend;
pub mod config;
pub mod deadline;
pub mod error;
pub mod key;
pub mod multipart;
pub mod object;
pub mod presign;
pub mod serve;
pub mod storage;
pub mod upload;

pub use crate::attachment::{
    Attachment, AttachmentKind, Fit, Rendition, Variant, VariantSpec, VariantState,
    VariantTransform,
};
pub use crate::config::{StorageBackendKind, StorageConfig, StorageHealthCheck};
pub use crate::deadline::{Deadlines, TimedStorage};
pub use crate::error::{BoxError, Error, Result};
pub use crate::key::StorageKey;
pub use crate::multipart::{CompletedPart, MultipartUpload, PartNumber};
pub use crate::object::{
    ByteStream, Checksum, Listing, ObjectMeta, PutOpts, Visibility, collect_bounded,
    stream_from_bytes,
};
pub use crate::presign::{PresignedPost, UploadConfirmation, UploadPolicy, confirm_upload};
pub use crate::serve::{ServeMode, ServedObject, serve};
pub use crate::storage::{Storage, StorageCapabilities};
pub use crate::upload::{Upload, accepts, sanitise_filename, sniff};

/// The version of this crate, for `moso doctor` and the boot log.
///
/// ```
/// assert!(!moso_storage::VERSION.is_empty());
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything an application that stores files imports.
///
/// ```no_run
/// use moso_storage::prelude::*;
///
/// async fn go(s: &dyn Storage, key: &StorageKey) -> Result<ByteStream> {
///     s.get(key).await
/// }
/// ```
pub mod prelude {
    pub use crate::{
        Attachment, AttachmentKind, ByteStream, Error, ObjectMeta, PutOpts, Result, Storage,
        StorageKey, Upload, UploadPolicy, Variant, Visibility,
    };
}

#[cfg(test)]
mod tests {
    /// The public surface resolves from the crate root, so an application
    /// writes `moso_storage::Storage` and not `moso_storage::storage::Storage`.
    #[test]
    fn the_frozen_surface_resolves_from_the_root() {
        fn exists<T>() {}

        exists::<crate::Checksum>();
        exists::<crate::CompletedPart>();
        exists::<crate::Deadlines>();
        exists::<crate::Error>();
        exists::<crate::Rendition>();
        exists::<crate::Listing>();
        exists::<crate::MultipartUpload>();
        exists::<crate::ObjectMeta>();
        exists::<crate::PartNumber>();
        exists::<crate::PresignedPost>();
        exists::<crate::PutOpts>();
        exists::<crate::ServedObject>();
        exists::<crate::StorageCapabilities>();
        exists::<crate::StorageConfig>();
        exists::<crate::StorageKey>();
        exists::<crate::UploadPolicy>();
        exists::<crate::Variant>();
        exists::<crate::VariantSpec>();
        exists::<crate::VariantState>();

        fn dyn_compatible(_: &dyn crate::Storage) {}
        let _ = dyn_compatible;
    }

    /// `Attachment<K>` and `Upload<K>` are generic over a marker with no data,
    /// so they stay `Send + Sync` whatever the marker is. This is the bound
    /// every entity field and every handler parameter relies on.
    #[test]
    fn the_generic_types_are_send_and_sync() {
        /// A kind with no variants, for the bound check only.
        struct Marker;

        impl crate::AttachmentKind for Marker {
            const NAME: &'static str = "Marker";
            const ACCEPT: &'static [&'static str] = &["application/octet-stream"];
            const MAX_SIZE: u64 = 1;
        }

        fn is_send_sync<T: Send + Sync>() {}
        is_send_sync::<crate::Attachment<Marker>>();

        fn is_send<T: Send>() {}
        is_send::<crate::Upload<Marker>>();
    }
}
