//! Reuse of an already-open relay request when one replaceable or
//! addressable coordinate is asked for on the same session (#1630).
//!
//! Opening a one-off for a coordinate is an ordinary question — "what is this
//! relay's current value for this coordinate?" — and this engine frequently
//! already has a request on that relay whose answer contains it. Asking again
//! costs a round trip per edit for no new information.
//!
//! What makes an existing request usable is decided from three facts this
//! engine already owns, plus nothing else:
//!
//! - whether the open request's filter selects a superset of the coordinate's
//!   own filter ([`contains`]);
//! - whether it returned the coordinate ([`ReturnedFrames::witnessed`]);
//! - whether the relay could have truncated its answer
//!   ([`ReturnedFrames::below_bound`]).
//!
//! Everything recorded here is process-local and dies with the request that
//! produced it. No durable row names a request, so a restart forgets every
//! witness and asks the ordinary question again.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ConcreteFilter, IndexedTagName, RelaySessionKey};
use nmp_router::SubId;
use nostr::nips::nip01::Coordinate;
use nostr::Event;

use super::observation::StoredEvents;
use super::EngineCore;

/// How many events this engine assumes a relay returns at most for one REQ
/// whose filter carries no `limit` of its own.
///
/// Fixed on purpose. A relay advertises its own ceiling in its NIP-11
/// document, and reading that is #744's subject; sourcing it here would make
/// absence depend on a document this engine does not yet fetch. Until then a
/// request that returned fewer than this many events for an unlimited filter
/// is the only shape that cannot have been truncated.
pub(super) const RELAY_RESULT_BOUND: u64 = 500;

/// One replaceable or addressable coordinate, in the shape an EVENT frame can
/// be reduced to without allocating a `Coordinate`'s relay hints.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct CoordinateKey {
    kind: u16,
    author: String,
    identifier: String,
}

impl CoordinateKey {
    fn of(coordinate: &Coordinate) -> Self {
        Self {
            kind: coordinate.kind.as_u16(),
            author: coordinate.public_key.to_hex(),
            identifier: coordinate.identifier.clone(),
        }
    }

    /// The coordinate an EVENT frame carries, for the kinds whose newest
    /// event IS that coordinate's current value. Every other kind has no
    /// coordinate and is counted but never witnessed.
    fn of_event(event: &Event) -> Option<Self> {
        (event.kind.is_replaceable() || event.kind.is_addressable()).then(|| Self {
            kind: event.kind.as_u16(),
            author: event.pubkey.to_hex(),
            identifier: event.tags.identifier().unwrap_or("").to_owned(),
        })
    }
}

/// What one outstanding wire request received from its relay, kept only as
/// the two facts absence depends on.
///
/// Counting happens at the frame boundary, before any acceptance decision:
/// an event this engine rejected still occupied a slot in the relay's bounded
/// answer, so 500 returned and 499 accepted must never read as an uncapped
/// result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReturnedFrames {
    returned: ReturnedCount,
    /// The coordinates among the returned frames, capped at
    /// [`RELAY_RESULT_BOUND`] entries so a long-lived subscription's witness
    /// set stays bounded. Losing a witness past the cap costs one extra REQ;
    /// it can never manufacture a proof.
    witnessed: BTreeSet<CoordinateKey>,
}

/// How many EVENT frames one request received, or that the number is not
/// knowable.
///
/// A variant rather than a count plus an "is it trustworthy" flag: the whole
/// point of the count is comparing it against a bound, and a number that
/// cannot be compared must not be spendable as one. `Unknown` is absorbing —
/// once a frame escapes attribution, no later frame restores the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnedCount {
    Exact(u64),
    Unknown,
}

impl ReturnedCount {
    fn increment(self) -> Self {
        match self {
            Self::Exact(count) => Self::Exact(count.saturating_add(1)),
            Self::Unknown => Self::Unknown,
        }
    }

    fn below(self, bound: u64) -> bool {
        matches!(self, Self::Exact(count) if count < bound)
    }
}

