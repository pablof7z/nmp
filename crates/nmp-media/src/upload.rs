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

use nmp_asset::{Sha256Hash, VerifiedAsset};
use nmp_blossom::{
    BlobDescriptor, BlossomClient, BlossomServerUrl, SignedAuthorization, UploadError,
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
/// `nmp_blossom::VerifiedUpload`, which carries the protocol-neutral
/// [`VerifiedAsset`] proof computed from the exact uploaded bytes (#884).
/// Private field: an `UploadedAsset` exists only by spending a
/// [`PreparedUpload`] through [`PreparedUpload::upload`] (the verified upload
/// witness is not forgeable here).
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

    /// The protocol-neutral exact-byte proof for the uploaded blob (#884).
    /// Its digest was computed from the bytes this seam HELD and uploaded,
    /// not read out of the server's descriptor; `url`/`mime_type` on it stay
    /// untrusted server text.
    pub fn asset(&self) -> &VerifiedAsset {
        self.verified.asset()
    }

    /// The content-addressed sha256 of the uploaded bytes -- the blob's
    /// identity. Read from the exact-byte proof, not from the server's
    /// descriptor claim (the integrity gate has already proven the two
    /// agree).
    pub fn sha256(&self) -> Sha256Hash {
        self.verified.asset().sha256()
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
