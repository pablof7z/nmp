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
    /// Destructive reset was refused because an engine in this process still
    /// owns the same canonical persistent-store path.
    StoreStillOpen { path: String },
    /// The OS refused one engine-owned transport/runtime thread, or the
    /// configured relay budget could not be represented safely. No partial
    /// engine escapes construction.
    ThreadUnavailable { component: String, reason: String },
    /// The finite native task executor had no immediately-startable slot.
    /// No stream or operation was accepted behind this refusal.
    ExecutorSaturated { component: String, capacity: usize },
    /// [`Engine::add_account`](crate::Engine::add_account)'s secret key did
    /// not parse as a valid nostr key (hex or bech32 `nsec`).
    InvalidSecretKey,
    /// A custom capability did not expose a stable registry identity.
    SignerMissingPublicKey,
    /// The shared account-signer/AUTH-policy capability registry reached its
    /// configured [`EngineConfig::max_auth_capabilities`](crate::EngineConfig::max_auth_capabilities)
    /// bound (#8). Nothing was registered behind this refusal.
    AuthCapabilityRegistryFull { limit: usize },
    /// The monotonic capability-instance namespace that distinguishes stale
    /// registrations from their replacements has been exhausted (#8). It
    /// never wraps or reuses an identity; registration fails closed instead.
    AuthCapabilityInstanceExhausted,
    /// A windowed [`Engine::observe`](crate::Engine::observe) declared an
    /// `initial` window size greater than its `max` ceiling (#485). Caught
    /// before the engine is touched; zero sizes are unrepresentable via
    /// `NonZeroUsize`.
    WindowInitialExceedsMax { initial: usize, max: usize },
    /// A windowed [`Engine::observe`](crate::Engine::observe) was given a
    /// selection that already carries a NIP-01 `limit` (#485). A window and a
    /// `limit` would be two competing owners of row membership.
    WindowSelectionHasLimit,
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
            Self::StoreStillOpen { path } => {
                write!(f, "persistent store is still open: {path}")
            }
            Self::ThreadUnavailable { component, reason } => {
                write!(f, "{component} thread unavailable: {reason}")
            }
            Self::ExecutorSaturated {
                component,
                capacity,
            } => write!(
                f,
                "{component} refused: native task executor is at capacity {capacity}"
            ),
            Self::InvalidSecretKey => write!(f, "invalid secret key"),
            Self::SignerMissingPublicKey => write!(f, "signer has no public key"),
            Self::AuthCapabilityRegistryFull { limit } => {
                write!(f, "AUTH capability registry is full at {limit} entries")
            }
            Self::AuthCapabilityInstanceExhausted => {
                write!(f, "AUTH capability instance space exhausted")
            }
            Self::WindowInitialExceedsMax { initial, max } => {
                write!(f, "window initial size {initial} exceeds its max {max}")
            }
            Self::WindowSelectionHasLimit => {
                write!(f, "a windowed selection must not also declare a limit")
            }
            Self::ReceiptCorrelationIdExhausted => {
                write!(f, "receipt correlation id namespace exhausted")
            }
            Self::EngineClosed => write!(f, "engine already shut down"),
        }
    }
}

impl std::error::Error for EngineError {}

impl EngineError {
    pub(crate) fn from_thread_error(error: nmp_engine::runtime::EngineThreadError) -> Self {
        match error {
            nmp_engine::runtime::EngineThreadError::ThreadUnavailable { component, reason } => {
                Self::ThreadUnavailable { component, reason }
            }
            nmp_engine::runtime::EngineThreadError::RelayBudgetOverflow { relay_limit } => {
                Self::ThreadUnavailable {
                    component: "relay worker budget".to_string(),
                    reason: format!(
                        "configured max_relays {relay_limit} cannot represent its finite retirement envelope"
                    ),
                }
            }
            // The runtime's finite shutdown drain (#8 U4) refuses new work
            // with a typed engine-level error; at this facade it is the same
            // closed-engine fact `EngineClosed` already names.
            nmp_engine::runtime::EngineThreadError::EngineShuttingDown => Self::EngineClosed,
        }
    }

    pub(crate) fn from_publish_error(err: nmp_engine::core::PublishError) -> Self {
        match err {
            nmp_engine::core::PublishError::ReceiptCorrelationIdExhausted => {
                Self::ReceiptCorrelationIdExhausted
            }
            nmp_engine::core::PublishError::EngineShuttingDown => Self::EngineClosed,
        }
    }

    pub(crate) fn from_add_signer_error(error: nmp_engine::runtime::AddSignerError) -> Self {
        match error {
            nmp_engine::runtime::AddSignerError::MissingPublicKey => Self::SignerMissingPublicKey,
            nmp_engine::runtime::AddSignerError::CapabilityInstanceExhausted => {
                Self::AuthCapabilityInstanceExhausted
            }
            nmp_engine::runtime::AddSignerError::RegistryFull { limit } => {
                Self::AuthCapabilityRegistryFull { limit }
            }
            nmp_engine::runtime::AddSignerError::EngineShuttingDown => Self::EngineClosed,
        }
    }

    pub(crate) fn from_add_auth_policy_error(
        error: nmp_engine::runtime::AddAuthPolicyError,
    ) -> Self {
        match error {
            nmp_engine::runtime::AddAuthPolicyError::CapabilityInstanceExhausted => {
                Self::AuthCapabilityInstanceExhausted
            }
            nmp_engine::runtime::AddAuthPolicyError::RegistryFull { limit } => {
                Self::AuthCapabilityRegistryFull { limit }
            }
            nmp_engine::runtime::AddAuthPolicyError::EngineShuttingDown => Self::EngineClosed,
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