impl ReturnedFrames {
    /// A request that has returned nothing yet, borrowable so the read path
    /// never has to insert an entry for a candidate it merely inspects.
    const NOTHING: &'static Self = &Self {
        returned: ReturnedCount::Exact(0),
        witnessed: BTreeSet::new(),
    };

    fn record(&mut self, event: &Event) {
        self.returned = self.returned.increment();
        if self.witnessed.len() as u64 >= RELAY_RESULT_BOUND {
            return;
        }
        if let Some(key) = CoordinateKey::of_event(event) {
            self.witnessed.insert(key);
        }
    }

    /// Give up on this request's count for good: a frame it may have produced
    /// escaped attribution, so its answer's size is no longer knowable.
    fn lose_count(&mut self) {
        self.returned = ReturnedCount::Unknown;
    }

    fn witnessed(&self, coordinate: &CoordinateKey) -> bool {
        self.witnessed.contains(coordinate)
    }

    /// Whether this request's answer provably was NOT truncated, and can
    /// therefore be read as "the relay returned everything it had".
    ///
    /// A filter's own `limit` binds below the relay ceiling; a request that
    /// hit either one may have had more to give.
    fn below_bound(&self, filter: &ConcreteFilter) -> bool {
        let bound = filter
            .limit
            .and_then(|limit| u64::try_from(limit).ok())
            .map_or(RELAY_RESULT_BOUND, |limit| limit.min(RELAY_RESULT_BOUND));
        self.returned.below(bound)
    }
}

impl Default for ReturnedFrames {
    fn default() -> Self {
        Self::NOTHING.clone()
    }
}

/// Whether every event `narrower` selects is also selected by `covering`.
///
/// Plain set containment over NIP-01's own filter fields: an unset field
/// matches everything, so `covering` may leave unset what `narrower`
/// constrains, but never the reverse. Extra constraints on `narrower` only
/// shrink it further and are therefore always fine.
///
/// `limit` is deliberately not read here. It bounds how much of a result set
/// a relay returns, not which events belong to it; that bound is
/// [`ReturnedFrames::below_bound`]'s question, and conflating the two is how a
/// truncated feed would masquerade as a complete one.
fn contains(covering: &ConcreteFilter, narrower: &ConcreteFilter) -> bool {
    fn field<T: Ord>(covering: &Option<BTreeSet<T>>, narrower: &Option<BTreeSet<T>>) -> bool {
        match (covering, narrower) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(covering), Some(narrower)) => narrower.is_subset(covering),
        }
    }

    let since = match (covering.since, narrower.since) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(covering), Some(narrower)) => covering <= narrower,
    };
    let until = match (covering.until, narrower.until) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(covering), Some(narrower)) => covering >= narrower,
    };

    field(&covering.kinds, &narrower.kinds)
        && field(&covering.authors, &narrower.authors)
        && field(&covering.ids, &narrower.ids)
        && covering.tags.iter().all(|(name, values)| {
            narrower
                .tags
                .get(name)
                .is_some_and(|narrower| narrower.is_subset(values))
        })
        && since
        && until
}

/// The exact ordinary filter one coordinate observation asks. No window and
/// no `limit`: the question is this coordinate's current value, and NIP-01
/// answers it with the newest event of that kind, author, and `d` tag.
pub(super) fn coordinate_filter(coordinate: &Coordinate) -> ConcreteFilter {
    let mut tags = BTreeMap::new();
    if coordinate.kind.is_addressable() {
        tags.insert(
            IndexedTagName::new('d').expect("d is an indexed Nostr tag"),
            BTreeSet::from([coordinate.identifier.clone()]),
        );
    }
    ConcreteFilter {
        kinds: Some(BTreeSet::from([coordinate.kind.as_u16()])),
        authors: Some(BTreeSet::from([coordinate.public_key.to_hex()])),
        tags,
        ..ConcreteFilter::default()
    }
}

