//! `nmp-store` — `EventStore` trait + `MemoryStore` + `RedbStore`: the one
//! mutating door (VISION §4 "the store", bug-class ledger #1), extended in
//! M3 step A1 with persistence, provenance merge, and coverage watermarks
//! (VISION §7 ledger #7 / #5).
//!
//! Insert runs **dedup-by-id first**, THEN replaceable/addressable
//! supersession (M1 plan §2.2): winner = newest `created_at`, tie-break
//! lexicographically-smallest id. `query` reuses `nostr::Filter::match_event`
//! — no hand-rolled event matching. A duplicate-id insert now MERGES relay
//! provenance into the stored row (ledger #5) instead of being a no-op.
//!
//! Coverage (`record_coverage`/`get_coverage`) implements
//! `docs/consults/2026-07-11-fable-coverage-attribution.md` at the store
//! layer — see [`coverage`] for the full ruling recap. Claim-based bounded
//! GC (`gc`) evicts only regular (non-addressed) events matched by no live
//! claim, lowering any coverage row it invalidates in the same step.
//!
//! Explicitly out of scope for M3 step A1 (owned by later steps): signature
//! verification, the engine's send-time attribution snapshots (this crate
//! only stores whatever interval it is told to record).

mod address_key;
mod coverage;
mod memory_store;
mod redb_store;

pub use coverage::{coverage_key, ClaimSet, CoverageInterval, CoverageKey, GcReport};
pub use memory_store::MemoryStore;
pub use redb_store::RedbStore;

use std::collections::BTreeMap;

use nmp_grammar::ConcreteFilter;
use nostr::{Event, EventId, Filter, RelayUrl, Timestamp};

/// Per-relay provenance for one stored event: which relays have delivered
/// this exact event id, and the latest wall-clock time each one did so
/// (ledger #5). A first-class field of the stored row, not a sidecar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    pub seen: BTreeMap<RelayUrl, Timestamp>,
}

impl Provenance {
    /// A fresh `Provenance` recording exactly one observation.
    pub(crate) fn first_observation(from: RelayObserved) -> Self {
        let mut seen = BTreeMap::new();
        seen.insert(from.relay, from.at);
        Self { seen }
    }

    /// Merge one more observation in. Returns `true` iff this observation
    /// changed the map: a relay not seen before, or a strictly later
    /// timestamp for a relay already seen. A redelivery from a relay at an
    /// equal-or-earlier timestamp than what is already recorded changes
    /// nothing and returns `false` — no index churn on a no-op merge.
    pub(crate) fn merge_observation(&mut self, from: &RelayObserved) -> bool {
        match self.seen.get(&from.relay) {
            None => {
                self.seen.insert(from.relay.clone(), from.at);
                true
            }
            Some(existing) if *existing < from.at => {
                self.seen.insert(from.relay.clone(), from.at);
                true
            }
            Some(_) => false,
        }
    }
}

/// A stored event plus its provenance. What `query` returns — every caller
/// gets provenance for free, never a bare `Event` (ledger #5's falsifier:
/// no `query` path returns an event without its provenance populated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub event: Event,
    pub provenance: Provenance,
}

/// Which relay delivered an event, and the engine's wall-clock time at
/// receipt — the `insert` door's second argument (M3 §3.1's `from:
/// RelayObserved`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayObserved {
    pub relay: RelayUrl,
    pub at: Timestamp,
}

impl RelayObserved {
    pub fn new(relay: RelayUrl, at: Timestamp) -> Self {
        Self { relay, at }
    }
}

/// The result of an [`EventStore::insert`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// Brand-new event id, not part of any replaceable/addressable
    /// competition (or the first event at that address).
    Inserted,
    /// This exact event id is already present. `provenance_grew` is `true`
    /// iff the merge actually changed the provenance map (M1's no-op stub
    /// becomes a real merge in M3 — ledger #5).
    Duplicate { provenance_grew: bool },
    /// A replaceable/addressable winner changed. `replaced` is the evicted
    /// row itself, handed back whole: the store is holding it at the exact
    /// moment of eviction, and this is the only moment it can be returned
    /// (retraction-and-negative-deltas.md §1.1) — the resolver's dirty-seed
    /// and the optimistic-write rollback path both need to `match_event`
    /// and re-insert this row after the store has already dropped it.
    Superseded {
        /// The full row that was superseded (dropped from the store).
        /// Boxed so the common `Inserted`/`Duplicate`/`Stale` variants stay
        /// small — `Superseded` is the rare, eviction-only case.
        replaced: Box<StoredEvent>,
    },
    /// This event is older than the current winner for its
    /// replaceable/addressable address (or ties on `created_at` but does not
    /// win the lexicographic id tie-break). Rejected: dropped, never stored.
    Stale,
    /// Refused at the door: never stored, nothing to retract
    /// (retraction-and-negative-deltas.md §1.1/§2/§3).
    Refused(RefuseReason),
}

