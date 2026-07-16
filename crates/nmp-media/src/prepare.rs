//! Stage 1 of the composition seam: turn raw image bytes into a
//! [`PreparedUpload`] -- the value that binds the exact bytes to the exact
//! kind:24242 authorization the app is about to sign (#559, epic #216
//! T15-C-MEDIA-COMPOSITION).
//!
//! # The safety invariant (this crate's real contribution)
//! A [`PreparedUpload`] OWNS the exact bytes it hashed. [`prepare`] computes
//! `Sha256Hash::of(&bytes)` ONCE, builds the BUD-11 `upload` authorization
//! draft bound to THAT hash, and holds the bytes, the hash, and the draft
//! together. The upload stage ([`PreparedUpload::upload`]) then uploads THOSE
//! held bytes -- so it is IMPOSSIBLE to authorize the hash of bytes A and
//! then upload bytes B. Separating "compute a hash", "authorize a hash", and
//! "upload some bytes" into three independent calls is exactly how that
//! mismatch happens; binding them in one owned value makes it unrepresentable.
//!
//! This crate NEVER signs: [`PreparedUpload::authorization_draft`] returns an
//! [`nostr::UnsignedEvent`] the app signs with its own `nmp-signer` machinery
//! (signing is UPSTREAM of the seam -- signing and publishing are orthogonal
//! stages, #47/#32).

use nostr::{PublicKey, Timestamp, UnsignedEvent};

use nmp_blossom::{upload_authorization_draft, AuthDraftError, Sha256Hash};

/// A prepared, not-yet-uploaded image: the exact bytes, their sha256, the
/// mime type, and the kind:24242 `upload` authorization draft bound to that
/// exact hash. Construct with [`prepare`]; sign
/// [`Self::authorization_draft`], then spend the value with
/// [`Self::upload`](crate::PreparedUpload::upload).
///
/// The fields are PRIVATE (`bytes`/`mime_type` are crate-visible only so the
/// sibling upload stage can send the HELD bytes): an external caller can
/// neither read the raw bytes back out nor construct a `PreparedUpload` whose
/// authorization binds a different hash than the bytes it carries.
#[derive(Debug)]
pub struct PreparedUpload {
    /// The EXACT bytes that were hashed and authorized -- uploaded verbatim
    /// by the upload stage. Crate-visible so `upload.rs` can send precisely
    /// these bytes; never exposed publicly.
    pub(crate) bytes: Vec<u8>,
    /// The mime type carried into the upload request's `Content-Type`.
    /// Crate-visible for the same reason as `bytes`.
    pub(crate) mime_type: String,
    /// `Sha256Hash::of(&bytes)`, computed once at [`prepare`] time.
    sha256: Sha256Hash,
    /// The kind:24242 `upload` authorization draft bound to `sha256`. The app
    /// signs THIS (the crate never holds keys).
    authorization_draft: UnsignedEvent,
}

/// [`prepare`]'s failure modes. Exhaustive (no `#[non_exhaustive]`): a
/// prepared upload is refused early and with a typed reason. Each variant is
/// constructed by a falsifier in `tests/composition.rs`.
///
/// This is a DISTINCT type from [`crate::MediaUploadError`] and
/// [`crate::MediaComposeError`]: the three stages fail into three separate
/// domains and the type system keeps them un-mergeable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    /// NIP-68 imeta REQUIRES a mime type (`m`), so an empty mime is refused
    /// HERE -- early and typed -- rather than surfacing much later as a
    /// compose-stage provenance failure. (A whitespace-only or otherwise
    /// malformed mime is the caller's concern; this gate only refuses the
    /// empty string, the one value that can never carry `m`.)
    EmptyMimeType,
    /// The BUD-11 authorization draft could not be built -- e.g. an
    /// `expiration` at or before `created_at` (a window expired at birth).
    /// Wraps the upstream [`AuthDraftError`] as ONE typed variant so the
    /// exact clause is preserved without flattening it.
    Authorization(AuthDraftError),
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyMimeType => f.write_str(
                "cannot prepare an upload with an empty mime type: NIP-68 imeta requires `m`",
            ),
            Self::Authorization(error) => {
                write!(f, "cannot prepare the upload authorization: {error}")
            }
        }
    }
}

impl std::error::Error for PrepareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EmptyMimeType => None,
            Self::Authorization(error) => Some(error),
        }
    }
}

/// Prepare `bytes` for a standalone Blossom upload: hash the EXACT bytes,
/// build the kind:24242 `upload` authorization draft bound to that hash, and
/// hold bytes + hash + mime + draft together (the safety invariant -- see the
/// module doc). Refuses an empty `mime_type` and any
/// [`AuthDraftError`](nmp_blossom::AuthDraftError) (e.g.
/// `expiration <= created_at`) as typed [`PrepareError`]s.
///
/// The returned draft is UNSIGNED: the app signs
/// [`PreparedUpload::authorization_draft`] with its own signer, validates it
/// into a `nmp_blossom::SignedAuthorization`, and passes that to
/// [`PreparedUpload::upload`](crate::PreparedUpload::upload).
pub fn prepare(
    bytes: Vec<u8>,
    mime_type: impl Into<String>,
    author: PublicKey,
    created_at: Timestamp,
    expiration: Timestamp,
    description: &str,
) -> Result<PreparedUpload, PrepareError> {
    let mime_type = mime_type.into();
    if mime_type.is_empty() {
        return Err(PrepareError::EmptyMimeType);
    }
    // Hash the EXACT bytes ONCE; every downstream stage refers to this value.
    let sha256 = Sha256Hash::of(&bytes);
    let authorization_draft =
        upload_authorization_draft(author, sha256, created_at, expiration, description)
            .map_err(PrepareError::Authorization)?;
    Ok(PreparedUpload {
        bytes,
        mime_type,
        sha256,
        authorization_draft,
    })
}

impl PreparedUpload {
    /// The UNSIGNED kind:24242 `upload` authorization draft bound to exactly
    /// [`Self::sha256`]. The app signs THIS with its own signer (the crate
    /// never holds keys) and validates it into a
    /// `nmp_blossom::SignedAuthorization`.
    pub fn authorization_draft(&self) -> &UnsignedEvent {
        &self.authorization_draft
    }

    /// The sha256 of the exact held bytes -- the hash the authorization is
    /// bound to and the hash the upload stage will send bytes matching.
    pub fn sha256(&self) -> Sha256Hash {
        self.sha256
    }

    /// The mime type these bytes will be uploaded with (imeta `m`).
    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }
}
