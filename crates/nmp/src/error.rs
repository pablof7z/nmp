//! [`EngineError`] -- the semantic error subset [`Engine`](crate::Engine)'s
//! synchronous verbs can fail with (canonical-facade-52-plan.md §1).
//!
//! This set is deliberately SMALL: it is only construction-time and
//! identity-parsing failures. The one thing it explicitly does NOT contain
//! is a "bad signed event" variant -- that guarantee lives at
//! `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary now
//! (Unit A0, #56, per the Fable checkpoint's Q2 ruling), so a tampered
//! `WritePayload::Signed` surfaces on the [`WriteStatus`](crate::WriteStatus)
//! receipt stream `publish` returns, not as a sync `Err` here. Duplicating a
//! second verify at this layer would recreate the exact entry-point-
//! dependent hole #52 exists to kill.

/// Every way [`Engine::new`](crate::Engine::new) or an identity verb can
/// fail closed. Errors are values across this boundary, never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// One of [`EngineConfig`](crate::EngineConfig)'s `indexer_relays`/
    /// `app_relays`/`fallback_relays` entries did not parse as a valid relay
    /// URL.
    InvalidRelayUrl { url: String },
    /// [`EngineConfig::store_path`](crate::EngineConfig::store_path) pointed
    /// at a file the on-disk store could not open.
    StoreOpenFailed { reason: String },
    /// [`Engine::add_account`](crate::Engine::add_account)'s secret key did
    /// not parse as a valid nostr key (hex or bech32 `nsec`).
    InvalidSecretKey,
    /// A registered signing capability reported no public key at all --
    /// never true for the key-backed signer `add_account` builds
    /// internally, but the underlying `SigningCapability::public_key() ->
    /// Option<PublicKey>` contract allows it, so this stays a typed state
    /// rather than an assumption (mirrors `nmp-ffi::FfiError::
    /// SignerHasNoPublicKey`).
    SignerHasNoPublicKey,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRelayUrl { url } => write!(f, "invalid relay url: {url:?}"),
            Self::StoreOpenFailed { reason } => write!(f, "could not open store: {reason}"),
            Self::InvalidSecretKey => write!(f, "invalid secret key"),
            Self::SignerHasNoPublicKey => write!(f, "signer reported no public key"),
        }
    }
}

impl std::error::Error for EngineError {}
