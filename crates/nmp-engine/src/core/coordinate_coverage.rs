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
    /// This coordinate HAS been asked on this session, by NIP-77's live-first
    /// barrier: a `limit: 0` REQ that requests no stored event, so its own
    /// terminal answers nothing. The answer arrives when reconciliation
    /// finishes and credits coverage (#1683).
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
            .or_else(|| self.reconciling_coordinate_session(coordinate, session))
            .map_or(CoordinateCoverage::Uncovered, |sub_id| {
                CoordinateCoverage::Reconciling { sub_id }
            })
    }

    /// An exact coordinate REQ this reducer has already minted but the
    /// transport has not accepted yet — awaiting handoff, or parked for
    /// retry after a refusal. Opening a second one would duplicate it.
    /// A Negentropy session already reconciling exactly this coordinate on
    /// this session.
    ///
    /// The barrier hands off to this and is abandoned, so without this the
    /// state would read as `Uncovered` again one step after the barrier's
    /// own EOSE — the same window, one moment later.
    fn reconciling_coordinate_session(
        &self,
        coordinate: &Coordinate,
        session: &RelaySessionKey,
    ) -> Option<SubId> {
        // Negentropy runs on the public session only, so it can speak for a
        // public question and never for a protected one — the same
        // access-context rule the rest of this module keeps free by carrying
        // it in the session key.
        if session.authenticate_as.is_some() {
            return None;
        }
        let mut found = None;
        for (sub_id, filter) in self.nip77.sessions_on_relay(&session.relay) {
            if coordinate_request_shape(filter, coordinate).is_some() {
                found = min_sub_id(found, sub_id);
            }
        }
        found
    }

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
    /// NIP-77's live-first barrier: `limit: 0` requests no stored event, so
    /// its own terminal answers nothing and the answer arrives through
    /// reconciliation instead.
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

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use nmp_router_testkit::test_relay;
    use nmp_store::RedbStore;
    use nmp_transport::{RelayFrame, RelayHandle as TransportRelayHandle};
    use nostr::{EventBuilder, Keys, Kind, RelayMessage, RelayUrl, SubscriptionId};

    use super::super::attribution::wire_sub_id_string;
    use super::super::{EngineMsg, RequestAttemptId, RequestHandoffOutcome, SignedEvent, WireOp};
    use super::*;

    /// One relay-pinned ordinary read plus the exact wire request it placed.
    struct Fixture {
        core: CoreState,
        session: RelaySessionKey,
        handle: TransportRelayHandle,
        sub_id: SubId,
    }

    fn relay() -> RelayUrl {
        test_relay(1)
    }

    fn contact_list(author: &Keys, created_at: u64) -> SignedEvent {
        EventBuilder::new(Kind::ContactList, format!("at {created_at}"))
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(author)
            .expect("fixture signs")
    }

    fn note(author: &Keys, created_at: u64) -> SignedEvent {
        EventBuilder::new(Kind::TextNote, format!("note {created_at}"))
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(author)
            .expect("fixture signs")
    }

    fn contact_list_coordinate(author: &Keys) -> Coordinate {
        Coordinate {
            kind: Kind::ContactList,
            public_key: author.public_key(),
            identifier: String::new(),
        }
    }

    fn placed_requests(
        effects: &[Effect],
    ) -> Vec<(RelaySessionKey, SubId, ConcreteFilter, RequestAttemptId)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Wire(delta) => Some(delta),
                _ => None,
            })
            .flat_map(|delta| {
                delta.ops.iter().flat_map(move |(session, ops)| {
                    ops.iter().filter_map(move |op| {
                        let WireOp::Req(sub_id, filter) = op else {
                            return None;
                        };
                        Some((
                            session.clone(),
                            sub_id.clone(),
                            filter.clone(),
                            delta.attempt_id(session, sub_id, filter),
                        ))
                    })
                })
            })
            .collect()
    }

    fn placed_request_count(effects: &[Effect]) -> usize {
        placed_requests(effects).len()
    }

    /// Open one relay-pinned ordinary read for `filter` and drive its REQ all
    /// the way to an accepted wire request.
    fn covering_read(filter: Filter) -> Fixture {
        let relay = relay();
        let session = RelaySessionKey::unauthenticated(relay.clone());
        let handle = TransportRelayHandle {
            slot: 3,
            generation: 1,
        };
        let mut core = CoreState::new(RedbStore::temporary().expect("temporary Redb store"), 32);
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        let demand = Demand::new(filter, ReadRouting::Explicit(vec![relay]))
            .expect("a relay-pinned read is nonempty");
        core.handle(EngineMsg::Subscribe(LiveQuery::single(demand)));
        let admitted = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(1u64)));
        let (_, sub_id, _, attempt_id) = placed_requests(&admitted)
            .into_iter()
            .find(|(request_session, ..)| request_session == &session)
            .expect("the pinned read places exactly one REQ");
        core.on_wire_request_handoff(RequestHandoffOutcome::Accepted { attempt_id, handle });
        Fixture {
            core,
            session,
            handle,
            sub_id,
        }
    }

    fn kind3_read() -> Fixture {
        covering_read(Filter {
            kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
            ..Filter::default()
        })
    }

    impl Fixture {
        fn deliver(&mut self, event: SignedEvent) -> Vec<Effect> {
            self.core.handle(EngineMsg::RelayFrame(
                self.handle,
                self.session.clone(),
                RelayFrame::from_message(RelayMessage::Event {
                    subscription_id: Cow::Owned(SubscriptionId::new(wire_sub_id_string(
                        &self.sub_id,
                    ))),
                    event: Cow::Owned(event),
                }),
            ))
        }

        fn end_stored_events(&mut self) -> Vec<Effect> {
            self.core.handle(EngineMsg::RelayFrame(
                self.handle,
                self.session.clone(),
                RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
                    SubscriptionId::new(wire_sub_id_string(&self.sub_id)),
                ))),
            ))
        }

        /// Report that the transport could not decode one text frame on this
        /// session, the way `PoolEvent::Health` does (#1668). Nothing here
        /// names a subscription, because the text that would have named one
        /// is the text that failed to parse.
        fn report_undecodable_frame(&mut self) -> Vec<Effect> {
            self.core.handle(EngineMsg::RelayHealth(
                self.handle,
                self.session.clone(),
                nmp_transport::RelayHealth {
                    undecodable_frame_count: 1,
                    ..nmp_transport::RelayHealth::default()
                },
            ))
        }

        /// Run the coordinate check the way #1631 will, and report how many
        /// REQs actually reached the wire because of it.
        fn check(&mut self, coordinate: &Coordinate) -> (CoordinateCoverage, usize) {
            let session = self.session.clone();
            let mut effects = Vec::new();
            let (coverage, _) =
                self.core
                    .open_coordinate_observation(coordinate, &session, &mut effects);
            let flushed = self
                .core
                .handle(EngineMsg::FlushWireAdmission(Timestamp::from(9u64)));
            let placed = placed_request_count(&effects) + placed_request_count(&flushed);
            (coverage, placed)
        }

        fn opened_coordinate_reqs(&self) -> u64 {
            self.core.coordinate_reuse_new_reqs.get()
        }
    }

    /// #1630's headline falsifier: a finished covering request that already
    /// delivered the coordinate answers every later check with ZERO new REQs.
    /// Break the reuse decision and this counter is no longer zero.
    #[test]
    fn a_covering_request_that_witnessed_the_coordinate_opens_no_new_req() {
        let alice = Keys::generate();
        let mut fixture = kind3_read();
        fixture.deliver(contact_list(&alice, 100));
        fixture.end_stored_events();

        let coordinate = contact_list_coordinate(&alice);
        let (first, first_placed) = fixture.check(&coordinate);
        let (second, second_placed) = fixture.check(&coordinate);

        assert_eq!(
            (
                fixture.opened_coordinate_reqs(),
                first_placed,
                second_placed
            ),
            (0, 0, 0),
            "reusing covering evidence must open zero coordinate REQs"
        );
        assert!(
            matches!(first, CoordinateCoverage::Witnessed { created_at, .. }
                if created_at == Timestamp::from(100u64)),
            "the covering request delivered this coordinate: {first:?}"
        );
        assert_eq!(second, first, "a second check reuses the same evidence");
    }

    /// The live half of the same rule: a covering request that is still
    /// streaming, but has already delivered the coordinate, is enough. This
    /// is true before EOSE precisely because a coordinate is not a set.
    #[test]
    fn a_streaming_covering_request_that_witnessed_the_coordinate_opens_no_new_req() {
        let alice = Keys::generate();
        let mut fixture = kind3_read();
        fixture.deliver(contact_list(&alice, 100));

        let (coverage, placed) = fixture.check(&contact_list_coordinate(&alice));
        assert!(matches!(coverage, CoordinateCoverage::Witnessed { .. }));
        assert_eq!((fixture.opened_coordinate_reqs(), placed), (0, 0));
    }

    /// The newest delivered version wins: a later relay event replaces the
    /// witness, so the caller never rebases onto a stale one.
    #[test]
    fn the_newest_delivered_version_is_the_witnessed_one() {
        let alice = Keys::generate();
        let mut fixture = kind3_read();
        fixture.deliver(contact_list(&alice, 100));
        let newer = contact_list(&alice, 200);
        fixture.deliver(newer.clone());

        let (coverage, _) = fixture.check(&contact_list_coordinate(&alice));
        assert_eq!(
            coverage,
            CoordinateCoverage::Witnessed {
                event_id: newer.id,
                created_at: Timestamp::from(200u64),
            }
        );
    }

    /// An uncovered coordinate costs exactly one ordinary REQ, and the
    /// second caller joins that in-flight exact request instead of sending a
    /// duplicate.
    #[test]
    fn an_uncovered_coordinate_costs_one_req_and_the_next_caller_joins_it() {
        let alice = Keys::generate();
        let unrelated = Keys::generate();
        let mut fixture = covering_read(Filter {
            kinds: Some(BTreeSet::from([Kind::TextNote.as_u16()])),
            authors: Some(Binding::Literal(BTreeSet::from([unrelated
                .public_key()
                .to_hex()]))),
            ..Filter::default()
        });

        let coordinate = contact_list_coordinate(&alice);
        let (first, first_placed) = fixture.check(&coordinate);
        assert_eq!(first, CoordinateCoverage::Uncovered);
        assert_eq!(first_placed, 1, "an uncovered coordinate costs one REQ");
        assert_eq!(fixture.opened_coordinate_reqs(), 1);

        let (second, second_placed) = fixture.check(&coordinate);
        assert!(
            matches!(second, CoordinateCoverage::InFlight { .. }),
            "the second caller joins the exact request already asking: {second:?}"
        );
        assert_eq!(second_placed, 0, "joining sends nothing");
        assert_eq!(
            fixture.opened_coordinate_reqs(),
            1,
            "only the first, uncovered check ever opened a REQ"
        );
    }

    /// A covering request that finished under the returned-frame bound
    /// without the coordinate proves this relay holds no value for it.
    #[test]
    fn a_finished_covering_request_under_the_bound_proves_absence() {
        let alice = Keys::generate();
        let other = Keys::generate();
        let mut fixture = kind3_read();
        fixture.deliver(contact_list(&other, 50));
        fixture.end_stored_events();

        let (coverage, placed) = fixture.check(&contact_list_coordinate(&alice));
        assert_eq!(coverage, CoordinateCoverage::ProvenAbsent);
        assert_eq!((fixture.opened_coordinate_reqs(), placed), (0, 0));
    }

    /// ... and a request that returned the full bound may have been
    /// truncated, so it proves nothing and costs exactly one REQ.
    #[test]
    fn a_covering_request_at_the_returned_frame_bound_cannot_prove_absence() {
        let alice = Keys::generate();
        let noisy = Keys::generate();
        let mut fixture = covering_read(Filter {
            kinds: Some(BTreeSet::from([
                Kind::TextNote.as_u16(),
                Kind::ContactList.as_u16(),
            ])),
            ..Filter::default()
        });
        for index in 0..RETURNED_FRAME_BOUND {
            fixture.deliver(note(&noisy, 1_000 + index));
        }
        fixture.end_stored_events();

        let (coverage, placed) = fixture.check(&contact_list_coordinate(&alice));
        assert_eq!(
            coverage,
            CoordinateCoverage::Uncovered,
            "a possibly truncated answer proves nothing"
        );
        assert_eq!((fixture.opened_coordinate_reqs(), placed), (1, 1));
    }

    /// A request whose time window is tighter than the coordinate question's
    /// cannot prove empty: an older current value would be below its floor.
    #[test]
    fn a_tighter_windowed_covering_request_cannot_prove_absence() {
        let alice = Keys::generate();
        let mut fixture = covering_read(Filter {
            kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
            since: Some(5_000),
            ..Filter::default()
        });
        fixture.end_stored_events();

        let (coverage, placed) = fixture.check(&contact_list_coordinate(&alice));
        assert_eq!(coverage, CoordinateCoverage::Uncovered);
        assert_eq!((fixture.opened_coordinate_reqs(), placed), (1, 1));
    }

    /// A returned EVENT frame this reducer cannot hand to one request erases
    /// the exact count, so absence is no longer provable from that request.
    #[test]
    fn an_unattributable_returned_frame_forfeits_the_absence_proof() {
        let alice = Keys::generate();
        let other = Keys::generate();
        let mut fixture = kind3_read();
        fixture.deliver(contact_list(&other, 50));
        let session = fixture.session.clone();
        fixture
            .core
            .record_returned_event_frame(&session, "a-subscription-nobody-owns");
        fixture.end_stored_events();

        let (coverage, placed) = fixture.check(&contact_list_coordinate(&alice));
        assert_eq!(coverage, CoordinateCoverage::Uncovered);
        assert_eq!((fixture.opened_coordinate_reqs(), placed), (1, 1));
    }

    /// #1668's falsifier, and the exact minimal pair of
    /// `a_finished_covering_request_under_the_bound_proves_absence`: the same
    /// covering request, finished under the bound without the coordinate,
    /// stops proving absence the moment the transport reports one text frame
    /// it could not decode.
    ///
    /// That frame may have been an EVENT for this very request. Before #1668
    /// the reducer never heard about it, so this setup read as a complete
    /// answer and declared the coordinate absent — the one silent way a
    /// truncated answer could masquerade as a whole one.
    #[test]
    fn an_undecodable_frame_forfeits_the_absence_proof() {
        let alice = Keys::generate();
        let other = Keys::generate();
        let mut fixture = kind3_read();
        fixture.deliver(contact_list(&other, 50));
        fixture.report_undecodable_frame();
        fixture.end_stored_events();

        let (coverage, placed) = fixture.check(&contact_list_coordinate(&alice));
        assert_eq!(
            coverage,
            CoordinateCoverage::Uncovered,
            "a session that dropped an undecodable frame can prove nothing absent"
        );
        assert_eq!(
            (fixture.opened_coordinate_reqs(), placed),
            (1, 1),
            "the exact coordinate query is used instead, exactly once"
        );
    }

    /// The undecodable report is a fact about the SESSION, not one request:
    /// the frame named no subscription, so every request still streaming
    /// there loses its exact count, not merely the one that happened to be
    /// checked.
    #[test]
    fn an_undecodable_frame_forfeits_absence_for_every_streaming_request() {
        let alice = Keys::generate();
        let mut fixture = kind3_read();
        let session = fixture.session.clone();

        // A second relay-pinned read on the same session, accepted onto the
        // wire alongside the first.
        let second = Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([Kind::TextNote.as_u16()])),
                ..Filter::default()
            },
            ReadRouting::Explicit(vec![relay()]),
        )
        .expect("a relay-pinned read is nonempty");
        fixture
            .core
            .handle(EngineMsg::Subscribe(LiveQuery::single(second)));
        let admitted = fixture
            .core
            .handle(EngineMsg::FlushWireAdmission(Timestamp::from(2u64)));
        for (_, _, _, attempt_id) in placed_requests(&admitted) {
            fixture
                .core
                .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
                    attempt_id,
                    handle: fixture.handle,
                });
        }

        fixture.report_undecodable_frame();

        let streaming_with_exact_counts = fixture
            .core
            .live_wire_requests
            .iter()
            .filter(|((request_session, _), live)| {
                request_session == &session
                    && matches!(live.stored_events, StoredEvents::Streaming { .. })
                    && live.returns.stored_frames != ReturnedFrames::Unattributable
            })
            .count();
        assert_eq!(
            streaming_with_exact_counts, 0,
            "one undecodable frame erases the count of every request streaming on the session"
        );

        fixture.end_stored_events();
        let (coverage, _) = fixture.check(&contact_list_coordinate(&alice));
        assert_eq!(coverage, CoordinateCoverage::Uncovered);
    }

    /// Public coverage never answers a NIP-42 question: coverage is keyed by
    /// the physical session, and the protected one may hold more.
    #[test]
    fn public_coverage_does_not_satisfy_a_protected_check() {
        let alice = Keys::generate();
        let mut fixture = kind3_read();
        fixture.deliver(contact_list(&alice, 100));
        fixture.end_stored_events();

        let protected = RelaySessionKey::new(relay(), Some(Keys::generate().public_key()));
        assert_eq!(
            fixture
                .core
                .coordinate_coverage(&contact_list_coordinate(&alice), &protected),
            CoordinateCoverage::Uncovered
        );
    }

    /// Restart forgets every process-local witness: a fresh reducer repeats
    /// the ordinary check from scratch, and no store row remembers the old
    /// request.
    #[test]
    fn restart_forgets_witnesses_and_repeats_the_check() {
        let alice = Keys::generate();
        let mut fixture = kind3_read();
        fixture.deliver(contact_list(&alice, 100));
        fixture.end_stored_events();
        assert!(matches!(
            fixture
                .core
                .coordinate_coverage(&contact_list_coordinate(&alice), &fixture.session),
            CoordinateCoverage::Witnessed { .. }
        ));

        let restarted = CoreState::new(RedbStore::temporary().expect("temporary Redb store"), 32);
        assert_eq!(
            restarted.coordinate_coverage(&contact_list_coordinate(&alice), &fixture.session),
            CoordinateCoverage::Uncovered
        );
    }

    #[test]
    fn selection_rejects_shapes_that_could_hide_the_coordinate() {
        let alice = Keys::generate();
        let coordinate = contact_list_coordinate(&alice);
        let base = ConcreteFilter {
            kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
            authors: Some(BTreeSet::from([alice.public_key().to_hex()])),
            ..ConcreteFilter::default()
        };
        assert!(selects_coordinate(&base, &coordinate));
        assert!(selects_coordinate(&ConcreteFilter::default(), &coordinate));
        assert!(!selects_coordinate(
            &ConcreteFilter {
                ids: Some(BTreeSet::from(["a".repeat(64)])),
                ..base.clone()
            },
            &coordinate
        ));
        assert!(!selects_coordinate(
            &ConcreteFilter {
                kinds: Some(BTreeSet::from([Kind::TextNote.as_u16()])),
                ..base.clone()
            },
            &coordinate
        ));
        assert!(!selects_coordinate(
            &ConcreteFilter {
                authors: Some(BTreeSet::from([Keys::generate().public_key().to_hex()])),
                ..base.clone()
            },
            &coordinate
        ));
        // A tag constraint that is not an addressable coordinate's own `#d`
        // may exclude the very event being asked about.
        assert!(!selects_coordinate(
            &ConcreteFilter {
                tags: BTreeMap::from([(
                    IndexedTagName::new('t').expect("t is an indexed Nostr tag"),
                    BTreeSet::from(["nostr".to_string()]),
                )]),
                ..base
            },
            &coordinate
        ));
    }

    #[test]
    fn an_addressable_coordinate_is_covered_only_by_a_matching_or_absent_d_tag() {
        let author = Keys::generate();
        let coordinate = Coordinate {
            kind: Kind::from(30_000u16),
            public_key: author.public_key(),
            identifier: "mine".to_string(),
        };
        let d = IndexedTagName::new('d').expect("d is an indexed Nostr tag");
        let base = ConcreteFilter {
            kinds: Some(BTreeSet::from([30_000u16])),
            authors: Some(BTreeSet::from([author.public_key().to_hex()])),
            ..ConcreteFilter::default()
        };
        assert!(selects_coordinate(&base, &coordinate));
        assert!(selects_coordinate(
            &ConcreteFilter {
                tags: BTreeMap::from([(d, BTreeSet::from(["mine".to_string()]))]),
                ..base.clone()
            },
            &coordinate
        ));
        assert!(!selects_coordinate(
            &ConcreteFilter {
                tags: BTreeMap::from([(d, BTreeSet::from(["theirs".to_string()]))]),
                ..base.clone()
            },
            &coordinate
        ));
        assert!(
            coordinate_request_shape(&base, &coordinate).is_none(),
            "an addressable request without a #d asks for every identifier"
        );
        assert_eq!(
            coordinate_request_shape(
                &ConcreteFilter {
                    tags: BTreeMap::from([(d, BTreeSet::from(["mine".to_string()]))]),
                    ..base
                },
                &coordinate
            ),
            Some(CoordinateRequestShape::Answerable)
        );
    }

    #[test]
    fn a_live_first_limit_zero_req_is_a_barrier_shape_not_an_answerable_one() {
        let alice = Keys::generate();
        let coordinate = contact_list_coordinate(&alice);
        let exact = ConcreteFilter {
            kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
            authors: Some(BTreeSet::from([alice.public_key().to_hex()])),
            ..ConcreteFilter::default()
        };
        assert_eq!(
            coordinate_request_shape(&exact, &coordinate),
            Some(CoordinateRequestShape::Answerable)
        );
        // #1683: the barrier is a shape of its own, not the absence of one.
        // Reading it as "no request asks this" is what let the publish gate
        // treat an asked question as an unasked one.
        assert_eq!(
            coordinate_request_shape(
                &ConcreteFilter {
                    limit: Some(0),
                    ..exact.clone()
                },
                &coordinate
            ),
            Some(CoordinateRequestShape::LiveFirstBarrier)
        );
        // A windowed request is genuinely not this question at all.
        assert_eq!(
            coordinate_request_shape(
                &ConcreteFilter {
                    until: Some(500),
                    ..exact
                },
                &coordinate
            ),
            None
        );
    }
}
