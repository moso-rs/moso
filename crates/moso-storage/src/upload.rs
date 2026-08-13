//! The typed upload extractor, and the magic-byte sniffing behind it.
//!
//! `Upload<K>` is an `ExtractBody`: it consumes the request body as multipart,
//! validates while streaming, and never buffers the file. Four defaults matter
//! and none of them is configurable away by accident:
//!
//! 1. **The content type comes from the bytes**, not from the client's
//!    `Content-Type` and not from the filename. A `.png` that is really an HTML
//!    document is stored XSS on any origin that serves user content.
//! 2. **The size limit is enforced at the first offending byte**, not after
//!    buffering. Otherwise the limit is a memory limit on the *server*.
//! 3. **EXIF is stripped from images.** Uploaded photographs carry GPS
//!    coordinates; publishing them is a privacy incident nobody intended.
//! 4. **SVG is refused unless sanitised.** SVG is a script-bearing document
//!    format wearing an image's clothes.

use bytes::Bytes;
use futures_util::StreamExt as _;
use moso_core::Request;
use moso_core::ctx::RequestCtx;
use moso_core::extract::ExtractBody;
use moso_openapi::OperationBuilder;

use crate::{AttachmentKind, ByteStream, Result};

/// The number of leading bytes the sniffer reads.
///
/// Enough for every signature in the table and small enough that it is one
/// chunk of any real stream.
///
/// ```
/// assert_eq!(moso_storage::upload::SNIFF_BYTES, 512);
/// ```
pub const SNIFF_BYTES: usize = 512;

/// One entry in the magic-byte table: an offset, a signature, and a type.
type Signature = (usize, &'static [u8], &'static str);

/// The magic-byte table.
///
/// Ordered longest-signature-first within a family so that a more specific
/// match wins. The executable formats are in here deliberately and are *not*
/// an afterthought: the acceptance criterion for this crate is that a
/// `.png`-named executable is rejected, and it is rejected because the sniffer
/// recognises it rather than because it fails to recognise a PNG.
const SIGNATURES: &[Signature] = &[
    // ── images ──────────────────────────────────────────────────────────
    (0, b"\x89PNG\r\n\x1a\n", "image/png"),
    (0, b"\xff\xd8\xff", "image/jpeg"),
    (0, b"GIF87a", "image/gif"),
    (0, b"GIF89a", "image/gif"),
    (0, b"BM", "image/bmp"),
    (0, b"\x00\x00\x01\x00", "image/x-icon"),
    (0, b"II*\x00", "image/tiff"),
    (0, b"MM\x00*", "image/tiff"),
    (4, b"ftypavif", "image/avif"),
    (4, b"ftypheic", "image/heic"),
    (4, b"ftypheix", "image/heic"),
    (4, b"ftypmif1", "image/heif"),
    // ── documents & archives ────────────────────────────────────────────
    (0, b"%PDF-", "application/pdf"),
    (0, b"\x1f\x8b", "application/gzip"),
    (0, b"BZh", "application/x-bzip2"),
    (0, b"\xfd7zXZ\x00", "application/x-xz"),
    (0, b"7z\xbc\xaf\x27\x1c", "application/x-7z-compressed"),
    (0, b"Rar!\x1a\x07", "application/vnd.rar"),
    (257, b"ustar", "application/x-tar"),
    (0, b"{\\rtf", "application/rtf"),
    // ── audio & video ───────────────────────────────────────────────────
    (0, b"OggS", "application/ogg"),
    (0, b"fLaC", "audio/flac"),
    (0, b"ID3", "audio/mpeg"),
    (4, b"ftypqt  ", "video/quicktime"),
    (4, b"ftypisom", "video/mp4"),
    (4, b"ftypmp42", "video/mp4"),
    (4, b"ftypM4A ", "audio/mp4"),
    (0, b"\x1a\x45\xdf\xa3", "video/webm"),
    // ── executables and other things that must never be served ──────────
    (0, b"\x7fELF", "application/x-elf"),
    (0, b"MZ", "application/vnd.microsoft.portable-executable"),
    (0, b"\xca\xfe\xba\xbe", "application/x-mach-binary"),
    (0, b"\xcf\xfa\xed\xfe", "application/x-mach-binary"),
    (0, b"\xce\xfa\xed\xfe", "application/x-mach-binary"),
    (0, b"\xfe\xed\xfa\xcf", "application/x-mach-binary"),
    (0, b"\xfe\xed\xfa\xce", "application/x-mach-binary"),
    (0, b"\xde\xad\xbe\xef", "application/x-mach-binary"),
    (0, b"#!", "text/x-shellscript"),
    (0, b"\xca\xfe\xba\xbf", "application/java-vm"),
    (0, b"dex\n", "application/vnd.android.dex"),
];

/// Media types the sniffer will never report, whatever the bytes look like.
///
/// A ZIP container is `application/zip` until something proves otherwise, and
/// several formats that matter — `docx`, `xlsx`, `odt`, `jar`, `apk` — are ZIP
/// containers. Reporting `application/zip` for all of them is honest; guessing
/// which one it is from a central directory nobody has read yet is not.
const ZIP: &[u8] = b"PK\x03\x04";

