//! `LocalKeySigner` (§3.3): sufficient for M3 — remote-signer breadth is a
//! seam only (§7 non-goal).

use std::fmt;

use nostr::nips::nip44;
use nostr::{Event as SignedEvent, Keys, PublicKey, UnsignedEvent};
use zeroize::Zeroizing;

use crate::capability::{CryptoCapability, SigningCapability};
use crate::op::{SignerError, SignerOp};

/// Implements both `SigningCapability` and `CryptoCapability` over a local
/// `nostr::Keys`. `keys` is private — nothing outside this module needs it,
/// and the raw secret is exposed only through `sign`/`nip44_*` (A3's impls
/// below).
///
/// `nostr::Keys`/`SecretKey` already erase their own internal copies on drop
/// (via `secp256k1`'s `non_secure_erase`), but that erase is explicitly
/// documented upstream as *not* hardened against compiler elision.
/// `secret_bytes` is a defense-in-depth raw copy of the same scalar, wrapped
/// in `zeroize::Zeroizing`, matching the NIP-46 secret-hardening precedent
/// (`Nip46Invitation::secret`): it guarantees a compiler-fenced wipe of this
/// signer's copy the moment it drops, independent of upstream's own
/// best-effort erase (docs/known-gaps.md's "old repo's raw-bytes/zeroize
/// hardening", #47).
pub struct LocalKeySigner {
    keys: Keys,
    // Never read back by this signer itself (`keys`/`nip44::{encrypt,decrypt}`
    // are the operational path) — held purely so its `Drop` glue zeroizes the
    // raw scalar. `#[allow(dead_code)]` records that deliberately, matching
    // `nmp-store`'s `OutboxIntentRecord` precedent for write-only-by-design
    // fields.
    #[allow(dead_code)]
    secret_bytes: Zeroizing<[u8; 32]>,
}

impl LocalKeySigner {
    /// Wrap an existing keypair.
    #[must_use]
    pub fn new(keys: Keys) -> Self {
        let secret_bytes = Zeroizing::new(keys.secret_key().to_secret_bytes());
        Self { keys, secret_bytes }
    }

    /// Generate a fresh keypair via OS RNG — convenience for tests/tooling.
    #[must_use]
    pub fn generate() -> Self {
        Self::new(Keys::generate())
    }
}

/// Redacted: never prints secret key material, matching `Nip46Signer`'s
/// `Debug` precedent of exposing only the public identity.
impl fmt::Debug for LocalKeySigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalKeySigner")
            .field("public_key", &self.keys.public_key())
            .finish_non_exhaustive()
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

    /// Falsifier: `{:?}` on `LocalKeySigner` must never leak the secret
    /// scalar — neither its hex form nor the raw bytes — only the public
    /// key, matching `Nip46Signer`/`Nip46Invitation`'s redacted `Debug`.
    #[test]
    fn debug_output_redacts_secret_key() {
        let signer = LocalKeySigner::generate();
        let secret_hex = signer.keys.secret_key().to_secret_hex();
        let secret_bytes = signer.keys.secret_key().to_secret_bytes();

        let debug = format!("{signer:?}");

        assert!(
            !debug.contains(&secret_hex),
            "Debug output must not contain the secret key hex"
        );
        assert!(
            !debug
                .as_bytes()
                .windows(secret_bytes.len())
                .any(|w| w == secret_bytes),
            "Debug output must not contain the raw secret bytes"
        );
        assert!(
            debug.contains(&signer.keys.public_key().to_hex()),
            "Debug output should still identify the signer by public key"
        );
    }

    /// Falsifier: dropping a `LocalKeySigner` wipes the raw secret-scalar
    /// copy held for zeroization hardening (`secret_bytes`). This is as far
    /// as the property is testable in safe Rust: we capture a raw pointer
    /// into the `Zeroizing<[u8; 32]>` storage before drop and read it back
    /// immediately after the owning scope ends (letting the compiler drop
    /// `signer` in place, rather than moving it into a `drop(signer)` call —
    /// a by-value move could relocate the bytes to a different stack slot
    /// before `Drop` runs, which would make the pointer point at stale,
    /// never-wiped memory and falsely fail). Reading through a pointer to a
    /// value that has gone out of scope is technically into dead storage,
    /// but nothing has reused the slot yet, and this is the standard way
    /// `zeroize`-guarded types are falsified: it is the most direct proof
    /// available that the wipe actually ran, not just that the type in
    /// principle supports it.
    #[test]
    fn secret_bytes_zeroized_on_drop() {
        let raw_ptr: *const [u8; 32];
        let before_drop: [u8; 32];
        {
            let signer = LocalKeySigner::generate();
            raw_ptr = &*signer.secret_bytes;
            before_drop = unsafe { *raw_ptr };
            // `signer` drops here, in place (never moved), which zeroizes
            // `secret_bytes` through its `Zeroizing` `Drop` impl.
        }

        // Sanity: this was real, non-zero secret material before drop —
        // otherwise the post-drop assertion below would be vacuous.
        assert_ne!(before_drop, [0u8; 32]);

        // SAFETY: see falsifier doc comment above.
        let after_drop = unsafe { *raw_ptr };
        assert_eq!(
            after_drop, [0u8; 32],
            "secret bytes must be zeroized once the signer drops"
        );
    }
}
