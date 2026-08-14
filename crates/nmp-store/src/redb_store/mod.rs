//! [`RedbStore`] — NMP's persistent `redb`-backed store (M3 step A1).
//!
//! Canonical events use an immutable portable binary note value addressed by
//! a compact monotonic `u64` key. Raw event ids map to that key, optional local
//! state has a dedicated compact value, and relay observations are fixed-width
//! `(event, interned-relay) -> timestamp` rows. Every ordered secondary index
//! points straight at the event key. Queries borrow note fields from redb
//! guards and join provenance only for returned rows. Displaced delivery rows
//! remain self-contained binary snapshots; other delivery/coverage metadata
//! remains typed JSON.
//!
//! Nothing here panics the embedding host over the contents of a file it
//! did not write. `redb`'s own errors are classified and returned by
//! [`schema::persist_err`], and — since #790 — every production decoder of a
//! store-owned row does the same: a malformed, truncated, or
//! schema-incompatible persisted value, and a broken relational invariant
//! such as an index naming a canonical row that is missing or will not
//! decode, both surface as `PersistenceError` through the owning typed store
//! door. They are classified [`crate::PersistenceFault::
//! Invariant`] rather than `Corrupted`: the backend is healthy, and the
//! decode happens before the enclosing write transaction commits, so
//! `DurabilityOutcome::Absent` is provable rather than merely convenient
//! (see that variant's doc). A decode failure is never allowed to become an
//! empty result, a skipped row, or a defaulted value — a false miss is
//! exactly the outcome the typed error exists to prevent.
//!
//! Every read door on this backend is fallible, including the two deadline
//! and coverage peeks that #763 widened last: an embedded host is the app
//! itself, so a `.expect()` on a read error was an application crash, not a
//! contained failure.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::AtomicU8;
#[cfg(any(
    test,
    feature = "bench-instrumentation",
    feature = "test-instrumentation"
))]
use std::sync::atomic::{AtomicU64, Ordering};

use nmp_grammar::{ConcreteFilter, ContextualAtom};
use nostr::{Event, EventId, Filter, Kind, PublicKey, RelayUrl, SingleLetterTag, Timestamp};
use redb::{Database, ReadableTable, TableDefinition};
#[cfg(test)]
use redb::{ReadableDatabase, ReadableTableMetadata, TableHandle};
use serde::{Deserialize, Serialize};

use crate::address_key::{address_key_for, address_key_for_coordinate, candidate_wins};
use crate::binary_event::{self, decode_hex_32, IndexedMatch, PreparedFilter, StoredEventView};
use crate::coverage::{
    coverage_key as compute_coverage_key, merge_interval, shrink_after_eviction, window_erase,
    GcVictimIndex, ShapeRecord,
};
use crate::persistent_store_lifetime::{
    acquire_for_open, reset_store, RedbStoreOpenError, RequiredLockedFileBackend, StoreOwnership,
};
#[cfg(test)]
use crate::AuthDenialSource;
use crate::{
    AcceptOutcome, AcceptWrite, AuthDenial, CloseIntentOutcome, CompensateOutcome,
    CoverageInterval, CoverageKey, EventCursor, EventStore, GcReport, GcRetentionSet,
    InsertOutcome, IntentId, IntentSigState, LocalOrigin, PersistenceError, PromoteOutcome,
    Provenance, PublishQueueAttempt, PublishQueueAttemptDetails, PublishQueueAttemptHandoff,
    PublishQueueAttemptOutcome, PublishQueueAttemptTransient, PublishQueueDeadline,
    PublishQueueDeadlineKind, PublishQueueInFlightPhase, PublishQueueIntent, PublishQueueLane,
    PublishQueueLaneKey, PublishQueueLaneState, PublishQueuePostHandoffState, PublishQueueReceipt,
    PublishQueueRouteRevision, PublishQueueTerminalOutcome, PublishQueueTransientCause,
    ReceiptState, RefuseReason, RelayObserved, RetractReason, SigState, StoredEvent,
    VerifiedSignature,
};

