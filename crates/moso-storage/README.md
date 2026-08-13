# moso-storage

Moso's object-storage battery: a framework-owned `Storage` trait, streamed uploads with magic-byte
sniffing, presigned direct upload, multipart, and typed entity attachments with variants.

Part of [Moso](https://github.com/lowsbarrel/moso). See `docs/03-batteries/34-mail-storage-realtime.md`
for the design.

```rust,ignore
use moso_storage::prelude::*;

async fn save(storage: &dyn Storage, key: &StorageKey, body: ByteStream) -> Result<()> {
    storage.put(key, body, PutOpts::new("image/png")).await?;
    Ok(())
}
```

## Status

**Implemented.** No `todo!()` remains.

| Piece | State |
| --- | --- |
| `Storage`, `StorageKey`, `ObjectMeta`, `PutOpts` | ✅ honest per-backend `StorageCapabilities` |
| `LocalStorage` | ✅ temp-file-then-rename, metadata sidecar, ranges, signed development URLs |
| `MemoryStorage` | ✅ the test double, which does **not** pretend to presign |
| `S3Storage` | ✅ SigV4, virtual-hosted and path addressing, presigned `PUT`, multipart; S3, R2, MinIO, Backblaze, Wasabi, Tigris |
| `GcsStorage` | ✅ service-account key (mints its own tokens, RS256), workload identity, or a supplied token; V4 signed URLs and presigned `PUT` with a key |
| `AzureStorage` | ✅ shared-key signing, block blobs, service SAS for signed URLs and presigned `PUT` |
| `Upload<K>` | ✅ magic-byte sniffing, streaming size cap, EXIF stripping, SVG refused unless inert |
| `serve` | ✅ `Range`, `ETag`/`If-None-Match`, `If-Range`, RFC 6266 filenames, `Content-Security-Policy: sandbox`; `storage.serve(&key)` or `serve(storage, &key)` |
| Presigned upload | ✅ bounded policy, and `confirm_upload` re-reads and **deletes** a non-conforming object |
| `Attachment<K>` | ✅ one JSON column, variants recorded `Pending` for a background job |
| `Deadlines` / `TimedStorage` | ✅ a whole-operation deadline for calls that answer once and a stall deadline for transfers; `StorageConfig::build` installs both |

Not here, on purpose: **no image codec**. `VariantSpec`, `Rendition`,
`Attachment::read_original`, `store_variant`, `mark_failed` and `is_complete` are the seam;
the encoder is the application's, and the crate's rustdoc ships a tested job body proving
the wiring with a byte-identity transform.

`tests/acceptance.rs` measures the two criteria a unit test cannot see: a 1 GiB upload
streams within the 20 MiB peak-RSS budget (1.4 MiB observed), and a `.png`-named Mach-O
executable is rejected through the real extractor.

## Licence

MIT - see the root [`LICENSE`](../../LICENSE).
