//! Stage 3 of the composition seam: pair each verified [`UploadedAsset`] with
//! its per-image presentation metadata and build the final NIP-68 kind:20
//! [`nostr::UnsignedEvent`] the app hands to the EXISTING publish path (#559,
//! epic #216 T15-C-MEDIA-COMPOSITION).
//!
//! This stage NEVER signs and NEVER publishes: it emits an unsigned kind:20
//! draft. The app signs it with `nmp-signer` and passes it to the existing
//! `publish()` -> WriteIntent path (DOWNSTREAM of this seam -- relay/publish
//! failure is a different domain from the [`MediaComposeError`] this stage
//! returns).
//!
//! Determinism (the Rust-side half of the cross-surface parity contract):
//! [`compose_picture`] with identical inputs yields identical tag structure
//! and content, because every value flows from the typed witnesses without
//! side channels. The direct/FFI `nmp-parity` oracle for the seam lands with
//! the later projection unit.

use nostr::{PublicKey, Timestamp, UnsignedEvent};

use nmp_nip68::{
    build_picture, ContentWarning, ImageDim, PictureBuildError, PictureImageError, PictureSpec,
};

use crate::upload::UploadedAsset;

/// One verified asset paired with its NIP-68 imeta presentation metadata --
/// the per-image optionals (`dim`/`alt`/`blurhash`/`thumbhash`/`fallback`)
/// that NIP-68 allows alongside the mandatory content-addressed `url`/`m`/`x`.
/// Construct with [`ComposedImage::new`] and layer optionals through the
/// builder setters (mirroring `nmp_nip68::PictureImage`'s own setters).
#[derive(Debug, Clone)]
pub struct ComposedImage {
    asset: UploadedAsset,
    dim: Option<ImageDim>,
    alt: Option<String>,
    blurhash: Option<String>,
    thumbhash: Option<String>,
    fallbacks: Vec<String>,
}

impl ComposedImage {
    /// Pair a verified [`UploadedAsset`] with (initially empty) presentation
    /// metadata. The mandatory `url`/`m`/`x` provenance comes from the asset's
    /// verified descriptor at compose time; only the optionals are set here.
    pub fn new(asset: UploadedAsset) -> Self {
        Self {
            asset,
            dim: None,
            alt: None,
            blurhash: None,
            thumbhash: None,
            fallbacks: Vec::new(),
        }
    }

    /// Attach pixel dimensions (imeta `dim`).
    pub fn with_dim(mut self, dim: ImageDim) -> Self {
        self.dim = Some(dim);
        self
    }

    /// Attach alt text (imeta `alt`).
    pub fn with_alt(mut self, alt: String) -> Self {
        self.alt = Some(alt);
        self
    }

    /// Attach a blurhash (imeta `blurhash`).
    pub fn with_blurhash(mut self, blurhash: String) -> Self {
        self.blurhash = Some(blurhash);
        self
    }

    /// Attach a thumbhash (imeta `thumbhash`).
    pub fn with_thumbhash(mut self, thumbhash: String) -> Self {
        self.thumbhash = Some(thumbhash);
        self
    }

    /// Append a fallback url (imeta `fallback`, repeatable).
    pub fn with_fallback(mut self, fallback: String) -> Self {
        self.fallbacks.push(fallback);
        self
    }
}

/// The event-level metadata for the composed kind:20 picture post: everything
/// that is NOT per-image. `images` is supplied separately to
/// [`compose_picture`]; this carries the `title`/`content-warning`/`t`
/// hashtags and the `content` description.
#[derive(Debug, Clone)]
pub struct PicturePost {
    /// Optional short title (`["title", ...]`).
    pub title: Option<String>,
    /// Free-text description of the post (the event `content`).
    pub description: String,
    /// Optional content warning (`["content-warning", reason?]`).
    pub content_warning: Option<ContentWarning>,
    /// Optional `t` hashtags (one `["t", tag]` each). Empties are refused
    /// downstream by `nmp_nip68::build_picture`.
    pub hashtags: Vec<String>,
}