#[cfg(feature = "bench-instrumentation")]
mod compact_index_bench;
#[cfg(feature = "bench-instrumentation")]
mod fjall_ingest_bench;
#[cfg(feature = "bench-instrumentation")]
mod lmdb_ingest_bench;
#[cfg(feature = "bench-instrumentation")]
mod packed_postings_bench;
mod postings;
mod postings_store;
#[cfg(feature = "bench-instrumentation")]
mod redo_index_bench;
#[cfg(feature = "bench-instrumentation")]
mod store_bench;

#[cfg(feature = "bench-instrumentation")]
pub use compact_index_bench::run_prepared_redb_compact_index_bench;
#[cfg(feature = "bench-instrumentation")]
pub use fjall_ingest_bench::{run_fjall_governed_ingest_bench, FjallGovernedIngestMetrics};
#[cfg(feature = "bench-instrumentation")]
pub use lmdb_ingest_bench::{
    run_lmdb_governed_ingest_bench, LmdbGovernedIngestMetrics, LmdbPackedWork,
};
#[cfg(feature = "bench-instrumentation")]
pub use packed_postings_bench::{
    run_packed_postings_bench, PackedPostingsBackend, PackedPostingsMetrics, PackedQueryMetrics,
};
#[cfg(feature = "bench-instrumentation")]
pub use redo_index_bench::{run_prepared_redb_redo_index_bench, RedbRedoIndexMetrics};
#[cfg(feature = "bench-instrumentation")]
pub use store_bench::{
    prepare_equivalent_store_corpus, run_prepared_redb_store_bench,
    run_prepared_redb_unified_index_bench, run_store_bench_variant, StoreBenchAttribution,
    StoreBenchMetrics, StoreBenchPreparedBatch, StoreBenchPreparedCorpus,
    StoreBenchPreparedMetrics, StoreBenchPreparedRecord, StoreBenchPreparedTable,
    StoreBenchProcessCounters, StoreBenchVariant,
};

mod schema;
#[cfg(any(test, feature = "test-instrumentation"))]
#[path = "testing_tests.rs"]
pub mod testing;
#[cfg(test)]
use schema::*;
pub(crate) mod publish_queue;
pub(crate) mod publish_queue_codec;
mod semantic_edit_codec;
#[cfg(test)]
use publish_queue::*;
mod canonical;
#[cfg(test)]
use canonical::*;
mod commit;
mod query;
#[cfg(test)]
use query::*;
mod ingest_txn;
mod mutation;
mod store;
#[cfg(test)]
pub(crate) use store::with_required_database_init_test_hook;
#[cfg(any(test, feature = "test-instrumentation"))]
pub use store::OrderedEventReadPause;
#[cfg(test)]
use store::RedbCrashPoint;
pub use store::RedbStore;
mod event_ops;
mod ingest;
pub(crate) mod publish_queue_ops;
mod semantic_edit_ops;
mod write_ops;

impl RedbStore {
    /// Insert an event observed via `from`. An already-expired event (NIP-40,
    /// judged against `from.at`) is `Refused` before anything else runs —
    /// never stored, nothing to retract. Otherwise dedup-by-id FIRST — on a
    /// hit, merge `from` into the existing row's provenance and return
    /// `Duplicate{provenance_grew}` with NO index churn. Next, a tombstone
    /// check (retraction-and-negative-deltas.md §2): an id (or address, at
    /// or before its permanently-recorded deletion ceiling) tombstoned by an
    /// earlier verified kind:5 is `Refused(Tombstoned)`, never stored.
    /// Otherwise run replaceable/addressable supersession (unchanged M1
    /// semantics). A kind:5 event is stored like any other regular event
    /// and, in the same call, drops every currently-held target it names
    /// whose author matches its own (NIP-09 author-only, enforced
    /// structurally) — see `Kind5Processed`.
    ///
    /// Fallible (issue #122): the ingest door runs on every relay EVENT
    /// frame, so a realistic persistence failure (disk full, I/O error) must
    /// return `Err(PersistenceError)` rather than panic the embedding app.
    /// Redb errors and store-owned decode/invariant failures propagate through
    /// this result rather than panicking the embedding app.
    pub fn insert(
        &mut self,
        event: Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        event_ops::insert(self, event, from)
    }

