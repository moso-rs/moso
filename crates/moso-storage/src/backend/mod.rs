//! The shipped [`Storage`](crate::Storage) implementations.
//!
//! | Backend | Feature | Notes |
//! | --- | --- | --- |
//! | `LocalStorage` | `local` (default) | a directory, plus a route that serves it in development |
//! | `MemoryStorage` | `memory` (default) | tests; the same semantics, none of the durability |
//! | `S3Storage` | `s3` | S3 and everything that speaks it: R2, MinIO, Backblaze, Wasabi, Tigris |
//! | `GcsStorage` | `gcs` | Google Cloud Storage |
//! | `AzureStorage` | `azure` | Azure Blob Storage |
//!
//! Every backend reports honest
//! [`StorageCapabilities`](crate::StorageCapabilities). `MemoryStorage` in
//! particular does *not* claim to presign, because a signed URL that only works
//! inside the test process would make a test pass that production fails.

#[cfg(feature = "cloud")]
mod cloud;
#[cfg(feature = "local")]
mod local;
#[cfg(feature = "memory")]
mod memory;

#[cfg(feature = "azure")]
pub use crate::backend::cloud::AzureStorage;
#[cfg(feature = "gcs")]
pub use crate::backend::cloud::GcsStorage;
#[cfg(feature = "s3")]
pub use crate::backend::cloud::{AddressingStyle, S3Storage};
#[cfg(feature = "local")]
pub use crate::backend::local::LocalStorage;
#[cfg(feature = "memory")]
pub use crate::backend::memory::MemoryStorage;

/// Lowercase hex, the encoding every signature and every ETag in this crate
/// uses.
///
/// One implementation rather than one per backend, so an ETag from the local
/// backend and one from S3 are comparable.
#[cfg(any(feature = "local", feature = "memory", feature = "cloud"))]
pub(crate) fn hex(bytes: &[u8]) -> String {
    /// The 16 lowercase hex digits.
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// The SHA-256 of a byte string, hex-encoded.
#[cfg(any(feature = "memory", feature = "cloud"))]
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// Compare two byte strings without leaking their contents through timing.
///
/// Comparing the SHA-256 digests removes the leak without a constant-time
/// primitive: the only thing an attacker learns from the timing is how far two
/// digests agree, and a digest cannot be walked backwards into the signature.
#[cfg(any(feature = "local", feature = "cloud"))]
pub(crate) fn digest_eq(a: &[u8], b: &[u8]) -> bool {
    let left = ring::digest::digest(&ring::digest::SHA256, a);
    let right = ring::digest::digest(&ring::digest::SHA256, b);
    left.as_ref() == right.as_ref()
}
