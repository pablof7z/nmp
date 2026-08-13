//! Bounded, exactly fenced encrypted-payload capability requests.
//!
//! This module owns only the generic envelope. Protocol capabilities still
//! choose the scheme, peer, plaintext codec, and migration policy. Engine
//! integration persists the semantic operation and reconstructs a request;
//! this layer makes a result for any other source or target unusable.

use std::time::Duration;

use zeroize::Zeroize;

use crate::{DecryptCapability, EncryptCapability, SignerError, SignerOp, SignerPublicKey};

/// Encryption selected by the conceptual capability for this exact request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadEncryption {
    Nip04,
    Nip44V2,
}

/// Finite byte ceilings declared by the conceptual capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayloadLimits {
    ciphertext_bytes: u32,
    plaintext_bytes: u32,
}

impl PayloadLimits {
    #[must_use]
    pub const fn new(ciphertext_bytes: u32, plaintext_bytes: u32) -> Self {
        Self {
            ciphertext_bytes,
            plaintext_bytes,
        }
    }

    #[must_use]
    pub const fn ciphertext_bytes(self) -> u32 {
        self.ciphertext_bytes
    }

    #[must_use]
    pub const fn plaintext_bytes(self) -> u32 {
        self.plaintext_bytes
    }
}

/// Exact authority under which one crypto result may be applied.
///
/// A receipt id is intentionally absent: one receipt can produce successive
/// materializations. The fence instead names the verified source, coordinate,
/// target revision, and exact normalized operation program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PayloadFence {
    source_event_id: [u8; 32],
    target_coordinate: [u8; 32],
    target_revision: u64,
    operation_digest: [u8; 32],
}

impl PayloadFence {
    #[must_use]
    pub const fn new(
        source_event_id: [u8; 32],
        target_coordinate: [u8; 32],
        target_revision: u64,
        operation_digest: [u8; 32],
    ) -> Self {
        Self {
            source_event_id,
            target_coordinate,
            target_revision,
            operation_digest,
        }
    }
}

/// One take-once decrypted plaintext allocation.
///
/// Deliberately neither `Clone`, `Debug`, `Display`, nor serializable. Drop
/// wipes the entire allocation through capacity, including unwind and stale
/// result paths.
pub struct TransientPlaintext(Vec<u8>);