    /// Insert a relay-delivery batch in input order in one Redb transaction,
    /// preserving the exact per-event governed semantics and outcomes of
    /// repeated [`Self::insert`] calls.
    pub fn insert_batch(
        &mut self,
        events: Vec<(Event, RelayObserved)>,
    ) -> Result<Vec<InsertOutcome>, PersistenceError> {
        event_ops::insert_batch(self, events)
    }

    /// Query current winners only (never a superseded/stale event), matched
    /// via `nostr::Filter::match_event`, each with its provenance attached.
    /// Fallible for the same reason as [`Self::insert`] (issue #122):
    /// a read-path I/O error surfaces as `Err` instead of panicking.
    ///
    /// `filter.limit` is NOT consulted by this LOCAL read path (#124): every
    /// currently-matching row is returned, in no particular order, regardless
    /// of `limit`. This is DELIBERATE, not
    /// an oversight — honoring `limit` locally requires a `created_at`-desc
    /// ordering + truncation, and choosing that ordering is an owner-
    /// reserved decision (issue #9's app-defined-sort-vs-closed-`OrderKey`
    /// fork, deferred to the Collection Tier-A gate), not something to
    /// settle as a side effect of this fix. Contrast with the WIRE path:
    /// `nmp_grammar::ConcreteFilter::to_nostr` DOES lower `limit` into this
    /// very `filter` before it ever reaches a relay, so a well-behaved
    /// relay caps what it SENDS you — a genuine, honored guarantee. But
    /// that guarantee governs the wire only; it says nothing about what a
    /// LATER local-only call to THIS method returns once the cache holds
    /// more than `limit` matching rows (reconnect replay, multiple relays
    /// each independently capped, etc.) — this method's own answer is
    /// uncapped regardless. `store_contract.rs` checks this exact contract;
    /// when #9 resolves, whoever implements
    /// ordered/truncated local reads updates that test, not just adds one.
    ///
    /// The app never sees this uncapped answer directly, though: the handle
    /// PROJECTION (`EngineCore::rows_and_evidence_for`, #124 via #139) caps the
    /// app-facing row set to the `limit` most recent by `created_at`
    /// (`EventId`-tiebroken). Redb uses the separate
    /// [`Self::query_newest`] door to pre-bound each root atom before
    /// that final merged cap. That is NIP-01 limit-recency SELECTION — WHICH
    /// rows survive — not a display ordering: the app receives an unordered,
    /// `EventId`-keyed `RowDelta` stream and sorts it itself, so #9's
    /// display-sort fork stays open and the two compose. This store door
    /// deliberately stays uncapped so unlimited reactive recompute and
    /// negentropy still see every match. A `Derived` node carrying an explicit
    /// limit uses [`Self::query_newest`] instead: its projection is
    /// defined over the selected newest `N`, not over the complete history.
    pub fn query(&self, filter: &Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query(self, filter)
    }

    /// Return at most `limit` current matches in NIP-01 newest-first
    /// selection order: `created_at` descending, then event id ascending.
    ///
    /// This is a distinct door from [`Self::query`], whose deliberately
    /// complete result is required by unlimited reactive recompute and
    /// negentropy. Handle root projections and explicitly limited `Derived`
    /// nodes use this bounded door. Redb uses an ordered index scan that stops
    /// as soon as `limit` accepted rows have been found.
    pub fn query_newest(
        &self,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest(self, filter, limit)
    }

