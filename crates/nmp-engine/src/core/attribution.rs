//! Coverage-attribution state for the request-scoped facts-before-claims
//! contract recorded in issue #816 and
//! `docs/design/query-demand-and-evidence.md`: the per-`SubId` FIFO of
//! send-time snapshots, the wire subscription-id -> `SubId` reverse lookup,
//! and the per-request claim sets those two are reconciled against.
//!
//! A `CoverageKey` is the whole coverage identity, at every layer: the router
//! puts keys on a `WireReq`, this module carries keys through the FIFO, and
//! `RedbStore::record_coverage` takes keys. Nothing between the demand and
//! the durable row needs the `ContextualAtom` a key was hashed from, and no
//! filter is ever stored in the database (#1849): a durable coverage row is
//! key + `from` + `through`, nothing more.
//!
//! This is a plain data structure with no I/O and no access to the store or
//! router — `CoreState` (`core/mod.rs`) is the one place both exist
//! together, and it is the one that actually calls
//! `RedbStore::record_coverage` with the claims this module hands back.

use std::collections::{BTreeSet, HashMap, VecDeque};

use nmp_grammar::{DescriptorHash, RelaySessionKey};
use nmp_router::SubId;
use nmp_store::{CoverageInterval, CoverageKey};
use nostr::Timestamp;

mod completion;
mod wire;

pub use wire::wire_sub_id_string;

/// One send-time snapshot (ruling §2): what a single outgoing REQ proves,
/// captured at the moment it was sent — never re-derived
/// from the sub's CURRENT filter later.
#[derive(Debug, Clone)]
struct AttributionSnapshot {
    send_id: AttributionSendId,
    coverage_claims: BTreeSet<CoverageKey>,
    filter_hash: DescriptorHash,
    floor: Option<Timestamp>,
    until: Option<Timestamp>,
    coverage_authority: CoverageAuthority,
}

/// Opaque identity of one exact send-time attribution snapshot. Ordinary
/// EOSE is intentionally ambiguous when a subscription id is overwritten,
/// so it uses FIFO intersection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AttributionSendId(u64);

impl AttributionSendId {
    pub(crate) fn revision(self) -> u64 {
        self.0
    }
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

    pub(crate) fn into_eligible_claims(self) -> Option<Vec<CompletedCoverageClaim>> {
        matches!(self.coverage_authority, CoverageAuthority::Eligible).then_some(self.claims)
    }
}

/// All coverage-attribution bookkeeping `CoreState` owns. Keyed by `SubId`
/// (which already embeds the relay — `SubId(RelayUrl, SkeletonHash)`), so a
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
    /// The claim set each immutable router-plan request owns until its exact
    /// physical CLOSE, independently of active logical owners and
    /// transport-generation snapshots. This is the engine's mirror of the
    /// router plan, and the set `release_live_request_claims_delta` shrinks.
    live_request_claims: HashMap<SubId, BTreeSet<CoverageKey>>,
}

