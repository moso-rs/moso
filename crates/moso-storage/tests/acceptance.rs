//! The acceptance criteria of `docs/03-batteries/34-mail-storage-realtime.md`,
//! measured rather than asserted by inspection.
//!
//! > **Storage:** a 1 GiB upload streams with < 20 MiB peak RSS; magic-byte
//! > sniffing rejects a `.png`-named executable; EXIF stripped; presigned
//! > upload round-trips; Range requests correct.
//!
//! Two of these are properties of the *whole path* rather than of any one
//! function, and a unit test cannot see them. They live here, as integration
//! tests over the real extractor and the real filesystem backend.

use std::time::Duration;

use futures_util::StreamExt as _;
use moso_core::extract::ExtractBody as _;
use moso_storage::{AttachmentKind, PutOpts, Storage as _, StorageKey, Upload};

// ---------------------------------------------------------------------------
// peak RSS
// ---------------------------------------------------------------------------

/// A gibibyte, the size the acceptance criterion names.
const ONE_GIB: u64 = 1024 * 1024 * 1024;

/// The peak-RSS budget the acceptance criterion names.
const RSS_BUDGET: u64 = 20 * 1024 * 1024;

/// The chunk a synthetic upload arrives in.
///
/// 256 KiB, which is roughly what a real socket delivers and is small enough
/// that a layer buffering "just one chunk" cannot hide inside the budget.
const CHUNK: usize = 256 * 1024;

/// This process's resident set size, in bytes.
///
/// Read without `unsafe`: `/proc/self/status` on Linux and `ps` on macOS.
/// Returns `None` where neither is available, and the test then reports that it
/// could not measure rather than passing vacuously.
fn resident_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kilobytes: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kilobytes * 1024);
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        let kilobytes: u64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()?;
        Some(kilobytes * 1024)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Samples this process's RSS on a background thread until it is told to stop.
