---
title: File storage
description: Stream objects through one Storage trait, validate uploads by their bytes rather than their names, presign direct uploads, serve files back with range and cache handling, and attach files to records.
order: 32
status: shipped
---

`moso-storage` is one trait over five backends, a validated key type, a body extractor that decides
an upload's media type by reading its bytes, and a response type that serves an object back with
`Range`, `ETag` and a sandbox policy already correct. Nothing in it buffers: a gibibyte moves
through a handler inside a couple of megabytes of resident memory, and there is a test that measures
it.

You describe each file kind with an `AttachmentKind` you write by hand, a handful of associated
constants. Variant rendering is a seam by design: no image codec is a dependency, so the encoder is
yours while the crate owns everything around it and `Attachment<K>` records every variant's state.

## Turning it on

The `moso` facade does not re-export `moso-storage`. Add the crate directly.

```toml title="Cargo.toml"
[dependencies]
moso = { version = "0.1" }
moso-storage = { version = "0.1" }
```

| Feature | Default | Adds | Needs |
| --- | --- | --- | --- |
| `local` | yes | `backend::LocalStorage` and its development serve route | a writable directory |
| `memory` | yes | `backend::MemoryStorage` | nothing |
| `s3` | no | `backend::S3Storage`, `backend::AddressingStyle` | S3, R2, MinIO, Backblaze, Wasabi or Tigris |
| `gcs` | no | `backend::GcsStorage` | Google Cloud Storage |
| `azure` | no | `backend::AzureStorage` | Azure Blob Storage |

`cloud` is a private feature the three cloud backends turn on. It pulls `reqwest` and `rustls`.

> [!NOTE]
> Adding `moso-storage` turns on `moso-core/multipart` unconditionally, because `Upload<K>` reads a
> `multipart/form-data` body. That feature is off by default in `moso-core`, so pulling this crate in
> changes the facade's feature resolution and adds a little compile time even if you never write an
> upload handler.

## The smallest thing that stores

```rust
use moso_storage::{PutOpts, Storage, StorageKey};

async fn save(storage: &dyn Storage, body: moso_storage::ByteStream)
    -> moso_storage::Result<()>
{
    let key = StorageKey::from_segments(["avatars", "usr_123", "original.png"])?;
    storage.put(&key, body, PutOpts::new("image/png")).await?;
    Ok(())
}
```

Everything on the read and write path is a `ByteStream`, which is
`Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>`. `stream_from_bytes(bytes)` makes one out of a
buffer you already have, and `collect_bounded(stream, limit, kind)` is the single place in the crate
that turns a stream back into bytes, with a bound you have to name.

In an application the storage handle arrives through
[dependency injection](./dependency-injection.md) as `Inject<dyn Storage>`, so no handler names a
backend.

## The `Storage` trait

| Method | Returns | Notes |
| --- | --- | --- |
| `name()` | `&'static str` | `"local"`, `"memory"`, `"s3"`, `"gcs"`, `"azure"` |
| `capabilities()` | `StorageCapabilities` | The honest limits, see below |
| `put(key, body, opts)` | `ObjectMeta` | Streams in; sniffs unless you opted out |
| `get(key)` | `ByteStream` | Streams out |
| `get_range(key, range)` | `ByteStream` | A byte range |
| `head(key)` | `Option<ObjectMeta>` | Metadata without a body |
| `delete(key)` | `bool` | Whether something was there |
| `delete_many(keys)` | `u64` | How many were deleted |
| `list(prefix, cursor)` | `Listing` | Objects, common prefixes, next cursor |
| `copy(from, to)` | `ObjectMeta` | Server-side where the backend can |
| `serve(key)` | `ServedObject` | A `Range`/`ETag`-aware response. Same as `serve(storage, key)` |
| `signed_url(key, ttl)` | `Url` | `Error::Unsupported` by default |
| `presigned_upload(key, policy)` | `PresignedPost` | `Error::Unsupported` by default |
| `multipart_start(key, opts)` | `MultipartUpload` | `Error::Unsupported` by default |
| `probe()` | `()` | Reachability, for a health check |

The last four have defaults that return `Error::Unsupported`, which is deliberate. A memory backend
that faked presigning would make a test pass that production fails. Ask before you act:

| Backend | Ranges | Signed URLs | Presign | Multipart | Min part | Server copy | Conditional | Public | Metadata | Max object |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `local` | yes | only with `served_at` | no | no | 0 | yes | no | no | yes | unbounded |
| `memory` | yes | no | no | no | 0 | yes | yes | yes | yes | the total cap |
| `s3` | yes | yes | yes | yes | 5 MiB | yes | yes | yes | yes | 5 TiB |
| `gcs` | yes | with a service-account key | with a service-account key | no | 0 | yes | yes | yes | yes | 5 TiB |
| `azure` | yes | yes | yes | no | 0 | yes | yes | no | yes | 190 TiB |