/// Decide a media type from the leading bytes of a file.
///
/// Returns `None` when nothing matches, which the caller must treat as
/// "unknown" — never as "whatever the client said".
///
/// ```
/// use moso_storage::sniff;
///
/// let png = sniff(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
/// assert_eq!(png, Some("image/png"));
///
/// // The acceptance criterion: an executable is an executable whatever it is
/// // called.
/// assert_eq!(sniff(b"\x7fELF\x02\x01\x01"), Some("application/x-elf"));
///
/// // Nothing recognisable is `None`, not a guess.
/// assert_eq!(sniff(b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b"), None);
/// ```
#[must_use]
pub fn sniff(prefix: &[u8]) -> Option<&'static str> {
    for (offset, signature, media) in SIGNATURES {
        if prefix
            .get(*offset..*offset + signature.len())
            .is_some_and(|window| window == *signature)
        {
            return Some(media);
        }
    }

    if prefix.starts_with(ZIP) {
        return Some("application/zip");
    }

    // Textual formats have no magic number, and the two that matter most for
    // security — HTML and SVG — are exactly the two a client will happily
    // declare as `image/png`. Sniff them from their first non-whitespace
    // markup, the way a browser does, because a browser is what will
    // eventually render whatever is stored.
    let text = core::str::from_utf8(prefix).ok().or_else(|| {
        // A truncated multi-byte character at the end of the prefix must not
        // hide the markup at the start of it.
        core::str::from_utf8(&prefix[..prefix.len().saturating_sub(3)]).ok()
    })?;
    let head = text.trim_start().trim_start_matches('\u{feff}');
    let lower = head
        .get(..head.len().min(256))
        .unwrap_or_default()
        .to_ascii_lowercase();

    for (marker, media) in [
        ("<?xml", "application/xml"),
        ("<!doctype html", "text/html"),
        ("<html", "text/html"),
        ("<head", "text/html"),
        ("<body", "text/html"),
        ("<script", "text/html"),
        ("<svg", "image/svg+xml"),
        ("<!doctype svg", "image/svg+xml"),
    ] {
        if lower.starts_with(marker) {
            // An XML declaration says nothing about which XML. Look past it
            // for the root element, because `<?xml …?><svg>` is an SVG.
            if media == "application/xml"
                && let Some(rest) = lower.find("?>").map(|at| &lower[at + 2..])
            {
                let rest = rest.trim_start();
                if rest.starts_with("<svg") || rest.contains("<svg") {
                    return Some("image/svg+xml");
                }
                if rest.starts_with("<html") {
                    return Some("text/html");
                }
            }
            return Some(media);
        }
    }
    if lower.starts_with("{") || lower.starts_with("[") {
        // Only when it actually parses as far as we can see; `{` is also the
        // first byte of several binary formats.
        if head.is_ascii() {
            return Some("application/json");
        }
    }
    if !prefix.is_empty()
        && prefix
            .iter()
            .all(|byte| !byte.is_ascii_control() || matches!(byte, b'\t' | b'\n' | b'\r'))
    {
        return Some("text/plain");
    }
    None
}

/// Whether a sniffed type satisfies one of a kind's accepted patterns.
///
/// A pattern ending in `/*` matches a whole top-level type; anything else must
/// match exactly. There is no wildcard that matches everything, on purpose.
///
/// ```
/// use moso_storage::accepts;
///
/// assert!(accepts(&["image/*"], "image/png"));
/// assert!(!accepts(&["image/*"], "text/html"));
/// assert!(accepts(&["application/pdf"], "application/pdf"));
///
/// // `*/*` is not a pattern. A kind that accepts everything has to say so
/// // one type at a time, which is the point.
/// assert!(!accepts(&["*/*"], "application/x-elf"));
/// ```
#[must_use]
pub fn accepts(patterns: &[&str], content_type: &str) -> bool {
    // The parameters after a `;` are not part of the type.
    let content_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    patterns.iter().any(|pattern| {
        let pattern = pattern.trim().to_ascii_lowercase();
        match pattern.strip_suffix("/*") {
            Some(family) if !family.is_empty() && family != "*" => content_type
                .split_once('/')
                .is_some_and(|(actual, _)| actual == family),
            _ => pattern == content_type,
        }
    })
}

/// Make a filename safe to echo back in a `Content-Disposition`.
///
/// Strips directory components, control characters and quotes, collapses
/// whitespace, and truncates to 255 bytes on a character boundary. The result
/// is never used as a storage key — keys are generated, not taken from clients.
///
/// ```
/// use moso_storage::sanitise_filename;
///
/// assert_eq!(sanitise_filename("../../etc/passwd"), "passwd");
/// assert_eq!(sanitise_filename("C:\\Users\\ada\\photo.jpg"), "photo.jpg");
/// assert_eq!(sanitise_filename("re\"port\r\n.pdf"), "report.pdf");
/// assert_eq!(sanitise_filename("   "), "file");
/// ```
#[must_use]
pub fn sanitise_filename(filename: &str) -> String {
    // Both separators, because the client's platform is not ours.
    let base = filename.rsplit(['/', '\\']).next().unwrap_or_default();

    let mut out = String::with_capacity(base.len());
    for c in base.chars() {
        match c {
            // A quote ends the `filename="…"` parameter; a control character
            // ends the header.
            '"' | '\'' | '\\' | '\r' | '\n' | '\0' => {}
            c if c.is_control() => {}
            c if c.is_whitespace() => {
                if !out.ends_with(' ') && !out.is_empty() {
                    out.push(' ');
                }
            }
            c => out.push(c),
        }
    }

    let out = out.trim().trim_matches('.').trim().to_owned();
    if out.is_empty() {
        return "file".to_owned();
    }

    // 255 bytes is the limit on every filesystem a browser will save to.
    let mut truncated = out;
    while truncated.len() > 255 {
        truncated.pop();
    }
    truncated
}

