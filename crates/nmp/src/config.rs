//! [`EngineConfig`] -- the ONLY relay/persistence input an app gives
//! [`Engine::new`](crate::Engine::new) (canonical-facade-52-plan.md §1).
//! Lifted out of `nmp-ffi`'s `NmpEngineConfig` (facade.rs), minus the
//! `uniffi::Record` derive -- that boundary now converts into this type
//! instead of assembling the engine itself (Unit B).
//!
//! Author routes are not configuration. They are neutral, session-owned
//! facts populated only by an attached protocol component.

use nostr::RelayUrl;

use crate::error::EngineError;

/// Construction config for [`Engine::new`](crate::Engine::new).
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// `None` -> in-memory store (nothing survives a restart). `Some(path)`
    /// -> a persistent on-disk store opened at that path, so the same file
    /// reopened across restarts makes THIS replica's already-ingested rows
    /// readable cold, offline. This is local availability, not a claim of
    /// global completeness -- a query snapshot only proves what this store
    /// has acquired, never `synced`/`authoritativeEmpty` (ledger #7 is
    /// still TARGET).
    pub store_path: Option<String>,
    /// Exact operator sources handed to the optional NIP-65 coordinator.
    /// Generic routing never reads or adds these relays.
    pub indexer_relays: Vec<String>,
    /// Operator app relay set (`Lane::OperatorApp`). Default empty.
    pub app_relays: Vec<String>,
    /// Operator fallback relay set (`Lane::OperatorFallback`). Default empty.
    pub fallback_relays: Vec<String>,
    /// The one whole-engine relay ceiling. The same effective value bounds
    /// the router's complete current demand and simultaneous physical
    /// transport workers. Access contexts never share a socket; when read and
    /// write contexts for the same admitted relay compete at this ceiling,
    /// NMP time-shares that relay's slot and restores the read afterward
    /// rather than requiring the app to multiply this value (#598). Zero is
    /// accepted only as a legacy spelling of the finite default. Refused
    /// query candidates remain explicit `LocalLimit` evidence.
    pub max_relays: usize,
    /// Maximum live account-signer and AUTH-policy registrations admitted by
    /// the shared capability registry (#8). Unlike the legacy zero-valued
    /// relay/task settings, zero intentionally admits NONE — a registration
    /// attempt then fails with
    /// [`EngineError::AuthCapabilityRegistryFull`](crate::EngineError::AuthCapabilityRegistryFull).
    pub max_auth_capabilities: usize,
    /// How many failed attempts at ONE relay are enough evidence to stop
    /// (#1031). Reaching it terminalises that lane as `RelayState::GaveUp`;
    /// other relays are unaffected.
    ///
    /// It counts OBSERVATIONS, never wall-clock, and that is the whole
    /// reason it is legitimate: "we tried N times and it failed N times" is
    /// evidence, while "N seconds passed" converts ignorance into a verdict.
    /// Time spent disconnected, parked on AUTH, or waiting for a route
    /// consumes no attempt and can never exhaust this.
    ///
    /// It bounds DELIVERY only. A write whose destination is not yet known,
    /// or whose signer has not attached, has no ceiling of any kind: it ends
    /// when knowledge is exhausted, when the signer arrives, or when the app
    /// removes it.
    pub max_publish_attempts: u64,
}

/// Enough failures at one relay to call it: roughly a day of the capped
/// 3s-doubling-to-300s backoff schedule, which is long enough that a relay
/// having a bad afternoon is not abandoned and short enough that a relay
/// that is simply gone stops holding an obligation open forever.
pub const DEFAULT_MAX_PUBLISH_ATTEMPTS: u64 = 16;

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            store_path: None,
            indexer_relays: Vec::new(),
            app_relays: Vec::new(),
            fallback_relays: Vec::new(),
            max_relays: nmp_transport::DEFAULT_MAX_RELAYS,
            max_auth_capabilities: crate::runtime::DEFAULT_MAX_AUTH_CAPABILITIES,
            max_publish_attempts: DEFAULT_MAX_PUBLISH_ATTEMPTS,
        }
    }
}

fn parse_relay_url(url: &str) -> Result<RelayUrl, EngineError> {
    RelayUrl::parse(url).map_err(|_| EngineError::InvalidRelayUrl {
        url: url.to_string(),
    })
}

pub(crate) fn build_routing_facts(
    config: &EngineConfig,
) -> Result<crate::core::RoutingFactStore, EngineError> {
    // Validate optional-component configuration even when that component is
    // not compiled in. Feature selection must not turn malformed input into
    // a silently accepted configuration.
    for url in &config.indexer_relays {
        parse_relay_url(url)?;
    }
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
    Ok(crate::core::RoutingFactStore::new(
        app_relays,
        fallback_relays,
    ))
}

#[cfg(feature = "nip65")]
pub(crate) fn build_nip65_sources(config: &EngineConfig) -> Result<Vec<RelayUrl>, EngineError> {
    config
        .indexer_relays
        .iter()
        .map(|url| parse_relay_url(url))
        .collect()
}

#[cfg(test)]
mod tests {
    use nmp_router::{AuthorRouteState, RoutingFacts};
    use nostr::Keys;

    use super::*;

    #[test]
    fn indexer_relays_are_not_generic_routing_facts() {
        let indexer = "wss://indexer.example";
        let config = EngineConfig {
            indexer_relays: vec![indexer.to_string()],
            ..EngineConfig::default()
        };

        let facts = build_routing_facts(&config).expect("valid indexer config");

        assert!(
            facts.operator_app_relays().is_empty(),
            "a NIP-65 source must not become an operator app/content lane"
        );
        assert!(
            facts.operator_fallback_relays().is_empty(),
            "a NIP-65 source must not become a generic fallback lane"
        );
        assert_eq!(
            facts.author_routes(&Keys::generate().public_key()),
            AuthorRouteState::Unknown,
            "configuration must not fabricate an author route"
        );
        #[cfg(feature = "nip65")]
        assert_eq!(
            build_nip65_sources(&config).expect("valid NIP-65 source"),
            vec![RelayUrl::parse(indexer).expect("valid relay")],
            "the same URL belongs only to the optional protocol assembly"
        );
    }
}