/// Why an [`EventStore::insert`] refused an event outright, before it ever
/// touched an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// The event's NIP-40 `expiration` tag is already in the past at the
    /// moment of insert (checked against the `RelayObserved` clock the
    /// caller passed in). Wired in this unit.
    AlreadyExpired,
    /// The event's id (or, for an addressable/replaceable target, its
    /// address) was tombstoned by an earlier verified kind:5 deletion from
    /// the same author. Not produced anywhere yet — the kind:5 processing
    /// unit (a separate #23 child) is what constructs this variant; it
    /// lands here now so that unit's match sites compile against a stable
    /// shape.
    Tombstoned,
}

/// Why an [`EventStore::remove`] call is removing a row. Exists so
/// diagnostics can count retractions per cause, and so `remove` reads as
/// self-documentingly *not* a general delete API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetractReason {
    /// An optimistic local write was rejected (or its whole intent failed)
    /// before ever being accepted.
    Rejected,
    /// Removed by a verified kind:5 deletion from the event's own author.
    Deleted,
    /// Removed because its NIP-40 `expiration` deadline passed.
    Expired,
}

/// The single mutating door onto the event store.
pub trait EventStore {
    /// Insert an event observed via `from`. An already-expired event (NIP-40,
    /// judged against `from.at`) is `Refused` before anything else runs —
    /// never stored, nothing to retract. Otherwise dedup-by-id FIRST — on a
    /// hit, merge `from` into the existing row's provenance and return
    /// `Duplicate{provenance_grew}` with NO index churn; otherwise run
    /// replaceable/addressable supersession (unchanged M1 semantics).
    fn insert(&mut self, event: Event, from: RelayObserved) -> InsertOutcome;

    /// Query current winners only (never a superseded/stale event), matched
    /// via `nostr::Filter::match_event`, each with its provenance attached.
    fn query(&self, filter: &Filter) -> Vec<StoredEvent>;

    /// Remove `id` from the store — clearing both the id index and, if `id`
    /// is the current replaceable/addressable winner for its address, the
    /// address index too — and hand back the removed row whole, or `None`
    /// if `id` was not held. Engine-facing only (kind:5 processing,
    /// optimistic-write rejection); never a general delete API.
    fn remove(&mut self, id: EventId, reason: RetractReason) -> Option<StoredEvent>;

    /// Drain every row whose NIP-40 `expiration` is `<= now`, removing each
    /// one and returning the full rows. Minimal seam for this unit: reads
    /// expiration straight off whatever is currently stored (no persistent
    /// expiration index yet — that is the NIP-40 unit's job), so this is
    /// honest but O(stored rows) per call.
    fn expire_due(&mut self, now: Timestamp) -> Vec<StoredEvent>;

    /// The earliest NIP-40 `expiration` deadline among currently stored
    /// rows, or `None` if nothing carries one. Same minimal-seam caveat as
    /// [`EventStore::expire_due`].
    fn next_expiration(&self) -> Option<Timestamp>;

    /// Record that `relay` has proven `proven` for `filter`'s window-erased
    /// shape (ruling §1/§3). Merge-only: no public lowering path exists
    /// outside `gc`.
    fn record_coverage(
        &mut self,
        filter: &ConcreteFilter,
        relay: &RelayUrl,
        proven: CoverageInterval,
    );

    /// The proven interval for `key` at `relay`, or `None` if no row exists
    /// — "no row = not covered" (harvest rule, unchanged). `None` is
    /// authoritative-unknown, never treated as authoritative-empty.
    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval>;

    /// Claim-based bounded GC (ruling §5): evicts every regular
    /// (non-replaceable, non-addressable) event matched by NO claim in
    /// `claims`. A claimed event, and every replaceable/addressable current
    /// winner, are ALWAYS retained — winners are never GC candidates at all,
    /// regardless of `claims`. When an evicted event falls inside a coverage
    /// row's proven interval and that row's retained shape matches it, the
    /// row is shrunk (or deleted, if the shrink empties it) in the same step
    /// — a watermark must never claim coverage of data no longer held.
    fn gc(&mut self, claims: &ClaimSet) -> GcReport;
}