/// What the requests already on one relay session prove about one
/// coordinate, and therefore whether a REQ is still required.
///
/// Closed and exhaustive: every member names the exact existing fact that
/// justifies it, and [`CoordinateCoverage::RequiresRequest`] is the only one
/// that costs a round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinateCoverage {
    /// A request whose filter contains this coordinate returned the
    /// coordinate's event on this session. There is nothing left to ask.
    Witnessed,
    /// A request whose filter contains this coordinate finished its stored
    /// events below the relay's result bound without ever returning it. This
    /// session holds nothing for the coordinate, and that absence is proven,
    /// not merely unobserved.
    ProvenEmpty,
    /// A request for exactly this coordinate is already outstanding on this
    /// session. A second caller attaches to it and adds no REQ of its own.
    JoinsOutstandingRequest,
    /// Nothing open answers the question. One ordinary REQ is required —
    /// because coverage is truncated, tighter-windowed, on another session,
    /// or simply absent.
    RequiresRequest,
}

impl EngineCore {
    /// Whether opening a one-off for `coordinate` on `session` still needs a
    /// REQ, and if not, which already-open request answers it.
    ///
    /// Reads only what already exists: the requests this engine has on this
    /// exact session, what each returned, and whether each finished.
    ///
    /// The public/protected split falls out of the key rather than a
    /// parameter: [`RelaySessionKey`] carries its own access context, so a
    /// Public request is never a candidate for a protected question and a
    /// protected request is never a candidate for a public one.
    #[doc(hidden)]
    pub fn coordinate_coverage(
        &self,
        coordinate: &Coordinate,
        session: &RelaySessionKey,
    ) -> CoordinateCoverage {
        let requested = coordinate_filter(coordinate);
        let key = CoordinateKey::of(coordinate);
        let mut proven_empty = false;
        let mut outstanding_exact =
            self.pending_request_evidence
                .iter()
                .any(|((candidate, _), queue)| {
                    candidate == session && queue.iter().any(|request| request.filter == requested)
                });

        for ((candidate, sub_id), live) in &self.live_wire_requests {
            if candidate != session || !contains(&live.filter, &requested) {
                continue;
            }
            let frames = self
                .returned_frames
                .get(&(candidate.clone(), sub_id.clone()))
                .unwrap_or(ReturnedFrames::NOTHING);
            if frames.witnessed(&key) {
                return self.decided(CoordinateCoverage::Witnessed);
            }
            match live.stored_events {
                StoredEvents::Finished { .. } => {
                    proven_empty |= frames.below_bound(&live.filter);
                }
                StoredEvents::Streaming { .. } => outstanding_exact |= live.filter == requested,
            }
        }

        self.decided(if proven_empty {
            CoordinateCoverage::ProvenEmpty
        } else if outstanding_exact {
            CoordinateCoverage::JoinsOutstandingRequest
        } else {
            CoordinateCoverage::RequiresRequest
        })
    }

    /// Count the verdicts that cost a REQ, and only those. Every reuse path
    /// leaves the counter alone, so a regression that stops reusing shows up
    /// as a number rather than a missing assertion.
    fn decided(&self, coverage: CoordinateCoverage) -> CoordinateCoverage {
        #[cfg(any(test, feature = "bench-instrumentation"))]
        if coverage == CoordinateCoverage::RequiresRequest {
            self.coordinate_reuse_new_reqs
                .set(self.coordinate_reuse_new_reqs.get().saturating_add(1));
        }
        coverage
    }

    /// Count one EVENT frame the relay returned under `wire_sub_id`.
    ///
    /// Called at the frame boundary, before ingest decides whether to accept
    /// the event: the count must be what the RELAY returned.
    ///
    /// A ledger exists only while its accepted wire request does, so the
    /// ledger's key set is always a subset of `live_wire_requests`' and can
    /// never outlive it.
    pub(super) fn record_returned_frame(
        &mut self,
        session: &RelaySessionKey,
        wire_sub_id: &str,
        event: &Event,
    ) {
        let key = self
            .attribution
            .sub_id_for_wire(session, wire_sub_id)
            .map(|sub_id| (session.clone(), sub_id))
            .filter(|key| self.live_wire_requests.contains_key(key));
        let Some(key) = key else {
            // This engine cannot say which accepted request's bounded answer
            // the frame belonged to, so nothing on this session may prove
            // absence until its requests are replaced.
            self.lose_returned_frame_counts(session);
            return;
        };
        self.returned_frames.entry(key).or_default().record(event);
    }

