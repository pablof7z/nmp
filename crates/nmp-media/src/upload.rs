//! Stage 2 of the composition seam: spend a [`PreparedUpload`] by performing
//! the STANDALONE async Blossom `PUT /upload` of its HELD bytes, yielding an
//! [`UploadedAsset`] (#559, epic #216 T15-C-MEDIA-COMPOSITION).
//!
//! This is the STANDALONE upload (Option 1). The engine-integrated DURABLE
//! upload -- persisted intent, reattachable receipt, crash-safety -- is the
//! additive #562 obligation whose witness types are identical to these; it is
//! NOT built here.
//!
//! The upload sends the bytes [`PreparedUpload`] hashed and authorized, so the
//! uploaded-bytes/authorized-hash pairing is structurally correct: the
//! underlying `nmp_blossom::BlossomClient::upload` re-hashes the bytes and
//! refuses (`UploadError::AuthorizationBlobMismatch`) unless the supplied
//! authorization binds exactly that hash -- and because we hand it the HELD
//! bytes, a substitution can only be caught, never sneak through.

use nmp_blossom::{
    BlobDescriptor, BlossomClient, BlossomServerUrl, Sha256Hash, SignedAuthorization, UploadError,
    VerifiedUpload,
};
use nmp_nip68::{PictureImage, PictureImageError};

use crate::prepare::PreparedUpload;

/// [`PreparedUpload::upload`]'s failure. A DISTINCT type from
/// [`crate::PrepareError`] and [`crate::MediaComposeError`]: an upload failure
/// can never be pattern-matched (or `?`-merged) as a prepare or compose
/// failure. Exhaustive (no `#[non_exhaustive]`).
///
/// The single [`Self::Blossom`] variant PRESERVES the whole separated Blossom
/// [`UploadError`] taxonomy rather than re-collapsing it: the caller still
/// sees `AuthorizationBlobMismatch`, `Sha256Mismatch`, `AuthRejected`,
/// `ServerError`, ... exactly as `nmp-blossom` distinguishes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaUploadError {
    /// The standalone Blossom upload failed. Carries the upstream
    /// [`UploadError`] verbatim -- the separated blob-operation taxonomy is
    /// never flattened into media-layer strings.
    Blossom(UploadError),
}

impl std::fmt::Display for MediaUploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blossom(error) => write!(f, "Blossom upload stage failed: {error}"),
        }
    }
}

impl std::error::Error for MediaUploadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Blossom(error) => Some(error),
        }
    }
}

/// A blob that has been uploaded and integrity-verified by Blossom: it wraps a
/// `nmp_blossom::VerifiedUpload`, whose descriptor's sha256 was PROVEN equal
/// to the uploaded bytes. Private field: an `UploadedAsset` exists only by
/// spending a [`PreparedUpload`] through [`PreparedUpload::upload`] (the
/// verified upload witness is not forgeable here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedAsset {
    verified: VerifiedUpload,
}

impl PreparedUpload {
    /// Perform the standalone async Blossom `PUT /upload` of the HELD bytes,
    /// consuming `self` -- a prepared upload is a used-once obligation, spent
    /// by uploading. Passes the held bytes and mime type to
    /// `nmp_blossom::BlossomClient::upload`; on success wraps the returned
    /// `VerifiedUpload` into an [`UploadedAsset`], on failure returns the
    /// upstream [`UploadError`] inside [`MediaUploadError::Blossom`].
    ///
    /// Because the bytes sent are exactly the bytes [`prepare()`](crate::prepare())
    /// hashed, `authorization` MUST bind that same hash or the client refuses
    /// with `UploadError::AuthorizationBlobMismatch` -- the seam cannot upload
    /// bytes an authorization did not cover.
    pub async fn upload(
        self,
        client: &BlossomClient,
        server: &BlossomServerUrl,
        authorization: &SignedAuthorization,
    ) -> Result<UploadedAsset, MediaUploadError> {
        let verified = client
            .upload(server, &self.bytes, Some(&self.mime_type), authorization)
            .await
            .map_err(MediaUploadError::Blossom)?;
        Ok(UploadedAsset { verified })
    }
}

impl UploadedAsset {
    /// The integrity-verified BUD-02 blob descriptor.
    pub fn descriptor(&self) -> &BlobDescriptor {
        self.verified.descriptor()
    }

    /// The content-addressed sha256 the server's descriptor was verified
    /// against -- the blob's identity.
    pub fn sha256(&self) -> Sha256Hash {
        self.verified.descriptor().sha256
    }

    /// Mint a NIP-68 [`PictureImage`] artifact reference from this verified
    /// asset (delegates to `PictureImage::from_verified_upload`). Fails with
    /// [`PictureImageError::MissingMimeType`] if the server's descriptor
    /// carried no mime type -- NIP-68 imeta requires `m`.
    pub fn picture_image(&self) -> Result<PictureImage, PictureImageError> {
        PictureImage::from_verified_upload(&self.verified)
    }

    /// Consume into a NIP-68 [`PictureImage`] artifact reference. Same
    /// provenance rule as [`Self::picture_image`].
    pub fn into_picture_image(self) -> Result<PictureImage, PictureImageError> {
        PictureImage::from_verified_upload(&self.verified)
    }
}
