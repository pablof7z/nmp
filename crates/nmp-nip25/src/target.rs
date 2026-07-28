use std::collections::BTreeSet;
use std::fmt;
use std::sync::mpsc::RecvError;

use nmp::{
    Binding, Demand, Engine, EngineError, EventId, Filter, Freshness, LiveQuery, Row, RowDelta,
};
use nostr::nips::nip25 as nostr_nip25;

/// A native Nostr-event reaction target qualified from one canonical NMP row.
///
/// Every field stays private. In particular, callers cannot provide a kind,
/// target author, address coordinate, or relay hint independently. The relay
/// hint is the deterministic first member of the canonical row's sorted
/// provenance set, or absent when the canonical row has no relay provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionTarget {
    pub(crate) inner: nostr_nip25::ReactionTarget,
}

impl ReactionTarget {
    pub(crate) fn from_canonical_row(row: Row) -> Result<Self, ReactionTargetError> {
        let event_id = row.event.id;
        row.event
            .verify()
            .map_err(|_| ReactionTargetError::TargetNotVerified { event_id })?;
        let relay_hint = row.sources.iter().next().cloned();
        Ok(Self {
            inner: nostr_nip25::ReactionTarget::new(&row.event, relay_hint),
        })
    }
}

/// Failure to qualify one reaction target through NMP's canonical read path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionTargetError {
    EngineClosed,
    CanonicalLookupUnavailable { reason: String },
    TargetNotFound { event_id: EventId },
    TargetNotVerified { event_id: EventId },
}

impl fmt::Display for ReactionTargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EngineClosed => f.write_str("engine already shut down"),
            Self::CanonicalLookupUnavailable { reason } => {
                write!(f, "canonical target lookup unavailable: {reason}")
            }
            Self::TargetNotFound { event_id } => {
                write!(f, "event {event_id} is not in the canonical NMP store")
            }
            Self::TargetNotVerified { event_id } => {
                write!(f, "canonical row {event_id} is not a verified signed event")
            }
        }
    }
}

impl std::error::Error for ReactionTargetError {}

fn map_engine_error(error: EngineError) -> ReactionTargetError {
    match error {
        EngineError::EngineClosed => ReactionTargetError::EngineClosed,
        other => ReactionTargetError::CanonicalLookupUnavailable {
            reason: other.to_string(),
        },
    }
}

fn map_recv_error(_: RecvError) -> ReactionTargetError {
    ReactionTargetError::CanonicalLookupUnavailable {
        reason: "canonical observation closed before its initial frame".to_string(),
    }
}

/// Qualify one native-event reaction target through an ordinary cache-only
/// query for `event_id`.
///
/// The temporary observation is released before this function returns.
/// `Freshness::CacheOnly` guarantees this lookup never opens network demand.
/// A caller-supplied row or relay array is not accepted, so fabricated native
/// `Row.sources` can affect neither NIP-25 hints nor routing.
pub fn reaction_target(
    engine: &Engine,
    event_id: EventId,
) -> Result<ReactionTarget, ReactionTargetError> {
    let mut demand = Demand::from_filter(Filter {
        ids: Some(Binding::Literal(BTreeSet::from([event_id.to_hex()]))),
        ..Filter::default()
    });
    demand.freshness = Freshness::CacheOnly;

    let observation = engine
        .observe(LiveQuery(demand), None)
        .map_err(map_engine_error)?;
    let frame = observation.recv().map_err(map_recv_error)?;
    let row = frame.deltas.into_iter().find_map(|delta| match delta {
        RowDelta::Added(row) if row.event.id == event_id => Some(row),
        RowDelta::Added(_) | RowDelta::SourcesGrew { .. } | RowDelta::Removed(_) => None,
    });
    drop(observation);

    row.ok_or(ReactionTargetError::TargetNotFound { event_id })
        .and_then(ReactionTarget::from_canonical_row)
}
