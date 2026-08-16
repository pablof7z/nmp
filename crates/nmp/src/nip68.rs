//! NIP-68 picture-first (kind:20) vocabulary projected through the canonical
//! facade (#558, #1563).
//!
//! `nmp-nip68` owns the kind:20 photo event schema exclusively: it builds an
//! immutable UNSIGNED kind:20 draft from content-addressed image artifacts
//! (an [`nmp_asset::VerifiedAsset`] proven from exact bytes, #898) and
//! decodes a kind:20 event into typed picture facts. This module re-exports
//! that vocabulary so the ONE supported product surface owns it for every
//! consumer, matching every other #1239-retrofitted family.
//!
//! Read-only consumption (decoding a kind:20 seen on a relay) needs only this
//! feature. Composing and uploading a new picture from raw bytes is
//! `nmp-media`'s own seam (#1707) -- a separate crate, not a facade feature.
//!
//! The crate is engine-free by construction: neither building nor decoding a
//! picture touches the engine, router, resolver, or store.

pub use nmp_nip68::{
    build_picture, decode_picture, decode_picture_from_raw, ContentWarning, DecodedImage, ImageDim,
    ImageDimError, Picture, PictureBuildError, PictureDiagnostic, PictureImage, PictureImageError,
    PictureSpec, PICTURE_KIND,
};
