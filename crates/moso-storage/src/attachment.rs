//! Typed attachments on entities: one column, no extra table, variants
//! generated in the background.
//!
//! ```text
//! #[derive(Entity)]
//! pub struct Product {
//!     #[entity(pk)]
//!     pub id: Id<Product>,
//!     #[entity(attachment(variants(thumb = "200x200", card = "600x400"),
//!                         accept = "image/*", max_size = "10MiB"))]
//!     pub image: Attachment<Image>,
//! }
//! ```
//!
//! The column stores an [`Attachment`] as JSON — key, size, content type,
//! checksum and the state of each variant — so adding an image to a model costs
//! one column and no join. Variants are produced by a `moso-jobs` job, so the
//! request that uploads a photo does not wait on image processing.
//!
//! # Why this crate does not depend on `moso-orm`
//!
//! It does not need to. [`Attachment`] is a plain `Serialize + Deserialize`
//! descriptor, which is exactly what `#[entity(attachment(..))]` needs in order
//! to store it through `moso_orm::Json`. The dependency edge stays absent
//! (`xtask/allow/dep-edges.toml`: `storage -> []`), so a service can store
//! files without compiling an ORM.
//!
//! It is also why the descriptor does not hold a storage handle, and why
//! [`Attachment::url`] takes one as an argument. A handle inside the value would
//! make it unserialisable — there is no JSON for an open connection pool — and
//! would bind a row read out of a database to whichever backend the process that
//! read it happens to be configured with. The descriptor is data; the store is
//! configuration; they meet at the call site.
//!
//! # Rendering a variant
//!
//! **No image codec is a dependency of this crate, and none is going to be.**
//! Encoders are large, they carry CVEs, and an application that wants AVIF and
//! an application that wants a 200 KiB binary should not be forced onto the same
//! one. What this crate owns is the *seam*: [`VariantSpec`] declares the work,
//! [`Attachment::read_original`] and [`Attachment::store_variant`] move the
//! bytes, [`Rendition`] is the handover, and [`Attachment::mark_failed`] records
//! the ones that will never succeed. Supplying the codec is the application's
//! job, and it is the only part of the loop below that is not written here.
//!
//! The whole of a `moso-jobs` job body, with a codec that copies bytes so the
//! wiring is proved end to end without one:
//!
//! ```
//! use bytes::Bytes;
//! use moso_storage::backend::MemoryStorage;
//! use moso_storage::{
//!     Attachment, AttachmentKind, Fit, Rendition, Storage, Upload, Variant, VariantSpec,
//!     VariantTransform, stream_from_bytes,
//! };
//!
//! /// A product photograph with one rendition.
//! struct Photo;
//!
//! impl AttachmentKind for Photo {
//!     const NAME: &'static str = "Photo";
//!     const ACCEPT: &'static [&'static str] = &["image/png"];
//!     const MAX_SIZE: u64 = 8 * 1024 * 1024;
//!     const VARIANTS: &'static [VariantSpec] = &[VariantSpec::new(
//!         "thumb",
//!         VariantTransform::Resize { width: 200, height: 200, fit: Fit::Cover },
//!     )];
//! }
//!
//! /// The one part this crate does not supply. Yours calls an image library —
//! /// through `moso::task::blocking()`, because encoding is CPU-bound and the
//! /// runtime is not yours to block. This one copies the bytes.
//! fn encode(transform: VariantTransform, original: Bytes) -> Result<Rendition, String> {
//!     match transform {
//!         VariantTransform::Resize { .. } => Ok(Rendition::new(original, "png", "image/png")),
//!         other => Err(format!("this encoder does not do {other:?}")),
//!     }
//! }
//!
//! /// The job body, in full.
//! async fn render(
//!     attachment: &mut Attachment<Photo>,
//!     storage: &dyn Storage,
//! ) -> moso_storage::Result<()> {
//!     let original = attachment.read_original(storage).await?;
//!     for spec in Photo::VARIANTS {
//!         let variant = Variant::dynamic(spec.name());
//!         match encode(spec.transform(), original.clone()) {
//!             Ok(rendition) => attachment.store_variant(storage, &variant, rendition).await?,
//!             // Recorded, not retried: an image the encoder cannot read will
//!             // not become readable on the fifth attempt.
//!             Err(reason) => attachment.mark_failed(&variant, reason),
//!         }
//!     }
//!     Ok(())
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> moso_storage::Result<()> {
//! let storage = MemoryStorage::new();
//! let upload = Upload::<Photo>::validated(
//!     "holiday snap.png",
//!     "image/png",
//!     Bytes::from_static(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR"),
//!     stream_from_bytes(Bytes::new()),
//!     None,
//! );
//! let mut attachment = Attachment::attach(upload, &storage, "products/prd_1").await?;
//! assert!(!attachment.is_complete(), "every declared variant starts pending");
//!
//! render(&mut attachment, &storage).await?;
//!
//! assert!(attachment.is_complete());
//! assert_eq!(
//!     attachment.key_for(&Variant::new("thumb")).as_str(),
//!     "products/prd_1/thumb.png",
//! );
//! # Ok(()) }
//! ```

use std::borrow::Cow;

use chrono::{DateTime, Utc};
use moso_schema::Url;
use serde::{Deserialize, Serialize};

use crate::{Checksum, Result, StorageKey};

/// One rendition of an attachment, named by the entity's declaration.
///
/// The names — `thumb`, `card` — are the application's, so this type carries a
/// name rather than being an enum. `#[entity(attachment(variants(..)))]`
/// generates a `const` per declared variant, so `Variant::new` at a call site
/// is a sign something is being looked up dynamically.
///
/// ```
/// use moso_storage::Variant;
///
/// const THUMB: Variant = Variant::new("thumb");
/// assert_eq!(THUMB.name(), "thumb");
/// assert_eq!(Variant::ORIGINAL.name(), "original");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Variant(Cow<'static, str>);

impl Variant {
    /// The bytes as uploaded. Always present, never generated.
    pub const ORIGINAL: Self = Self(Cow::Borrowed("original"));

    /// A variant by name.
    ///
    /// ```
    /// use moso_storage::Variant;
    ///
    /// const CARD: Variant = Variant::new("card");
    /// assert_eq!(CARD.name(), "card");
    /// ```
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    /// A variant whose name is only known at runtime.
    ///
    /// ```no_run
    /// use moso_storage::Variant;
    ///
    /// # fn f(name: String) {
    /// let _ = Variant::dynamic(name);
    /// # }
    /// ```
    #[must_use]
    pub fn dynamic(name: impl Into<String>) -> Self {
        Self(Cow::Owned(name.into()))
    }

    /// The variant's name.
    ///
    /// ```
    /// use moso_storage::Variant;
    ///
    /// assert_eq!(Variant::ORIGINAL.name(), "original");
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }

    /// Whether this is [`Variant::ORIGINAL`].
    ///
    /// ```
    /// use moso_storage::Variant;
    ///
    /// assert!(Variant::ORIGINAL.is_original());
    /// ```
    #[must_use]
    pub fn is_original(&self) -> bool {
        self.0 == "original"
    }
}

