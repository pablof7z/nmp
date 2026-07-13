//! [`EngineError`] -- the semantic error subset [`Engine`](crate::Engine)'s
//! verbs can fail with (canonical-facade-52-plan.md §1).
//!
//! This set is deliberately SMALL: construction-time failures, identity
//! parsing, and the closed-lifecycle state. The one thing it explicitly
//! does NOT contain is a "bad signed event" variant -- that guarantee lives
//! at `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary now
//! (Unit A0, #56, per the Fable checkpoint's Q2 ruling), so a tampered
//! `WritePayload::Signed` surfaces on the [`WriteStatus`](crate::WriteStatus)
//! receipt stream `publish` returns, not as a sync `Err` here. Duplicating a
//! second verify at this layer would recreate the exact entry-point-
//! dependent hole #52 exists to kill.

/// Every way [`Engine::new`](crate::Engine::new) or a subsequent verb can
/// fail closed. Errors are values across this boundary -- a call made after
/// [`Engine::shutdown`](crate::Engine::shutdown) is [`Self::EngineClosed`],
/// never a panic and never a silently disconnected channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// One of [`EngineConfig`](crate::EngineConfig)'s `indexer_relays`/
    /// `app_relays`/`fallback_relays` entries did not parse as a valid relay
    /// URL.
    InvalidRelayUrl { url: String },
    /// [`EngineConfig::store_path`](crate::EngineConfig::store_path) pointed
    /// at a file the on-disk store could not open.
    StoreOpenFailed { reason: String },
    /// [`Engine::reset_persistent_store`](crate::Engine::reset_persistent_store)
    /// could not remove the requested closed persistent store.
    StoreResetFailed { reason: String },
    /// [`Engine::add_account`](crate::Engine::add_account)'s secret key did
    /// not parse as a valid nostr key (hex or bech32 `nsec`).
    InvalidSecretKey,
    /// A custom capability did not expose a stable registry identity.
    SignerMissingPublicKey,
    /// The upper-half namespace reserved for failures rejected before
    /// durable acceptance has been completely consumed.
    ReceiptCorrelationIdExhausted,
    /// [`Engine::shutdown`](crate::Engine::shutdown) has already run --
    /// every other verb fails closed with this variant instead of racing
    /// the engine thread's own teardown (see [`crate::Engine`]'s doc for
    /// the serialized lifecycle gate that makes this exhaustive).
    EngineClosed,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRelayUrl { url } => write!(f, "invalid relay url: {url:?}"),
            Self::StoreOpenFailed { reason } => write!(f, "could not open store: {reason}"),
            Self::StoreResetFailed { reason } => write!(f, "could not reset store: {reason}"),
            Self::InvalidSecretKey => write!(f, "invalid secret key"),
            Self::SignerMissingPublicKey => write!(f, "signer has no public key"),
            Self::ReceiptCorrelationIdExhausted => {
                write!(f, "receipt correlation id namespace exhausted")
            }
            Self::EngineClosed => write!(f, "engine already shut down"),
        }
    }
}

impl std::error::Error for EngineError {}

impl EngineError {
    pub(crate) fn from_publish_error(err: nmp_engine::core::PublishError) -> Self {
        match err {
            nmp_engine::core::PublishError::ReceiptCorrelationIdExhausted => {
                Self::ReceiptCorrelationIdExhausted
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_correlation_exhaustion_maps_and_displays_truthfully() {
        let error = EngineError::from_publish_error(
            nmp_engine::core::PublishError::ReceiptCorrelationIdExhausted,
        );
        assert_eq!(error, EngineError::ReceiptCorrelationIdExhausted);
        assert_eq!(
            error.to_string(),
            "receipt correlation id namespace exhausted"
        );
    }
}