impl TransientPlaintext {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn as_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.0)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for TransientPlaintext {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Provider-returned ciphertext. It is opaque to the generic envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedPayload(String);

impl EncryptedPayload {
    #[must_use]
    pub fn new(ciphertext: String) -> Self {
        Self(ciphertext)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Capability input after the envelope has admitted its ciphertext bound.
pub struct DecryptPayloadRequest {
    scheme: PayloadEncryption,
    peer: SignerPublicKey,
    ciphertext: String,
}

impl DecryptPayloadRequest {
    #[must_use]
    pub fn new(scheme: PayloadEncryption, peer: SignerPublicKey, ciphertext: String) -> Self {
        Self {
            scheme,
            peer,
            ciphertext,
        }
    }

    #[must_use]
    pub const fn scheme(&self) -> PayloadEncryption {
        self.scheme
    }

    #[must_use]
    pub const fn peer(&self) -> SignerPublicKey {
        self.peer
    }

    #[must_use]
    pub fn ciphertext(&self) -> &str {
        &self.ciphertext
    }

    #[must_use]
    pub fn into_parts(self) -> (PayloadEncryption, SignerPublicKey, String) {
        (self.scheme, self.peer, self.ciphertext)
    }
}

/// Capability input carrying the one transient plaintext owner.
pub struct EncryptPayloadRequest {
    scheme: PayloadEncryption,
    peer: SignerPublicKey,
    plaintext: TransientPlaintext,
}

impl EncryptPayloadRequest {
    #[must_use]
    pub fn new(
        scheme: PayloadEncryption,
        peer: SignerPublicKey,
        plaintext: TransientPlaintext,
    ) -> Self {
        Self {
            scheme,
            peer,
            plaintext,
        }
    }

    #[must_use]
    pub const fn scheme(&self) -> PayloadEncryption {
        self.scheme
    }

    #[must_use]
    pub const fn peer(&self) -> SignerPublicKey {
        self.peer
    }

    #[must_use]
    pub fn plaintext(&self) -> &TransientPlaintext {
        &self.plaintext
    }

    #[must_use]
    pub fn into_parts(self) -> (PayloadEncryption, SignerPublicKey, TransientPlaintext) {
        (self.scheme, self.peer, self.plaintext)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PayloadError {
    CiphertextTooLarge { actual: usize, max: u32 },
    PlaintextTooLarge { actual: usize, max: u32 },
    Capability(SignerError),
}

impl From<SignerError> for PayloadError {
    fn from(error: SignerError) -> Self {
        Self::Capability(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StalePayloadResult;

pub struct FencedPlaintext {
    fence: PayloadFence,
    plaintext: TransientPlaintext,
}

impl FencedPlaintext {
    pub fn accept(self, current: PayloadFence) -> Result<TransientPlaintext, StalePayloadResult> {
        if self.fence == current {
            Ok(self.plaintext)
        } else {
            Err(StalePayloadResult)
        }
    }
}

pub struct FencedCiphertext {
    fence: PayloadFence,
    ciphertext: EncryptedPayload,
}

impl FencedCiphertext {
    pub fn accept(self, current: PayloadFence) -> Result<EncryptedPayload, StalePayloadResult> {
        if self.fence == current {
            Ok(self.ciphertext)
        } else {
            Err(StalePayloadResult)
        }
    }
}

pub struct DecryptOperation {
    fence: PayloadFence,
    plaintext_limit: u32,
    operation: SignerOp<TransientPlaintext>,
}

impl DecryptOperation {
    pub fn wait(self, timeout: Duration) -> Result<FencedPlaintext, PayloadError> {
        finish_plaintext(
            self.fence,
            self.plaintext_limit,
            self.operation.wait(timeout),
        )
    }

    pub async fn recv_async(self) -> Result<FencedPlaintext, PayloadError> {
        let fence = self.fence;
        let limit = self.plaintext_limit;
        finish_plaintext(fence, limit, self.operation.recv_async().await)
    }
}

pub struct EncryptOperation {
    fence: PayloadFence,
    ciphertext_limit: u32,
    operation: SignerOp<EncryptedPayload>,
}

impl EncryptOperation {
    pub fn wait(self, timeout: Duration) -> Result<FencedCiphertext, PayloadError> {
        finish_ciphertext(
            self.fence,
            self.ciphertext_limit,
            self.operation.wait(timeout),
        )
    }

    pub async fn recv_async(self) -> Result<FencedCiphertext, PayloadError> {
        let fence = self.fence;
        let limit = self.ciphertext_limit;
        finish_ciphertext(fence, limit, self.operation.recv_async().await)
    }
}

/// Runtime-free dispatcher for bounded encrypted-payload work.
pub struct EncryptedPayloadService;

impl EncryptedPayloadService {
    pub fn decrypt(
        capability: &dyn DecryptCapability,
        fence: PayloadFence,
        scheme: PayloadEncryption,
        peer: SignerPublicKey,
        ciphertext: String,
        limits: PayloadLimits,
    ) -> Result<DecryptOperation, PayloadError> {
        if ciphertext.len() > limits.ciphertext_bytes as usize {
            return Err(PayloadError::CiphertextTooLarge {
                actual: ciphertext.len(),
                max: limits.ciphertext_bytes,
            });
        }
        let operation = capability.decrypt(DecryptPayloadRequest::new(scheme, peer, ciphertext));
        Ok(DecryptOperation {
            fence,
            plaintext_limit: limits.plaintext_bytes,
            operation,
        })
    }

    pub fn encrypt(
        capability: &dyn EncryptCapability,
        fence: PayloadFence,
        scheme: PayloadEncryption,
        peer: SignerPublicKey,
        plaintext: TransientPlaintext,
        limits: PayloadLimits,
    ) -> Result<EncryptOperation, PayloadError> {
        if plaintext.len() > limits.plaintext_bytes as usize {
            return Err(PayloadError::PlaintextTooLarge {
                actual: plaintext.len(),
                max: limits.plaintext_bytes,
            });
        }
        let operation = capability.encrypt(EncryptPayloadRequest::new(scheme, peer, plaintext));
        Ok(EncryptOperation {
            fence,
            ciphertext_limit: limits.ciphertext_bytes,
            operation,
        })
    }
}

fn finish_plaintext(
    fence: PayloadFence,
    limit: u32,
    result: Result<TransientPlaintext, SignerError>,
) -> Result<FencedPlaintext, PayloadError> {
    let plaintext = result?;
    if plaintext.len() > limit as usize {
        return Err(PayloadError::PlaintextTooLarge {
            actual: plaintext.len(),
            max: limit,
        });
    }
    Ok(FencedPlaintext { fence, plaintext })
}

fn finish_ciphertext(
    fence: PayloadFence,
    limit: u32,
    result: Result<EncryptedPayload, SignerError>,
) -> Result<FencedCiphertext, PayloadError> {
    let ciphertext = result?;
    if ciphertext.len() > limit as usize {
        return Err(PayloadError::CiphertextTooLarge {
            actual: ciphertext.len(),
            max: limit,
        });
    }
    Ok(FencedCiphertext { fence, ciphertext })
}

#[cfg(test)]
#[path = "payload/tests.rs"]
mod tests;
