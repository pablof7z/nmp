//! `LocalKeySigner` (§3.3): sufficient for M3 — remote-signer breadth is a
//! seam only (§7 non-goal).

use nostr::nips::nip44;
use nostr::{Event as SignedEvent, Keys, PublicKey, UnsignedEvent};

use crate::capability::{CryptoCapability, SigningCapability};
use crate::op::{SignerError, SignerOp};

/// Implements both `SigningCapability` and `CryptoCapability` over a local
/// `nostr::Keys` (A3 provides the impls). `keys` is `pub` in Step 0 purely
/// so the scaffold compiles cleanly before any consuming impl exists; A3
/// may tighten visibility once `sign`/`nip44_*` read it.
pub struct LocalKeySigner {
    pub keys: nostr::Keys,
}

impl LocalKeySigner {
    /// Wrap an existing keypair.
    #[must_use]
    pub fn new(keys: Keys) -> Self {
        Self { keys }
    }

    /// Generate a fresh keypair via OS RNG — convenience for tests/tooling.
    #[must_use]
    pub fn generate() -> Self {
        Self::new(Keys::generate())
    }
}

impl SigningCapability for LocalKeySigner {
    fn public_key(&self) -> Option<PublicKey> {
        Some(self.keys.public_key())
    }

    /// Signs synchronously — the local key never blocks on I/O, so this
    /// always resolves as `SignerOp::Ready`.
    fn sign(&self, unsigned: UnsignedEvent) -> SignerOp<SignedEvent> {
        // The engine is the only caller and always stamps `unsigned.pubkey`
        // from this signer's own `public_key()`; a mismatch means the
        // caller built the template for a different identity, which must
        // not silently produce an event under this signer's key.
        if unsigned.pubkey != self.keys.public_key() {
            return SignerOp::err(SignerError::Rejected(format!(
                "unsigned event pubkey {} does not match signer pubkey {}",
                unsigned.pubkey,
                self.keys.public_key()
            )));
        }
        match unsigned.sign_with_keys(&self.keys) {
            Ok(event) => SignerOp::ok(event),
            Err(e) => SignerOp::err(SignerError::Rejected(format!("sign failed: {e}"))),
        }
    }
}

/// Co-located with the signer because the KEY LIVES IN THE ENGINE (M0
/// amendment, ledger #12): decrypting gift-wrap/private-list ciphertext
/// requires the same secret material `sign` uses, so this capability lives
/// on the same type rather than behind a separate app-facing door.
impl CryptoCapability for LocalKeySigner {
    /// NIP-44 encrypt via rust-nostr's `nostr::nips::nip44` — no scratch
    /// crypto (memory rule).
    fn nip44_encrypt(&self, peer: PublicKey, plaintext: &str) -> SignerOp<String> {
        match nip44::encrypt(self.keys.secret_key(), &peer, plaintext, nip44::Version::V2) {
            Ok(ciphertext) => SignerOp::ok(ciphertext),
            Err(e) => SignerOp::err(SignerError::Rejected(format!("nip44 encrypt failed: {e}"))),
        }
    }

    /// NIP-44 decrypt via rust-nostr. Turns gift-wrap/private-list
    /// ciphertext into raw plaintext tokens — the caller (engine) owns any
    /// further parsing; this capability never assumes the stored content
    /// was plaintext to begin with.
    fn nip44_decrypt(&self, peer: PublicKey, ciphertext: &str) -> SignerOp<String> {
        match nip44::decrypt(self.keys.secret_key(), &peer, ciphertext) {
            Ok(plaintext) => SignerOp::ok(plaintext),
            Err(e) => SignerOp::err(SignerError::Rejected(format!("nip44 decrypt failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{Kind, Timestamp};

    fn unsigned_for(signer: &LocalKeySigner, content: &str) -> UnsignedEvent {
        UnsignedEvent::new(
            signer.keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            Vec::new(),
            content,
        )
    }

    #[test]
    fn sign_then_verify_round_trip() {
        let signer = LocalKeySigner::generate();
        let unsigned = unsigned_for(&signer, "hello from nmp-signer");

        let signed = match signer.sign(unsigned) {
            SignerOp::Ready(Ok(event)) => event,
            SignerOp::Ready(Err(e)) => panic!("sign failed: {e}"),
            SignerOp::Pending(_) => panic!("local signer must resolve synchronously"),
        };

        assert_eq!(signed.pubkey, signer.keys.public_key());
        assert!(signed.verify().is_ok(), "signed event must verify");
    }

    #[test]
    fn sign_rejects_pubkey_mismatch() {
        let signer = LocalKeySigner::generate();
        let other = LocalKeySigner::generate();
        let unsigned = unsigned_for(&other, "wrong identity");

        match signer.sign(unsigned) {
            SignerOp::Ready(Err(SignerError::Rejected(_))) => {}
            other => panic!("expected Rejected mismatch, got {other:?}"),
        }
    }

    #[test]
    fn nip44_encrypt_decrypt_round_trip() {
        let alice = LocalKeySigner::generate();
        let bob = LocalKeySigner::generate();
        let plaintext = "the quick brown fox — nip-44 round trip";

        let ciphertext =
            match CryptoCapability::nip44_encrypt(&alice, bob.keys.public_key(), plaintext) {
                SignerOp::Ready(Ok(ct)) => ct,
                SignerOp::Ready(Err(e)) => panic!("nip44 encrypt failed: {e}"),
                SignerOp::Pending(_) => panic!("local signer must resolve synchronously"),
            };

        let decrypted =
            match CryptoCapability::nip44_decrypt(&bob, alice.keys.public_key(), &ciphertext) {
                SignerOp::Ready(Ok(pt)) => pt,
                SignerOp::Ready(Err(e)) => panic!("nip44 decrypt failed: {e}"),
                SignerOp::Pending(_) => panic!("local signer must resolve synchronously"),
            };

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn public_key_matches_keys() {
        let signer = LocalKeySigner::generate();
        assert_eq!(signer.public_key(), Some(signer.keys.public_key()));
    }
}
