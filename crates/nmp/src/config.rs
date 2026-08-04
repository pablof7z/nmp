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
    /// Local/private relay HOSTS to re-admit from OTHER PEOPLE's data
    /// (issues #121, #1251).
    ///
    /// A loopback / RFC-1918 / link-local relay named by someone else — a
    /// stranger's kind:10002, a relay hint in their event — is refused by
    /// default, because such an address means nothing outside its own network
    /// and naming one is the cheapest SSRF pivot there is. Listing a host here
    /// (`"127.0.0.1"`, `"localhost"`) re-admits that exact host from any
    /// source. Matched host-only (port- and path-insensitive).
    ///
    /// This knob is NOT how an app reaches its own local relay. Relays THIS
    /// app declared (`app_relays`, `fallback_relays`, `WriteRouting::Explicit`,
    /// `RelayScope::on`) and relays a signed-in identity declared in its own
    /// relay list are heeded on their provenance alone, dial included: they
    /// are describing their own network. Default empty (fail closed).
    ///
    /// One deliberate exception, recorded in `docs/known-gaps.md`: NIP-11
    /// document acquisition is provenance-blind, so fetching a LOCAL relay's
    /// NIP-11 document still requires its host here.
    pub allowed_local_relay_hosts: Vec<String>,
    /// Whether this process can reach a Tor hidden service (#1251).
    ///
    /// `.onion` is not a "my network" address, it is a reachability question,
    /// so it is not on the provenance axis and
    /// [`Self::allowed_local_relay_hosts`] grants it nothing. Declaring
    /// reachability makes OTHER people's `.onion` relays usable — a relay list
    /// belonging to someone you follow, a hint in their event — not only ones
    /// this app or its own identities declared, which are heeded regardless.
    ///
    /// NMP installs no Tor transport and never probes for one. This is the app
    /// stating that reachability exists; heeding is permission to try, so a
    /// hidden service that turns out to be unreachable simply fails to
    /// connect. Default `false`.
    pub tor_reachable: bool,
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
            allowed_local_relay_hosts: Vec::new(),
            tor_reachable: false,
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

/// Build the engine's relay admission policy (issues #121, #1251) from the
/// two operator declarations that widen it. Both only ever affect relays
/// SOMEONE ELSE named; what this app and its own identities declared is
/// heeded without either. The runtime threads the same policy into the
/// transport dial and the NIP-11 resolved-IP guard, so one owner answers the
/// address question everywhere.
pub(crate) fn build_admission_policy(config: &EngineConfig) -> crate::core::RelayAdmissionPolicy {
    crate::core::RelayAdmissionPolicy::new(
        config.allowed_local_relay_hosts.iter().cloned(),
        if config.tor_reachable {
            nmp_network_policy::OnionReachability::Reachable
        } else {
            nmp_network_policy::OnionReachability::Unreachable
        },
    )
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