    /// Return only the canonical ids from [`Self::query_newest`].
    ///
    /// Consumers that need selection identity but not event payloads use this
    /// door so Redb can project ids from ordered indexes without allocating
    /// owned content.
    pub fn query_newest_ids(
        &self,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<EventId>, PersistenceError> {
        event_ops::query_newest_ids(self, filter, limit)
    }

    /// Return the first `limit` canonical newest rows visible under a pin on
    /// `pinned` — [`Provenance::visible_under_pin`] is the one rule that
    /// decides which those are.
    ///
    /// This is the store-side projection required by a Strict pinned cache:
    /// the bound applies *after* visibility, never before it. Filtering an
    /// already-limited agnostic page can under-fill the result even when
    /// older visible rows exist. Redb tests visibility while walking its
    /// ordered index and stops only after `limit` visible rows have been
    /// accepted.
    pub fn query_newest_under_pin(
        &self,
        filter: &Filter,
        pinned: &BTreeSet<RelayUrl>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_under_pin(self, filter, pinned, limit)
    }

    /// Return at most `limit` current matches strictly after `before` in the
    /// canonical newest-first order used by [`Self::query_newest`].
    ///
    /// The exact exclusive predicate is:
    /// `created_at < before.created_at ||
    /// (created_at == before.created_at && id > before.event_id)`.
    /// This predicate intersects the filter's ordinary inclusive time window;
    /// it never rewrites that window or turns a cursor into relay acquisition
    /// authority. Redb implements it with an exact ordered-index range.
    pub fn query_newest_before(
        &self,
        filter: &Filter,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before(self, filter, before, limit)
    }

    /// Pinned counterpart of [`Self::query_newest_before`]. The cursor
    /// remains exact and exclusive, while `limit` counts only rows visible
    /// under a pin on `pinned` ([`Provenance::visible_under_pin`]).
    pub fn query_newest_before_under_pin(
        &self,
        filter: &Filter,
        pinned: &BTreeSet<RelayUrl>,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before_under_pin(self, filter, pinned, before, limit)
    }

    /// Return one canonical newest-first page from the UNION of `filters`,
    /// strictly after `before` in that order.
    ///
    /// A row matching more than one filter appears once. The global `limit`
    /// applies only after that de-duplication and merge, so callers can repair
    /// one bounded projection with one logical store read even when its
    /// resolved selection has multiple concrete roots. Redb evaluates each
    /// root with an ordered bounded scan: no row ranked
    /// below the first `limit` matches of its own root can enter the global
    /// first `limit` of the union.
    /// This remains selection-only; callers own presentation ordering.
    pub fn query_newest_before_any(
        &self,
        filters: &[Filter],
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before_any(self, filters, before, limit)
    }

    /// Pinned counterpart of [`Self::query_newest_before_any`]. The
    /// page bound counts only de-duplicated union rows visible under a pin
    /// on `pinned` ([`Provenance::visible_under_pin`]).
    pub fn query_newest_before_any_under_pin(
        &self,
        filters: &[Filter],
        pinned: &BTreeSet<RelayUrl>,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before_any_under_pin(self, filters, pinned, before, limit)
    }

    /// Remove `id` from the store — clearing both the id index and, if `id`
    /// is the current replaceable/addressable winner for its address, the
    /// address index too — and hand back the removed row whole, or `None`
    /// if `id` was not held. Engine-facing only (kind:5 processing,
    /// optimistic-write rejection); never a general delete API.
    pub fn remove(
        &mut self,
        id: EventId,
        _reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        event_ops::remove(self, id, _reason)
    }

    /// Drain every row whose NIP-40 `expiration` is `<= now`, removing each
    /// one (through the same [`Self::remove`] door) and returning the
    /// full rows. Index-backed (retraction-and-negative-deltas.md §3.1): a
    /// persistent `(expiry_ts -> {id})` index is maintained on every insert
    /// and every removal, so this drains in `O(log n + due)`, not a full
    /// scan.
    pub fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::expire_due(self, now)
    }

    /// The earliest NIP-40 `expiration` deadline among currently stored
    /// rows, or `Ok(None)` if nothing carries one. Index-backed: peeks the
    /// minimum of the same persistent expiration index `expire_due` drains.
    ///
    /// Fallible for the same reason every other read door is (#122/#763): a
    /// durable read can fail for reasons that are not a bug in the caller —
    /// a disk error, a latched handle, a poisoned lock — and on an embedded
    /// host a panic here takes the whole application down. `Ok(None)` is
    /// honest absence and NOTHING else; a read that could not answer is
    /// `Err`.
    pub fn next_expiration(&self) -> Result<Option<Timestamp>, PersistenceError> {
        event_ops::next_expiration(self)
    }

    /// Atomically record every coverage claim earned by one completed
    /// request. Each tuple is `(atom, relay, proven interval)`. The coverage
    /// identity is the full [`ContextualAtom`], never a bare
    /// `ConcreteFilter`; the caller that owns request attribution supplies
    /// the complete batch. A successful return makes every merged claim
    /// visible, while an error may make none or the entire batch visible but
    /// never a prefix. Merge-only: no public lowering path exists outside
    /// `gc`.
    pub fn record_coverage(
        &mut self,
        claims: &[(ContextualAtom, RelayUrl, CoverageInterval)],
    ) -> Result<(), PersistenceError> {
        event_ops::record_coverage(self, claims)
    }

