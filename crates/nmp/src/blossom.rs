//! Blossom blob storage projected through the canonical facade (#1239).
//!
//! `nmp-blossom` owns the Blossom authorization drafts, the blob descriptor
//! codec, and the HTTP client that uploads, lists, mirrors and deletes. This
//! module re-exports that vocabulary so the ONE supported product surface owns
//! it for every consumer.
//!
//! The gap this closes has a live consumer: mosaico names `nmp-blossom` and
//! `nmp-asset` as two extra git dependencies in its own `Cargo.toml`, beside
//! the facade, to upload and verify attachments -- while a Swift app reaches
//! the same client by linking the one staticlib. Enabling the `blossom`
//! feature is still "names `nmp` alone".
//!
//! The authorization drafts are unsigned events the caller signs through the
//! ordinary signing path; the client is engine-free and holds no engine
//! handle, so nothing here is a second way to publish. Exact-byte identity
//! stays with its one owner (#884) and is re-exported at [`crate::asset`],
//! which the `blossom` feature turns on.

pub use nmp_blossom::{
    delete_authorization_draft, list_authorization_draft, upload_authorization_draft,
    AuthDraftError, AuthValidationError, BlobDescriptor, BlossomClient, BlossomClientConfig,
    BlossomServerUrl, BlossomVerb, ClientBuildError, DeleteError, DescriptorError,
    ExpectedAuthorization, ListError, ListPage, MirrorError, ServerUrlError, SignedAuthorization,
    UploadError, VerifiedUpload, DEFAULT_MAX_LIST_RESPONSE_BYTES, DEFAULT_MAX_RESPONSE_BYTES,
    DEFAULT_REQUEST_DEADLINE, MAX_DESCRIPTOR_BYTES,
};