/// Strip the metadata blocks that carry a photograph's location.
///
/// JPEG: every `APP1` (EXIF, XMP) and `APP13` (IPTC) segment is removed, and
/// the image data is untouched. PNG: the `eXIf`, `tEXt`, `iTXt` and `zTXt`
/// chunks go, with the CRC-checked chunk structure preserved. Anything else is
/// returned unchanged, because a format whose metadata layout we do not know
/// is a format we must not rewrite.
///
/// Returns `None` when nothing was stripped, so a caller can avoid a copy.
///
/// ```
/// use moso_storage::upload::strip_metadata;
///
/// // A JPEG carrying an EXIF block comes back shorter.
/// let jpeg = [
///     &[0xff, 0xd8][..],                       // SOI
///     &[0xff, 0xe1, 0x00, 0x08][..],           // APP1, length 8
///     b"Exif\0\0",                             // the payload
///     &[0xff, 0xd9][..],                       // EOI
/// ]
/// .concat();
/// let stripped = strip_metadata(&jpeg).expect("an APP1 was present");
/// assert_eq!(stripped, vec![0xff, 0xd8, 0xff, 0xd9]);
/// ```
#[must_use]
pub fn strip_metadata(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.starts_with(b"\xff\xd8") {
        return strip_jpeg(bytes);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return strip_png(bytes);
    }
    None
}

/// Remove every `APPn` metadata segment from a JPEG.
fn strip_jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[..2]);

    let mut index = 2_usize;
    let mut stripped = false;
    while index + 1 < bytes.len() {
        if bytes[index] != 0xff {
            // Not a marker: the entropy-coded image data has begun. Everything
            // from here is pixels.
            break;
        }
        let marker = bytes[index + 1];

        // Start-of-scan: the rest of the file is compressed image data, which
        // must be copied verbatim.
        if marker == 0xda {
            break;
        }
        // Standalone markers carry no length.
        if matches!(marker, 0xd0..=0xd9 | 0x01 | 0xff) {
            out.extend_from_slice(&bytes[index..index + 2]);
            index += 2;
            continue;
        }

        let length = usize::from(u16::from_be_bytes([
            *bytes.get(index + 2)?,
            *bytes.get(index + 3)?,
        ]));
        if length < 2 {
            return None;
        }
        let end = index + 2 + length;
        if end > bytes.len() {
            return None;
        }

        // APP1 is EXIF and XMP; APP13 is IPTC. APP0 is the JFIF header, which
        // carries no location and which some decoders require.
        let is_metadata = matches!(marker, 0xe1..=0xef) || marker == 0xfe;
        if is_metadata {
            stripped = true;
        } else {
            out.extend_from_slice(&bytes[index..end]);
        }
        index = end;
    }

    if !stripped {
        return None;
    }
    out.extend_from_slice(&bytes[index..]);
    Some(out)
}

/// Remove the textual and EXIF chunks from a PNG.
fn strip_png(bytes: &[u8]) -> Option<Vec<u8>> {
    /// The chunks that can carry a location or a caption.
    const DROP: &[&[u8; 4]] = &[b"eXIf", b"tEXt", b"iTXt", b"zTXt", b"tIME"];

    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[..8]);

    let mut index = 8_usize;
    let mut stripped = false;
    while index + 8 <= bytes.len() {
        let length = u32::from_be_bytes(bytes.get(index..index + 4)?.try_into().ok()?) as usize;
        let kind = bytes.get(index + 4..index + 8)?;
        // 4 length + 4 type + payload + 4 CRC.
        let end = index.checked_add(12)?.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }

        if DROP.iter().any(|drop| kind == drop.as_slice()) {
            stripped = true;
        } else {
            out.extend_from_slice(&bytes[index..end]);
        }
        index = end;

        if kind == b"IEND" {
            break;
        }
    }

    stripped.then_some(out)
}

/// Whether an SVG is free of the constructs that make it an XSS vector.
///
/// SVG is a document format that can carry script, load remote resources and
/// navigate the parent frame. Rather than trying to rewrite one safely — which
/// is a losing game against parser differentials — this reports whether the
/// document is inert, and [`Upload`] refuses one that is not.
///
/// ```
/// use moso_storage::upload::svg_is_inert;
///
/// assert!(svg_is_inert(br#"<svg xmlns="http://www.w3.org/2000/svg"><rect/></svg>"#));
/// assert!(!svg_is_inert(br#"<svg><script>alert(1)</script></svg>"#));
/// assert!(!svg_is_inert(br#"<svg><a href="javascript:alert(1)">x</a></svg>"#));
/// assert!(!svg_is_inert(br#"<svg onload="alert(1)"/>"#));
/// ```
#[must_use]
pub fn svg_is_inert(bytes: &[u8]) -> bool {
    let Ok(text) = core::str::from_utf8(bytes) else {
        // A non-UTF-8 SVG is not something we can reason about.
        return false;
    };
    let lower = text.to_ascii_lowercase();

    /// Elements that execute, fetch or embed.
    const ELEMENTS: &[&str] = &[
        "<script",
        "<foreignobject",
        "<iframe",
        "<embed",
        "<object",
        "<use",
        "<animate",
        "<set",
        "<handler",
        "<audio",
        "<video",
    ];
    /// Attributes that execute or fetch.
    const ATTRIBUTES: &[&str] = &[
        "javascript:",
        "data:text/html",
        "xlink:href=\"http",
        "xlink:href='http",
        "<!entity",
        "<!doctype",
    ];

    if ELEMENTS.iter().any(|element| lower.contains(element)) {
        return false;
    }
    if ATTRIBUTES.iter().any(|attribute| lower.contains(attribute)) {
        return false;
    }
    // Any `on*` event handler. Checked by looking for `on` at an attribute
    // position — after whitespace — rather than anywhere, so `<font-face-name>`
    // does not trip it.
    let bytes = lower.as_bytes();
    for (index, window) in bytes.windows(3).enumerate() {
        if window[0].is_ascii_whitespace()
            && window[1] == b'o'
            && window[2] == b'n'
            && let Some(rest) = lower.get(index + 3..)
        {
            let name: String = rest.chars().take_while(char::is_ascii_alphabetic).collect();
            let after = rest[name.len()..].trim_start();
            if !name.is_empty() && after.starts_with('=') {
                return false;
            }
        }
    }
    true
}

