//! One-shot engine-authorized Blossom upload for native consumers (#971).
//!
//! Before this module, a Swift/Kotlin app reaching Blossom through
//! [`crate::blossom`] had to orchestrate the full BUD-11 dance itself:
//! compute the exact-byte sha256, choose a `created_at`/`expiration`
//! window, build the kind:24242 draft, sign it, validate the signed
//! authorization, and keep the hash paired with the exact bytes it
//! authorized -- five separate calls a native app could get wrong in ways
//! Rust already makes unrepresentable ([`nmp::media::PreparedUpload`]).
//!
//! [`NmpEngine::upload_blossom`] is the one call: the app supplies only
//! product inputs (server, bytes, content type, description) and NMP owns
//! author/clock resolution, the exact-bytes/hash/draft binding, signing
//! through the engine's own registered signer, BUD-11 validation, and the
//! real HTTP upload. It returns [`crate::blossom::FfiBlobDescriptor`] --
//! the SAME verified-descriptor vocabulary [`crate::blossom::
//! FfiBlossomClient::upload`] already returns -- rather than a second
//! verified-asset type; the exact-bytes proof that descriptor's `sha256`
//! carries is exactly the one [`nmp::media::UploadedAsset`] proves (#898),
//! never a caller-suppliable claim.
//!
//! No author, event kind, tags, raw unsigned event, sign request, signed
//! authorization, timestamp, expiration, blob hash, or callback crosses
//! this boundary -- every one of those is resolved or generated inside
//! this one call.

use nostr::{PublicKey, Timestamp};

use crate::blossom::{
    auth_draft_error_to_ffi, auth_validation_error_to_ffi, descriptor_to_ffi,
    server_url_error_to_ffi, upload_error_to_ffi, FfiBlobDescriptor, FfiBlossomAuthError,
    FfiBlossomServerUrlError, FfiBlossomUploadError,
};
use crate::convert::sign_event_failure;
use crate::facade::NmpEngine;
use crate::types::FfiSignEventFailure;

/// A governed authorization lifetime for the one-shot upload: long enough
/// for a real HTTP round trip under load, short enough that a leaked or
/// logged authorization is not a standing credential. The app never
/// chooses this -- #971 explicitly removes `expiration` from the caller
/// surface.
const UPLOAD_AUTHORIZATION_LIFETIME_SECS: u64 = 300;

/// [`NmpEngine::upload_blossom`]'s exhaustive failure taxonomy. Each stage
/// keeps its own real upstream shape -- reused wholesale from
/// [`crate::blossom`] where that stage already has one -- rather than
/// flattening five domains into new prose. Every variant traces to a
/// concrete Rust-side path; none stands in for "something went wrong".
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiUploadBlossomError {
    /// No current account is selected.
    SignedOut,
    /// The engine is closed.
    EngineClosed,
    /// `content_type` was empty -- NIP-68 imeta requires a mime type, and
    /// this is refused before any signing or I/O
    /// (`nmp::media::PrepareError::EmptyMimeType` mirror).
    EmptyContentType,
    /// The BUD-11 draft could not be built, or the signed authorization
    /// failed validation -- `nmp::media::PrepareError::Authorization` and
    /// `nmp::blossom::AuthValidationError` share this one taxonomy exactly
    /// as [`crate::blossom::FfiBlossomAuthError`] already does for the
    /// low-level doors.
    Authorization { error: FfiBlossomAuthError },
    /// The engine's registered signer rejected, was unavailable, or
    /// produced an invalid result -- the exact same taxonomy
    /// [`crate::facade::NmpSignEventHandle::signed`] surfaces.
    Sign { error: FfiSignEventFailure },
    /// `server_url` failed admission.
    InvalidServerUrl { error: FfiBlossomServerUrlError },
    /// The HTTP client could not be constructed.
    ClientBuild { reason: String },
    /// The real upload failed -- the exact same taxonomy
    /// [`crate::blossom::FfiBlossomClient::upload`] surfaces.
    Upload { error: FfiBlossomUploadError },
}

impl std::fmt::Display for FfiUploadBlossomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignedOut => f.write_str("no current account is selected"),
            Self::EngineClosed => f.write_str("the engine is closed"),
            Self::EmptyContentType => {
                f.write_str("cannot upload with an empty content type: imeta requires `m`")
            }
            Self::Authorization { error } => write!(f, "authorization failed: {error}"),
            Self::Sign { error } => write!(f, "signing failed: {error}"),
            Self::InvalidServerUrl { error } => write!(f, "invalid server url: {error:?}"),
            Self::ClientBuild { reason } => {
                write!(f, "Blossom HTTP client construction failed: {reason}")
            }
            Self::Upload { error } => write!(f, "upload failed: {error}"),
        }
    }
}

impl std::error::Error for FfiUploadBlossomError {}

/// Cancel the engine's sign-only operation if this scope is left before a
/// result was ever received -- e.g. the caller's async task is cancelled
/// while awaiting the signer. `SignEventCancel::cancel` is documented
/// idempotent and safe after completion, so this never needs to be
/// "defused" on the success path.
struct CancelSignOnDrop(nmp::SignEventCancel);

impl Drop for CancelSignOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn sign_start_error(error: nmp::SignEventError) -> FfiUploadBlossomError {
    match error {
        nmp::SignEventError::NoCurrentSigningProvider => FfiUploadBlossomError::SignedOut,
        nmp::SignEventError::EngineClosed | nmp::SignEventError::Cancelled => {
            FfiUploadBlossomError::EngineClosed
        }
        nmp::SignEventError::InvalidRequest { .. }
        | nmp::SignEventError::SignerUnavailable { .. }
        | nmp::SignEventError::SignerRejected { .. }
        | nmp::SignEventError::InvalidSignerOutput { .. } => FfiUploadBlossomError::Sign {
            error: sign_event_failure(error),
        },
    }
}

