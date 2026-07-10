//! Remote-signer SEAM only (§3.3, §7 non-goal): NIP-46/NIP-55 transport is
//! NOT built in M3. This trait exists so a future remote signer can plug
//! into the same capability surface `LocalKeySigner` implements.

use nostr::PublicKey;

pub trait RemoteSignerHandle {
    fn public_key(&self) -> Option<PublicKey>;
}