/// How a variant's pixels are derived from the original.
///
/// ```
/// use moso_storage::{Fit, VariantTransform};
///
/// let t = VariantTransform::Resize { width: 200, height: 200, fit: Fit::Cover };
/// assert!(matches!(t, VariantTransform::Resize { .. }));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum VariantTransform {
    /// Fit into a box.
    Resize {
        /// Target width in pixels.
        width: u32,
        /// Target height in pixels.
        height: u32,
        /// How the aspect ratio is reconciled.
        fit: Fit,
    },
    /// Re-encode without resizing, e.g. to WebP.
    Reencode {
        /// The target media type.
        format: &'static str,
        /// Encoder quality, 1–100.
        quality: u8,
    },
}

/// How a resize reconciles the source and target aspect ratios.
///
/// ```
/// use moso_storage::Fit;
///
/// assert_eq!(Fit::default(), Fit::Cover);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// Fill the box, cropping the overflow. The default: a thumbnail grid with
    /// ragged edges is worse than a crop.
    #[default]
    Cover,
    /// Fit inside the box, leaving the other dimension shorter.
    Contain,
    /// Stretch to the box, distorting the image. Rarely what anyone wants.
    Fill,
}

/// A declared variant: its name and how it is produced.
///
/// ```
/// use moso_storage::{Fit, VariantSpec, VariantTransform};
///
/// const THUMB: VariantSpec = VariantSpec::new(
///     "thumb",
///     VariantTransform::Resize { width: 200, height: 200, fit: Fit::Cover },
/// );
/// assert_eq!(THUMB.name(), "thumb");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VariantSpec {
    /// The variant's name.
    name: &'static str,
    /// How it is derived.
    transform: VariantTransform,
}

impl VariantSpec {
    /// Declare a variant.
    ///
    /// ```
    /// use moso_storage::{Fit, VariantSpec, VariantTransform};
    ///
    /// let _ = VariantSpec::new(
    ///     "card",
    ///     VariantTransform::Resize { width: 600, height: 400, fit: Fit::Cover },
    /// );
    /// ```
    #[must_use]
    pub const fn new(name: &'static str, transform: VariantTransform) -> Self {
        Self { name, transform }
    }

