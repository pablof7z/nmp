//! Protocol-neutral signing and cryptography capabilities.

use crate::op::SignerOp;
use crate::value::{SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent};
use zeroize::Zeroize;

/// Opaque, versioned material required to reconstruct a persistable signer.
///
/// The provider identifier is public metadata. `payload` is always redacted
/// from formatting and wiped when the descriptor is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SigningProviderId(&'static str);

impl SigningProviderId {
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

pub struct SigningProviderDescriptor {
    provider: SigningProviderId,
    version: u16,
    payload: Vec<u8>,
}

impl SigningProviderDescriptor {
    #[must_use]
    pub fn new(provider: SigningProviderId, version: u16, payload: Vec<u8>) -> Self {
        Self {
            provider,
            version,
            payload,
        }
    }

    #[must_use]
    pub fn provider(&self) -> SigningProviderId {
        self.provider
    }

    #[must_use]
    pub fn version(&self) -> u16 {
        self.version
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl std::fmt::Debug for SigningProviderDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningProviderDescriptor")
            .field("provider", &self.provider)
            .field("version", &self.version)
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

impl Drop for SigningProviderDescriptor {
    fn drop(&mut self) {
        self.payload.zeroize();
    }
}

/// Signing capability. `sign` may complete synchronously or later
/// (`SignerOp::Pending`) — the caller polls it on the engine's recv loop.
/// Step 0: signature only, no default body — A3 provides the
/// `LocalKeySigner` impl.
pub trait SigningCapability {
    fn public_key(&self) -> Option<SignerPublicKey>;
    /// Current transport/capability availability. Local signers use the
    /// default `true`; remote signers report their live connection state.
    /// This is only an event-race repair hint after a retryable completion,
    /// never permission to select a different identity.
    fn is_available(&self) -> bool {
        true
    }
    /// Return a transient reconstruction descriptor when this provider can
    /// participate in whole-session persistence. Runtime availability is not
    /// part of the descriptor.
    fn persistence_descriptor(&self) -> Option<SigningProviderDescriptor> {
        None
    }
    fn sign(&self, unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent>;
}

/// Co-located with the signer because the KEY LIVES IN THE ENGINE (guarantee
/// #12, M0 amendment: identity-as-input otherwise breaks). Emits decrypted
/// RAW tokens — still zero presentation. Step 0: signature only.
pub trait CryptoCapability {
    fn nip44_encrypt(&self, peer: SignerPublicKey, plaintext: &str) -> SignerOp<String>;
    fn nip44_decrypt(&self, peer: SignerPublicKey, ciphertext: &str) -> SignerOp<String>;
}
