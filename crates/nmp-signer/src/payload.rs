//! Bounded, exactly fenced encrypted-payload capability requests.
//!
//! This module owns only the generic envelope. Protocol capabilities still
//! choose the scheme, peer, plaintext codec, and migration policy. Engine
//! integration persists the semantic operation and reconstructs a request;
//! this layer makes a result for any other source or target unusable.

use zeroize::Zeroize;

use crate::SignerPublicKey;

mod service;

pub use service::{
    DecryptOperation, EncryptOperation, EncryptedPayloadService, FencedCiphertext, FencedPlaintext,
    PayloadError, StalePayloadResult,
};

/// Encryption selected by the conceptual capability for this exact request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PayloadEncryption {
    Nip04,
    Nip44V2,
}

/// Source identity bound to one materialization attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PayloadSource {
    Absent,
    Event([u8; 32]),
}

/// Capability-owned plaintext codec identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PayloadCodecId([u8; 16]);

impl PayloadCodecId {
    #[must_use]
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

/// Capability policy identity and exact crypto choices for one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PayloadPolicy {
    id: [u8; 16],
    revision: u64,
    codec: PayloadCodecId,
    scheme: PayloadEncryption,
    peer: SignerPublicKey,
}

impl PayloadPolicy {
    #[must_use]
    pub const fn new(
        id: [u8; 16],
        revision: u64,
        codec: PayloadCodecId,
        scheme: PayloadEncryption,
        peer: SignerPublicKey,
    ) -> Self {
        Self {
            id,
            revision,
            codec,
            scheme,
            peer,
        }
    }
}

/// Finite byte ceilings declared by the conceptual capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    source: PayloadSource,
    target_coordinate_digest: [u8; 32],
    target_revision: u64,
    operation_digest: [u8; 32],
    policy: PayloadPolicy,
    limits: PayloadLimits,
}

impl PayloadFence {
    #[must_use]
    pub const fn new(
        source: PayloadSource,
        target_coordinate_digest: [u8; 32],
        target_revision: u64,
        operation_digest: [u8; 32],
        policy: PayloadPolicy,
        limits: PayloadLimits,
    ) -> Self {
        Self {
            source,
            target_coordinate_digest,
            target_revision,
            operation_digest,
            policy,
            limits,
        }
    }
}

/// One take-once decrypted plaintext allocation.
///
/// Deliberately neither `Clone`, `Debug`, `Display`, nor serializable. Drop
/// wipes the entire allocation through capacity, including unwind and stale
/// result paths.
///
/// ```compile_fail
/// let plaintext = nmp_signer::TransientPlaintext::new(b"secret".to_vec());
/// let copy = plaintext.clone();
/// ```
///
/// ```compile_fail
/// let plaintext = nmp_signer::TransientPlaintext::new(b"secret".to_vec());
/// println!("{plaintext:?}");
/// ```
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

#[cfg(test)]
#[path = "payload/tests.rs"]
mod tests;