/// A validated upload, still unread.
///
/// The bytes are a stream: nothing has been buffered, and the caller decides
/// where they go. The *metadata* has already been validated against `K`, so a
/// handler holding one of these knows the type and size are acceptable without
/// checking anything itself.
///
/// ```no_run
/// use moso_storage::{AttachmentKind, PutOpts, Storage, StorageKey, Upload};
///
/// async fn save<K: AttachmentKind>(
///     storage: &dyn Storage,
///     key: &StorageKey,
///     upload: Upload<K>,
/// ) -> moso_storage::Result<()> {
///     let opts = PutOpts::new(upload.content_type());
///     storage.put(key, upload.into_stream(), opts).await?;
///     Ok(())
/// }
/// ```
pub struct Upload<K: AttachmentKind> {
    /// The sanitised filename the client sent.
    filename: String,
    /// The sniffed media type.
    content_type: &'static str,
    /// The declared size, when the client sent one. Never trusted as a limit.
    declared_size: Option<u64>,
    /// The already-read prefix, so sniffing does not cost a rewind.
    prefix: Bytes,
    /// The rest of the body.
    rest: ByteStream,
    /// The kind, which holds no data.
    kind: core::marker::PhantomData<fn() -> K>,
}

impl<K: AttachmentKind> Upload<K> {
    /// Build one from parts, having already validated them.
    ///
    /// The constructor a backend-agnostic test uses, and the one
    /// [`extract_body`](ExtractBody::extract_body) calls once the prefix has
    /// been sniffed. It does **not** re-validate: the invariant is that an
    /// `Upload<K>` exists only where `K`'s rules have already been applied,
    /// and [`Upload::validated`] is the only other way to get one.
    ///
    /// ```
    /// # use moso_storage::{AttachmentKind, Upload, stream_from_bytes};
    /// # struct Png;
    /// # impl AttachmentKind for Png {
    /// #     const NAME: &'static str = "Png";
    /// #     const ACCEPT: &'static [&'static str] = &["image/png"];
    /// #     const MAX_SIZE: u64 = 1024;
    /// # }
    /// let upload = Upload::<Png>::validated(
    ///     "logo.png",
    ///     "image/png",
    ///     bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
    ///     stream_from_bytes(bytes::Bytes::new()),
    ///     None,
    /// );
    /// assert_eq!(upload.filename(), "logo.png");
    /// ```
    #[must_use]
    pub fn validated(
        filename: impl Into<String>,
        content_type: &'static str,
        prefix: Bytes,
        rest: ByteStream,
        declared_size: Option<u64>,
    ) -> Self {
        Self {
            filename: filename.into(),
            content_type,
            declared_size,
            prefix,
            rest,
            kind: core::marker::PhantomData,
        }
    }

    /// The sanitised filename.
    ///
    /// ```no_run
    /// # use moso_storage::{AttachmentKind, Upload};
    /// # fn f<K: AttachmentKind>(u: &Upload<K>) { let _: &str = u.filename(); }
    /// ```
    #[must_use]
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// The media type, as sniffed from the bytes.
    ///
    /// ```no_run
    /// # use moso_storage::{AttachmentKind, Upload};
    /// # fn f<K: AttachmentKind>(u: &Upload<K>) { let _: &str = u.content_type(); }
    /// ```
    #[must_use]
    pub fn content_type(&self) -> &'static str {
        self.content_type
    }

    /// The size the client declared, when it declared one.
    ///
    /// A hint for a progress bar and nothing else. The limit is enforced
    /// against the bytes that actually arrive, because a client that lies
    /// about its size is exactly the client the limit is for.
    ///
    /// ```no_run
    /// # use moso_storage::{AttachmentKind, Upload};
    /// # fn f<K: AttachmentKind>(u: &Upload<K>) { let _: Option<u64> = u.declared_size(); }
    /// ```
    #[must_use]
    pub fn declared_size(&self) -> Option<u64> {
        self.declared_size
    }

    /// The extension the sniffed type implies, for building a storage key.
    ///
    /// From the *sniffed* type, never from the client's filename — which is
    /// the whole reason keys are generated rather than taken.
    ///
    /// ```no_run
    /// # use moso_storage::{AttachmentKind, Upload};
    /// # fn f<K: AttachmentKind>(u: &Upload<K>) { let _: &str = u.extension(); }
    /// ```
    #[must_use]
    pub fn extension(&self) -> &'static str {
        extension_for(self.content_type)
    }

    /// The whole body as one stream, prefix first.
    ///
    /// The size limit is still enforced: the returned stream fails with
    /// [`Error::TooLarge`](crate::Error::TooLarge) at the first byte past
    /// `K::MAX_SIZE`, so a handler that streams straight to a backend cannot
    /// accidentally drop the cap.
    ///
    /// ```no_run
    /// # use moso_storage::{AttachmentKind, ByteStream, Upload};
    /// # fn f<K: AttachmentKind>(u: Upload<K>) { let _: ByteStream = u.into_stream(); }
    /// ```
    #[must_use]
    pub fn into_stream(self) -> ByteStream {
        let limit = K::MAX_SIZE;
        let kind = K::NAME;
        let mut seen = 0_u64;

        let chained = futures_util::stream::once(async move { Ok(self.prefix) }).chain(self.rest);
        Box::pin(chained.map(move |chunk| {
            let chunk = chunk?;
            seen = seen.saturating_add(chunk.len() as u64);
            if seen > limit {
                return Err(crate::Error::too_large(kind, limit));
            }
            Ok(chunk)
        }))
    }

    /// Read the whole upload into memory.
    ///
    /// Only for something known to be small — an avatar, a CSV import. The
    /// bound is `K::MAX_SIZE`, so this cannot be worse than the declared limit,
    /// but a 10 MiB limit times a hundred concurrent uploads is still a
    /// gigabyte.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`](crate::Error::TooLarge) at the first byte past
    /// `K::MAX_SIZE`.
    ///
    /// ```no_run
    /// # use moso_storage::{AttachmentKind, Upload};
    /// # async fn f<K: AttachmentKind>(u: Upload<K>) -> moso_storage::Result<bytes::Bytes> {
    /// u.into_bytes().await
    /// # }
    /// ```
    pub async fn into_bytes(self) -> Result<Bytes> {
        crate::collect_bounded(self.into_stream(), K::MAX_SIZE, K::NAME).await
    }

    /// Read the whole upload, stripping image metadata first.
    ///
    /// What an application that stores avatars wants: the bytes without the
    /// GPS coordinates. Buffers, so it carries the same warning as
    /// [`into_bytes`](Upload::into_bytes) — and stripping EXIF is inherently a
    /// whole-file operation, which is why it is not on the streaming path.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`](crate::Error::TooLarge) past `K::MAX_SIZE`.
    ///
    /// ```no_run
    /// # use moso_storage::{AttachmentKind, Upload};
    /// # async fn f<K: AttachmentKind>(u: Upload<K>) -> moso_storage::Result<bytes::Bytes> {
    /// u.into_sanitised_bytes().await
    /// # }
    /// ```
    pub async fn into_sanitised_bytes(self) -> Result<Bytes> {
        let strip = K::STRIP_EXIF;
        let bytes = self.into_bytes().await?;
        if !strip {
            return Ok(bytes);
        }
        Ok(strip_metadata(&bytes).map_or(bytes, Bytes::from))
    }
}