#[uniffi::export]
impl NmpEngine {
    /// Upload `blob` to `server_url` end to end: hash the exact bytes,
    /// build and sign a BUD-11 `upload` authorization through the current
    /// account's registered signer, validate it, and perform the real
    /// `PUT /upload` -- all as one call. `content_type` becomes the imeta
    /// `m` a later NIP-68 composition step requires and must be non-empty.
    /// `description` is the human-readable BUD-11 authorization reason
    /// shown to the relay/server operator, not the eventual post text.
    ///
    /// Returns the [`FfiBlobDescriptor`] whose `sha256` was proven equal to
    /// the hash of exactly the bytes uploaded (#898) -- the same
    /// exact-bytes guarantee `nmp::media::UploadedAsset` carries in Rust,
    /// projected as the descriptor vocabulary already crossing this
    /// boundary rather than a second verified-asset type.
    pub async fn upload_blossom(
        &self,
        server_url: String,
        blob: Vec<u8>,
        content_type: String,
        description: String,
    ) -> Result<FfiBlobDescriptor, FfiUploadBlossomError> {
        let author: PublicKey = self
            .engine
            .session()
            .map_err(|_| FfiUploadBlossomError::EngineClosed)?
            .current_pubkey
            .ok_or(FfiUploadBlossomError::SignedOut)?;

        let server = nmp::blossom::BlossomServerUrl::parse(&server_url).map_err(|error| {
            FfiUploadBlossomError::InvalidServerUrl {
                error: server_url_error_to_ffi(error),
            }
        })?;

        let created_at = Timestamp::now();
        let expiration = Timestamp::from(created_at.as_secs() + UPLOAD_AUTHORIZATION_LIFETIME_SECS);
        let prepared = nmp::media::prepare(
            blob,
            content_type,
            author,
            created_at,
            expiration,
            &description,
        )
        .map_err(|error| match error {
            nmp::media::PrepareError::EmptyMimeType => FfiUploadBlossomError::EmptyContentType,
            nmp::media::PrepareError::Authorization(error) => {
                FfiUploadBlossomError::Authorization {
                    error: auth_draft_error_to_ffi(error),
                }
            }
        })?;

        // Sign the BUD-11 draft through the engine's own registered signer,
        // inline -- no handle, no cancellation token, no draft crosses this
        // boundary (#971's own "must not accept a sign request or raw
        // unsigned event" requirement).
        let draft = prepared.authorization_draft();
        let request = nmp::SignEventRequest {
            created_at: draft.created_at,
            kind: draft.kind,
            tags: draft.tags.clone().to_vec(),
            content: draft.content.clone(),
        };
        let (sender, receiver) = nmp::fifo_channel::<Result<nmp::Event, nmp::SignEventError>>();
        let cancel = self
            .engine
            .sign_event_with_completion(request, move |result| {
                sender.send(result);
            })
            .map_err(sign_start_error)?;
        let _cancel_guard = CancelSignOnDrop(cancel);
        let signed = match receiver.into_async().next().await {
            Ok(Some(Ok(event))) => event,
            Ok(Some(Err(error))) => return Err(sign_start_error(error)),
            Ok(None) | Err(_) => {
                return Err(FfiUploadBlossomError::Sign {
                    error: FfiSignEventFailure::AlreadyConsumed,
                })
            }
        };

        let auth = nmp::blossom::SignedAuthorization::validate(
            signed,
            &nmp::blossom::ExpectedAuthorization {
                verb: nmp::blossom::BlossomVerb::Upload,
                blob: Some(prepared.sha256()),
            },
            created_at,
        )
        .map_err(|error| FfiUploadBlossomError::Authorization {
            error: auth_validation_error_to_ffi(error),
        })?;

        let client = nmp::blossom::BlossomClient::new(nmp::blossom::BlossomClientConfig::default())
            .map_err(|error| FfiUploadBlossomError::ClientBuild {
                reason: error.reason,
            })?;
        // `reqwest`'s async transport needs an actual tokio I/O reactor
        // polling it, which uniffi's Swift/Kotlin async bridge does not
        // provide for an exported `async fn`. #704 replaced the old
        // per-fetch throwaway runtime this exact problem used to need
        // (`relay_information_service.rs`'s deleted `http_runtime()`) with
        // spawning the I/O-bound work as a real task on the engine's own
        // shared multi-thread runtime and awaiting the `JoinHandle` --
        // which works correctly whether or not THIS function's own caller
        // happens to be inside a reactor already (a plain `block_on` would
        // panic when it is, e.g. under `#[tokio::test]`).
        let runtime = self
            .engine
            .adapter_runtime()
            .map_err(|_| FfiUploadBlossomError::EngineClosed)?;
        let asset = runtime
            .spawn(async move { prepared.upload(&client, &server, &auth).await })
            .await
            .map_err(|_| FfiUploadBlossomError::EngineClosed)?
            .map_err(|error| match error {
                nmp::media::MediaUploadError::Blossom(error) => FfiUploadBlossomError::Upload {
                    error: upload_error_to_ffi(error),
                },
            })?;

        Ok(descriptor_to_ffi(asset.descriptor().clone()))
    }
}
