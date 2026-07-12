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
    /// that exact host. Matched host-only (port- and path-insensitive). This
    /// never affects the operator relay lanes above — those are trusted
    /// config, not discovery, and are admitted regardless. Default empty
    /// (fail closed).
    pub allowed_local_relay_hosts: Vec<String>,
    /// OPT-IN, defense-in-depth ceiling on the number of relays the transport
    /// pool will keep a live worker for at once (issue #121). `0` (the
    /// default) imposes no cap — preserving prior behavior. A non-zero value
    /// caps concurrent relay workers; dials past it are refused and counted
    /// in `DiagnosticsSnapshot::relays_rejected_over_cap`.
    ///
    /// This is NOT the primary worker-exhaustion defense. Fan-out per query
    /// is already bounded structurally by `nmp-router`'s greedy k-cover
    /// solver (`crates/nmp-router/src/solver.rs`), whose selection is capped
    /// and drawn only from each author's OWN relays (`route.rs`) — so the
    /// number of relays a single (validly-signed) kind:10002 lists is
    /// irrelevant to worker count. This pool cap is a coarse absolute
    /// backstop an operator may opt into on top of that.
    pub max_relays: usize,
}

fn parse_relay_url(url: &str) -> Result<RelayUrl, EngineError> {
    RelayUrl::parse(url).map_err(|_| EngineError::InvalidRelayUrl {
        url: url.to_string(),
    })
}

/// Build the engine's discovered-relay admission policy (issue #121) from the
/// operator's opt-in local-host list. Everything not listed here is subject
/// to the secure default: a discovered private/loopback/onion relay is
/// rejected.
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