    /// The variant's name.
    ///
    /// ```
    /// # use moso_storage::{Fit, VariantSpec, VariantTransform};
    /// # const S: VariantSpec = VariantSpec::new("t",
    /// #     VariantTransform::Resize { width: 1, height: 1, fit: Fit::Cover });
    /// assert_eq!(S.name(), "t");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// How it is derived.
    ///
    /// ```
    /// # use moso_storage::{Fit, VariantSpec, VariantTransform};
    /// # const S: VariantSpec = VariantSpec::new("t",
    /// #     VariantTransform::Resize { width: 1, height: 1, fit: Fit::Cover });
    /// let _ = S.transform();
    /// ```
    #[must_use]
    pub const fn transform(&self) -> VariantTransform {
        self.transform
    }
}

/// Bytes an encoder produced, on their way to becoming a variant.
///
/// The handover between the application's codec and this crate's storage: the
/// three things that have to be decided *by whatever encoded the image* and
/// cannot be derived from the [`VariantSpec`]. A `Resize` may or may not change
/// the format, and only the encoder knows which.
///
/// ```
/// use moso_storage::Rendition;
///
/// let rendition = Rendition::new(bytes::Bytes::from_static(b"RIFF"), "webp", "image/webp");
/// assert_eq!(rendition.extension(), "webp");
/// assert_eq!(rendition.content_type(), "image/webp");
/// assert_eq!(rendition.bytes().len(), 4);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Rendition {
    /// The encoded bytes.
    bytes: bytes::Bytes,
    /// The extension the variant's key ends in.
    extension: String,
    /// The media type to store, which may differ from the original's.
    content_type: String,
}

impl Rendition {
    /// The bytes an encoder produced, and what they are.
    ///
    /// ```
    /// use moso_storage::Rendition;
    ///
    /// let _ = Rendition::new(bytes::Bytes::from_static(b"\x89PNG"), "png", "image/png");
    /// ```
    #[must_use]
    pub fn new(
        bytes: bytes::Bytes,
        extension: impl Into<String>,
        content_type: impl Into<String>,
    ) -> Self {
        Self {
            bytes,
            extension: extension.into(),
            content_type: content_type.into(),
        }
    }

    /// The encoded bytes.
    ///
    /// ```
    /// # use moso_storage::Rendition;
    /// assert!(Rendition::new(bytes::Bytes::new(), "png", "image/png").bytes().is_empty());
    /// ```
    #[must_use]
    pub fn bytes(&self) -> &bytes::Bytes {
        &self.bytes
    }

    /// The extension the variant's key ends in.
    ///
    /// ```
    /// # use moso_storage::Rendition;
    /// assert_eq!(Rendition::new(bytes::Bytes::new(), "webp", "image/webp").extension(), "webp");
    /// ```
    #[must_use]
    pub fn extension(&self) -> &str {
        &self.extension
    }

    /// The media type to store.
    ///
    /// ```
    /// # use moso_storage::Rendition;
    /// let rendition = Rendition::new(bytes::Bytes::new(), "webp", "image/webp");
    /// assert_eq!(rendition.content_type(), "image/webp");
    /// ```
    #[must_use]
    pub fn content_type(&self) -> &str {
        &self.content_type
    }
}

/// Where a variant has got to.
///
/// Stored in the column, so a UI can render a placeholder for a variant that is
/// still being generated instead of a broken image.
///
/// ```no_run
/// use moso_storage::VariantState;
///
/// # fn f(s: &VariantState) {
/// let ready = matches!(s, VariantState::Ready { .. });
/// let _ = ready;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
#[non_exhaustive]
pub enum VariantState {
    /// The generation job has not run yet.
    Pending,
    /// The variant exists.
    Ready {
        /// Where it is.
        key: StorageKey,
        /// How many bytes.
        size: u64,
        /// Its media type, which may differ from the original's.
        content_type: String,
    },
    /// Generation failed and will not be retried automatically.
    Failed {
        /// Why, in one line, safe to show an operator.
        reason: String,
    },
}

/// What an attachment field accepts and produces.
///
/// Implemented by the marker type in `Attachment<Image>`. The consts are what
/// `#[entity(attachment(..))]` fills in, and what
/// [`Upload`](crate::Upload) enforces while streaming.
///
/// ```
/// use moso_storage::{AttachmentKind, Fit, VariantSpec, VariantTransform};
///
/// /// A product photograph.
/// pub struct Image;
///
/// impl AttachmentKind for Image {
///     const NAME: &'static str = "Image";
///     const ACCEPT: &'static [&'static str] = &["image/png", "image/jpeg", "image/webp"];
///     const MAX_SIZE: u64 = 10 * 1024 * 1024;
///     const VARIANTS: &'static [VariantSpec] = &[VariantSpec::new(
///         "thumb",
///         VariantTransform::Resize { width: 200, height: 200, fit: Fit::Cover },
///     )];
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not an attachment kind",
    label = "not an attachment kind",
    note = "an attachment kind is a marker type carrying `NAME`, `ACCEPT`, `MAX_SIZE` and \
            `VARIANTS` — it holds no data and is never constructed",
    note = "help: write `impl AttachmentKind for {Self}` with those four constants, or let \
            `#[entity(attachment(accept = \"image/*\", max_size = \"10MiB\"))]` generate it"
)]
pub trait AttachmentKind: Send + Sync + 'static {
    /// The kind's name, for diagnostics and the storage key's prefix.
    const NAME: &'static str;

    /// The accepted media types. A trailing `/*` matches a whole type.
    ///
    /// Checked against the *sniffed* type, never the declared one.
    const ACCEPT: &'static [&'static str];

    /// The largest accepted upload, in bytes. Enforced while streaming.
    const MAX_SIZE: u64;

    /// The variants generated in the background. May be empty.
    const VARIANTS: &'static [VariantSpec] = &[];

    /// Whether to strip EXIF metadata from images. On by default.
    ///
    /// Uploaded photographs routinely carry GPS coordinates, and publishing
    /// them is a privacy incident nobody intended. Turning this off is a
    /// deliberate act with a documented reason.
    const STRIP_EXIF: bool = true;
}