    /// Record that an exact returned-frame count is no longer available for
    /// anything outstanding on `session`.
    fn lose_returned_frame_counts(&mut self, session: &RelaySessionKey) {
        let outstanding: Vec<_> = self
            .live_wire_requests
            .keys()
            .filter(|(candidate, _)| candidate == session)
            .cloned()
            .collect();
        for key in outstanding {
            self.returned_frames.entry(key).or_default().lose_count();
        }
    }

    /// Drop the ledger for one exact wire request. A replacement REQ under
    /// the same subscription id is a different question with a different
    /// answer, so it starts its count at zero.
    pub(super) fn forget_returned_frames(&mut self, session: &RelaySessionKey, sub_id: &SubId) {
        self.returned_frames
            .remove(&(session.clone(), sub_id.clone()));
    }

    pub(super) fn forget_session_returned_frames(&mut self, session: &RelaySessionKey) {
        self.returned_frames
            .retain(|(candidate, _), _| candidate != session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{Keys, Kind};

    fn filter(
        kinds: Option<[u16; 1]>,
        authors: Option<[&str; 1]>,
        since: Option<u64>,
        until: Option<u64>,
    ) -> ConcreteFilter {
        ConcreteFilter {
            kinds: kinds.map(|kinds| kinds.into_iter().collect()),
            authors: authors.map(|authors| {
                authors
                    .into_iter()
                    .map(std::string::ToString::to_string)
                    .collect()
            }),
            since,
            until,
            ..ConcreteFilter::default()
        }
    }

    #[test]
    fn an_unset_field_contains_a_constrained_one_but_never_the_reverse() {
        let broad = filter(Some([3]), None, None, None);
        let narrow = filter(Some([3]), Some(["ab"]), None, None);
        assert!(contains(&broad, &narrow));
        assert!(!contains(&narrow, &broad));
    }

    #[test]
    fn a_tighter_window_never_contains_a_wider_one() {
        let requested = filter(Some([3]), Some(["ab"]), None, None);
        assert!(!contains(
            &filter(Some([3]), None, Some(100), None),
            &requested
        ));
        assert!(!contains(
            &filter(Some([3]), None, None, Some(100)),
            &requested
        ));
        assert!(contains(&filter(Some([3]), None, None, None), &requested));
    }

    #[test]
    fn a_tag_constraint_the_narrower_filter_lacks_breaks_containment() {
        let mut tagged = filter(Some([3]), None, None, None);
        tagged.tags.insert(
            IndexedTagName::new('p').unwrap(),
            BTreeSet::from(["ab".to_string()]),
        );
        assert!(!contains(
            &tagged,
            &filter(Some([3]), Some(["ab"]), None, None)
        ));
    }

    #[test]
    fn an_addressable_coordinate_filter_pins_its_d_tag() {
        let author = Keys::generate().public_key();
        let addressable = Coordinate {
            kind: Kind::from_u16(30_023),
            public_key: author,
            identifier: "essay".to_string(),
        };
        let replaceable = Coordinate {
            kind: Kind::ContactList,
            public_key: author,
            identifier: String::new(),
        };
        assert_eq!(
            coordinate_filter(&addressable)
                .tags
                .get(&IndexedTagName::new('d').unwrap()),
            Some(&BTreeSet::from(["essay".to_string()]))
        );
        assert!(coordinate_filter(&replaceable).tags.is_empty());
    }

    #[test]
    fn a_filter_limit_binds_below_the_relay_result_bound() {
        let frames = ReturnedFrames {
            returned: ReturnedCount::Exact(9),
            ..ReturnedFrames::default()
        };
        let unlimited = filter(Some([3]), None, None, None);
        let mut limited = unlimited.clone();
        limited.limit = Some(9);
        assert!(frames.below_bound(&unlimited));
        assert!(!frames.below_bound(&limited));
    }

    #[test]
    fn a_ledger_that_lost_its_count_can_never_prove_absence() {
        let mut frames = ReturnedFrames::default();
        frames.lose_count();
        assert!(!frames.below_bound(&filter(Some([3]), None, None, None)));
    }
}
