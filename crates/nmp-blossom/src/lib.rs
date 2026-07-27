//! `nmp-blossom` -- opt-in Blossom (BUD-01/02/04/11/12) blob core
//! (#545 upload + #551 mirror/delete/list, epic #216 T15-A-BLOSSOM). Per
//! `docs/design/protocol-modules-and-composition.md` §3: "Blossom uploads
//! bytes, verifies them, and returns an asset reference" -- and blob
//! operation failure and Nostr publication failure are DISTINCT results,
//! which is why this crate's per-operation taxonomies ([`UploadError`],
//! [`MirrorError`], [`DeleteError`], [`ListError`]) exist apart from any
//! engine receipt stream, and apart from each other.
//!
//! Engine-free, kind-agnostic core discipline (the `nmp-nip29` template):
//! this crate NEVER signs -- the draft builders
//! ([`upload_authorization_draft`], [`delete_authorization_draft`],
//! [`list_authorization_draft`]) emit an [`nostr::UnsignedEvent`] and the
//! caller signs it with the existing `nmp-signer` machinery, because
//! signing and publishing are orthogonal stages (#47/#32) -- and it never
//! touches the engine: no router, resolver, store, or engine dependency.
//! Its HTTP client reimplements the engine's NIP-11 admission discipline
//! from `nmp-transport`'s public pure classifiers (see `client.rs`).
//!
//! Exact-byte identity is NOT owned here (#884): [`nmp_asset::Sha256Hash`]
//! and the [`nmp_asset::VerifiedAsset`] witness live in the protocol-neutral
//! `nmp-asset` crate, and this crate re-exports neither. Only
//! [`BlossomClient::upload`] observes the exact bytes, so only `upload` can
//! mint a [`VerifiedUpload`] carrying that witness; `mirror` (which never
//! sees the bytes) returns the server's integrity-gated [`BlobDescriptor`]
//! and nothing stronger.
//!
//! This unit covers the BUD-11 verbs (upload, BUD-04 mirror, BUD-12
//! delete/list); the `get`/`media` endpoints, platform projection, and
//! the NIP-68/composition layers are tracked follow-ups under epic #216
//! (see `docs/known-gaps.md`).

mod auth;
mod client;
mod descriptor;

pub use auth::{
    delete_authorization_draft, list_authorization_draft, upload_authorization_draft,
    AuthDraftError, AuthValidationError, BlossomVerb, ExpectedAuthorization, SignedAuthorization,
};
pub use client::{
    BlossomClient, BlossomClientConfig, BlossomServerUrl, ClientBuildError, DeleteError, ListError,
    ListPage, MirrorError, ServerUrlError, UploadError, VerifiedUpload,
    DEFAULT_MAX_LIST_RESPONSE_BYTES, DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_REQUEST_DEADLINE,
};
pub use descriptor::{BlobDescriptor, DescriptorError, MAX_DESCRIPTOR_BYTES};