impl AttributionState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Install or replace the immutable claim set retained by one planned
    /// physical request. Replaying the same request is idempotent.
    pub(crate) fn retain_live_request_claims(
        &mut self,
        sub_id: &SubId,
        claims: BTreeSet<CoverageKey>,
    ) {
        self.live_request_claims.insert(sub_id.clone(), claims);
    }

    /// Add exact claim owners to an existing immutable request without
    /// revisiting its incumbent claim set.
    pub(crate) fn retain_added_live_request_claims(
        &mut self,
        sub_id: &SubId,
        added: &BTreeSet<CoverageKey>,
    ) {
        let retained = self.live_request_claims.entry(sub_id.clone()).or_default();
        retained.extend(added.iter().cloned());
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
        if let Some(retained) = self.live_request_claims.get_mut(sub_id) {
            for key in removed {
                retained.remove(key);
            }
            empty = retained.is_empty();
        }
        if empty {
            self.live_request_claims.remove(sub_id);
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

    /// Extend only the exact current send-time snapshot.
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
            current.coverage_claims.insert(key.clone());
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
        for key in removed {
            current.coverage_claims.remove(key);
        }
        true
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
        fifo.remove(position);
        if fifo.is_empty() {
            self.inflight.remove(sub_id);
        }
    }

    /// Whether any send on `sub_id` is still outstanding. An entry exists in
    /// `inflight` exactly while its FIFO is non-empty — the emptiness clause
    /// this used to carry was a workaround for two completion paths that
    /// popped the last snapshot and left the entry behind (#1850).
    pub(crate) fn has_inflight(&self, sub_id: &SubId) -> bool {
        self.inflight.contains_key(sub_id)
    }

    pub(crate) fn discard_wire_mapping(&mut self, session: &RelaySessionKey, sub_id: &SubId) {
        self.sub_id_by_wire
            .remove(&(session.clone(), wire_sub_id_string(sub_id)));
    }

    pub(crate) fn release_live_request_claims(&mut self, sub_id: &SubId) {
        self.live_request_claims.remove(sub_id);
    }

    /// Exact structural consistency for every mirror this owner keeps, by
    /// identity rather than by count.
    ///
    /// [`Self::counts`] next to this counts things — the right instrument for
    /// leaks and boundedness, and the wrong one for structure. An empty claim
    /// set or an empty FIFO left filed under its key is a leak the count
    /// cannot see, because the entry itself is what leaked.
    ///
    /// The `sub_id_by_wire` clause is the same shape of unchecked assumption:
    /// [`Self::discard_sub`] reconstructs a mapping's session key from the
    /// `SubId` alone (`RelaySessionKey::new(sub_id.0, sub_id.2)`), which is
    /// only exact while every mapping is filed under the session its own
    /// `SubId` names. Nothing checked that; a mapping filed under any other
    /// session leaks forever and the count is identical either way.
    #[cfg(feature = "bench-instrumentation")]
    pub(super) fn assert_consistent(&self, at: &str) {
        for (sub_id, claims) in &self.live_request_claims {
            assert!(
                !claims.is_empty(),
                "{at}: attribution kept an empty live-request claim set for {sub_id:?}"
            );
        }

        for (sub_id, snapshots) in &self.inflight {
            assert!(
                !snapshots.is_empty(),
                "{at}: attribution kept an empty in-flight FIFO for {sub_id:?}"
            );
        }

        for ((session, wire), sub_id) in &self.sub_id_by_wire {
            assert_eq!(
                session,
                &RelaySessionKey::new(sub_id.0.clone(), sub_id.2),
                "{at}: attribution filed a wire mapping under a session its own SubId does \
                 not name, which discard_sub can never find again"
            );
            assert_eq!(
                wire,
                &wire_sub_id_string(sub_id),
                "{at}: attribution filed a wire mapping under a string its own SubId does \
                 not spell"
            );
        }
    }

    #[cfg(feature = "bench-instrumentation")]
    pub(super) fn counts(&self) -> AttributionCounts {
        AttributionCounts {
            inflight_subs: self.inflight.len(),
            wire_keys: self.sub_id_by_wire.len(),
            live_request_keys: self.live_request_claims.len(),
        }
    }
}

/// The three numbers `CoreOwnershipCensus` carries for this owner, named.
///
/// It replaces an eleven-element `(usize, ..., usize)` tuple destructured
/// positionally at the one call site. Every sibling owner (`RequestAttempts`,
/// `WireOwnership`, `HistorySessions`, `RequestReplacements`)
/// already returns a named struct; attribution was the one that did not, and
/// eleven interchangeable positional `usize`s mean any adjacent pair could be
/// transposed with the whole suite still green. Eight of the eleven counted
/// the shape registry and its three refcount mirrors, all deleted with it.
#[cfg(feature = "bench-instrumentation")]
pub(super) struct AttributionCounts {
    pub(super) inflight_subs: usize,
    pub(super) wire_keys: usize,
    pub(super) live_request_keys: usize,
}
