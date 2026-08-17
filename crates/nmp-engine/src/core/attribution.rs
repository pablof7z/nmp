//! Coverage-attribution state for the request-scoped facts-before-claims
//! contract recorded in issue #816 and
//! `docs/design/query-demand-and-evidence.md`: the per-`SubId` FIFO of
//! send-time snapshots, the wire subscription-id -> `SubId` reverse lookup,
//! and the `CoverageKey` -> retained window-erased shape registry
//! `record_coverage` needs (the store only ever sees whatever
//! `ConcreteFilter` it is handed; `CoreState` is the one place that knows
//! which shape a given key came from). This registry is in-memory and
//! request-scoped — no filter is ever stored in the database (#1849): a
//! durable coverage row is key + `from` + `through`, nothing more.
//!
//! This is a plain data structure with no I/O and no access to the store or
//! router — `CoreState` (`core/mod.rs`) is the one place both exist
//! together, and it is the one that actually calls
//! `RedbStore::record_coverage` with the shape this module hands back.

use std::collections::{BTreeSet, HashMap, VecDeque};

use nmp_grammar::{ContextualAtom, DescriptorHash, RelaySessionKey};
use nmp_router::{DemandKey, SubId};
use nmp_store::{coverage_claim_atoms, coverage_key, CoverageInterval, CoverageKey};
use nostr::Timestamp;

mod completion;
mod wire;

pub use wire::wire_sub_id_string;

/// One send-time snapshot (ruling §2): what a single outgoing REQ (or NEG
/// session) proves, captured at the moment it was sent — never re-derived
/// from the sub's CURRENT filter later.
#[derive(Debug, Clone)]
struct AttributionSnapshot {
    send_id: AttributionSendId,
    event_failure_target: AttributionSendId,
    coverage_claims: BTreeSet<CoverageKey>,
    filter_hash: DescriptorHash,
    floor: Option<Timestamp>,
    until: Option<Timestamp>,
    coverage_authority: CoverageAuthority,
}

/// Opaque identity of one exact send-time attribution snapshot. Ordinary
/// EOSE is intentionally ambiguous when a subscription id is overwritten,
/// so it uses FIFO intersection. A NEG completion is correlated to the
/// exact NEG session that completed and uses this identity instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AttributionSendId(u64);

impl AttributionSendId {
    pub(crate) fn revision(self) -> u64 {
        self.0
    }
}