/// A file attached to an entity, as the column stores it.
///
/// ```no_run
/// use moso_storage::{Attachment, AttachmentKind, Variant};
///
/// # fn f<K: AttachmentKind>(a: &Attachment<K>) {
/// let _ = a.variant(&Variant::new("thumb"));
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Attachment<K: AttachmentKind> {
    /// Where the original is.
    key: StorageKey,
    /// The filename the client sent, sanitised. For `Content-Disposition` only.
    filename: String,
    /// The sniffed media type of the original.
    content_type: String,
    /// How many bytes the original is.
    size: u64,
    /// The original's checksum.
    checksum: Option<Checksum>,
    /// When it was attached.
    attached_at: DateTime<Utc>,
    /// Each declared variant's state, by name.
    variants: std::collections::BTreeMap<String, VariantState>,
    /// The kind, which holds no data.
    #[serde(skip)]
    kind: core::marker::PhantomData<fn() -> K>,
}

impl<K: AttachmentKind> Attachment<K> {
    /// Store an upload and build the descriptor for it.
    ///
    /// The one way an application creates an attachment. It generates the key
    /// — never taking one from the client — writes the bytes, and records
    /// every declared variant as [`VariantState::Pending`], because generating
    /// them is a background job's work and not this request's.
    ///
    /// `prefix` is where the object goes: `"products/prd_123"` gives
    /// `products/prd_123/original.png`. The extension comes from the *sniffed*
    /// type.
    ///
    /// # Errors
    ///
    /// [`Error::Key`](crate::Error::Key) when `prefix` is not a legal key
    /// prefix, and whatever the backend reports for the write.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, Storage, Upload};
    /// # async fn f<K: AttachmentKind>(s: &dyn Storage, u: Upload<K>)
    /// #     -> moso_storage::Result<Attachment<K>> {
    /// Attachment::attach(u, s, "products/prd_123").await
    /// # }
    /// ```
    pub async fn attach(
        upload: crate::Upload<K>,
        storage: &dyn crate::Storage,
        prefix: &str,
    ) -> Result<Self> {
        let filename = upload.filename().to_owned();
        let content_type = upload.content_type().to_owned();
        let extension = upload.extension();

        let mut segments: Vec<String> = prefix
            .split(crate::key::SEPARATOR)
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect();
        segments.push(format!("{}.{extension}", Variant::ORIGINAL.name()));
        let key = StorageKey::from_segments(segments)?;

        // Stripping EXIF is a whole-file operation, so a kind that wants it
        // buffers — bounded by `K::MAX_SIZE` — and a kind that does not
        // streams straight through.
        let meta = if K::STRIP_EXIF {
            let bytes = upload.into_sanitised_bytes().await?;
            storage
                .put(
                    &key,
                    crate::stream_from_bytes(bytes),
                    crate::PutOpts::new(&content_type).trust_content_type(),
                )
                .await?
        } else {
            storage
                .put(
                    &key,
                    upload.into_stream(),
                    crate::PutOpts::new(&content_type).trust_content_type(),
                )
                .await?
        };

        // Every declared variant starts pending, so a UI can render a
        // placeholder rather than a broken image while the job runs.
        let variants = K::VARIANTS
            .iter()
            .map(|spec| (spec.name().to_owned(), VariantState::Pending))
            .collect();

        Ok(Self {
            key,
            filename,
            content_type,
            size: meta.size,
            checksum: meta.checksum,
            attached_at: Utc::now(),
            variants,
            kind: core::marker::PhantomData,
        })
    }