    /// The proven interval for `key` at `relay`, or `Ok(None)` if no row
    /// exists. `Ok(None)` means this relay has no persisted interval for
    /// this key; it makes no wider claim.
    ///
    /// Fallible for the same reason [`Self::next_expiration`] is
    /// (#122/#763). The distinction is load-bearing here rather than merely
    /// tidy: "no coverage is proven" drives a refetch, while "the store
    /// could not be read" must not be answered as absent coverage, or a
    /// corrupt/unreadable watermark reads as an honest cache miss.
    pub fn get_coverage(
        &self,
        key: CoverageKey,
        relay: &RelayUrl,
    ) -> Result<Option<CoverageInterval>, PersistenceError> {
        event_ops::get_coverage(self, key, relay)
    }

    /// Apply an EXPLICIT durable-retention policy by running claim-based GC
    /// (ruling §5): evicts every regular
    /// (non-replaceable, non-addressable) event matched by NO claim in
    /// `claims`. A claimed event, and every replaceable/addressable current
    /// winner, are ALWAYS retained — winners are never GC candidates at all,
    /// regardless of `claims`. When an evicted event falls inside a coverage
    /// row's proven interval and that row's retained shape matches it, the
    /// row is shrunk (or deleted, if the shrink empties it) in the same step
    /// — a watermark must never claim coverage of data no longer held.
    ///
    /// GC exclusion for open intents (Fable checkpoint R5): a row with
    /// local provenance still in `SigState::Pending` is NEVER a GC
    /// candidate, regardless of `claims` — structurally the same
    /// unconditional retention already given to replaceable/addressable
    /// winners, so an unsigned pending row can never be evicted before it
    /// ever signs. Once `promote_signed` flips it to `Signed`, it is an
    /// ordinary event again, GC-able like any other under `claims`.
    ///
    /// This is never an ordinary startup, query, shutdown, or implicit
    /// memory-pressure maintenance step. The production engine does not call
    /// this door: verified durable rows are retained by default. A host that
    /// deliberately adopts a quota, disk-pressure, or user-selected retention
    /// policy must make that policy inspectable and invoke this destructive
    /// door explicitly. Query/result/delivery bounds limit resident work; they
    /// are not permission to call `gc` or delete durable history.
    ///
    /// This contract does not promise infinite disk. It makes the transition
    /// from retained history to policy-evicted history explicit, reportable,
    /// and coverage-safe.
    pub fn gc(&mut self, claims: &GcRetentionSet) -> Result<GcReport, PersistenceError> {
        event_ops::gc(self, claims)
    }
}

