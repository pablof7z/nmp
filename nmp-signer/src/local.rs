//! `LocalKeySigner` (§3.3): sufficient for M3 — remote-signer breadth is a
//! seam only (§7 non-goal).

/// Implements both `SigningCapability` and `CryptoCapability` over a local
/// `nostr::Keys` (A3 provides the impls). `keys` is `pub` in Step 0 purely
/// so the scaffold compiles cleanly before any consuming impl exists; A3
/// may tighten visibility once `sign`/`nip44_*` read it.
pub struct LocalKeySigner {
    pub keys: nostr::Keys,
}