    /// Read the original's bytes, bounded by `K::MAX_SIZE`.
    ///
    /// The first line of a rendering job. Bounded because an encoder needs the
    /// whole image at once and the bound is already declared: nothing can be at
    /// this key that [`Upload<K>`](crate::Upload) would have let through, so
    /// `K::MAX_SIZE` is both the honest limit and the only one that does not
    /// need a second number invented for it.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`](crate::Error::NotFound) when the original is gone,
    /// [`Error::TooLarge`](crate::Error::TooLarge) when what is stored is
    /// somehow larger than `K::MAX_SIZE`, and whatever the backend reports.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, Storage};
    /// # async fn f<K: AttachmentKind>(a: &Attachment<K>, s: &dyn Storage)
    /// #     -> moso_storage::Result<bytes::Bytes> {
    /// a.read_original(s).await
    /// # }
    /// ```
    pub async fn read_original(&self, storage: &dyn crate::Storage) -> Result<bytes::Bytes> {
        let body = storage.get(&self.key).await?;
        crate::collect_bounded(body, K::MAX_SIZE, K::NAME).await
    }

    /// Write an encoded variant and record it as ready.
    ///
    /// The last two lines of a rendering job, in one call:
    /// [`variant_key`](Attachment::variant_key) decides where it goes, the
    /// bytes are written with sniffing off — they came from the application's
    /// own encoder, so re-deriving the type would be work with no new
    /// information — and [`mark_ready`](Attachment::mark_ready) records the
    /// result. The caller then saves the descriptor back to its column.
    ///
    /// A failure here is the *store* failing, and the job should retry. An
    /// encoder that cannot read the image is a different outcome:
    /// [`mark_failed`](Attachment::mark_failed) records that one and it is not
    /// retried.
    ///
    /// # Errors
    ///
    /// [`Error::Key`](crate::Error::Key) when the derived key would be too
    /// long, and whatever the backend reports for the write.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, Rendition, Storage, Variant};
    /// # async fn f<K: AttachmentKind>(a: &mut Attachment<K>, s: &dyn Storage, r: Rendition)
    /// #     -> moso_storage::Result<()> {
    /// a.store_variant(s, &Variant::new("thumb"), r).await
    /// # }
    /// ```
    pub async fn store_variant(
        &mut self,
        storage: &dyn crate::Storage,
        variant: &Variant,
        rendition: Rendition,
    ) -> Result<()> {
        let key = self.variant_key(variant, rendition.extension())?;
        let meta = storage
            .put(
                &key,
                crate::stream_from_bytes(rendition.bytes.clone()),
                crate::PutOpts::new(rendition.content_type()).trust_content_type(),
            )
            .await?;
        self.mark_ready(variant, key, meta.size, rendition.content_type);
        Ok(())
    }

    /// Record a variant as ready, having generated it.
    ///
    /// What a background job calls when it has written the rendition itself;
    /// [`store_variant`](Attachment::store_variant) is the version that writes
    /// it for you. The descriptor is then saved back to the entity's column.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, StorageKey, Variant};
    /// # fn f<K: AttachmentKind>(mut a: Attachment<K>, key: StorageKey) {
    /// a.mark_ready(&Variant::new("thumb"), key, 4096, "image/webp");
    /// # }
    /// ```
    pub fn mark_ready(
        &mut self,
        variant: &Variant,
        key: StorageKey,
        size: u64,
        content_type: impl Into<String>,
    ) {
        self.variants.insert(
            variant.name().to_owned(),
            VariantState::Ready {
                key,
                size,
                content_type: content_type.into(),
            },
        );
    }

    /// Record a variant as failed, with a reason an operator can read.
    ///
    /// Failures are recorded rather than retried forever: an image the encoder
    /// cannot read will not become readable on the fifth attempt, and a queue
    /// full of such jobs hides the ones that matter.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, Variant};
    /// # fn f<K: AttachmentKind>(mut a: Attachment<K>) {
    /// a.mark_failed(&Variant::new("thumb"), "the decoder rejected the image");
    /// # }
    /// ```
    pub fn mark_failed(&mut self, variant: &Variant, reason: impl Into<String>) {
        self.variants.insert(
            variant.name().to_owned(),
            VariantState::Failed {
                reason: reason.into(),
            },
        );
    }

    /// The key a variant *should* be written to.
    ///
    /// Derived from the original's, so an object and its renditions sort
    /// together and one prefix listing finds them all. The background job asks
    /// for this rather than inventing a key.
    ///
    /// # Errors
    ///
    /// [`Error::Key`](crate::Error::Key) when the derived key would be too
    /// long.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, StorageKey, Variant};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) -> moso_storage::Result<StorageKey> {
    /// a.variant_key(&Variant::new("thumb"), "webp")
    /// # }
    /// ```
    pub fn variant_key(&self, variant: &Variant, extension: &str) -> Result<StorageKey> {
        let name = format!("{}.{extension}", variant.name());
        self.key.with_name(&name)
    }

