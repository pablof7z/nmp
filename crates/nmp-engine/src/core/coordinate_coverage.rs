//! Reusing ordinary query evidence for one replaceable/addressable
//! coordinate (#1630).
//!
//! A caller that needs one relay's CURRENT value for a coordinate — the
//! per-relay publish gate of #1631 is the first — must not open its own REQ
//! when the ordinary query owner already asked that relay the same question,
//! or a broader one whose answer necessarily contains it. This module is
//! that lookup, and the one door that opens an ordinary observation when
//! (and only when) nothing covers it.
//!
//! Three deliberate non-mechanisms, from #1630's own stop point: there is no
//! second request lifecycle (the evidence read here is the live-wire-request
//! bookkeeping every ordinary REQ already produces), no publisher-private
//! cache (nothing is stored per caller), and no durable request identity
//! (every witness here dies with its request, so a restart simply repeats
//! the ordinary check).
//!
//! ## What "covering" means, exactly
//!
//! A request covers a coordinate when every event that relay holds for that
//! coordinate is inside the request's SELECTION: no `ids` constraint, a
//! `kinds`/`authors` set that is absent or contains the coordinate's, and no
//! tag constraint other than a `#d` that contains an addressable
//! coordinate's identifier. Selection is only half of it — the window and
//! the result bound decide what the request can then PROVE:
//!
//! - **Presence.** A covering request with no `until` that delivered the
//!   coordinate has this relay's current value for it. NIP-01 returns the
//!   newest matching events, so a newer version could only have been hidden
//!   by an upper time bound — truncation drops older events, never newer
//!   ones, and a `since` floor cannot hide anything above itself.
//! - **Absence.** A covering request proves the relay holds nothing only if
//!   it also had no `since`/`until` at all, finished its stored events with
//!   a committed coverage interval from the beginning of time (the existing
//!   authority: a router-bounded REQ never earns one), and returned strictly
//!   fewer frames than the bound below.
//!
//! Everything else is ambiguous and costs exactly one ordinary REQ.
//!
//! The returned-frame count is taken at this reducer's own frame doors, so
//! it counts what the relay RETURNED rather than what the store accepted —
//! a stale, refused, or otherwise unstored event still counts. Every frame
//! the reducer can see but cannot hand to one request erases that count
//! instead of being ignored. One class remains invisible: `nmp-transport`
//! drops a text frame it cannot parse without any engine-visible signal
//! (#1668), so a relay that both truncates at the bound and emits an
//! undecodable frame could have its truncated answer read as complete.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    Binding, CacheMode, ConcreteFilter, Demand, Filter, Freshness, IndexedTagName, LiveQuery,
    ReadRouting, RelaySessionKey,
};
use nmp_router::SubId;
use nmp_store::CoverageInterval;
use nostr::nips::nip01::Coordinate;
use nostr::{Event, EventId, Timestamp};

use super::observation::StoredEvents;
use super::{CoreState, Effect, ObservationId};

/// The number of returned EVENT frames at or above which one request's
/// stored-events answer is treated as possibly truncated by the relay.
///
/// Fixed on purpose. Reading a relay's advertised NIP-11 `default_limit`
/// instead is #744's subject and must not expand this path; until it lands,
/// one conservative constant is the honest bound.
const RETURNED_FRAME_BOUND: u64 = 500;

/// Upper bound on how many distinct coordinates one request remembers
/// having delivered.
///
/// A long-lived unconstrained subscription would otherwise accumulate one
/// entry per replaceable coordinate the relay ever streams to it. Hitting
/// the bound costs reuse (the check falls back to one ordinary REQ), never
/// correctness.
const WITNESS_LIMIT: usize = 500;

/// The exact number of EVENT frames one relay returned during a request's
/// stored-events phase, or the fact that no exact number is available.
///
/// A frame this reducer cannot attribute to a specific request would make
/// every count on that session an undercount, and an undercount is exactly
/// what would let a truncated answer masquerade as a complete one. Such a
/// frame therefore erases the count rather than being ignored: a request
/// with no exact count can still WITNESS a coordinate, but can never prove
/// one absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReturnedFrames {
    Counted(u64),
    Unattributable,
}

