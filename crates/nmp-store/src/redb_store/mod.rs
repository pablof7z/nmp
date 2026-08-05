//! [`RedbStore`] — the persistent, `redb`-backed `EventStore` (M3 step A1).
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
//! decode, both surface as `PersistenceError` through the owning
//! `EventStore` door. They are classified [`crate::PersistenceFault::
//! Invariant`] rather than `Corrupted`: the backend is healthy, and the
//! decode happens before the enclosing write transaction commits, so
//! `DurabilityOutcome::Absent` is provable rather than merely convenient
//! (see that variant's doc). A decode failure is never allowed to become an
//! empty result, a skipped row, or a defaulted value — a false miss is
//! exactly the outcome the typed error exists to prevent.
//!
//! Two peek doors, `next_expiration` and `get_coverage`, are still infallible
//! at the trait and therefore still `.expect()` at the end of an otherwise
//! fallible chain. Widening them is #763's unit, not this one.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::AtomicU8;
#[cfg(any(test, feature = "bench-instrumentation"))]
use std::sync::atomic::{AtomicU64, Ordering};

use nmp_grammar::{ConcreteFilter, ContextualAtom};
use nostr::secp256k1::schnorr::Signature;
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
#[cfg(test)]
use schema::*;
mod publish_queue;
mod publish_queue_codec;
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
#[cfg(test)]
use store::RedbCrashPoint;
pub use store::RedbStore;
mod event_ops;
mod ingest;
mod publish_queue_ops;
mod write_ops;

impl EventStore for RedbStore {
    fn insert(
        &mut self,
        event: Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        event_ops::insert(self, event, from)
    }

    fn insert_batch(
        &mut self,
        events: Vec<(Event, RelayObserved)>,
    ) -> Result<Vec<InsertOutcome>, PersistenceError> {
        event_ops::insert_batch(self, events)
    }

    fn query(&self, filter: &Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query(self, filter)
    }

    fn query_newest(
        &self,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest(self, filter, limit)
    }

    fn query_newest_ids(
        &self,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<EventId>, PersistenceError> {
        event_ops::query_newest_ids(self, filter, limit)
    }

    fn query_newest_under_pin(
        &self,
        filter: &Filter,
        pinned: &BTreeSet<RelayUrl>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_under_pin(self, filter, pinned, limit)
    }

    fn query_newest_before(
        &self,
        filter: &Filter,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before(self, filter, before, limit)
    }

    fn query_newest_before_under_pin(
        &self,
        filter: &Filter,
        pinned: &BTreeSet<RelayUrl>,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before_under_pin(self, filter, pinned, before, limit)
    }

    fn query_newest_before_any(
        &self,
        filters: &[Filter],
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before_any(self, filters, before, limit)
    }

    fn query_newest_before_any_under_pin(
        &self,
        filters: &[Filter],
        pinned: &BTreeSet<RelayUrl>,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before_any_under_pin(self, filters, pinned, before, limit)
    }

    fn remove(
        &mut self,
        id: EventId,
        _reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        event_ops::remove(self, id, _reason)
    }

    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::expire_due(self, now)
    }

    fn next_expiration(&self) -> Option<Timestamp> {
        event_ops::next_expiration(self)
    }

    fn record_coverage(
        &mut self,
        claims: &[(ContextualAtom, RelayUrl, CoverageInterval)],
    ) -> Result<(), PersistenceError> {
        event_ops::record_coverage(self, claims)
    }

    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        event_ops::get_coverage(self, key, relay)
    }

    fn gc(&mut self, claims: &GcRetentionSet) -> Result<GcReport, PersistenceError> {
        event_ops::gc(self, claims)
    }

    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        write_ops::accept_write(self, accept)
    }

    fn promote_signed(
        &mut self,
        intent_id: IntentId,
        sig: Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        write_ops::promote_signed(self, intent_id, sig)
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

    fn record_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<PublishQueueRouteRevision, PersistenceError> {
        publish_queue_ops::record_route_revision(self, intent_id, relays)
    }

    fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueRouteRevision>, PersistenceError> {
        publish_queue_ops::recover_route_revisions(self, intent_id)
    }

    fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueAttempt>, PersistenceError> {
        publish_queue_ops::recover_attempts(self, intent_id)
    }

    fn bootstrap_publish_queue_lanes(
        &mut self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        publish_queue_ops::bootstrap_publish_queue_lanes(self, intent_id)
    }

    fn recover_publish_queue_lanes(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        publish_queue_ops::recover_publish_queue_lanes(self, intent_id)
    }

    fn due_publish_queue_deadlines(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<PublishQueueDeadline>, PersistenceError> {
        publish_queue_ops::due_publish_queue_deadlines(self, now, limit)
    }

    fn next_publish_queue_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        publish_queue_ops::next_publish_queue_deadline(self)
    }

    fn set_lane_waiting(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        publish_queue_ops::set_lane_waiting(self, key, expected_revision, auth)
    }

    fn set_lane_eligible(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        since: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        publish_queue_ops::set_lane_eligible(self, key, expected_revision, since)
    }

    fn set_lane_transient(
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

    fn suspend_lane_attempt(
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

    fn start_lane_attempt(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        event: Event,
        started_at: Timestamp,
    ) -> Result<(PublishQueueAttempt, PublishQueueLane), PersistenceError> {
        publish_queue_ops::start_lane_attempt(self, key, expected_revision, event, started_at)
    }

    fn record_lane_handoff(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        ordinal: u64,
        detail: PublishQueueAttemptHandoff,
        next: PublishQueuePostHandoffState,
    ) -> Result<PublishQueueLane, PersistenceError> {
        publish_queue_ops::record_lane_handoff(self, key, expected_revision, ordinal, detail, next)
    }

    fn finish_lane_attempt(
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

    fn deny_lane_auth(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        denial: AuthDenial,
    ) -> Result<PublishQueueLane, PersistenceError> {
        publish_queue_ops::deny_lane_auth(self, key, expected_revision, denial)
    }

    fn recover_attempt_details(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueAttemptDetails>, PersistenceError> {
        publish_queue_ops::recover_attempt_details(self, intent_id)
    }

    fn close_terminal_intent(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        publish_queue_ops::close_terminal_intent(self, intent_id)
    }

    fn close_unroutable_intent(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        publish_queue_ops::close_unroutable_intent(self, intent_id)
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
