//! [`EngineConfig`] -- the ONLY relay/persistence input an app gives
//! [`Engine::new`](crate::Engine::new) (canonical-facade-52-plan.md §1).
//! Lifted out of `nmp-ffi`'s `NmpEngineConfig` (facade.rs), minus the
//! `uniffi::Record` derive -- that boundary now converts into this type
//! instead of assembling the engine itself (Unit B).
//!
//! Everything past these three relay lanes is discovered live by the engine
//! itself, via its own internal kind:10002 auto-discovery against the
//! configured indexers (M5's self-bootstrapping outbox) -- there is no
//! bootstrap phase and no pre-resolved write-relay map for a caller to
//! supply.

use nostr::RelayUrl;

use crate::error::EngineError;

/// Construction config for [`Engine::new`](crate::Engine::new).
#[derive(Clone, Debug, Default)]
pub struct EngineConfig {
    /// `None` -> in-memory store (nothing survives a restart). `Some(path)`
    /// -> a persistent on-disk store opened at that path (the same file
    /// reopened across restarts is what makes a cold, offline read
    /// authoritative -- ledger #7).
    pub store_path: Option<String>,
    /// Operator indexer relay set (`Lane::IndexerDiscovery`) -- eligible
    /// only for discovery-kind atoms. This, plus `app_relays`/
    /// `fallback_relays` below, is the entire relay fact set a caller ever
    /// supplies; every author's write relays (including the app's own
    /// account) are discovered live from here.
    pub indexer_relays: Vec<String>,
    /// Operator app relay set (`Lane::AppRelay`). Default empty.
    pub app_relays: Vec<String>,
    /// Operator fallback relay set (`Lane::Fallback`). Default empty.
    pub fallback_relays: Vec<String>,
}

fn parse_relay_url(url: &str) -> Result<RelayUrl, EngineError> {
    RelayUrl::parse(url).map_err(|_| EngineError::InvalidRelayUrl {
        url: url.to_string(),
    })
}

pub(crate) fn build_directory(
    config: &EngineConfig,
) -> Result<nmp_router::LiveDirectory, EngineError> {
    let indexers = config
        .indexer_relays
        .iter()
        .map(|u| parse_relay_url(u))
        .collect::<Result<Vec<_>, _>>()?;
    let app_relays = config
        .app_relays
        .iter()
        .map(|u| parse_relay_url(u))
        .collect::<Result<Vec<_>, _>>()?;
    let fallback_relays = config
        .fallback_relays
        .iter()
        .map(|u| parse_relay_url(u))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(nmp_router::LiveDirectory::builder()
        .indexers(indexers)
        .app_relays(app_relays)
        .fallback_relays(fallback_relays)
        .build())
}