> [!WARNING]
> `memory` never signs, and `local` signs only once `served_at` has given it a route to check the
> signature against. On GCS, signing needs the **service-account key**: workload identity and a
> supplied access token are bearer credentials and hold no private key, so `capabilities()` reports
> `signed_urls: false` and `Attachment::url` fails with an error naming what to configure. Serve
> those through your own handler with `storage.serve(&key)` instead of handing out a URL.

## Keys and paths

A storage key is not a path and must not be treated as one. `StorageKey` validates once, on
construction, and the type is the proof. The local backend joins a key onto a directory, so `..`
would be a traversal; S3 accepts almost any byte sequence, so a newline is a name nobody can type
again in a console.

Rejected on construction: an empty key, more than 1024 bytes, a leading or trailing `/`, any `\`,
an empty segment (`//`), `.` or `..` as a segment, and any control character. The 1024-byte limit is
S3's and the smallest of the four remote backends, so a key that works in development works in
production. `StorageKey` revalidates on deserialisation, so a key that arrived in a JSON column is
checked again.

```rust
use moso_storage::StorageKey;

// One segment per element. A segment containing `/` is an error, not an extra level.
let key = StorageKey::from_segments(["avatars", user_id, "original.png"])?;
assert!(StorageKey::from_segments(["a", "b/c"]).is_err());

let thumb = key.with_suffix("thumb")?;      // avatars/usr_1/original.thumb.png
let sibling = key.with_name("meta.json")?;  // avatars/usr_1/meta.json

assert_eq!(key.name(), "original.png");
assert_eq!(key.extension(), Some("png"));
assert_eq!(key.prefix(), "avatars/usr_1");
assert!(key.is_under("avatars"));
```

Build keys from user input with `from_segments` and never with `format!`. `is_under` is
segment-aware, so `"avatars"` does not match `"avatars-old/x"`.

## Writing objects

`PutOpts` carries everything about a write except the bytes.

| Builder | Effect |
| --- | --- |
| `PutOpts::new(content_type)` | The declared type. Sniffing on, `Private`, overwrite allowed |
| `.cache_control(value)` | A `Cache-Control` stored with the object and replayed on serve |
| `.content_disposition(value)` | A stored `Content-Disposition` |
| `.metadata(key, value)` | An arbitrary pair, on backends whose `metadata` capability is true |
| `.visibility(Visibility::Public)` | Public where the backend supports it |
| `.expect_checksum(checksum)` | A SHA-256 verified while streaming; a mismatch is `Error::Checksum` |
| `.if_absent()` | Refuse to overwrite, on backends with `conditional_writes` |
| `.trust_content_type()` | Turns sniffing **off**. See the warning |

`PutOpts::default()` declares `application/octet-stream`.

> [!CAUTION]
> `trust_content_type()` reads like an assertion and is in fact a downgrade: it disables the
> byte-level content check. Use it only for bytes your own code produced, or for bytes that a
> `Upload<K>` extractor has already sniffed. `Attachment::attach` uses it for exactly that second
> reason.

Reading back is symmetrical. `head` is a metadata-only request, `get_range` takes a `Range<u64>`,
`list` walks a prefix with a cursor:

```rust
let mut cursor = None;
loop {
    let page = storage.list("avatars/", cursor.as_deref()).await?;
    for object in &page.objects {
        tracing::info!(key = %object.key.as_str(), size = object.size);
    }
    match page.cursor {
        Some(next) => cursor = Some(next),
        None => break,
    }
}
```

`Listing` also carries `prefixes`, the common prefixes a delimited listing found, which is how you
walk a hierarchy one level at a time.

## Accepting an upload

`Upload<K>` is a body extractor over `multipart/form-data`. `K` is a marker type describing what
kind of file this endpoint takes.

```rust title="src/uploads.rs"
use moso_storage::AttachmentKind;

/// A product photograph: images only, and small.
pub struct Image;

impl AttachmentKind for Image {
    const NAME: &'static str = "Image";
    const ACCEPT: &'static [&'static str] = &["image/*"];
    const MAX_SIZE: u64 = 4 * 1024 * 1024;
}
```

`ACCEPT` patterns are either an exact media type (`"application/pdf"`) or a family wildcard ending
in `/*` (`"image/*"`). Parameters after a `;` are ignored and matching is case-insensitive.
`"*/*"` is **not** a pattern and matches nothing, so a kind that accepts everything has to name
every type it takes.

