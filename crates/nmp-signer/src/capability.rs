//! `SigningCapability` + `CryptoCapability` (§3.3).

use nostr::{Event as SignedEvent, PublicKey, UnsignedEvent};

use crate::op::SignerOp;

/// Signing capability. `sign` may complete synchronously or later
/// (`SignerOp::Pending`) — the caller polls it on the engine's recv loop.
/// Step 0: signature only, no default body — A3 provides the
/// `LocalKeySigner` impl.
pub trait SigningCapability {
    fn public_key(&self) -> Option<PublicKey>;
    /// Current transport/capability availability. Local signers use the
    /// default `true`; remote signers report their live connection state.
    /// This is only an event-race repair hint after a retryable completion,
    /// never permission to select a different identity.
    fn is_available(&self) -> bool {
        true
    }
    fn sign(&self, unsigned: UnsignedEvent) -> SignerOp<SignedEvent>;
}

/// Co-located with the signer because the KEY LIVES IN THE ENGINE (ledger
/// #12, M0 amendment: identity-as-input otherwise breaks). Emits decrypted
/// RAW tokens — still zero presentation. Step 0: signature only.
pub trait CryptoCapability {
    fn nip44_encrypt(&self, peer: PublicKey, plaintext: &str) -> SignerOp<String>;
    fn nip44_decrypt(&self, peer: PublicKey, ciphertext: &str) -> SignerOp<String>;
}