    /// Every variant the kind declared, whatever state it is in.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, Variant, VariantState};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) {
    /// for (variant, state) in a.variants() {
    ///     let _: (Variant, &VariantState) = (variant, state);
    /// }
    /// # }
    /// ```
    pub fn variants(&self) -> impl Iterator<Item = (Variant, &VariantState)> {
        self.variants
            .iter()
            .map(|(name, state)| (Variant::dynamic(name.clone()), state))
    }

    /// Whether every declared variant has been generated.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) { let _: bool = a.is_complete(); }
    /// ```
    #[must_use]
    pub fn is_complete(&self) -> bool {
        K::VARIANTS.iter().all(|spec| {
            matches!(
                self.variants.get(spec.name()),
                Some(VariantState::Ready { .. }),
            )
        })
    }

    /// When it was attached.
    ///
    /// ```no_run
    /// # use chrono::{DateTime, Utc};
    /// # use moso_storage::{Attachment, AttachmentKind};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) { let _: DateTime<Utc> = a.attached_at(); }
    /// ```
    #[must_use]
    pub fn attached_at(&self) -> DateTime<Utc> {
        self.attached_at
    }

    /// The original's checksum, when the backend computed one.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, Checksum};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) { let _: Option<&Checksum> = a.checksum(); }
    /// ```
    #[must_use]
    pub fn checksum(&self) -> Option<&Checksum> {
        self.checksum.as_ref()
    }

    /// Where the original is.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, StorageKey};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) { let _: &StorageKey = a.key(); }
    /// ```
    #[must_use]
    pub fn key(&self) -> &StorageKey {
        &self.key
    }

    /// The sanitised original filename.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) { let _: &str = a.filename(); }
    /// ```
    #[must_use]
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// The sniffed media type.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) { let _: &str = a.content_type(); }
    /// ```
    #[must_use]
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// The original's size in bytes.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) { let _: u64 = a.size(); }
    /// ```
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// One variant's state, or `None` when the kind never declared it.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, Variant, VariantState};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) {
    /// let _: Option<&VariantState> = a.variant(&Variant::new("thumb"));
    /// # }
    /// ```
    #[must_use]
    pub fn variant(&self, variant: &Variant) -> Option<&VariantState> {
        self.variants.get(variant.name())
    }

    /// The storage key of a ready variant, falling back to the original.
    ///
    /// Falling back rather than returning `None` is deliberate: a page that
    /// renders a slightly-too-large image while the thumbnail job runs is
    /// better than a page with a hole in it.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, StorageKey, Variant};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) {
    /// let _: &StorageKey = a.key_for(&Variant::new("thumb"));
    /// # }
    /// ```
    #[must_use]
    pub fn key_for(&self, variant: &Variant) -> &StorageKey {
        match self.variants.get(variant.name()) {
            Some(VariantState::Ready { key, .. }) => key,
            // A page that renders a slightly-too-large image while the
            // thumbnail job runs is better than a page with a hole in it.
            _ => &self.key,
        }
    }

    /// A URL for a variant, signed by `storage` and valid for `ttl`.
    ///
    /// # Why this takes three arguments
    ///
    /// Because `attachment.url()` cannot exist. The descriptor is a **column
    /// value**: it is serialised into `jsonb`, read back by a different process
    /// on a different machine, and `#[derive(Serialize)]` is what makes that
    /// work. A storage handle inside it would have to be serialised too — there
    /// is no JSON for a connection pool — and a row written by a process talking
    /// to S3 would carry that backend into a process configured for a local
    /// directory. A TTL inside it would be worse: a signature minted when the
    /// row was *written* would already have expired by the time anything read
    /// it.
    ///
    /// So the descriptor stays data and the store stays configuration, and the
    /// call site — which has both, one from `Inject<dyn Storage>` and one from
    /// the row — is where they meet.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) when the backend
    /// cannot sign: `memory` never can, and `local` only once `served_at` has
    /// given it a route to check the signature against. Check
    /// [`StorageCapabilities::signed_urls`](crate::StorageCapabilities::signed_urls)
    /// first, or serve the object through [`Storage::serve`](crate::Storage::serve)
    /// instead of handing out a URL.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, Storage, Variant};
    /// # use std::time::Duration;
    /// # async fn f<K: AttachmentKind>(a: &Attachment<K>, s: &dyn Storage)
    /// #     -> moso_storage::Result<()> {
    /// let _url = a.url(s, &Variant::ORIGINAL, Duration::from_secs(300)).await?;
    /// # Ok(()) }
    /// ```
    pub async fn url(
        &self,
        storage: &dyn crate::Storage,
        variant: &Variant,
        ttl: std::time::Duration,
    ) -> Result<Url> {
        storage.signed_url(self.key_for(variant), ttl).await
    }

    /// Every declared variant that is ready.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, Variant};
    /// # fn f<K: AttachmentKind>(a: &Attachment<K>) { let _: Vec<Variant> = a.ready_variants(); }
    /// ```
    #[must_use]
    pub fn ready_variants(&self) -> Vec<Variant> {
        self.variants
            .iter()
            .filter(|(_, state)| matches!(state, VariantState::Ready { .. }))
            .map(|(name, _)| Variant::dynamic(name.clone()))
            .collect()
    }