`STRIP_EXIF` defaults to `true` and `VARIANTS` defaults to empty.

The handler declares the upload as its body:

```rust title="src/routes/photos.rs"
use moso::prelude::*;
use moso::response::Created;
use moso_storage::{PutOpts, Storage, StorageKey, Upload};

use crate::uploads::Image;

/// What the client gets back.
#[derive(Schema, serde::Serialize)]
pub struct Stored {
    /// Where the object landed.
    pub key: String,
    /// How many bytes arrived.
    pub size: u64,
}

/// Store a product photograph.
#[endpoint]
async fn upload(
    Inject(storage): Inject<dyn Storage>,
    file: Upload<Image>,
) -> Result<Created<Json<Stored>>> {
    let name = format!("original.{}", file.extension());
    let key = StorageKey::from_segments(["photos", "prd_1", &name])?;
    let content_type = file.content_type();
    let meta = storage
        .put(&key, file.into_stream(), PutOpts::new(content_type).trust_content_type())
        .await?;
    Ok(Created::at(
        format!("/photos/{}", meta.key.as_str()),
        Json(Stored { key: meta.key.as_str().to_owned(), size: meta.size }),
    ))
}
```

`trust_content_type()` is correct here and only here: the extractor already decided the media type
from the bytes, so re-sniffing the same stream would be work with no new information.

### What the extractor does before your code runs

1. Parses the `multipart/form-data` body and finds the field named `file`. That name is fixed
   (`moso_storage::upload::FIELD`) and is not configurable. A missing field is a 422 that names it.
2. Sanitises the filename with `sanitise_filename`.
3. Buffers at most 512 bytes (`upload::SNIFF_BYTES`) and calls `sniff` on them.
4. Checks the sniffed type against `K::ACCEPT`. A mismatch is a 422 with field pointer `/file`
   naming what the bytes actually are.
5. For an SVG, checks `upload::svg_is_inert` and refuses one that is not.
6. Streams the rest through a two-slot channel, so a slow storage backend applies backpressure to
   the socket rather than filling memory.

`Upload::extension()` comes from the sniffed type, never from the filename. `content_type()` is the
sniffed type too. `declared_size()` is what the client claimed, which is a hint.

### The bytes decide, not the name

The client's `Content-Type` and the filename's extension are hints. A `.png` that is really an HTML
document is stored XSS on any origin that serves user content, and validating the declared type does
nothing about it.

```rust
// A 64-bit Mach-O header, followed by enough bytes to look like a file.
let mut executable = vec![
    0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00,
    0x00,
];
executable.extend_from_slice(&[0x00; 1024]);

let error = extract(upload_request("logo.png", "image/png", &executable))
    .await
    .expect_err("the bytes are an executable, whatever the request claims");

assert_eq!(error.status(), http::StatusCode::UNPROCESSABLE_ENTITY);
let detail = error.detail().unwrap_or_default().to_owned();
assert!(detail.contains("x-mach-binary"));
```

`sniff` recognises images (PNG, JPEG, GIF, BMP, ICO, TIFF, AVIF, HEIC, HEIF), documents (PDF, RTF),
archives (gzip, bzip2, xz, 7z, RAR, tar, ZIP), audio and video (Ogg, FLAC, MP3, QuickTime, MP4, M4A,
WebM) and, on purpose, executables: ELF, PE, Mach-O, shell scripts, JVM class files and Android dex.
Text formats are sniffed the way a browser does, from the first markup: `application/xml`,
`text/html`, `image/svg+xml`, `application/json`, then `text/plain` as a last resort.

Two behaviours worth knowing. ZIP is always reported as `application/zip` and never guessed further,
because `docx`, `xlsx`, `odt`, `jar` and `apk` are all ZIP containers. And `sniff` on bytes it does
not recognise returns `None`, which `Upload<K>` treats as `application/octet-stream`, which most
`ACCEPT` lists reject. That surprises people uploading an exotic format and it is the intended
behaviour.

### SVG and EXIF

`svg_is_inert` refuses an SVG containing `<script`, `<foreignobject`, `<iframe`, `<embed`,
`<object`, `<use`, `<animate`, `<set`, `<handler`, `<audio`, `<video`, a `javascript:` or
`data:text/html` URL, a remote `xlink:href`, a `<!entity`, a `<!doctype`, or any `on*` event
handler attribute. Rewriting an SVG safely is a losing game against parser differentials, so the
crate reports whether the document is inert and refuses the ones that are not.

`strip_metadata` removes EXIF, XMP and IPTC from JPEG (dropping `APP1` through `APP15` and `COM`,
keeping JFIF and the scan data) and the textual and EXIF chunks from PNG (`eXIf`, `tEXt`, `iTXt`,
`zTXt`, `tIME`). Any other format comes back unchanged. It runs when `K::STRIP_EXIF` is true and you
call `into_sanitised_bytes()`.