/// Which request loses coverage authority if an EVENT delivered under the
/// send being recorded fails its event-store transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventFailureTarget {
    /// Ordinary REQ, backlog REQ, live REQ, and NEG own their own failure.
    ThisSend,
    /// A temporary missing-id REQ is only the ingestion tail of the original
    /// NEG request, so its EVENT failures poison that retained owner.
    Correlated(AttributionSendId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverageAuthority {
    Eligible,
    Poisoned(CoveragePoison),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoveragePoison {
    LimitedRequest,
    EventCommitFailed,
    MissingShape,
}

impl CoverageAuthority {
    fn poison(&mut self, reason: CoveragePoison) {
        if matches!(self, Self::Eligible) {
            *self = Self::Poisoned(reason);
        }
    }
}

/// One removed attribution owner. Completion owns both the exact request
/// identity and its monotonic coverage authority until the one persistence
/// door either commits every claim or retires it without a claim.
#[derive(Debug)]
pub(crate) struct CompletedAttribution {
    sub_id: SubId,
    send_id: AttributionSendId,
    filter_hash: DescriptorHash,
    coverage_authority: CoverageAuthority,
    claims: Vec<CompletedCoverageClaim>,
}

#[derive(Debug)]
pub(crate) struct CompletedCoverageClaim {
    pub(crate) key: CoverageKey,
    pub(crate) atom: ContextualAtom,
    pub(crate) interval: CoverageInterval,
}

impl CompletedAttribution {
    pub(crate) fn sub_id(&self) -> &SubId {
        &self.sub_id
    }

    pub(crate) fn send_id(&self) -> AttributionSendId {
        self.send_id
    }

    pub(crate) fn filter_hash(&self) -> DescriptorHash {
        self.filter_hash
    }

    /// Exact interval proven by this one physical generation, before durable
    /// rows merge it with older history. Absent means the completion was
    /// poisoned, empty, or did not carry one unanimous generation interval.
    pub(crate) fn eligible_generation_interval(&self) -> Option<CoverageInterval> {
        if !matches!(self.coverage_authority, CoverageAuthority::Eligible) {
            return None;
        }
        let first = self.claims.first()?.interval;
        self.claims
            .iter()
            .all(|claim| claim.interval == first)
            .then_some(first)
    }

    #[cfg(test)]
    pub(crate) fn eligible_claims(&self) -> Option<Vec<(CoverageKey, CoverageInterval)>> {
        matches!(self.coverage_authority, CoverageAuthority::Eligible).then(|| {
            self.claims
                .iter()
                .map(|claim| (claim.key, claim.interval))
                .collect()
        })
    }

    pub(crate) fn into_eligible_claims(self) -> Option<Vec<CompletedCoverageClaim>> {
        matches!(self.coverage_authority, CoverageAuthority::Eligible).then_some(self.claims)
    }
}

/// All coverage-attribution bookkeeping `CoreState` owns. Keyed by `SubId`
/// (which already embeds the relay — `SubId(RelayUrl, SkeletonHash, AccessContext)`), so a
/// FIFO lookup is also implicitly relay-scoped.
///
/// It holds state and the invariants over that state, and nothing else: no
/// `store`, no `router`, no `resolver`, no `Effect`. Anything that has to
/// emit is orchestration and stays on `CoreState`. `RequestAttempts` and
/// `HistorySessions` restate this same contract; it is written here because
/// `request_attempt.rs` cites "the `AttributionState` contract, verbatim"
/// and until now there was no verbatim text to cite (#1739).
#[derive(Debug, Default)]
pub(crate) struct AttributionState {
    next_send_id: u64,
    inflight: HashMap<SubId, VecDeque<AttributionSnapshot>>,
    /// `(session, wire-format subscription_id string) -> SubId`, populated at
    /// send time. `nmp-transport::Pool` is an unimplemented Step 0 shell in
    /// M3 step B, so there is no pre-existing wire convention to conform
    /// to; `CoreState` invents and owns this string entirely (see
    /// `wire_sub_id_string` below) and is the only reader of it, via this
    /// map — never by re-parsing the string back into a hash. Keyed by the
    /// full [`RelaySessionKey`] (never a bare URL): NIP-42 visibility is
    /// connection-scoped, so the SAME wire string on the SAME relay under a
    /// DIFFERENT access context is a different physical session's sub and
    /// must never resolve to this one.
    sub_id_by_wire: HashMap<(RelaySessionKey, String), SubId>,
    /// `CoverageKey -> the ContextualAtom it came from (#106 -- widened
    /// from a bare `ConcreteFilter`: the store's `record_coverage` now
    /// takes a `&ContextualAtom`, since `CoverageKey` itself is a
    /// context-inclusive hash, so retaining only the selection shape would
    /// no longer be enough to reconstruct the right key at EOSE time).
    /// `CoreState` only ever has a `CoverageKey` at attribution time (from
    /// `WireReq::coverage_claims`), so it must retain the FULL atom separately to
    /// be able to call that door at all. Pruned each recompile by
    /// [`Self::prune_shapes`] (mirroring `CoreState`'s own
    /// `nip11_information` pruning in `core/mod.rs::recompile`) against the
    /// union of the current `active_demand()` and every `CoverageKey` still
    /// `coverage_claims` by an outstanding `inflight` snapshot — see that method's
    /// doc for why both sets are required.
    shape_by_key: HashMap<CoverageKey, ContextualAtom>,
    /// Exact logical demand identities currently retaining each shape.
    /// Multiple windowed DemandKeys may share one window-erased CoverageKey.
    active_demands: BTreeSet<DemandKey>,
    active_shape_owner_counts: HashMap<CoverageKey, usize>,
    /// Immutable router-plan requests retain every claim shape until their
    /// exact physical CLOSE, independently of active logical owners and
    /// transport-generation snapshots.
    live_request_claims: HashMap<SubId, BTreeSet<CoverageKey>>,
    live_shape_owner_counts: HashMap<CoverageKey, usize>,
    /// Outstanding send-time snapshots retaining each shape after its active
    /// demand has left. Counts make completion/discard an exact-key update.
    inflight_shape_owner_counts: HashMap<CoverageKey, usize>,
}

impl AttributionState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Learn the ContextualAtom behind every atom in `demand` (called once
    /// per recompile, from the resolver's full current `active_demand()` —
    /// cheap, and the only way `CoreState` ever sees the atoms' shapes at
    /// all). `demand` carries each atom's full `ContextualAtom` identity
    /// (#106) so the retained value is keyed AND populated the SAME way
    /// `record_send`/`attribute_eose`'s `CoverageKey`s already are.
    pub(crate) fn observe_demand<'a>(
        &mut self,
        demand: impl IntoIterator<Item = &'a ContextualAtom>,
    ) {
        for atom in demand {
            self.observe_atom(atom);
        }
    }

    pub(crate) fn observe_atom(&mut self, atom: &ContextualAtom) {
        let demand = DemandKey::for_atom(atom);
        if !self.active_demands.insert(demand) {
            return;
        }
        for claim in coverage_claim_atoms(atom) {
            let key = coverage_key(&claim);
            self.shape_by_key.entry(key).or_insert(claim);
            *self.active_shape_owner_counts.entry(key).or_insert(0) += 1;
        }
    }

    pub(crate) fn release_atom(&mut self, atom: &ContextualAtom) {
        let demand = DemandKey::for_atom(atom);
        if !self.active_demands.remove(&demand) {
            return;
        }
        for claim in coverage_claim_atoms(atom) {
            self.release_active_shape_owner(coverage_key(&claim));
        }
    }

    /// Prune `shape_by_key` down to keys still reachable from SOMEWHERE
    /// (finding E3, epic #507): called once per recompile, right after
    /// [`Self::observe_demand`] (same `demand` argument), mirroring
    /// `CoreState`'s own `nip11_information.retain(..)` immediately below
    /// it in `core/mod.rs::recompile` -- without this, `shape_by_key` grows
    /// once per distinct atom shape ever demanded for the life of the
    /// process, which for a long-running client visiting many distinct
    /// profiles/queries over a session is unbounded.
    ///
    /// A key is still reachable, and MUST be retained, if EITHER:
    /// - it is `coverage_key(atom)` for some atom in the CURRENT `demand`
    ///   (the same set [`Self::observe_demand`] was just called with), or
    /// - it is still `coverage_claims` by some snapshot outstanding in `inflight`.
    ///
    /// The second clause is load-bearing, not defensive: `attribute_eose`
    /// intersects EVERY still-outstanding snapshot on a sub (ruling §2,
    /// see its own doc), and a sub's outstanding snapshots can span
    /// multiple recompiles -- an atom can leave `active_demand()` (the
    /// resolver moves on) while its already-sent REQ is still awaiting
    /// EOSE, and that REQ's `coverage_claims` keys must keep resolving via
    /// `shape_of` whenever that EOSE (or NEG-MSG completion) finally
    /// arrives, arbitrarily many recompiles later. Pruning against
    /// `demand` alone would silently turn that later `shape_of` lookup
    /// into `None` and drop a coverage credit that was legitimately
    /// earned -- over-pruning, a correctness bug. Retaining a key that
    /// satisfies neither clause is merely a stale entry: still harmless,
    /// per this struct's own doc, exactly as it was before this method
    /// existed -- so under-pruning here is the acceptable failure mode,
    /// never over-pruning.
    pub(crate) fn prune_shapes<'a>(
        &mut self,
        demand: impl IntoIterator<Item = &'a ContextualAtom>,
    ) {
        let mut active_demands = BTreeSet::new();
        let mut active_shape_owner_counts = HashMap::new();
        for atom in demand {
            let demand = DemandKey::for_atom(atom);
            active_demands.insert(demand);
            for claim in coverage_claim_atoms(atom) {
                let key = coverage_key(&claim);
                *active_shape_owner_counts.entry(key).or_insert(0) += 1;
                self.shape_by_key.entry(key).or_insert(claim);
            }
        }
        self.active_demands = active_demands;
        self.active_shape_owner_counts = active_shape_owner_counts;
        self.shape_by_key.retain(|key, _| {
            self.active_shape_owner_counts.contains_key(key)
                || self.live_shape_owner_counts.contains_key(key)
                || self.inflight_shape_owner_counts.contains_key(key)
        });
    }

    /// Install or replace the immutable claim set retained by one planned
    /// physical request. Replaying the same request is idempotent.
    pub(crate) fn retain_live_request_claims(
        &mut self,
        sub_id: &SubId,
        claims: BTreeSet<CoverageKey>,
    ) {
        let previous = self
            .live_request_claims
            .insert(sub_id.clone(), claims.clone())
            .unwrap_or_default();
        for key in claims.difference(&previous) {
            *self.live_shape_owner_counts.entry(*key).or_insert(0) += 1;
        }
        for key in previous.difference(&claims) {
            self.release_live_shape_owner(*key);
        }
    }

    /// Add exact claim owners to an existing immutable request without
    /// revisiting its incumbent claim set.
    pub(crate) fn retain_added_live_request_claims(
        &mut self,
        sub_id: &SubId,
        added: &BTreeSet<CoverageKey>,
    ) {
        let retained = self.live_request_claims.entry(sub_id.clone()).or_default();
        for key in added {
            if retained.insert(*key) {
                *self.live_shape_owner_counts.entry(*key).or_insert(0) += 1;
            }
        }
    }

    /// Release exact local claim ownership from a still-running immutable
    /// request. Callers separately shrink the current pending/accepted
    /// attribution generation: immutable wire bytes do not make a departed
    /// local owner eligible for a later coverage claim.
    pub(crate) fn release_live_request_claims_delta(
        &mut self,
        sub_id: &SubId,
        removed: &BTreeSet<CoverageKey>,
    ) {
        let mut empty = false;
        let mut released = Vec::new();
        if let Some(retained) = self.live_request_claims.get_mut(sub_id) {
            for key in removed {
                if retained.remove(key) {
                    released.push(*key);
                }
            }
            empty = retained.is_empty();
        }
        if empty {
            self.live_request_claims.remove(sub_id);
        }
        for key in released {
            self.release_live_shape_owner(key);
        }
    }

    /// Attach newly-owned contained claims to the current identical-filter
    /// request generation without rewriting wire bytes or mutating an older
    /// overwritten FIFO revision.
    pub(crate) fn extend_current_request_claims(
        &mut self,
        sub_id: &SubId,
        filter_hash: DescriptorHash,
        added_claims: BTreeSet<CoverageKey>,
    ) -> (bool, BTreeSet<CoverageKey>, bool) {
        let previous = self.live_request_claims.get(sub_id);
        let had_previous_claims = previous.is_some_and(|claims| !claims.is_empty());
        let added: BTreeSet<_> = added_claims
            .into_iter()
            .filter(|key| previous.is_none_or(|claims| !claims.contains(key)))
            .collect();
        let current_matches = self
            .inflight
            .get(sub_id)
            .and_then(VecDeque::back)
            .is_some_and(|current| current.filter_hash == filter_hash);
        if !current_matches {
            return (had_previous_claims, added, false);
        }
        self.retain_added_live_request_claims(sub_id, &added);
        self.extend_current_send_claims(sub_id, filter_hash, &added);
        (had_previous_claims, added, true)
    }

    /// Extend only the exact current send-time snapshot. NIP-77 role
    /// generations are children of an immutable router-plan request: their
    /// claim-shape lifetime is owned by the plan record, not by a second
    /// child-level live-request claim set.
    pub(crate) fn extend_current_send_claims(
        &mut self,
        sub_id: &SubId,
        filter_hash: DescriptorHash,
        added: &BTreeSet<CoverageKey>,
    ) -> bool {
        let current_matches = self
            .inflight
            .get(sub_id)
            .and_then(VecDeque::back)
            .is_some_and(|current| current.filter_hash == filter_hash);
        if !current_matches {
            return false;
        }
        let current = self
            .inflight
            .get_mut(sub_id)
            .and_then(VecDeque::back_mut)
            .expect("the matching current request snapshot remains live");
        for key in added {
            if current.coverage_claims.insert(*key) {
                *self.inflight_shape_owner_counts.entry(*key).or_insert(0) += 1;
            }
        }
        true
    }

    /// Remove claims that no current local owner contributes from the exact
    /// pending/accepted generation for a byte-identical physical request.
    /// Older overwritten FIFO revisions are immutable and deliberately left
    /// alone; `back_mut` is the same current-generation boundary used by the
    /// additive metadata path above.
    pub(crate) fn remove_current_send_claims(
        &mut self,
        sub_id: &SubId,
        filter_hash: DescriptorHash,
        removed: &BTreeSet<CoverageKey>,
    ) -> bool {
        let current_matches = self
            .inflight
            .get(sub_id)
            .and_then(VecDeque::back)
            .is_some_and(|current| current.filter_hash == filter_hash);
        if !current_matches {
            return false;
        }
        let current = self
            .inflight
            .get_mut(sub_id)
            .and_then(VecDeque::back_mut)
            .expect("the matching current request snapshot remains live");
        let released: Vec<_> = removed
            .iter()
            .copied()
            .filter(|key| current.coverage_claims.remove(key))
            .collect();
        for key in released {
            let count = self
                .inflight_shape_owner_counts
                .get_mut(&key)
                .expect("every current request claim owns an inflight shape ref");
            *count = count
                .checked_sub(1)
                .expect("an inflight request claim refcount cannot be zero");
            if *count == 0 {
                self.inflight_shape_owner_counts.remove(&key);
            }
            self.remove_unowned_shape(key);
        }
        true
    }

    pub(crate) fn claim_shape(&self, key: CoverageKey) -> Option<ContextualAtom> {
        self.shape_by_key.get(&key).cloned()
    }

    pub(crate) fn discard_send_revision(&mut self, sub_id: &SubId, revision: u64) {
        let Some(fifo) = self.inflight.get_mut(sub_id) else {
            return;
        };
        let Some(position) = fifo
            .iter()
            .position(|snapshot| snapshot.send_id.revision() == revision)
        else {
            return;
        };
        let snapshot = fifo.remove(position).unwrap();
        if fifo.is_empty() {
            self.inflight.remove(sub_id);
        }
        self.release_snapshot(&snapshot);
    }

    pub(crate) fn has_inflight(&self, sub_id: &SubId) -> bool {
        self.inflight
            .get(sub_id)
            .is_some_and(|snapshots| !snapshots.is_empty())
    }

    pub(crate) fn discard_wire_mapping(&mut self, session: &RelaySessionKey, sub_id: &SubId) {
        self.sub_id_by_wire
            .remove(&(session.clone(), wire_sub_id_string(sub_id)));
    }

    pub(crate) fn release_live_request_claims(&mut self, sub_id: &SubId) {
        let Some(claims) = self.live_request_claims.remove(sub_id) else {
            return;
        };
        for key in claims {
            self.release_live_shape_owner(key);
        }
    }
}