impl ReturnedFrames {
    fn record(&mut self) {
        if let Self::Counted(count) = self {
            *count = count.saturating_add(1);
        }
    }

    fn erase(&mut self) {
        *self = Self::Unattributable;
    }

    fn under(self, bound: u64) -> bool {
        matches!(self, Self::Counted(count) if count < bound)
    }
}

/// One coordinate value one request actually delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WitnessedCoordinate {
    pub(super) event_id: EventId,
    pub(super) created_at: Timestamp,
}

/// Process-local evidence about what one accepted REQ returned. It is
/// created with the request, dropped with it, and never persisted.
#[derive(Debug, Clone)]
pub(super) struct RequestReturnEvidence {
    stored_frames: ReturnedFrames,
    witnessed: BTreeMap<Coordinate, WitnessedCoordinate>,
}

impl Default for RequestReturnEvidence {
    /// A request that has returned nothing yet has returned exactly nothing
    /// — an exact zero, never an absent count.
    fn default() -> Self {
        Self {
            stored_frames: ReturnedFrames::Counted(0),
            witnessed: BTreeMap::new(),
        }
    }
}

/// What the ordinary query owner already knows about one relay session's
/// current value for one replaceable/addressable coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoordinateCoverage {
    /// A covering request on this session delivered the coordinate, and
    /// nothing about that request's shape could have hidden a newer
    /// version. Zero new REQ.
    Witnessed {
        event_id: EventId,
        created_at: Timestamp,
    },
    /// A covering request finished its stored events, over the whole of
    /// time, under the returned-frame bound, without the coordinate. This
    /// relay holds no value for it. Zero new REQ.
    ProvenAbsent,
    /// A request asking this coordinate and nothing else is already
    /// outstanding on this session. Its terminal answers this caller too;
    /// zero duplicate REQ.
    InFlight { sub_id: SubId },
    /// This coordinate HAS been asked on this session, by a `limit: 0` REQ
    /// that requests no stored event, so its own terminal answers nothing
    /// (#1683). No engine path currently mints such a request; an app whose
    /// own live query carries `limit: 0` still reaches this classification.
    ///
    /// Deliberately not folded into [`Self::Uncovered`]. The two are opposite
    /// facts — "asked, answer still coming" versus "nothing has asked" — and
    /// collapsing them leaves a caller that needs an answer choosing between
    /// waiting forever and acting without one. A caller that cannot act
    /// unanswered waits for this; a caller that can, proceeds knowing it was
    /// asked.
    Reconciling { sub_id: SubId },
    /// Nothing covers it. Exactly one ordinary REQ answers it.
    Uncovered,
}

impl CoreState {
    /// Count one EVENT frame this relay returned, against the request that
    /// asked for it.
    ///
    /// A frame whose wire subscription id resolves to no live request is not
    /// merely uncounted — it erases the exact count of every request still
    /// streaming on that session, because it could have belonged to any of
    /// them.
    pub(in crate::core) fn record_returned_event_frame(
        &mut self,
        session: &RelaySessionKey,
        wire_sub_id: &str,
    ) {
        let Some(sub_id) = self.attribution.sub_id_for_wire(session, wire_sub_id) else {
            self.erase_returned_frame_counts(session);
            return;
        };
        let Some(live) = self.live_wire_requests.get_mut(&(session.clone(), sub_id)) else {
            self.erase_returned_frame_counts(session);
            return;
        };
        // Frames after the stored-events terminal are the live tail, not part
        // of the answer whose truncation this bound is about.
        if matches!(live.stored_events, StoredEvents::Streaming { .. }) {
            live.returns.stored_frames.record();
        }
    }

    /// Erase the exact returned-frame count of every request still streaming
    /// on `session`.
    ///
    /// The callers are every door at which this reducer learns a relay
    /// returned an EVENT frame it cannot hand to one specific request: a
    /// preparsed committed-observation hit (which carries no subscription
    /// id), an unknown wire subscription id, and a health report of frames
    /// the transport rejected before the reducer ever saw them.
    pub(in crate::core) fn erase_returned_frame_counts(&mut self, session: &RelaySessionKey) {
        for ((request_session, _), live) in &mut self.live_wire_requests {
            if request_session == session
                && matches!(live.stored_events, StoredEvents::Streaming { .. })
            {
                live.returns.stored_frames.erase();
            }
        }
    }