impl EventStore for RedbStore {
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        write_ops::accept_write(self, accept)
    }

    fn replaceable_operation_snapshot(
        &self,
        coordinate: &nostr::nips::nip01::Coordinate,
    ) -> Result<Option<crate::RecoveredSemanticResource>, PersistenceError> {
        semantic_edit_ops::snapshot(self, coordinate)
    }

    fn install_replaceable_materialization(
        &mut self,
        rematerialize: crate::SemanticRematerialize,
    ) -> Result<crate::SemanticInstallOutcome, PersistenceError> {
        semantic_edit_ops::install(self, rematerialize)
    }

    fn install_replaceable_source_materialization(
        &mut self,
        install: crate::SemanticSourceInstall,
    ) -> Result<crate::SemanticInstallOutcome, PersistenceError> {
        semantic_edit_ops::install_source(self, install)
    }

    fn advance_replaceable_source_round(
        &mut self,
        coordinate: &nostr::nips::nip01::Coordinate,
        fact: crate::SemanticSourceRoundFact,
    ) -> Result<crate::SemanticSourceRoundOutcome, PersistenceError> {
        semantic_edit_ops::advance_source_round_fact(self, coordinate, fact)
    }

    fn close_replaceable_operation_cohort(
        &mut self,
        close: crate::SemanticCohortClose,
    ) -> Result<crate::SemanticCohortCloseOutcome, PersistenceError> {
        semantic_edit_ops::close_cohort(self, close)
    }

    fn promote_signed(
        &mut self,
        target: crate::PromotionTarget,
        verified: VerifiedSignature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        match target {
            crate::PromotionTarget::Event(intent_id) => {
                write_ops::promote_signed(self, intent_id, verified)
            }
            crate::PromotionTarget::ReplaceableMaterialization(target) => {
                let crate::ReplaceableMaterializationTarget {
                    coordinate,
                    expected_source_revision,
                    expected_program_digest,
                    expected_materialization,
                    expected_event_id,
                } = *target;
                semantic_edit_ops::promote(
                    self,
                    coordinate,
                    expected_source_revision,
                    expected_program_digest,
                    expected_materialization,
                    expected_event_id,
                    verified,
                )
            }
        }
    }

    fn compensate_write_with_state(
        &mut self,
        intent_id: IntentId,
        reason: crate::CompensationReason,
    ) -> Result<CompensateOutcome, PersistenceError> {
        write_ops::compensate_write_with_state(self, intent_id, reason)
    }

    fn enumerate_publish_queue_receipts(
        &self,
    ) -> Result<Vec<crate::PublishQueueReceipt>, PersistenceError> {
        publish_queue_ops::enumerate_publish_queue_receipts(self)
    }

    fn publish_queue_receipts_after(
        &self,
        after: Option<u64>,
        limit: u8,
    ) -> Result<Vec<crate::PublishQueueReceipt>, PersistenceError> {
        publish_queue_ops::publish_queue_receipts_after(self, after, limit)
    }

    fn remove_publish_queue_entry(
        &mut self,
        receipt_id: u64,
    ) -> Result<crate::RemoveQueueEntryOutcome, PersistenceError> {
        publish_queue_ops::remove_publish_queue_entry(self, receipt_id)
    }

    fn recover_publish_queue(&self) -> Result<Vec<PublishQueueIntent>, PersistenceError> {
        publish_queue_ops::recover_publish_queue(self)
    }

    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<PublishQueueReceipt>, PersistenceError> {
        publish_queue_ops::reattach_receipt(self, receipt_id)
    }

    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError> {
        publish_queue_ops::lookup_correlation(self, token)
    }

    fn accept_refused(
        &mut self,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
        reason: crate::RefuseReason,
    ) -> Result<u64, PersistenceError> {
        publish_queue_ops::accept_refused(self, frozen_id, expected_pubkey, reason)
    }
}

impl RedbStore {
    /// Append the next canonical resolved-route revision for an open intent.
    /// This must commit before any attempt starts or wire publication for a
    /// relay in the revision.
    pub fn record_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<PublishQueueRouteRevision, PersistenceError> {
        publish_queue_ops::record_route_revision(self, intent_id, relays)
    }

    /// Recover every resolved-route revision in ascending ordinal order.
    pub fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueRouteRevision>, PersistenceError> {
        publish_queue_ops::recover_route_revisions(self, intent_id)
    }

    /// Read all retained attempt facts for one intent in stable key order.
    pub fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueAttempt>, PersistenceError> {
        publish_queue_ops::recover_attempts(self, intent_id)
    }

    /// Idempotently seed every missing lane from bounded route/attempt
    /// ranges. Existing cursors are validated and retained.
    pub fn bootstrap_publish_queue_lanes(
        &mut self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        publish_queue_ops::bootstrap_publish_queue_lanes(self, intent_id)
    }

    pub fn recover_publish_queue_lanes(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        publish_queue_ops::recover_publish_queue_lanes(self, intent_id)
    }

    /// Read at most `limit` due rows in stable `(time,intent,relay)` order.
    pub fn due_publish_queue_deadlines(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<PublishQueueDeadline>, PersistenceError> {
        publish_queue_ops::due_publish_queue_deadlines(self, now, limit)
    }

    pub fn next_publish_queue_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        publish_queue_ops::next_publish_queue_deadline(self)
    }

    pub fn set_lane_waiting(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        publish_queue_ops::set_lane_waiting(self, key, expected_revision, auth)
    }

    pub fn set_lane_eligible(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        since: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        publish_queue_ops::set_lane_eligible(self, key, expected_revision, since)
    }