That is the trade-off between the two consumption methods:

| Method | Memory | EXIF stripped |
| --- | --- | --- |
| `into_stream()` | constant, still enforces `K::MAX_SIZE` | no |
| `into_bytes()` | bounded by `K::MAX_SIZE` | no |
| `into_sanitised_bytes()` | bounded by `K::MAX_SIZE` | yes, when `K::STRIP_EXIF` |

Stream when the file is large and the format is not one that carries location data. Buffer when it
is a user photograph.

### Streaming large bodies

`into_stream()` keeps the whole path constant-memory, and `K::MAX_SIZE` is still enforced at the
first offending byte rather than at the end of the transfer:

```rust
let upload = Upload::<Bulk>::validated(
    "one-gibibyte.bin",
    "application/octet-stream",
    bytes::Bytes::new(),
    synthetic_gib(ONE_GIB),
    Some(ONE_GIB),
);

let meta = storage
    .put(
        &key,
        upload.into_stream(),
        PutOpts::new("application/octet-stream").trust_content_type(),
    )
    .await
    .expect("the write succeeds");
```

The acceptance test around that snippet measures peak RSS. It reports `1 GiB streamed; peak RSS grew
1.4 MiB against a 20 MiB budget`.

The cloud backends are the documented exception. SigV4 signs a hash of the payload, so a single
`PutObject` has to know all of its bytes. A single `put` accepts up to 64 MiB on S3, 64 MiB on GCS
and 256 MiB on Azure. Anything larger on S3 goes through `multipart_start`. On GCS and Azure there
is no larger path in this codebase.

## Presigned direct uploads

For files that should never traverse your process, hand the client a presigned target and confirm
afterwards. S3, Azure, and GCS-with-a-service-account-key support this.

```rust
use std::time::Duration;
use moso_storage::{Storage, StorageKey, UploadPolicy, Visibility};

let policy = UploadPolicy::new(1..=8 * 1024 * 1024, Duration::from_secs(600))
    .accept(["image/png", "image/jpeg"])
    .visibility(Visibility::Private)
    .metadata("uploaded-by", user_id);

let post = storage.presigned_upload(&key, policy).await?;
```

Both bounds of the size range and the TTL are mandatory arguments to `UploadPolicy::new`, because a
presigned URL with no size cap is an open bucket with extra steps. `PresignedPost` carries the
`url`, the `method`, the `fields` to send, the `key`, `expires_at` and `max_size`, and it is
`Serialize`, so it goes straight into a JSON response. All three backends produce a presigned `PUT`,
so `method` is always `"PUT"`; `fields` is what the client **must** send as headers:

| Backend | `url` | `fields` |
| --- | --- | --- |
| `s3` | a SigV4 query-signed `PUT` | `x-amz-meta-*`, plus `x-amz-acl: public-read` when the policy asked for a public object |
| `gcs` | a V4 signed `PUT` on `storage.googleapis.com` | `x-goog-meta-*`, plus `x-goog-acl: public-read`. **These are covered by the signature**, so a client that omits one gets a 403 |
| `azure` | a blob URL with a service SAS (`sp=cw`) | `x-ms-blob-type: BlockBlob`, which is not optional, plus `x-ms-meta-*` |

The client calls you back, and the callback re-checks the object against the same policy:

```rust
let storage = moso_storage::backend::MemoryStorage::new();
let key = StorageKey::new("uploads/direct/original.png").expect("a valid key");
let policy = moso_storage::UploadPolicy::new(1..=4096, Duration::from_secs(600))
    .accept(["image/png"])
    .metadata("uploaded-by", "usr_1");

let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
png.extend_from_slice(&[0x33; 64]);
storage
    .put(
        &key,
        moso_storage::stream_from_bytes(bytes::Bytes::from(png.clone())),
        PutOpts::new("application/octet-stream"),
    )
    .await
    .expect("the direct upload lands");

let confirmation = moso_storage::confirm_upload(&storage, &key, &policy)
    .await
    .expect("the object conforms");
assert!(confirmation.accepted);
assert_eq!(confirmation.meta.size, png.len() as u64);
assert_eq!(confirmation.meta.content_type, "image/png");
```

`confirm_upload` is not a formality. It reads the object's real metadata with `head`, and when the
object is over `max_size`, under `min_size`, or (if the policy names accepted types) fails the same
`sniff` and `svg_is_inert` checks the extractor would have applied, it **deletes the object** before
returning the error. Nothing that failed the policy stays in the bucket.