    /// Remember that one request delivered one replaceable/addressable
    /// coordinate. Only the newest delivered value per coordinate is kept.
    pub(in crate::core) fn record_coordinate_witness(
        &mut self,
        session: &RelaySessionKey,
        wire_sub_id: &str,
        coordinate: Coordinate,
        witness: WitnessedCoordinate,
    ) {
        let Some(sub_id) = self.attribution.sub_id_for_wire(session, wire_sub_id) else {
            return;
        };
        let Some(live) = self.live_wire_requests.get_mut(&(session.clone(), sub_id)) else {
            return;
        };
        // A request whose shape can never support a presence claim keeps no
        // witnesses at all, rather than accumulating unusable ones.
        if !request_can_witness(&live.filter) {
            return;
        }
        let full = live.returns.witnessed.len() >= WITNESS_LIMIT;
        match live.returns.witnessed.entry(coordinate) {
            Entry::Occupied(mut slot) => {
                if witness.created_at > slot.get().created_at {
                    slot.insert(witness);
                }
            }
            Entry::Vacant(slot) => {
                if !full {
                    slot.insert(witness);
                }
            }
        }
    }

    /// What this session's ordinary requests already prove about
    /// `coordinate`. Pure read: it sends nothing and records nothing.
    ///
    /// Access context is part of `session`, so public coverage can never
    /// answer a NIP-42 question and vice versa.
    pub(in crate::core) fn coordinate_coverage(
        &self,
        coordinate: &Coordinate,
        session: &RelaySessionKey,
    ) -> CoordinateCoverage {
        let mut absent = false;
        let mut in_flight: Option<SubId> = None;
        let mut reconciling: Option<SubId> = None;
        for ((request_session, sub_id), live) in &self.live_wire_requests {
            if request_session != session || !selects_coordinate(&live.filter, coordinate) {
                continue;
            }
            if live.filter.until.is_none() {
                if let Some(witness) = live.returns.witnessed.get(coordinate) {
                    return CoordinateCoverage::Witnessed {
                        event_id: witness.event_id,
                        created_at: witness.created_at,
                    };
                }
            }
            match live.stored_events {
                StoredEvents::Finished {
                    committed_interval, ..
                } => {
                    absent |= proves_absence(
                        &live.filter,
                        committed_interval,
                        live.returns.stored_frames,
                    );
                }
                StoredEvents::Streaming { .. } => {
                    match coordinate_request_shape(&live.filter, coordinate) {
                        Some(CoordinateRequestShape::Answerable) => {
                            in_flight = min_sub_id(in_flight, sub_id);
                        }
                        Some(CoordinateRequestShape::LiveFirstBarrier) => {
                            reconciling = min_sub_id(reconciling, sub_id);
                        }
                        None => {}
                    }
                }
            }
        }
        if absent {
            return CoordinateCoverage::ProvenAbsent;
        }
        if let Some(sub_id) = in_flight.or_else(|| {
            self.awaiting_coordinate_request(
                coordinate,
                session,
                CoordinateRequestShape::Answerable,
            )
        }) {
            return CoordinateCoverage::InFlight { sub_id };
        }
        // Only after no answerable request exists: a barrier means the
        // question is asked but its own terminal will not answer it.
        reconciling
            .or_else(|| {
                self.awaiting_coordinate_request(
                    coordinate,
                    session,
                    CoordinateRequestShape::LiveFirstBarrier,
                )
            })
            .map_or(CoordinateCoverage::Uncovered, |sub_id| {
                CoordinateCoverage::Reconciling { sub_id }
            })
    }