    /// Delete the original and every variant.
    ///
    /// Called by the entity's delete hook. Failures are reported rather than
    /// swallowed: an orphaned object costs money forever.
    ///
    /// # Errors
    ///
    /// Whatever the backend reports for the first key that fails.
    ///
    /// ```no_run
    /// # use moso_storage::{Attachment, AttachmentKind, Storage};
    /// # async fn f<K: AttachmentKind>(a: &Attachment<K>, s: &dyn Storage)
    /// #     -> moso_storage::Result<()> { a.purge(s).await }
    /// ```
    pub async fn purge(&self, storage: &dyn crate::Storage) -> Result<()> {
        let mut keys = vec![self.key.clone()];
        for state in self.variants.values() {
            if let VariantState::Ready { key, .. } = state {
                keys.push(key.clone());
            }
        }
        storage.delete_many(&keys).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage as _;

    /// A product photograph with two renditions.
    struct Image;

    impl AttachmentKind for Image {
        const NAME: &'static str = "Image";
        const ACCEPT: &'static [&'static str] = &["image/png", "image/jpeg"];
        const MAX_SIZE: u64 = 10 * 1024 * 1024;
        const VARIANTS: &'static [VariantSpec] = &[
            VariantSpec::new(
                "thumb",
                VariantTransform::Resize {
                    width: 200,
                    height: 200,
                    fit: Fit::Cover,
                },
            ),
            VariantSpec::new(
                "card",
                VariantTransform::Resize {
                    width: 600,
                    height: 400,
                    fit: Fit::Cover,
                },
            ),
        ];
    }

    async fn attached() -> (crate::backend::MemoryStorage, Attachment<Image>) {
        let storage = crate::backend::MemoryStorage::new();
        let upload = crate::Upload::<Image>::validated(
            "holiday snap.png",
            "image/png",
            bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR"),
            crate::stream_from_bytes(bytes::Bytes::new()),
            None,
        );
        let attachment = Attachment::attach(upload, &storage, "products/prd_123")
            .await
            .expect("attaches");
        (storage, attachment)
    }

    /// The key is generated from the prefix and the *sniffed* type, never from
    /// the client's filename.
    #[tokio::test]
    async fn attaching_generates_the_key_rather_than_taking_one() {
        let (storage, attachment) = attached().await;

        assert_eq!(attachment.key().as_str(), "products/prd_123/original.png");
        assert_eq!(attachment.filename(), "holiday snap.png");
        assert_eq!(attachment.content_type(), "image/png");
        assert_eq!(
            storage.keys(),
            vec!["products/prd_123/original.png".to_owned()]
        );
    }

    /// The request that uploads a photo must not wait on image processing, so
    /// every declared variant starts pending.
    #[tokio::test]
    async fn every_declared_variant_starts_pending() {
        let (_, attachment) = attached().await;

        assert!(matches!(
            attachment.variant(&Variant::new("thumb")),
            Some(VariantState::Pending),
        ));
        assert!(attachment.ready_variants().is_empty());
        assert!(!attachment.is_complete());
        assert_eq!(attachment.variants().count(), 2);
    }

    /// A page with a slightly-too-large image beats a page with a hole in it.
    #[tokio::test]
    async fn a_pending_variant_falls_back_to_the_original() {
        let (_, mut attachment) = attached().await;
        let thumb = Variant::new("thumb");

        assert_eq!(attachment.key_for(&thumb), attachment.key());

        let key = attachment.variant_key(&thumb, "webp").expect("valid");
        assert_eq!(key.as_str(), "products/prd_123/thumb.webp");
        attachment.mark_ready(&thumb, key.clone(), 4096, "image/webp");

        assert_eq!(attachment.key_for(&thumb), &key);
        assert_eq!(attachment.ready_variants(), vec![thumb]);
        assert!(!attachment.is_complete(), "`card` is still pending");
    }

    /// A variant that will never generate is recorded rather than retried
    /// forever, and still falls back.
    #[tokio::test]
    async fn a_failed_variant_is_recorded_and_still_falls_back() {
        let (_, mut attachment) = attached().await;
        let thumb = Variant::new("thumb");
        attachment.mark_failed(&thumb, "the decoder rejected the image");

        assert!(matches!(
            attachment.variant(&thumb),
            Some(VariantState::Failed { .. }),
        ));
        assert_eq!(attachment.key_for(&thumb), attachment.key());
        assert!(attachment.ready_variants().is_empty());
    }

    /// The whole rendering loop, with an encoder that copies bytes. No image
    /// codec is a dependency, so what is tested here is the *seam*: read the
    /// original, hand it to something, store what comes back, record it.
    #[tokio::test]
    async fn the_rendering_seam_composes_into_a_job() {
        let (storage, mut attachment) = attached().await;

        let original = attachment
            .read_original(&storage)
            .await
            .expect("the original is readable");
        assert!(original.starts_with(b"\x89PNG"));

        for spec in Image::VARIANTS {
            let variant = Variant::dynamic(spec.name());
            // The identity "encoder", which is enough to prove the wiring.
            let rendition = Rendition::new(original.clone(), "png", "image/png");
            attachment
                .store_variant(&storage, &variant, rendition)
                .await
                .expect("the variant is stored");
        }

        assert!(attachment.is_complete());
        assert_eq!(attachment.ready_variants().len(), 2);
        assert_eq!(
            attachment.key_for(&Variant::new("thumb")).as_str(),
            "products/prd_123/thumb.png",
        );
        assert_eq!(
            storage.keys(),
            vec![
                "products/prd_123/card.png".to_owned(),
                "products/prd_123/original.png".to_owned(),
                "products/prd_123/thumb.png".to_owned(),
            ],
        );

        // And the size recorded is the one the backend reported, not one the
        // caller passed in and could get wrong.
        match attachment.variant(&Variant::new("thumb")) {
            Some(VariantState::Ready {
                size, content_type, ..
            }) => {
                assert_eq!(*size, original.len() as u64);
                assert_eq!(content_type, "image/png");
            }
            other => panic!("expected a ready variant, got {other:?}"),
        }
    }

    /// An encoder that refuses is a recorded outcome and not an error the job
    /// retries, and the descriptor still falls back to the original.
    #[tokio::test]
    async fn an_encoder_that_refuses_leaves_the_variant_failed() {
        let (storage, mut attachment) = attached().await;
        let thumb = Variant::new("thumb");

        // The shape of the job's `Err` arm.
        attachment.mark_failed(&thumb, "this encoder does not do Reencode");

        assert!(!attachment.is_complete());
        assert_eq!(attachment.key_for(&thumb), attachment.key());
        assert_eq!(storage.len(), 1, "nothing was written for a failed encode");
    }

    /// An orphaned object costs money forever, so purging takes the original
    /// and every rendition.
    #[tokio::test]
    async fn purging_removes_the_original_and_every_ready_variant() {
        let (storage, mut attachment) = attached().await;

        let thumb_key = StorageKey::new("products/prd_123/thumb.webp").expect("valid");
        storage
            .put(
                &thumb_key,
                crate::stream_from_bytes(bytes::Bytes::from_static(b"thumb")),
                crate::PutOpts::new("image/webp").trust_content_type(),
            )
            .await
            .expect("stores");
        attachment.mark_ready(&Variant::new("thumb"), thumb_key, 5, "image/webp");

        attachment.purge(&storage).await.expect("purges");
        assert!(storage.is_empty(), "{:?}", storage.keys());
    }

    /// The column stores the descriptor as JSON, so it has to survive one.
    #[tokio::test]
    async fn an_attachment_round_trips_through_json() {
        let (_, mut attachment) = attached().await;
        attachment.mark_ready(
            &Variant::new("thumb"),
            StorageKey::new("products/prd_123/thumb.webp").expect("valid"),
            4096,
            "image/webp",
        );

        let json = serde_json::to_string(&attachment).expect("serialises");
        let back: Attachment<Image> = serde_json::from_str(&json).expect("deserialises");

        assert_eq!(back.key(), attachment.key());
        assert_eq!(back.size(), attachment.size());
        assert_eq!(back.ready_variants(), vec![Variant::new("thumb")]);
        assert_eq!(
            back.key_for(&Variant::new("thumb")).as_str(),
            "products/prd_123/thumb.webp",
        );
    }

    /// The privacy default runs on the way in: an attached photograph does not
    /// publish where it was taken.
    #[tokio::test]
    async fn attaching_strips_exif_by_default() {
        let storage = crate::backend::MemoryStorage::new();

        let mut jpeg = vec![0xff_u8, 0xd8];
        let exif = b"Exif\x00\x00GPSLatitude 51.5";
        jpeg.extend_from_slice(&[0xff, 0xe1]);
        jpeg.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(exif);
        jpeg.extend_from_slice(&[0xff, 0xd9]);

        let upload = crate::Upload::<Image>::validated(
            "photo.jpg",
            "image/jpeg",
            bytes::Bytes::from(jpeg),
            crate::stream_from_bytes(bytes::Bytes::new()),
            None,
        );
        let attachment = Attachment::attach(upload, &storage, "products/prd_1")
            .await
            .expect("attaches");

        let stored = crate::collect_bounded(
            storage.get(attachment.key()).await.expect("reads"),
            4096,
            "Image",
        )
        .await
        .expect("collects");
        assert!(!stored.windows(3).any(|window| window == b"GPS"));
    }
}