struct RssSampler {
    /// Set to stop the thread.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The highest reading, in bytes.
    peak: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The sampling thread.
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RssSampler {
    /// Start sampling every few milliseconds.
    fn start() -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

        let handle = {
            let stop = std::sync::Arc::clone(&stop);
            let peak = std::sync::Arc::clone(&peak);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Some(rss) = resident_bytes() {
                        peak.fetch_max(rss, std::sync::atomic::Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };

        Self {
            stop,
            peak,
            handle: Some(handle),
        }
    }

    /// Stop sampling and report the highest reading.
    fn finish(mut self) -> u64 {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.peak.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A stream of `total` bytes, produced a chunk at a time and never held.
fn synthetic_gib(total: u64) -> moso_storage::ByteStream {
    Box::pin(futures_util::stream::unfold(
        0_u64,
        move |sent| async move {
            if sent >= total {
                return None;
            }
            let size = CHUNK.min((total - sent) as usize);
            // A fresh allocation each time: reusing one buffer would hide a layer
            // that keeps a reference to what it was handed.
            let chunk = bytes::Bytes::from(vec![0xab_u8; size]);
            Some((Ok(chunk), sent + size as u64))
        },
    ))
}

/// A kind that will take a gibibyte.
struct Bulk;

impl AttachmentKind for Bulk {
    const NAME: &'static str = "Bulk";
    const ACCEPT: &'static [&'static str] = &["application/octet-stream"];
    const MAX_SIZE: u64 = 8 * ONE_GIB;
}

/// A gibibyte crosses `Upload` and `LocalStorage` inside the 20 MiB budget.
///
/// The measurement is of *this process*, sampled on a thread while the transfer
/// runs, and the baseline taken before it starts is subtracted — so what is
/// asserted is the transfer's own growth and not the test harness's footprint.
///
/// The backend is wrapped in `TimedStorage` with a **one-second** whole-operation
/// deadline, which makes this test prove two things at once: that no layer on
/// the put path collects, and that the deadline machinery does not kill a
/// transfer that is making steady progress. A gibibyte takes far longer than a
/// second; only the stall deadline may end it, and nothing here stalls.
///
/// This is the one test in the crate that a refactor can break by accident: any
/// layer that collects, buffers, or clones a chunk into a `Vec` that outlives it
/// shows up here as several hundred megabytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_gibibyte_streams_within_the_peak_rss_budget() {
    let Some(baseline) = resident_bytes() else {
        // Reporting rather than passing vacuously: a platform where the
        // measurement is unavailable has not met the criterion, it has skipped
        // it, and the difference has to be visible.
        eprintln!(
            "SKIPPED: peak RSS cannot be measured on this platform, so the 20 MiB budget was \
             not checked",
        );
        return;
    };

    let root = std::env::temp_dir().join(format!(
        "moso-storage-acceptance-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
    ));
    let storage = moso_storage::TimedStorage::new(
        moso_storage::backend::LocalStorage::new(&root),
        moso_storage::Deadlines::new(Duration::from_secs(1), Duration::from_secs(30)),
    );
    let key = StorageKey::new("bulk/one-gibibyte.bin").expect("a valid key");

    let sampler = RssSampler::start();

    // The whole documented path: a validated `Upload` whose bytes are streamed
    // straight into the backend, with the size cap enforced as they pass.
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
        .expect(
            "the write succeeds, and a one-second whole-operation deadline does not touch a \
                 transfer that is making progress",
        );

    let peak = sampler.finish();
    let growth = peak.saturating_sub(baseline);

    // Clean up before asserting, so a failure does not also leave a gibibyte
    // behind on the machine.
    let _ = tokio::fs::remove_dir_all(&root).await;

    assert_eq!(meta.size, ONE_GIB, "every byte arrived");
    eprintln!(
        "1 GiB streamed; peak RSS grew {:.1} MiB against a {:.0} MiB budget",
        growth as f64 / (1024.0 * 1024.0),
        RSS_BUDGET as f64 / (1024.0 * 1024.0),
    );
    assert!(
        growth < RSS_BUDGET,
        "peak RSS grew by {growth} bytes ({:.1} MiB) while streaming 1 GiB, against a \
         {RSS_BUDGET}-byte budget — some layer on the put path is collecting",
        growth as f64 / (1024.0 * 1024.0),
    );
}

// ---------------------------------------------------------------------------
// magic-byte sniffing, end to end
// ---------------------------------------------------------------------------

/// A product photograph: images only, and small.
struct Image;

impl AttachmentKind for Image {
    const NAME: &'static str = "Image";
    const ACCEPT: &'static [&'static str] = &["image/*"];
    const MAX_SIZE: u64 = 4 * 1024 * 1024;
}

/// The multipart boundary the fixtures use.
const BOUNDARY: &str = "moso-acceptance-boundary";

/// A `multipart/form-data` request carrying one file part.
fn upload_request(filename: &str, declared: &str, body: &[u8]) -> moso_core::Request {
    let mut payload = Vec::new();
    payload.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    payload.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: {declared}\r\n\r\n",
        )
        .as_bytes(),
    );
    payload.extend_from_slice(body);
    payload.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    http::Request::builder()
        .method(http::Method::POST)
        .uri("/upload")
        .header(
            http::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(axum::body::Body::from(payload))
        .expect("a well-formed request")
}

/// Run the real extractor over a request.
async fn extract(request: moso_core::Request) -> moso_core::Result<Upload<Image>> {
    let state = std::sync::Arc::new(moso_core::AppState::for_tests());
    let (mut parts, body) = request.into_parts();
    let ctx = moso_core::ctx::RequestCtx::new(state, &parts);
    parts.extensions.clear();
    Upload::<Image>::extract_body(moso_core::Request::from_parts(parts, body), &ctx).await
}