    pub fn set_lane_transient(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
    ) -> Result<PublishQueueLane, PersistenceError> {
        publish_queue_ops::set_lane_transient(
            self,
            key,
            expected_revision,
            ordinal,
            eligible_at,
            cause,
            raw_reason,
        )
    }

    /// End the current ordinal as a nonterminal wait with no deadline.
    /// The attempt detail and waiting cursor advance atomically, so restart
    /// cannot mistake an AUTH/offline wait for a live ambiguous send.
    #[allow(clippy::too_many_arguments)]
    pub fn suspend_lane_attempt(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        ordinal: u64,
        at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
        auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        publish_queue_ops::suspend_lane_attempt(
            self,
            key,
            expected_revision,
            ordinal,
            at,
            cause,
            raw_reason,
            auth,
        )
    }

    /// Atomically append new immutable v1 Started evidence, additive details,
    /// and advance an eligible cursor to awaiting handoff.
    pub fn start_lane_attempt(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        event: Event,
        started_at: Timestamp,
    ) -> Result<(PublishQueueAttempt, PublishQueueLane), PersistenceError> {
        publish_queue_ops::start_lane_attempt(self, key, expected_revision, event, started_at)
    }

    /// Atomically retain handoff evidence and apply the engine-selected next
    /// fact, maintaining the typed deadline index in the same commit.
    pub fn record_lane_handoff(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        ordinal: u64,
        detail: PublishQueueAttemptHandoff,
        next: PublishQueuePostHandoffState,
    ) -> Result<PublishQueueLane, PersistenceError> {
        publish_queue_ops::record_lane_handoff(self, key, expected_revision, ordinal, detail, next)
    }

    /// Make the current attempt terminal without rewriting its immutable v1
    /// Started row. Exact ordinal + lane revision reject late ACKs against a
    /// newer attempt; detail, cursor, and deadline removal share one commit.
    pub fn finish_lane_attempt(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        ordinal: u64,
        outcome: PublishQueueAttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        publish_queue_ops::finish_lane_attempt(
            self,
            key,
            expected_revision,
            ordinal,
            outcome,
            finished_at,
        )
    }

    /// Atomically finish an exact AUTH-waiting lane without fabricating an
    /// EVENT attempt. Exact lane revision is checked before idempotence, so a
    /// stale writer can never borrow success from a newer terminal fact.
    pub fn deny_lane_auth(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        denial: AuthDenial,
    ) -> Result<PublishQueueLane, PersistenceError> {
        publish_queue_ops::deny_lane_auth(self, key, expected_revision, denial)
    }

    pub fn recover_attempt_details(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueAttemptDetails>, PersistenceError> {
        publish_queue_ops::recover_attempt_details(self, intent_id)
    }

    /// Delete bounded open-work rows only after a non-empty lane set is all
    /// terminal. Receipts and all route/attempt/detail evidence are retained.
    pub fn close_terminal_intent(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        publish_queue_ops::close_terminal_intent(self, intent_id)
    }

    /// Delete an intent's bounded open-work rows when it owns NO lanes at all.
    ///
    /// The exact structural complement of [`Self::close_terminal_intent`],
    /// which requires a NON-EMPTY all-terminal lane set. Zero lanes is a fact
    /// this crate can check for itself, so neither door asks the store to
    /// guess at routing policy: the engine calls this one only when its own
    /// resolution reported knowledge exhausted with zero destinations, and
    /// the store still refuses if any lane exists.
    ///
    /// Without it a write that resolved to nowhere kept its open-work row
    /// forever — unremovable (the removal door refuses an open intent),
    /// uncancellable once signed, and replayed on every boot. That is the
    /// FIRST-RUN path now that a fresh install with no reachable relay list
    /// terminates as `NoDestination`, so it is a leak on the most common
    /// path rather than an edge case.
    ///
    /// Receipts stay retained and reattachable, exactly as
    /// [`Self::close_terminal_intent`] leaves them.
    pub fn close_unroutable_intent(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        publish_queue_ops::close_unroutable_intent(self, intent_id)
    }
}

#[cfg(test)]
mod corruption_tests;

#[cfg(test)]
mod crash_atomicity_tests;

#[cfg(test)]
mod commit_structure_tests;

#[cfg(test)]
mod durability_tests;

#[cfg(test)]
mod tests;