/// [`compose_picture`]'s failure modes. A DISTINCT type from
/// [`crate::PrepareError`] and [`crate::MediaUploadError`]: a compose failure
/// can never be pattern-matched (or `?`-merged) as a prepare or upload
/// failure -- there is deliberately no `From` impl between these domains, so
/// `?` cannot silently merge them. Exhaustive (no `#[non_exhaustive]`); each
/// variant is constructed by a falsifier in `tests/composition.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaComposeError {
    /// No images were supplied -- a kind:20 picture with no image artifact is
    /// unrepresentable (#421). Refused HERE, before any nip68 assembly, so the
    /// empty-composition case is its own explicit outcome.
    NoImages,
    /// An asset could not mint a NIP-68 image artifact (its verified
    /// descriptor carried no mime type). The per-image provenance failure,
    /// preserving the upstream [`PictureImageError`].
    Image(PictureImageError),
    /// `nmp_nip68::build_picture` refused the assembled spec (e.g. an empty
    /// hashtag). Preserves the upstream [`PictureBuildError`]; distinct from
    /// [`Self::Image`] so a provenance failure and a draft-assembly failure
    /// stay separable.
    Build(PictureBuildError),
}

impl std::fmt::Display for MediaComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoImages => {
                f.write_str("cannot compose a NIP-68 kind:20 picture with zero image artifacts")
            }
            Self::Image(error) => write!(f, "compose stage: image provenance failed: {error}"),
            Self::Build(error) => write!(f, "compose stage: kind:20 assembly failed: {error}"),
        }
    }
}

impl std::error::Error for MediaComposeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoImages => None,
            Self::Image(error) => Some(error),
            Self::Build(error) => Some(error),
        }
    }
}

/// Build the final unsigned NIP-68 kind:20 draft from verified assets. For
/// each [`ComposedImage`], mints a `nmp_nip68::PictureImage` from the asset's
/// verified descriptor (provenance failure -> [`MediaComposeError::Image`])
/// and layers its optionals, then assembles a `nmp_nip68::PictureSpec` and
/// calls `nmp_nip68::build_picture` (assembly failure ->
/// [`MediaComposeError::Build`]). Refuses an empty `images` with
/// [`MediaComposeError::NoImages`].
///
/// The returned [`UnsignedEvent`] is what the app signs and hands to the
/// EXISTING `publish()` -> WriteIntent path -- publication is NOT performed
/// here (a different failure domain).
///
/// `created_at` is explicit (never `now()`) so the composed body is
/// deterministic and byte-identical across the direct and FFI surfaces --
/// the cross-surface parity contract (#559); the caller supplies the same
/// stamp it will publish under.
pub fn compose_picture(
    author: PublicKey,
    created_at: Timestamp,
    images: Vec<ComposedImage>,
    post: &PicturePost,
) -> Result<UnsignedEvent, MediaComposeError> {
    if images.is_empty() {
        return Err(MediaComposeError::NoImages);
    }

    let mut picture_images = Vec::with_capacity(images.len());
    for composed in images {
        // Provenance is re-derived from the verified descriptor at compose
        // time (url/m/x by construction, #421); a missing mime is a typed
        // per-image failure, never a silently dropped image.
        let mut image = composed
            .asset
            .picture_image()
            .map_err(MediaComposeError::Image)?;
        if let Some(dim) = composed.dim {
            image = image.with_dim(dim);
        }
        if let Some(alt) = composed.alt {
            image = image.with_alt(alt);
        }
        if let Some(blurhash) = composed.blurhash {
            image = image.with_blurhash(blurhash);
        }
        if let Some(thumbhash) = composed.thumbhash {
            image = image.with_thumbhash(thumbhash);
        }
        for fallback in composed.fallbacks {
            image = image.with_fallback(fallback);
        }
        picture_images.push(image);
    }

    let spec = PictureSpec {
        images: picture_images,
        description: post.description.clone(),
        title: post.title.clone(),
        content_warning: post.content_warning.clone(),
        hashtags: post.hashtags.clone(),
    };

    build_picture(author, created_at, &spec).map_err(MediaComposeError::Build)
}