Do not record the attachment in your database until `confirm_upload` returns `accepted`.

## Multipart uploads

For very large objects on S3.

```rust
use moso_storage::{PartNumber, PutOpts, Storage};

let upload = storage.multipart_start(&key, PutOpts::new("video/mp4")).await?;
let mut parts = Vec::new();
for (index, chunk) in chunks.into_iter().enumerate() {
    let number = PartNumber::new(index as u32 + 1)?;
    parts.push(upload.upload_part(number, chunk).await?);
}
let meta = upload.complete(parts).await?;
```

`PartNumber` rejects 0 and anything above 10 000. The minimum part size is checked locally before
completion, so an undersized part fails before the provider bills for the attempt, and parts are
sorted on completion so a concurrent upload does not produce a corrupt object. `complete` and
`abort` both consume the handle.

> [!IMPORTANT]
> `MultipartUpload` warns on drop when neither `complete` nor `abort` ran, naming the backend, the
> key and the upload id. Uncompleted parts are storage you pay for and never see. The warning
> carries exactly what an `abort-multipart-upload` call needs.

## Serving files back

`storage.serve(&key)` produces a `ServedObject` that implements `IntoResponse` and `Describe`, so a
handler returning it documents 200, 206, 304, 404 and 416 without you writing anything.

```rust
use moso_storage::{ServedObject, Storage, StorageKey};

async fn download(
    storage: &dyn Storage,
    key: &StorageKey,
    headers: &http::HeaderMap,
) -> moso_storage::Result<ServedObject> {
    Ok(storage.serve(key).await?.filename("report.pdf").evaluate(headers))
}
```

The free function `moso_storage::serve(storage, key)` is the same operation and delegates to the
method, so the two cannot drift. Use whichever reads better where you are.

`evaluate(headers)` applies the conditional and range headers. The order matters and is fixed:
`If-None-Match` wins over `If-Modified-Since` when both are present, entity tags compare weakly
(`W/"abc"` equals `"abc"`), an `If-Range` that does not match falls back to the whole object, and
`Range` is parsed last. A multi-range request is answered with the whole object rather than
`multipart/byteranges`.

Every response carries, always:

| Header | Value |
| --- | --- |
| `Content-Type` | from the stored metadata, not from the key's extension |
| `X-Content-Type-Options` | `nosniff` |
| `Content-Security-Policy` | `sandbox` |
| `X-Frame-Options` | `DENY` |
| `Accept-Ranges` | `bytes` |
| `Content-Disposition` | RFC 6266, with both an ASCII `filename=` and a `filename*=UTF-8''` |

Plus `ETag`, `Last-Modified`, `Cache-Control`, `Content-Length` and `Content-Range` where they
apply. `.inline()` switches the disposition from `attachment` to `inline`; the default is
`Download`, because serving user content inline on your own origin is how a stored XSS becomes a
session theft. The sandbox CSP is there for the times you serve it inline anyway.

`from_parts(key, meta, body)` builds a `ServedObject` when you already have the metadata and a body,
which avoids a second `head`. `map_body(|body| …)` replaces the stream while keeping everything
`evaluate` decided: the hatch a metering or checksumming layer uses, and the one `TimedStorage`
uses to attach the stall deadline.

### Serving the local backend in development

`LocalStorage` can serve its own directory over a signed, expiring route, but only if you gave it a
signing key.

```rust title="src/boot.rs"
use std::sync::Arc;
use moso_core::config::SecretBytes;
use moso_storage::backend::LocalStorage;

let storage = Arc::new(
    LocalStorage::new("var/uploads").served_at("/_storage", SecretBytes::new(signing_key)),
);
let router = app_routes().merge(LocalStorage::routes(Arc::clone(&storage)));
```

`LocalStorage::routes` returns an **empty** router when `served_at` was never called, because an
unauthenticated route over a directory is a file server nobody asked for. The signature is
HMAC-SHA256 over `"{key}\n{expiry}"`, checked in constant time before the file is opened. The routes
go through `Router::mount_axum`, so they are absent from the
[OpenAPI document](./openapi.md). And `LocalStorage::signed_url` produces
`http://localhost{base}/...`, because the backend does not know the origin it is served from. It is
a development URL and nothing more.

Mounting it needs the concrete `Arc<LocalStorage>`, so build the backend by hand when you want the
route. `StorageConfig::build` returns `Arc<dyn Storage>`, which cannot be downcast back.

## Attaching a file to a record

`Attachment<K>` is a plain JSON descriptor: the key, filename, content type, size, checksum, attach
time and the state of each declared variant. It needs no extra table and it does not depend on the
ORM.