    /// An exact coordinate REQ this reducer has already minted but the
    /// transport has not accepted yet — awaiting handoff, or parked for
    /// retry after a refusal. Opening a second one would duplicate it.
    fn awaiting_coordinate_request(
        &self,
        coordinate: &Coordinate,
        session: &RelaySessionKey,
        shape: CoordinateRequestShape,
    ) -> Option<SubId> {
        let mut found = None;
        for ((request_session, _), queue) in &self.pending_request_evidence {
            if request_session != session {
                continue;
            }
            for request in queue {
                if coordinate_request_shape(&request.filter, coordinate) == Some(shape) {
                    found = min_sub_id(found, &request.sub_id);
                }
            }
        }
        for attempt in self.attempts.retried_attempts_for_session(session) {
            if coordinate_request_shape(&attempt.filter, coordinate) == Some(shape) {
                found = min_sub_id(found, &attempt.sub_id);
            }
        }
        found
    }

    /// Ask the ordinary query owner for one relay session's current value
    /// for `coordinate`, opening exactly one ordinary observation when — and
    /// only when — nothing already covers it.
    ///
    /// The coverage returned is the answer AT DECISION TIME, so the one case
    /// that opens anything reports [`CoordinateCoverage::Uncovered`] together
    /// with the observation that will answer it. That observation id is the
    /// caller's to close; no owner map is kept here, so an observation this
    /// door does not hand back is one it never opened.
    pub(in crate::core) fn open_coordinate_observation(
        &mut self,
        coordinate: &Coordinate,
        session: &RelaySessionKey,
        effects: &mut Vec<Effect>,
    ) -> (CoordinateCoverage, Option<ObservationId>) {
        let coverage = self.coordinate_coverage(coordinate, session);
        if !matches!(coverage, CoordinateCoverage::Uncovered) {
            return (coverage, None);
        }
        let demand = Demand::new(
            coordinate_filter(coordinate),
            ReadRouting::Explicit(vec![session.relay.clone()]),
        )
        .expect("one relay-pinned coordinate demand is never empty");
        let demand = Demand {
            cache: CacheMode::Strict,
            freshness: Freshness::Live,
            // This read is pinned to ONE existing session, so it must ask as
            // whoever that session already is — otherwise a coordinate check
            // on an authenticated socket would be answered by a demand that
            // named nobody.
            authenticate_as: session.authenticate_as,
            ..demand
        };
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.coordinate_reuse_new_reqs
            .set(self.coordinate_reuse_new_reqs.get().saturating_add(1));
        let opened = self.on_subscribe(LiveQuery::single(demand));
        let observation = opened.iter().find_map(|effect| match effect {
            Effect::EmitRows(id, ..) => Some(*id),
            _ => None,
        });
        effects.extend(opened);
        (coverage, observation)
    }
}

fn min_sub_id(current: Option<SubId>, candidate: &SubId) -> Option<SubId> {
    match current {
        Some(current) if current <= *candidate => Some(current),
        _ => Some(candidate.clone()),
    }
}

/// The ordinary demand filter for one coordinate: its kind and author, plus
/// the `d` identifier an addressable kind carries, over the whole of time.
fn coordinate_filter(coordinate: &Coordinate) -> Filter {
    let mut tags = BTreeMap::new();
    if coordinate.kind.is_addressable() {
        tags.insert(
            IndexedTagName::new('d').expect("d is an indexed Nostr tag"),
            Binding::Literal(BTreeSet::from([coordinate.identifier.clone()])),
        );
    }
    Filter {
        kinds: Some(BTreeSet::from([coordinate.kind.as_u16()])),
        authors: Some(Binding::Literal(BTreeSet::from([coordinate
            .public_key
            .to_hex()]))),
        tags,
        ..Filter::default()
    }
}

/// The coordinate one event is the current candidate for, or `None` for a
/// kind NIP-01 gives no coordinate at all.
pub(super) fn event_coordinate(event: &Event) -> Option<Coordinate> {
    if event.kind.is_addressable() {
        Some(Coordinate {
            kind: event.kind,
            public_key: event.pubkey,
            identifier: event.tags.identifier().unwrap_or_default().to_string(),
        })
    } else if event.kind.is_replaceable() {
        Some(Coordinate {
            kind: event.kind,
            public_key: event.pubkey,
            identifier: String::new(),
        })
    } else {
        None
    }
}