/// A Mach-O executable named `logo.png` and declared `image/png` is refused.
///
/// The acceptance criterion, run through the extractor an application actually
/// writes rather than through the sniffer directly: the filename lies, the
/// `Content-Type` lies, and the bytes do not.
#[tokio::test]
async fn a_png_named_executable_is_rejected_end_to_end() {
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
    assert!(
        detail.contains("x-mach-binary"),
        "the error should name what the bytes actually are: {detail}",
    );

    // The other two shapes of the same attack.
    for (name, body) in [
        ("logo.png", &b"\x7fELF\x02\x01\x01\x00 and then some"[..]),
        (
            "avatar.png",
            &b"<!DOCTYPE html><script>alert(1)</script>"[..],
        ),
    ] {
        assert!(
            extract(upload_request(name, "image/png", body))
                .await
                .is_err(),
            "`{name}` carrying {:?} must be refused",
            core::str::from_utf8(&body[..8.min(body.len())]),
        );
    }
}

/// A real PNG is accepted, or the check above is a very safe way to reject
/// every upload.
#[tokio::test]
async fn a_real_png_is_accepted_and_arrives_intact() {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(b"\x00\x00\x00\x0dIHDR");
    png.extend_from_slice(&[0x11; 512]);

    let upload = extract(upload_request(
        "holiday snap.png",
        "application/octet-stream",
        &png,
    ))
    .await
    .expect("a PNG is a PNG");

    assert_eq!(upload.content_type(), "image/png");
    assert_eq!(upload.filename(), "holiday snap.png");
    assert_eq!(upload.extension(), "png");

    let bytes = upload.into_bytes().await.expect("collects");
    assert_eq!(bytes.len(), png.len(), "every byte survived the sniffing");
    assert_eq!(bytes.as_ref(), png.as_slice());
}

/// An SVG carrying a script is refused even though it is an image.
#[tokio::test]
async fn a_scriptable_svg_is_refused_even_though_it_is_an_image() {
    let hostile = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#;
    assert!(
        extract(upload_request("drawing.svg", "image/svg+xml", hostile))
            .await
            .is_err(),
    );

    // A plain drawing goes through.
    let plain = br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1"/></svg>"#;
    let upload = extract(upload_request("drawing.svg", "image/svg+xml", plain))
        .await
        .expect("an inert SVG is an image");
    assert_eq!(upload.content_type(), "image/svg+xml");
}

/// A body with no `file` part names the field that is missing rather than
/// failing with something unactionable.
#[tokio::test]
async fn a_body_with_no_file_part_names_the_field() {
    let payload = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nhi\r\n\
         --{BOUNDARY}--\r\n",
    );
    let request = http::Request::builder()
        .method(http::Method::POST)
        .uri("/upload")
        .header(
            http::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(axum::body::Body::from(payload))
        .expect("a well-formed request");

    let error = extract(request).await.expect_err("no file part");
    assert_eq!(error.status(), http::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(error.detail().unwrap_or_default().contains("`file`"));
}

/// The cap is enforced against the bytes that arrive, not the ones the client
/// claimed, and it fires before the whole body has been read.
#[tokio::test]
async fn an_oversized_upload_is_refused_while_streaming() {
    struct Tiny;
    impl AttachmentKind for Tiny {
        const NAME: &'static str = "Tiny";
        const ACCEPT: &'static [&'static str] = &["image/*"];
        const MAX_SIZE: u64 = 64;
    }

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0x22; 4096]);

    let state = std::sync::Arc::new(moso_core::AppState::for_tests());
    let request = upload_request("big.png", "image/png", &png);
    let (mut parts, body) = request.into_parts();
    let ctx = moso_core::ctx::RequestCtx::new(state, &parts);
    parts.extensions.clear();

    let error = Upload::<Tiny>::extract_body(moso_core::Request::from_parts(parts, body), &ctx)
        .await
        .expect_err("past the limit");
    assert_eq!(error.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
}

// ---------------------------------------------------------------------------
// EXIF, presigning and ranges, over the real backend
// ---------------------------------------------------------------------------

/// A JPEG carrying a GPS-bearing EXIF block.
fn jpeg_with_exif() -> Vec<u8> {
    let mut jpeg = vec![0xff_u8, 0xd8];
    let exif = b"Exif\x00\x00GPSLatitude 51.5074 GPSLongitude -0.1278";
    jpeg.extend_from_slice(&[0xff, 0xe1]);
    jpeg.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    jpeg.extend_from_slice(exif);
    // A JFIF header, which must survive.
    jpeg.extend_from_slice(&[0xff, 0xe0, 0x00, 0x04, 0x00, 0x00]);
    jpeg.extend_from_slice(&[0xff, 0xd9]);
    jpeg
}

