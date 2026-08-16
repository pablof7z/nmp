//! `nmp-nip68` -- the opt-in NIP-68 picture-first (kind:20) protocol crate
//! (#558, epic #216 T15-B-NIP68-IMETA).
//!
//! Per `docs/design/protocol-modules-and-composition.md` §3, this crate OWNS
//! the NIP-68 photo event schema exclusively: "Composition does not transfer
//! ownership: a context owner may wrap an artifact, but only the artifact owner
//! may define the artifact." It builds an immutable UNSIGNED kind:20 draft from
//! content-addressed image artifacts (an [`nmp_asset::VerifiedAsset`] proven
//! from exact bytes, #898) and decodes a kind:20 event into typed picture
//! facts. This crate names no storage-protocol crate: minting an artifact
//! needs only the protocol-neutral proof, never the Blossom upload/mirror
//! machinery that produced it.
//!
//! Same discipline as `nmp-nip29`/`nmp-blossom`: this crate NEVER signs (it
//! emits an [`nostr::UnsignedEvent`] for the caller's existing `nmp-signer`
//! machinery -- signing and publishing are orthogonal stages, #47/#32) and
//! NEVER touches the engine (no router/resolver/store/engine dependency).
//!
//! Artifact provenance is STRUCTURAL: [`PictureImage`] carries `url`/`m`/`x` by
//! construction (private fields, provenance-only constructors), a descriptor
//! without a mime type cannot mint one, and a spec with zero images is refused
//! -- the #421 "protected kind without artifact provenance fails" contract.
//!
//! FFI/Swift/Kotlin projection and the T15-C upload->build->sign->publish
//! composition seam (#559) are SEPARATE later units -- see `docs/known-gaps.md`.

mod build;
mod decode;
mod image;

pub use build::{build_picture, ContentWarning, PictureBuildError, PictureSpec, PICTURE_KIND};
pub use decode::{
    decode_picture, decode_picture_from_raw, DecodedImage, Picture, PictureDiagnostic,
};
pub use image::{ImageDim, ImageDimError, PictureImage, PictureImageError};
