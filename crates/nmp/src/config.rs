//! [`EngineConfig`] -- the ONLY relay/persistence input an app gives
//! [`Engine::new`](crate::Engine::new) (#52 §1).
//! Lifted out of `nmp-ffi`'s `NmpEngineConfig` (facade.rs), minus the
//! `uniffi::Record` derive -- that boundary now converts into this type
//! instead of assembling the engine itself (Unit B).
//!
//! Author routes are not configuration. They are neutral, session-owned
//! facts populated only by an attached protocol component.

use nmp_runtime::EngineClock;
use nostr::RelayUrl;

use crate::error::EngineError;

/// Construction config for [`Engine::new`](crate::Engine::new).
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// `None` -> an engine-owned temporary Redb store (nothing survives this
    /// engine's lifetime). `Some(path)` -> a persistent Redb store opened at that path, so the same file
    /// reopened across restarts makes THIS replica's already-ingested rows
    /// readable cold, offline. This is local availability, not a claim of
    /// global completeness -- a query snapshot only proves what this store
    /// has acquired, never `synced`/`authoritativeEmpty` (guarantee #7 is
    /// still TARGET).
    pub store_path: Option<String>,
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
    /// What time it is, as far as this engine's reducer is concerned.
    ///
    /// The default is unpinned and is what an app that has no opinion about
    /// the clock gets: every reading is the real system clock, and there is
    /// no code to write. An app that DOES have an opinion -- one replaying a
    /// recorded session, one on a device whose clock is skewed, one driving a
    /// scenario that says "30 days pass" -- constructs an [`EngineClock`],
    /// states a time on it, hands it here, and keeps its clone to state more
    /// times later:
    ///
    /// ```no_run
    /// use nmp::{Engine, EngineClock, EngineConfig, Timestamp};
    ///
    /// let clock = EngineClock::new();
    /// clock.set(Timestamp::from_secs(1_600_000_000));
    /// let engine = Engine::new(EngineConfig {
    ///     clock: clock.clone(),
    ///     ..EngineConfig::default()
    /// })?;
    /// // Later, the same clock moves the engine that is already running.
    /// clock.advance(std::time::Duration::from_secs(30 * 24 * 60 * 60));
    /// # Ok::<(), nmp::EngineError>(())
    /// ```
    ///
    /// It is construction input rather than something handed back from a
    /// running engine because store recovery reads it. A clock reachable only
    /// after `Engine::new` returns cannot state the time recovery ran at, and
    /// recovery is exactly where an expiry sweep or a parked write first
    /// consults the wall.
    ///
    /// It governs the REDUCER only. Reconnect backoff and the background-gap
    /// detector are the transport's own clocks and are untouched by this: a
    /// question about an expiry wants a stated instant, a question about a
    /// reconnect wants a compressed schedule, and one knob answering both
    /// would make the second one lie.
    pub clock: EngineClock,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            store_path: None,
            app_relays: Vec::new(),
            fallback_relays: Vec::new(),
            max_relays: nmp_transport::DEFAULT_MAX_RELAYS,
            max_auth_capabilities: nmp_runtime::DEFAULT_MAX_AUTH_CAPABILITIES,
            max_publish_attempts: nmp_engine::publish_queue::DEFAULT_MAX_PUBLISH_ATTEMPTS,
            clock: EngineClock::new(),
        }
    }
}

fn parse_relay_url(url: &str) -> Result<RelayUrl, EngineError> {
    RelayUrl::parse(url).map_err(|_| EngineError::InvalidRelayUrl {
        url: url.to_string(),
    })
}

pub(crate) fn build_routing_fact_relays(
    config: &EngineConfig,
) -> Result<(Vec<RelayUrl>, Vec<RelayUrl>), EngineError> {
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
    Ok((app_relays, fallback_relays))
}