/// A photograph's coordinates do not reach the bucket.
#[tokio::test]
async fn exif_is_stripped_before_the_bytes_are_stored() {
    let jpeg = jpeg_with_exif();
    let upload = extract(upload_request("photo.jpg", "image/jpeg", &jpeg))
        .await
        .expect("a JPEG is an image");
    assert_eq!(upload.content_type(), "image/jpeg");

    let stored = upload
        .into_sanitised_bytes()
        .await
        .expect("collects and strips");

    assert!(
        !stored.windows(3).any(|window| window == b"GPS"),
        "the coordinates must not survive",
    );
    assert!(
        stored.windows(2).any(|window| window == [0xff, 0xe0]),
        "the JFIF header must survive, or some decoders reject the image",
    );
    assert!(stored.starts_with(&[0xff, 0xd8]) && stored.ends_with(&[0xff, 0xd9]));
}

/// A presigned upload round-trips: the policy binds the object, the client
/// writes it, and the callback confirms it against the backend rather than
/// against what the client said.
#[tokio::test]
async fn a_direct_upload_round_trips_through_its_policy() {
    let storage = moso_storage::backend::MemoryStorage::new();
    let key = StorageKey::new("uploads/direct/original.png").expect("a valid key");
    let policy = moso_storage::UploadPolicy::new(1..=4096, Duration::from_secs(600))
        .accept(["image/png"])
        .metadata("uploaded-by", "usr_1");

    // The client uploads straight to the backend; the application never sees
    // the bytes, which is the whole point.
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

    // And an object that does not conform is refused *and removed*, so a client
    // cannot leave a rejected file in the bucket.
    let hostile = StorageKey::new("uploads/direct/hostile.png").expect("a valid key");
    storage
        .put(
            &hostile,
            moso_storage::stream_from_bytes(bytes::Bytes::from_static(b"\x7fELF\x02\x01\x01\x00")),
            PutOpts::new("application/octet-stream"),
        )
        .await
        .expect("the direct upload lands");

    assert!(
        moso_storage::confirm_upload(&storage, &hostile, &policy)
            .await
            .is_err()
    );
    assert_eq!(
        storage.keys(),
        vec!["uploads/direct/original.png".to_owned()],
        "the rejected object must not survive",
    );
}

/// Range requests are correct over the real filesystem backend, at both ends
/// and in the suffix form a resumed download uses.
#[tokio::test]
async fn range_requests_are_correct_over_the_local_backend() {
    let root = std::env::temp_dir().join(format!(
        "moso-storage-range-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
    ));
    let storage = moso_storage::backend::LocalStorage::new(&root);
    let key = StorageKey::new("ranged/data.bin").expect("a valid key");

    // 300 KiB, so the read crosses the backend's 64 KiB chunk boundary several
    // times — which is where an off-by-one in the remaining count would show.
    let payload: Vec<u8> = (0..300 * 1024).map(|index| (index % 251) as u8).collect();
    storage
        .put(
            &key,
            moso_storage::stream_from_bytes(bytes::Bytes::from(payload.clone())),
            PutOpts::new("application/octet-stream").trust_content_type(),
        )
        .await
        .expect("stores");

    for (start, end) in [
        (0_u64, 1_u64),
        (0, 65_536),
        (65_535, 65_537),
        (100_000, 200_000),
        (payload.len() as u64 - 1, payload.len() as u64),
    ] {
        let mut collected = Vec::new();
        let mut stream = storage
            .get_range(&key, start..end)
            .await
            .expect("the range read succeeds");
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.expect("no read error"));
        }
        assert_eq!(
            collected,
            &payload[start as usize..end as usize],
            "bytes {start}..{end} came back wrong",
        );
    }

    let _ = tokio::fs::remove_dir_all(&root).await;
}