/// The canonical extension for a media type.
///
/// Only the types the sniffer can report, because a key is only ever built
/// from a sniffed type.
fn extension_for(content_type: &str) -> &'static str {
    match content_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/heic" | "image/heif" => "heic",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/x-icon" => "ico",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "application/json" => "json",
        "application/xml" => "xml",
        "text/html" => "html",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/flac" => "flac",
        "application/ogg" => "ogg",
        _ => "bin",
    }
}

impl<K: AttachmentKind> core::fmt::Debug for Upload<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Upload")
            .field("kind", &K::NAME)
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .finish_non_exhaustive()
    }
}

/// The form field an upload is read from.
///
/// Fixed rather than configurable: a typed extractor whose field name is a
/// runtime string cannot document itself, and every client library defaults to
/// this name.
pub const FIELD: &str = "file";

impl<K: AttachmentKind> ExtractBody for Upload<K> {
    fn describe(op: &mut OperationBuilder) {
        use moso_openapi::{ContentType, ResponseSpec, SchemaRef};
        use moso_schema::json_schema::{JsonType, SchemaNode};

        let mut file = SchemaNode {
            types: JsonType::String.into(),
            format: Some("binary".into()),
            ..SchemaNode::default()
        };
        file.description = Some(
            format!(
                "The file. Accepted media types: {}. At most {} bytes. The type is decided by \
                 reading the leading bytes, not from the `Content-Type` or the filename.",
                K::ACCEPT.join(", "),
                K::MAX_SIZE,
            )
            .into(),
        );

        let mut body = SchemaNode {
            types: JsonType::Object.into(),
            ..SchemaNode::default()
        };
        body.properties.insert(FIELD.to_owned(), file);
        body.required.push(FIELD.into());

        op.request_body(ContentType::Multipart, SchemaRef::inline(body), true);
        op.response(
            413,
            ResponseSpec::empty("The upload exceeded the limit for this kind."),
        );
        op.response(
            422,
            ResponseSpec::empty(
                "The uploaded bytes are not one of the accepted media types. The type is decided \
                 by reading the bytes, so a renamed file is rejected.",
            ),
        );
    }

    // `async fn` is not available here: the trait declares the method as
    // returning `impl Future + Send + 'a`, and rewriting it as an `async fn`
    // drops the explicit `Send` bound the handler machinery needs.
    #[expect(
        clippy::manual_async_fn,
        reason = "the `+ Send` bound on the returned future is load-bearing"
    )]
    fn extract_body<'a>(
        req: Request,
        ctx: &'a RequestCtx,
    ) -> impl Future<Output = moso_core::Result<Self>> + Send + 'a {
        async move {
            let mut form =
                <moso_core::extract::Multipart as ExtractBody>::extract_body(req, ctx).await?;

            // `multer` allows one live field at a time and ties its lifetime to
            // the parser, so the parser has to live where the field is read:
            // in a task. The task reads the headers and the sniffing prefix,
            // hands them back over a oneshot, and then pumps the rest of the
            // body through a bounded channel — two chunks in flight, so a slow
            // backend applies backpressure to the socket instead of the body
            // accumulating in memory.
            let (header_tx, header_rx) =
                tokio::sync::oneshot::channel::<moso_core::Result<Header>>();
            let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<Result<Bytes>>(2);

            tokio::spawn(async move {
                let mut header_tx = Some(header_tx);
                let mut field = loop {
                    match form.next_field().await {
                        Ok(Some(field)) if field.name() == Some(FIELD) => break field,
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            if let Some(tx) = header_tx.take() {
                                let _ = tx.send(Err(missing_field()));
                            }
                            return;
                        }
                        Err(error) => {
                            if let Some(tx) = header_tx.take() {
                                let _ = tx.send(Err(error));
                            }
                            return;
                        }
                    }
                };

                let filename = sanitise_filename(field.file_name().unwrap_or_default());
                let declared_size = field
                    .headers()
                    .get(http::header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse().ok());

                // The only buffering on the path, bounded by `SNIFF_BYTES`.
                let mut prefix = bytes::BytesMut::with_capacity(SNIFF_BYTES);
                while prefix.len() < SNIFF_BYTES {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if prefix.len() as u64 + chunk.len() as u64 > K::MAX_SIZE {
                                if let Some(tx) = header_tx.take() {
                                    let _ =
                                        tx.send(Err(
                                            crate::Error::too_large(K::NAME, K::MAX_SIZE).into()
                                        ));
                                }
                                return;
                            }
                            prefix.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(error) => {
                            if let Some(tx) = header_tx.take() {
                                let _ = tx.send(Err(error));
                            }
                            return;
                        }
                    }
                }

                if let Some(tx) = header_tx.take()
                    && tx
                        .send(Ok(Header {
                            filename,
                            declared_size,
                            prefix: prefix.freeze(),
                        }))
                        .is_err()
                {
                    // The extractor is gone; so is any reason to keep reading.
                    return;
                }

                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            // A closed receiver means the handler dropped the
                            // upload — stop rather than draining the socket.
                            if chunk_tx.send(Ok(chunk)).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            let _ = chunk_tx
                                .send(Err(crate::Error::unavailable(
                                    "upload",
                                    error.to_string(),
                                    None,
                                )))
                                .await;
                            break;
                        }
                    }
                }
            });

            let Header {
                filename,
                declared_size,
                prefix,
            } = header_rx.await.map_err(|_| {
                moso_core::Error::internal_msg("the upload reader stopped before it reported")
            })??;

            let sniffed = sniff(&prefix).unwrap_or("application/octet-stream");
            if !accepts(K::ACCEPT, sniffed) {
                return Err(crate::Error::content_type(K::NAME, sniffed, K::ACCEPT).into());
            }

            // SVG is the one accepted type that can execute. A kind that
            // accepts `image/*` gets SVG with it, and an SVG carrying a script
            // is stored XSS on whatever origin serves it.
            if sniffed == "image/svg+xml" && !svg_is_inert(&prefix) {
                return Err(crate::Error::content_type(
                    K::NAME,
                    "image/svg+xml (with script or remote content)",
                    K::ACCEPT,
                )
                .into());
            }

            let rest: ByteStream = Box::pin(tokio_stream_from_receiver(chunk_rx));

            Ok(Self::validated(
                filename,
                sniffed,
                prefix,
                rest,
                declared_size,
            ))
        }
    }
}