```rust title="src/uploads.rs"
use moso_storage::{AttachmentKind, Fit, VariantSpec, VariantTransform};

pub struct Photo;

impl AttachmentKind for Photo {
    const NAME: &'static str = "Photo";
    const ACCEPT: &'static [&'static str] = &["image/jpeg", "image/png", "image/webp"];
    const MAX_SIZE: u64 = 8 * 1024 * 1024;
    const VARIANTS: &'static [VariantSpec] = &[
        VariantSpec::new(
            "thumb",
            VariantTransform::Resize { width: 200, height: 200, fit: Fit::Cover },
        ),
    ];
}
```

`attach` stores the bytes and returns the descriptor:

```rust
let attachment = Attachment::<Photo>::attach(file, storage.as_ref(), "products/prd_1").await?;
```

It derives the key itself: the prefix split on `/`, plus a final segment `original.{extension}`
where the extension comes from the sniffed type. Every declared variant starts `Pending`.

Persist the descriptor in a JSON column on the entity that owns it:

```rust title="src/entities/product.rs"
use moso::db::prelude::*;
use moso_orm::Json;
use moso_storage::Attachment;

use crate::uploads::Photo;

/// Something for sale.
#[derive(Entity, Debug, Clone)]
#[entity(table = "products")]
pub struct Product {
    /// The primary key.
    #[entity(pk)]
    pub id: Id<Product>,
    /// What it is called.
    pub name: String,
    /// The product photograph, or nothing yet.
    pub image: Option<Json<Attachment<Photo>>>,
}
```

`Json<T>` stores the descriptor in a `jsonb` column and is the honest version of a hidden JSON
field: the wrapper is visible in the entity, so nobody has to remember which columns are secretly
serialised.

`Attachment::attach` never touches a database. Writing the descriptor is your code, in the same
transaction as whatever else the request changed.

| Method | What it does |
| --- | --- |
| `key()` / `key_for(&variant)` | The original key; the variant's key, falling back to the original |
| `variant_key(&variant, extension)` | The key a variant should be written to |
| `read_original(storage)` | The original's bytes, bounded by `K::MAX_SIZE` |
| `store_variant(storage, &variant, rendition)` | Write an encoded variant and record it ready |
| `mark_ready(&variant, key, size, content_type)` | Record a rendition you wrote yourself |
| `mark_failed(&variant, reason)` | Record one that could not be produced |
| `variants()` / `ready_variants()` / `is_complete()` | What exists so far |
| `url(storage, &variant, ttl)` | A signed URL, on a backend that can sign |
| `purge(storage)` | Delete the original and every ready variant |

`url` takes the store and the TTL as arguments because the descriptor is a **column value**: a
storage handle inside it could not be serialised into `jsonb`, and a row written by a process
talking to S3 would drag that backend into a process configured for a local directory. A TTL inside
it would be worse: a signature minted when the row was *written* would have expired long before
anything read it. The descriptor is data, the store is configuration, and the call site has both.

`Variant::ORIGINAL` names the stored bytes. `Variant::new("thumb")` is a `const` constructor for a
static name and `Variant::dynamic(name)` takes an owned one.

### Rendering a variant

**No image codec is a dependency of this crate, and none is planned.** Encoders are large, they
carry CVEs, and an application that wants AVIF and one that wants a 200 KiB binary should not be
forced onto the same one. Supplying the codec is your job. What the crate owns is everything around
it, and it composes into a [background job](./jobs.md) that is about fifteen lines:

```rust title="src/jobs/render_variants.rs"
use bytes::Bytes;
use moso_storage::{Attachment, AttachmentKind, Rendition, Storage, Variant, VariantTransform};

use crate::uploads::Photo;

/// The one part the framework does not supply. Yours calls an image library
/// through `moso::task::blocking()`, because encoding is CPU-bound and the
/// runtime is not yours to block.
fn encode(transform: VariantTransform, original: Bytes) -> Result<Rendition, String> {
    match transform {
        VariantTransform::Resize { width, height, .. } => {
            let bytes = resize_somehow(original, width, height)?;
            Ok(Rendition::new(bytes, "webp", "image/webp"))
        }
        other => Err(format!("this encoder does not do {other:?}")),
    }
}

pub async fn render(
    attachment: &mut Attachment<Photo>,
    storage: &dyn Storage,
) -> moso_storage::Result<()> {
    let original = attachment.read_original(storage).await?;
    for spec in Photo::VARIANTS {
        let variant = Variant::dynamic(spec.name());
        match encode(spec.transform(), original.clone()) {
            Ok(rendition) => attachment.store_variant(storage, &variant, rendition).await?,
            // Recorded, not retried: an image the encoder cannot read will not
            // become readable on the fifth attempt.
            Err(reason) => attachment.mark_failed(&variant, reason),
        }
    }
    Ok(())
}
```

