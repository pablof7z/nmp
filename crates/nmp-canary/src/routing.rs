//! What `WriteRouting::Auto` actually does when nothing is configured.
//!
//! ## The claim under test
//!
//! "Reads get outbox routing for free; writes do not. `WriteRouting::Auto` is
//! refused unless outbox indexers are configured, and an app author reads that
//! refusal as their own misconfiguration."
//!
//! ## What the Rust surface says
//!
//! Not refused. `Engine::new_with_capabilities_and_routing`'s own doc:
//! "`None` discovers nothing: every author stays `Unknown`, operator lanes and
//! explicit routes carry everything they carry, and an `Auto` write whose
//! author is unknown **parks on knowledge rather than failing**."
//!
//! There is no `EngineError` variant for it either. The full list is
//! `InvalidRelayUrl`, `StoreOpenFailed`, `StoreAlreadyOpen`,
//! `StoreUnsupportedSchema`, `StoreResetFailed`, `StoreStillOpen`,
//! `EngineStartFailed`, `MissingReplaceableCapability`,
//! `DuplicateReplaceableCapability`, `ObservationUnavailable`,
//! `AuthCapabilityRegistryFull`, `AuthCapabilityInstanceExhausted`,
//! `WindowInitialExceedsMax`, `WindowSelectionHasLimit`,
//! `WindowAggregateResultLimit`, `EngineClosed`, `PublishRefused` -- nothing
//! about routing or indexers. `nmp-ffi` adds none.
//!
//! ## So this module measures instead of arguing
//!
//! [`probe`] publishes one `Auto` kind:1 against a stated configuration and
//! reports what actually happened inside a budget. [`matrix`] runs the four
//! interesting configurations. The finding is whatever comes out, and the
//! numbers are in the exerciser's output rather than in prose here.
//!
//! ## What IS true, and is the more useful version of the claim
//!
//! A parked write is indistinguishable from a slow one at the app's surface.
//! `WriteFact::Destinations { complete: false, awaiting_author_routes }` is the
//! only door that says why, it is only on the LIVE receipt stream, and
//! `PublishQueueEntry` -- the per-row and post-restart door -- does not carry
//! it (see the `findings` entry on PublishQueueEntry). An app whose composer spinner never stops has
//! exactly one place to look and it is the place a UI does not keep open.

use std::time::Duration;

use nmp::{
    Engine, EngineConfig, EventBuilder, Identity, Kind, PublicKey, RelayUrl, WriteFact,
    WriteIntent, WriteOutcome, WritePayload, WriteRouting,
};

/// One measured configuration.
#[derive(Debug, Clone)]
pub struct Probe {
    pub label: &'static str,
    pub app_relays: Vec<String>,
    pub indexers: Vec<RelayUrl>,
}

/// What actually happened to one `Auto` write.
#[derive(Debug, Clone)]
pub struct Observed {
    pub label: &'static str,
    /// `Err` iff `Engine::publish` itself refused. This is the field the
    /// "Auto is refused" claim predicts is populated.
    pub publish_refused: Option<String>,
    pub route_complete: bool,
    pub intended_relays: usize,
    pub awaiting_authors: usize,
    pub outcome: Option<WriteOutcome>,
    pub facts_seen: usize,
}

impl std::fmt::Display for Observed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.publish_refused {
            Some(reason) => write!(f, "{:<28} REFUSED at publish: {reason}", self.label),
            None => write!(
                f,
                "{:<28} accepted; route_complete={} intended={} awaiting_authors={} outcome={:?} ({} fact(s))",
                self.label,
                self.route_complete,
                self.intended_relays,
                self.awaiting_authors,
                self.outcome,
                self.facts_seen
            ),
        }
    }
}

/// Build an engine for `probe`, publish one `Auto` kind:1, and report.
pub fn run(probe: &Probe, budget: Duration) -> Result<Observed, nmp::EngineError> {
    let config = EngineConfig {
        store_path: None,
        app_relays: probe.app_relays.clone(),
        ..EngineConfig::default()
    };
    let route_provider: Option<Box<dyn nmp::AuthorRouteProvider>> = if probe.indexers.is_empty() {
        None
    } else {
        Some(Box::new(nmp_outbox::Nip65Outbox::new(
            probe.indexers.clone(),
        )))
    };
    let engine = Engine::new_with_capabilities_and_routing(config, Vec::new(), route_provider)?;
    let author: PublicKey = engine
        .add_private_key_account(&[3u8; 32], true)
        .expect("a valid secret key")
        .public_key;

    let intent = WriteIntent {
        payload: WritePayload::Event(EventBuilder::new(Kind::from(1u16)).content("routing probe")),
        routing: WriteRouting::Auto,
        identity: Identity::Explicit(author),
    };

    let mut observed = Observed {
        label: probe.label,
        publish_refused: None,
        route_complete: false,
        intended_relays: 0,
        awaiting_authors: 0,
        outcome: None,
        facts_seen: 0,
    };

    match engine.publish(intent) {
        Err(error) => observed.publish_refused = Some(error.to_string()),
        Ok(stream) => {
            let deadline = std::time::Instant::now() + budget;
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match stream.statuses.recv_timeout(remaining) {
                    Ok(fact) => {
                        observed.facts_seen += 1;
                        match fact {
                            WriteFact::Destinations {
                                relays,
                                complete,
                                awaiting_author_routes,
                            } => {
                                observed.intended_relays = relays.len();
                                observed.route_complete = complete;
                                observed.awaiting_authors = awaiting_author_routes.len();
                            }
                            WriteFact::Outcome(outcome) => {
                                observed.outcome = Some(outcome);
                                break;
                            }
                            WriteFact::Signing(_) | WriteFact::Relay { .. } => {}
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    engine.shutdown();
    Ok(observed)
}

/// The four configurations worth telling apart.
#[must_use]
pub fn matrix() -> Vec<Probe> {
    let indexer = RelayUrl::parse("wss://127.0.0.1:1").expect("a well-formed relay url");
    vec![
        Probe {
            label: "nothing configured",
            app_relays: Vec::new(),
            indexers: Vec::new(),
        },
        Probe {
            label: "app relays, no indexers",
            app_relays: vec!["wss://127.0.0.1:1".to_string()],
            indexers: Vec::new(),
        },
        Probe {
            label: "indexers, no app relays",
            app_relays: Vec::new(),
            indexers: vec![indexer.clone()],
        },
        Probe {
            label: "both",
            app_relays: vec!["wss://127.0.0.1:1".to_string()],
            indexers: vec![indexer],
        },
    ]
}
