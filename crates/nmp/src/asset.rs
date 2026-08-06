//! Exact-byte asset identity projected through the canonical facade (#1239).
//!
//! `nmp-asset` is #884's ONE protocol-neutral owner of "these are the bytes I
//! meant": a [`Sha256Hash`] is computed from bytes, parsed from lowercase hex,
//! and compared. It is not a protocol family and owns no schema, no network
//! client and no storage.
//!
//! It has its own feature because verifying received bytes is worth doing
//! without speaking to a Blossom server -- mosaico hashes a received
//! attachment against the digest a message claimed, on a path that contacts
//! nothing. The `blossom` feature turns this one on, because
//! `BlobDescriptor`/`VerifiedUpload` name [`Sha256Hash`] in their public
//! signatures and a door whose own return types the caller cannot name is
//! still a second Cargo dependency.

pub use nmp_asset::{Sha256Hash, Sha256HexError, VerifiedAsset};