Then save the descriptor back to its column. The crate's rustdoc carries this exact loop as a
**tested** example with a byte-identity `encode`, so the wiring is proved end to end without a codec.

Two failures, on purpose. An encoder that refuses is `mark_failed` and `Ok(())`: the job is done,
and nothing retries it. A `store_variant` that fails is the *store* failing and returns `Err`, so
the job retries. Until either runs, `is_complete()` is false and `key_for(&variant)` returns the
original, so a template asking for a thumbnail gets the full image rather than a broken one.

`read_original` is bounded by `K::MAX_SIZE`, which is the same number `Upload<K>` enforced on the
way in. Nothing can be at that key that was larger, so no second limit has to be invented.
`Rendition::new(bytes, extension, content_type)` is the handover: only the encoder knows whether a
resize also changed the format, so it says.

## Configuration and boot

`StorageConfig` is a plain struct with a builder. This crate reads no environment variables; mapping
your configuration onto these fields is your application's code. See
[configuration](./configuration.md).

| Field | Default | Effect |
| --- | --- | --- |
| `backend` | `Local` | Which backend to build |
| `bucket` | `None` | The bucket or container. Required for S3, GCS and Azure |
| `region` | `None` | S3 region, defaulting to `us-east-1` |
| `access_key` / `secret_key` | `None` | Credentials, the secret as a `SecretString`. On GCS `secret_key` is the whole service-account JSON, the literal `metadata`, or an access token; on Azure `access_key` is the account name |
| `endpoint` | `None` | A compatible gateway, which also switches S3 to path addressing |
| `prefix` | `None` | A key prefix applied to every operation |
| `root` | `var/uploads` | The local directory |
| `public_base` | `None` | Set through `served_at` |
| `url_ttl` | 300s | How long a signed URL lasts |
| `timeout` | 30s | How long a call that **answers once** may take |
| `stall_timeout` | 30s | How long a transfer may move **no bytes** |

`StorageBackendKind::parse` accepts the vendor aliases: `r2`, `minio`, `b2`, `wasabi` and `tigris`
all parse to `S3` (as do `backblaze`, `google` and `blob`, which are not in
`StorageBackendKind::NAMES`). The error text of `validate` names the keys an application reads:
`STORAGE_BACKEND`, `STORAGE_BUCKET`, `STORAGE_ACCESS_KEY`, `STORAGE_SECRET_KEY`, `STORAGE_ROOT`,
`STORAGE_URL_TTL`, `STORAGE_TIMEOUT` and `STORAGE_STALL_TIMEOUT`.

```rust title="src/boot.rs"
use std::sync::Arc;
use moso_storage::{Storage, StorageBackendKind, StorageConfig};

fn storage() -> Result<Arc<dyn Storage>, Box<dyn std::error::Error>> {
    let config = StorageConfig::new(StorageBackendKind::Memory);
    config.validate()?;
    for warning in config.warnings(false) {
        tracing::warn!("{warning}");
    }
    Ok(config.build()?)
}
```

Register the health check yourself, because the batteries do not register their own:

```rust
let storage = storage()?;
let app = App::new(config)
    .health_check("storage", StorageConfig::health_check(Arc::clone(&storage)))
    .provide_dyn::<dyn Storage>(storage);
```

`StorageHealthCheck` is non-critical by default, so a briefly unreachable bucket degrades the
instance rather than removing it from rotation. `.critical(true)` changes that. See
[health and shutdown](./health-and-shutdown.md).

### The two deadlines

`StorageConfig::build` returns the backend wrapped in `TimedStorage`, so both deadlines are enforced
without you doing anything. Which one applies is decided by the **shape** of the call, not its name:

| Shape | Bound by | Restarts on progress | Operations |
| --- | --- | --- | --- |
| answers once | `timeout` | no | `head`, `delete`, `delete_many`, `list`, `copy`, `serve`, `signed_url`, `presigned_upload`, `multipart_start`, `probe`, and each multipart part |
| moves bytes | `stall_timeout` | **yes** | `put`, `get`, `get_range` |

A single number gets both wrong: 30 seconds is far too long for a `head` that should answer in
20 ms, and far too short for a gibibyte moving steadily at 40 MB/s. So a transfer is bounded by how
long it may move *nothing*, and the clock restarts on every chunk. A 1 GiB download that is making
progress runs as long as it needs; one whose socket goes quiet is abandoned after `stall_timeout`.