/// Whether every event a relay holds for `coordinate` is inside `filter`'s
/// SELECTION. Window and result bound are deliberately excluded — they
/// decide what a covering request can prove, not whether it covers.
fn selects_coordinate(filter: &ConcreteFilter, coordinate: &Coordinate) -> bool {
    if filter.ids.is_some() {
        return false;
    }
    if filter
        .kinds
        .as_ref()
        .is_some_and(|kinds| !kinds.contains(&coordinate.kind.as_u16()))
    {
        return false;
    }
    if filter
        .authors
        .as_ref()
        .is_some_and(|authors| !authors.contains(&coordinate.public_key.to_hex()))
    {
        return false;
    }
    // Any tag constraint other than an addressable coordinate's own `#d`
    // may exclude the very event we are asking about.
    filter.tags.iter().all(|(tag, values)| {
        tag.as_char() == 'd'
            && coordinate.kind.is_addressable()
            && values.contains(&coordinate.identifier)
    })
}

/// A request whose selection is this coordinate and nothing else, over the
/// whole of time. Its terminal answers the coordinate question directly, so
/// a second caller joins it rather than sending a duplicate REQ.
/// What a request whose selection is exactly one coordinate can do about it.
///
/// The `limit: 0` case used to be an unnamed rejection inside
/// the exactness predicate, which made a request that HAD asked the
/// question indistinguishable from no request at all (#1683). Naming it here
/// is what lets [`CoordinateCoverage`] tell those apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoordinateRequestShape {
    /// Asks for the stored event, so its terminal is an answer.
    Answerable,
    /// `limit: 0` requests no stored event, so its own terminal answers
    /// nothing.
    LiveFirstBarrier,
}

/// The shape of `filter` as a question about `coordinate`, or `None` when it
/// is not exactly that question.
fn coordinate_request_shape(
    filter: &ConcreteFilter,
    coordinate: &Coordinate,
) -> Option<CoordinateRequestShape> {
    let addressing = if coordinate.kind.is_addressable() {
        filter.tags.len() == 1 && filter.tags.values().all(|values| values.len() == 1)
    } else {
        filter.tags.is_empty()
    };
    let exact = selects_coordinate(filter, coordinate)
        && addressing
        && filter.kinds.as_ref().is_some_and(|kinds| kinds.len() == 1)
        && filter
            .authors
            .as_ref()
            .is_some_and(|authors| authors.len() == 1)
        && filter.since.is_none()
        && filter.until.is_none();
    if !exact {
        return None;
    }
    Some(if filter.limit == Some(0) {
        CoordinateRequestShape::LiveFirstBarrier
    } else {
        CoordinateRequestShape::Answerable
    })
}

/// Whether one FINISHED covering request proves this relay holds no event
/// for the coordinate it never delivered.
fn proves_absence(
    filter: &ConcreteFilter,
    committed_interval: Option<CoverageInterval>,
    frames: ReturnedFrames,
) -> bool {
    // A narrower window than the coordinate question's own cannot prove
    // empty: a floor hides older values, a ceiling hides newer ones.
    if filter.since.is_some() || filter.until.is_some() {
        return false;
    }
    // The existing coverage authority: a request the router bounded with a
    // NIP-01 `limit`, or whose events failed to commit, never commits an
    // interval at all.
    let Some(interval) = committed_interval else {
        return false;
    };
    interval.from.as_secs() == 0 && frames.under(returned_frame_bound(filter))
}

/// The count at or above which this request's answer may have been
/// truncated: the fixed relay bound, or the request's own `limit` when that
/// is tighter.
fn returned_frame_bound(filter: &ConcreteFilter) -> u64 {
    filter
        .limit
        .and_then(|limit| u64::try_from(limit).ok())
        .map_or(RETURNED_FRAME_BOUND, |limit| {
            limit.min(RETURNED_FRAME_BOUND)
        })
}

/// Whether a request's shape could ever support a presence claim. Checked
/// once per request when recording, so a request that can never witness
/// keeps no witnesses.
fn request_can_witness(filter: &ConcreteFilter) -> bool {
    filter.ids.is_none()
        && filter.until.is_none()
        && filter.tags.keys().all(|tag| tag.as_char() == 'd')
}