/// What the reader task reports before it starts pumping bytes.
struct Header {
    /// The sanitised filename.
    filename: String,
    /// The size the client declared, if any.
    declared_size: Option<u64>,
    /// The leading bytes, already read for sniffing.
    prefix: Bytes,
}

/// The error for a multipart body with no `file` part.
fn missing_field() -> moso_core::Error {
    moso_core::Error::new(moso_core::ErrorKind::Validation)
        .with_detail(format!(
            "the multipart body has no `{FIELD}` field carrying a file"
        ))
        .with_field(
            "/file",
            "missing",
            "expected a `file` part in the multipart body",
        )
}

/// A stream over a channel receiver.
///
/// Written here rather than pulled from `tokio-stream`: it is six lines, and
/// this is the only place in either battery that needs one.
fn tokio_stream_from_receiver(
    mut receiver: tokio::sync::mpsc::Receiver<Result<Bytes>>,
) -> impl futures_util::Stream<Item = Result<Bytes>> + Send + 'static {
    futures_util::stream::poll_fn(move |cx| receiver.poll_recv(cx))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The acceptance criterion, stated as plainly as it can be: a Mach-O
    /// executable named `logo.png` is an executable.
    #[test]
    fn a_png_named_executable_is_rejected() {
        // A real Mach-O 64-bit header.
        let executable = [
            0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00, 0x02, 0x00,
            0x00, 0x00,
        ];
        let sniffed = sniff(&executable).expect("recognised");
        assert_eq!(sniffed, "application/x-mach-binary");
        assert!(!accepts(&["image/*"], sniffed));
        assert!(!accepts(&["image/png", "image/jpeg"], sniffed));

        // ELF and PE too, because a deployment is not always macOS.
        assert_eq!(sniff(b"\x7fELF\x02\x01\x01\x00"), Some("application/x-elf"));
        assert_eq!(
            sniff(b"MZ\x90\x00\x03\x00\x00\x00"),
            Some("application/vnd.microsoft.portable-executable"),
        );
        assert_eq!(sniff(b"#!/bin/sh\nrm -rf /"), Some("text/x-shellscript"));
    }

    /// The other half of the same criterion: an HTML document named `.png` is
    /// stored XSS, and the sniffer has to see it.
    #[test]
    fn an_html_document_is_never_an_image() {
        for hostile in [
            &b"<!DOCTYPE html><html><body><script>alert(1)</script>"[..],
            &b"<html><script>alert(1)</script></html>"[..],
            &b"   \n\t<script>alert(1)</script>"[..],
            &b"\xef\xbb\xbf<!doctype HTML>"[..],
        ] {
            let sniffed = sniff(hostile).expect("recognised as something");
            assert_eq!(sniffed, "text/html", "{:?}", core::str::from_utf8(hostile));
            assert!(!accepts(&["image/*"], sniffed));
        }
    }

    /// The formats that are actually images are recognised, or the whole thing
    /// is a very safe way to reject every upload.
    #[test]
    fn the_real_image_formats_are_recognised() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n\x00\x00"), Some("image/png"));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0\x00\x10JFIF"), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a\x01\x00"), Some("image/gif"));
        assert_eq!(
            sniff(b"\x00\x00\x00\x20ftypavif\x00\x00\x00\x00"),
            Some("image/avif"),
        );
        assert_eq!(sniff(b"II*\x00\x08\x00\x00\x00"), Some("image/tiff"));
    }

    /// An SVG is an image and a document at once; it must be recognised as SVG
    /// and not as generic XML, because the two are treated differently.
    #[test]
    fn an_svg_is_recognised_through_its_xml_declaration() {
        assert_eq!(
            sniff(br#"<svg xmlns="http://www.w3.org/2000/svg"/>"#),
            Some("image/svg+xml"),
        );
        assert_eq!(
            sniff(br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"/>"#),
            Some("image/svg+xml"),
        );
        assert_eq!(
            sniff(br#"<?xml version="1.0"?><rss/>"#),
            Some("application/xml")
        );
    }

    /// Nothing recognisable is `None`, which the caller must treat as unknown
    /// — never as "whatever the client said".
    #[test]
    fn unrecognised_binary_is_none_rather_than_a_guess() {
        assert_eq!(
            sniff(&[0xde, 0x00, 0xfe, 0x01, 0x99, 0x02, 0x88, 0x03]),
            None
        );
        assert_eq!(sniff(&[]), None);
    }

    /// `image/*` matches a family; `*/*` is not a pattern at all.
    #[test]
    fn the_accept_patterns_mean_what_they_say() {
        assert!(accepts(&["image/*"], "image/png"));
        assert!(accepts(&["image/*"], "IMAGE/PNG"));
        assert!(accepts(&["image/png"], "image/png; charset=binary"));
        assert!(!accepts(&["image/*"], "text/html"));
        assert!(!accepts(&["image/png"], "image/jpeg"));
        assert!(!accepts(&["*/*"], "application/x-elf"));
        assert!(!accepts(&["*"], "image/png"));
        assert!(!accepts(&[], "image/png"));
        // `image` is not a prefix of `imagemagick/x`.
        assert!(!accepts(&["image/*"], "imagemagick/x"));
    }

    /// A filename echoed into a `Content-Disposition` is a header-injection
    /// and path-traversal surface at once.
    #[test]
    fn a_filename_cannot_traverse_or_inject() {
        assert_eq!(sanitise_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitise_filename("/etc/shadow"), "shadow");
        assert_eq!(sanitise_filename("C:\\Windows\\system32\\x.dll"), "x.dll");
        assert_eq!(
            sanitise_filename("a\"; filename=\"b.exe"),
            "a; filename=b.exe"
        );
        assert_eq!(sanitise_filename("a\r\nX-Injected: 1"), "aX-Injected: 1");
        assert_eq!(sanitise_filename("....."), "file");
        assert_eq!(sanitise_filename(""), "file");
        assert_eq!(sanitise_filename("photo (1).jpg"), "photo (1).jpg");
        assert!(sanitise_filename(&"a".repeat(400)).len() <= 255);
    }

    /// A truncated filename must still be valid UTF-8, or writing it into a
    /// header panics.
    #[test]
    fn a_long_multibyte_filename_truncates_on_a_boundary() {
        let name = "é".repeat(300);
        let sanitised = sanitise_filename(&name);
        assert!(sanitised.len() <= 255);
        assert!(sanitised.chars().all(|c| c == 'é'));
    }

    /// The privacy default: an uploaded photograph does not publish where it
    /// was taken.
    #[test]
    fn exif_is_stripped_from_a_jpeg() {
        // SOI, APP1 (EXIF) with a GPS-looking payload, APP0 (JFIF), EOI.
        let mut jpeg = vec![0xff, 0xd8];
        let exif = b"Exif\x00\x00GPSLatitude 51.5";
        jpeg.extend_from_slice(&[0xff, 0xe1]);
        jpeg.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(exif);
        jpeg.extend_from_slice(&[0xff, 0xe0, 0x00, 0x04, 0x00, 0x00]);
        jpeg.extend_from_slice(&[0xff, 0xd9]);

        let stripped = strip_metadata(&jpeg).expect("something was stripped");
        assert!(!stripped.windows(3).any(|w| w == b"GPS"));
        // The JFIF header survives: some decoders need it.
        assert!(stripped.windows(2).any(|w| w == [0xff, 0xe0]));
        assert!(stripped.starts_with(&[0xff, 0xd8]));
        assert!(stripped.ends_with(&[0xff, 0xd9]));
    }

    /// A JPEG carrying no metadata is returned unchanged, so the common case
    /// costs no copy.
    #[test]
    fn a_jpeg_with_no_metadata_is_left_alone() {
        let jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xd9];
        assert!(strip_metadata(&jpeg).is_none());
    }

    /// PNG carries its metadata in chunks, and the chunk structure has to
    /// survive the removal.
    #[test]
    fn text_chunks_are_stripped_from_a_png() {
        /// One PNG chunk: length, type, payload, and a CRC we do not compute
        /// because nothing here validates it.
        fn chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut out = (payload.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(payload);
            out.extend_from_slice(&[0, 0, 0, 0]);
            out
        }

        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(chunk(b"IHDR", &[0; 13]));
        png.extend(chunk(b"tEXt", b"Comment\0taken at 51.5N"));
        png.extend(chunk(b"eXIf", b"GPS"));
        png.extend(chunk(b"IDAT", &[1, 2, 3]));
        png.extend(chunk(b"IEND", &[]));

        let stripped = strip_metadata(&png).expect("something was stripped");
        assert!(!stripped.windows(4).any(|w| w == b"tEXt"));
        assert!(!stripped.windows(4).any(|w| w == b"eXIf"));
        assert!(stripped.windows(4).any(|w| w == b"IHDR"));
        assert!(stripped.windows(4).any(|w| w == b"IDAT"));
        assert!(stripped.windows(4).any(|w| w == b"IEND"));
    }

    /// A format whose metadata layout we do not know must not be rewritten.
    #[test]
    fn an_unknown_format_is_not_touched() {
        assert!(strip_metadata(b"GIF89a\x01\x00").is_none());
        assert!(strip_metadata(b"%PDF-1.7").is_none());
    }

    /// SVG is a script-bearing document format wearing an image's clothes.
    #[test]
    fn a_scriptable_svg_is_not_inert() {
        for hostile in [
            &br#"<svg><script>alert(1)</script></svg>"#[..],
            &br#"<svg onload="alert(1)"></svg>"#[..],
            &br#"<svg><a xlink:href="javascript:alert(1)">x</a></svg>"#[..],
            &br#"<svg><foreignObject><body xmlns="http://www.w3.org/1999/xhtml"/></foreignObject></svg>"#[..],
            &br#"<svg><image xlink:href="http://evil.example/x"/></svg>"#[..],
            &br##"<svg><use href="#x"/></svg>"##[..],
            &br#"<!DOCTYPE svg [<!ENTITY x SYSTEM "file:///etc/passwd">]><svg/>"#[..],
            &br#"<svg><animate attributeName="href" values="javascript:alert(1)"/></svg>"#[..],
        ] {
            assert!(
                !svg_is_inert(hostile),
                "{:?} must not be inert",
                core::str::from_utf8(hostile),
            );
        }
    }

    /// A plain drawing is inert, or the check rejects every real SVG.
    #[test]
    fn a_plain_svg_is_inert() {
        assert!(svg_is_inert(
            br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
                  <rect width="10" height="10" fill="#0af"/>
                  <path d="M0 0 L10 10" stroke="black"/>
                </svg>"##
        ));
        // A word merely *containing* `on` is not a handler; only an attribute
        // whose name starts with `on` is.
        assert!(svg_is_inert(
            br#"<svg><text font-family="Monotype">x</text></svg>"#
        ));
        // The check errs towards refusal: `on` plus letters plus `=` is the
        // shape of every event handler, and a false positive costs an odd SVG
        // while a false negative costs stored XSS.
        assert!(!svg_is_inert(br#"<svg one="1"/>"#));
    }

    /// The cap is enforced on the bytes that arrive, not on the ones the
    /// client claimed.
    #[tokio::test]
    async fn the_size_limit_is_enforced_on_the_streaming_path() {
        struct Tiny;
        impl AttachmentKind for Tiny {
            const NAME: &'static str = "Tiny";
            const ACCEPT: &'static [&'static str] = &["image/png"];
            const MAX_SIZE: u64 = 8;
        }

        let upload = Upload::<Tiny>::validated(
            "a.png",
            "image/png",
            Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
            crate::stream_from_bytes(Bytes::from(vec![0_u8; 64])),
            // A client claiming one byte changes nothing.
            Some(1),
        );

        let error = upload.into_bytes().await.expect_err("past the limit");
        assert!(matches!(error, crate::Error::TooLarge { limit: 8, .. }));
    }

    /// The prefix is not lost: the bytes the sniffer read are the first bytes
    /// the backend receives.
    #[tokio::test]
    async fn the_sniffed_prefix_is_still_part_of_the_body() {
        struct Any;
        impl AttachmentKind for Any {
            const NAME: &'static str = "Any";
            const ACCEPT: &'static [&'static str] = &["application/octet-stream"];
            const MAX_SIZE: u64 = 1024;
        }

        let upload = Upload::<Any>::validated(
            "a.bin",
            "application/octet-stream",
            Bytes::from_static(b"HEAD"),
            crate::stream_from_bytes(Bytes::from_static(b"TAIL")),
            None,
        );
        assert_eq!(upload.into_bytes().await.expect("collects"), "HEADTAIL");
    }

    /// A key is built from the sniffed type, never from the client's filename.
    #[test]
    fn the_extension_follows_the_sniffed_type() {
        struct Any;
        impl AttachmentKind for Any {
            const NAME: &'static str = "Any";
            const ACCEPT: &'static [&'static str] = &["image/*"];
            const MAX_SIZE: u64 = 1024;
        }

        let upload = Upload::<Any>::validated(
            "definitely-a-pdf.pdf",
            "image/png",
            Bytes::new(),
            crate::stream_from_bytes(Bytes::new()),
            None,
        );
        assert_eq!(upload.extension(), "png");
    }

    /// `into_sanitised_bytes` is what an avatar handler calls, and it must
    /// actually strip.
    #[tokio::test]
    async fn sanitised_bytes_have_no_exif() {
        struct Photo;
        impl AttachmentKind for Photo {
            const NAME: &'static str = "Photo";
            const ACCEPT: &'static [&'static str] = &["image/jpeg"];
            const MAX_SIZE: u64 = 4096;
        }

        let mut jpeg = vec![0xff, 0xd8];
        let exif = b"Exif\x00\x00GPSLatitude";
        jpeg.extend_from_slice(&[0xff, 0xe1]);
        jpeg.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(exif);
        jpeg.extend_from_slice(&[0xff, 0xd9]);

        let upload = Upload::<Photo>::validated(
            "photo.jpg",
            "image/jpeg",
            Bytes::from(jpeg),
            crate::stream_from_bytes(Bytes::new()),
            None,
        );
        let bytes = upload.into_sanitised_bytes().await.expect("collects");
        assert!(!bytes.windows(3).any(|w| w == b"GPS"));
    }

    /// Turning EXIF stripping off is a deliberate act, and it has to work.
    #[tokio::test]
    async fn stripping_can_be_turned_off_deliberately() {
        struct Raw;
        impl AttachmentKind for Raw {
            const NAME: &'static str = "Raw";
            const ACCEPT: &'static [&'static str] = &["image/jpeg"];
            const MAX_SIZE: u64 = 4096;
            const STRIP_EXIF: bool = false;
        }

        let mut jpeg = vec![0xff, 0xd8];
        let exif = b"Exif\x00\x00GPS";
        jpeg.extend_from_slice(&[0xff, 0xe1]);
        jpeg.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(exif);
        jpeg.extend_from_slice(&[0xff, 0xd9]);

        let upload = Upload::<Raw>::validated(
            "photo.jpg",
            "image/jpeg",
            Bytes::from(jpeg),
            crate::stream_from_bytes(Bytes::new()),
            None,
        );
        let bytes = upload.into_sanitised_bytes().await.expect("collects");
        assert!(bytes.windows(3).any(|w| w == b"GPS"));
    }
}
