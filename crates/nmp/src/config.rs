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
    /// Local/private relay HOSTS the operator EXPLICITLY opts into despite
    /// the SSRF admission policy (issue #121). Discovered (network-sourced
    /// kind:10002) relays on a loopback / RFC-1918 / link-local / `.onion`
    /// host are rejected by default; listing a host here (e.g. `"127.0.0.1"`
    /// or `"localhost"` for a local dev relay) re-admits DISCOVERED relays on
    /// that exact host. Matched host-only (port- and path-insensitive).
    /// Operator relay lanes bypass discovered-relay admission, but every
    /// socket still passes the resolved-IP dial guard; an intentional local
    /// operator relay therefore needs the same explicit host opt-in. Default
    /// empty (fail closed).
    pub allowed_local_relay_hosts: Vec<String>,
    /// The one whole-engine relay ceiling. The same effective value bounds
    /// the router's complete current demand and the transport's live workers;
    /// zero is accepted only as a legacy spelling of the finite default.
    /// Refused query candidates remain explicit `LocalLimit` evidence.
    pub max_relays: usize,
    /// Maximum live account-signer and AUTH-policy registrations admitted by
    /// the shared capability registry (#8). Unlike the legacy zero-valued
    /// relay/task settings, zero intentionally admits NONE — a registration
    /// attempt then fails with
    /// [`EngineError::AuthCapabilityRegistryFull`](crate::EngineError::AuthCapabilityRegistryFull).
    pub max_auth_capabilities: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            store_path: None,
            indexer_relays: Vec::new(),
            app_relays: Vec::new(),
            fallback_relays: Vec::new(),
            allowed_local_relay_hosts: Vec::new(),
            max_relays: nmp_transport::DEFAULT_MAX_RELAYS,
            max_auth_capabilities: nmp_engine::runtime::DEFAULT_MAX_AUTH_CAPABILITIES,
        }
    }
}

fn parse_relay_url(url: &str) -> Result<RelayUrl, EngineError> {
    RelayUrl::parse(url).map_err(|_| EngineError::InvalidRelayUrl {
        url: url.to_string(),
    })
}

/// Build the engine's discovered-relay admission policy (issue #121) from the
/// operator's opt-in local-host list. Everything not listed here is subject
/// to the secure default: a discovered private/loopback/onion relay is
/// rejected. The runtime also threads this policy's normalized host set into
/// the transport and NIP-11 resolved-IP guards.
pub(crate) fn build_admission_policy(
    config: &EngineConfig,
) -> nmp_engine::core::RelayAdmissionPolicy {
    nmp_engine::core::RelayAdmissionPolicy::new(config.allowed_local_relay_hosts.iter().cloned())
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
