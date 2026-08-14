use nostr::RelayUrl;

use super::Engine;
use crate::error::EngineError;
use crate::relay_information::{
    RelayInformationCachePolicy, RelayInformationError, RelayInformationSnapshot,
};

/// Failure of an explicit NIP-11 one-shot: lifecycle/URL validation stays
/// distinct from network/document acquisition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayInformationRequestError {
    Engine(EngineError),
    Acquisition(RelayInformationError),
}

impl std::fmt::Display for RelayInformationRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine(error) => error.fmt(f),
            Self::Acquisition(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RelayInformationRequestError {}

impl Engine {
    /// Acquire a relay's NIP-11 document once through the engine-owned,
    /// bounded, single-flight cache. This is intentionally not `observe_*`:
    /// NIP-11 is one HTTP representation, not a stream. Callers choose when
    /// to refresh; ordinary relay reconnects reuse the same freshness rules.
    pub async fn relay_information(
        &self,
        relay: &str,
        policy: RelayInformationCachePolicy,
    ) -> Result<RelayInformationSnapshot, RelayInformationRequestError> {
        let relay = RelayUrl::parse(relay).map_err(|_| {
            RelayInformationRequestError::Engine(EngineError::InvalidRelayUrl {
                url: relay.to_string(),
            })
        })?;
        let handle = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            guard.as_ref().map(|inner| inner.handle.clone()).ok_or(
                RelayInformationRequestError::Engine(EngineError::EngineClosed),
            )?
        };
        handle
            .relay_information_async(relay, policy)
            .await
            .map_err(RelayInformationRequestError::Acquisition)
    }

    #[cfg(test)]
    pub(super) fn relay_information_retention_census(
        &self,
    ) -> crate::relay_information_service::RelayInformationRetentionCensus {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard
            .as_ref()
            .map(|inner| crate::runtime::relay_information_retention_census(&inner.handle))
            .expect("test census requires an open engine")
    }
}