For `put` the watchdog wraps the whole operation and is reset by the body stream, because the stall
can be at either end (a client that stopped sending, or a backend that stopped reading), and both
look the same from here. For `get` the *setup* call is bounded by `timeout` and the body it hands
back is bounded by `stall_timeout`, per chunk. Nothing is buffered to do it.

The two failures are separate values:

| Variant | Means | Status |
| --- | --- | --- |
| `Error::Timeout` | the store never answered | 504, retryable |
| `Error::Stalled` | the store answered and then went quiet halfway through | 504, retryable |

A 504 and not a 503, deliberately: the store being slow says nothing about whether *this* instance is
healthy, and a 503 would take it out of rotation over one slow bucket.

`Deadlines::NONE` is what a backend built by hand enforces: nothing. Wrap it yourself to change
that:

```rust title="src/boot.rs"
use std::sync::Arc;
use moso_storage::{Deadlines, Storage, TimedStorage, backend::LocalStorage};

// `LocalStorage::routes` needs the concrete backend, so build it by hand and
// apply the same policy `build()` would have. `Arc<S>` is itself a `Storage`,
// so the route and the wrapper share one backend rather than two.
let backend = Arc::new(LocalStorage::new("var/uploads"));
let router = app_routes().merge(LocalStorage::routes(Arc::clone(&backend)));
let storage: Arc<dyn Storage> =
    Arc::new(TimedStorage::new(Arc::clone(&backend), config.deadlines()));
```

`TimedStorage` is transparent: `name()` and `capabilities()` are still the real backend's, so a log
line still says `s3` and a capability check still answers for the real store. `inner()` and
`into_inner()` get it back.

### Backend-specific traps at boot

`GcsStorage::new` takes one of three things, and which one decides what the backend can do. A
service-account JSON key, verbatim, mints its own access tokens (RS256, `ring`) and signs URLs. The
literal string `metadata` is workload identity: the metadata server issues a token per request and
no private key ever exists in the process, so nothing can be signed. Anything else is treated as an
access token and used verbatim, likewise unable to sign. A malformed JSON key fails **at
construction**, naming what is missing, rather than 401-ing at the first upload.

`AzureStorage` signs its SAS with the same account key the requests use, so a key that is not base64
fails with an error pointing at the account's `Access keys` blade rather than a 403 at the browser.

And `MemoryStorage` has a 256 MiB total cap, adjustable with `max_total_bytes`, so a runaway test
fails loudly instead of exhausting the machine.

## Failure modes

`moso_storage::Error` converts into the framework's [error model](./errors.md).

| Variant | Status | Retryable | When |
| --- | --- | --- | --- |
| `NotFound` | 404 | no | The key is not there |
| `ContentType` | 422, pointer `/file` | no | The sniffed type is not in `K::ACCEPT`, or an SVG is not inert |
| `Key` | 422, pointer `/file` | no | The key failed validation, including the local backend's second path check |
| `TooLarge` | 413, pointer `/file` | no | The body exceeded `K::MAX_SIZE` or the policy's `max_size` |
| `Checksum` | 500 | no | `expect_checksum` did not match what streamed through |
| `Unavailable` | 503 | **yes** | The backend was unreachable or returned a 5xx |
| `Timeout` | 504 | **yes** | A call that answers once ran past `timeout` |
| `Stalled` | 504 | **yes** | A transfer moved no bytes for `stall_timeout` |
| `Refused` | 500 | no | The backend refused permanently |
| `Unsupported` | 500 | no | The backend does not implement the operation |
| `Config` | 500 | no | A configuration contradiction |

`error.retryable()` is true for the three transient variants: `Unavailable`, `Timeout` and
`Stalled`. `error.is_not_found()` is the one you usually branch on. `error.backend()` names which
backend produced it, which is what makes a log line useful when two are configured, and `Timeout`
and `Stalled` also name the *operation* so a log line says which call was slow rather than only that
storage was.

Other things that catch people out:

- The multipart field name is `file` and is not configurable.
- `MemoryStorage` reports `signed_urls: false`. There is no URL a browser could follow into this
  process's heap, and pretending otherwise would make tests lie.
- `LocalStorage` re-checks the joined path against the canonicalised root, so a symlink pointing out
  of the root is refused even though `StorageKey` already forbade `..`.
- `local` reports `conditional_writes: false`, so `if_absent()` does nothing useful there. Check
  `capabilities()` before relying on it.

## See also

- [Extractors](./extractors.md) for how `Upload<K>` is classified as the body of a handler.
- [Responses](./responses.md) for what `ServedObject` contributes to a route.
- [Background jobs](./jobs.md) for where variant generation belongs.
- [Security](./security.md) for the wider picture on serving user content.
- [Sending mail](./mail.md), the other battery with the same shape.
