//! [`RedbStore`] — the persistent, `redb`-backed `EventStore` (M3 step A1).
//!
//! Canonical events use an immutable portable binary note value addressed by
//! a compact monotonic `u64` key. Raw event ids map to that key, optional local
//! state has a dedicated compact value, and relay observations are fixed-width
//! `(event, interned-relay) -> timestamp` rows. Every ordered secondary index
//! points straight at the event key. Queries borrow note fields from redb
//! guards and join provenance only for returned rows. Displaced outbox rows
//! remain self-contained binary snapshots; other outbox/coverage metadata
//! remains typed JSON.
//!
//! `redb`'s own errors (`TableError`/`StorageError`/…) are all invariant
//! violations for this crate's purposes — a healthy embedded DB file does
//! not fail to open a table it created itself, or fail to commit a
//! transaction it started — so they are `.expect()`ed rather than threaded
//! through `EventStore`'s trait signatures (which, matching `MemoryStore`,
//! carry no `Result` at all). A real I/O error here means the on-disk file
//! is corrupt, not a reachable, recoverable condition this crate's callers
//! could usefully branch on today.

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
    open_and_register, reset_store, OpenStoreRegistration, RegisteredOpen,
};
use crate::{
    AcceptOutcome, AcceptWrite, AttemptHandoffDetail, AttemptOutcome, AttemptTransientDetail,
    ClaimSet, CloseIntentOutcome, CompensateOutcome, CoverageInterval, CoverageKey, DeadlineKind,
    EventCursor, EventStore, GcReport, InFlightPhase, InsertOutcome, IntentId, IntentSigState,
    LaneDeadline, LaneKey, LaneState, LocalOrigin, PersistenceError, PostHandoffState,
    PromoteOutcome, Provenance, ReceiptState, RecoveredAttempt, RecoveredAttemptDetails,
    RecoveredIntent, RecoveredLane, RecoveredReceipt, RecoveredRouteRevision, RefuseReason,
    RelayObserved, RetractReason, SigState, StoredEvent, TransientCause, WriteDurability,
};

#[cfg(feature = "bench-instrumentation")]
mod compact_index_bench;
#[cfg(feature = "bench-instrumentation")]
mod fjall_ingest_bench;
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
mod outbox;
#[cfg(test)]
use outbox::*;
mod canonical;
#[cfg(test)]
use canonical::*;
mod query;
#[cfg(feature = "bench-instrumentation")]
pub use query::set_bench_exact_cardinality;
#[cfg(test)]
use query::*;
mod ingest_txn;
mod mutation;
mod store;
#[cfg(test)]
use store::RedbCrashPoint;
pub use store::RedbStore;
mod event_ops;
#[cfg(test)]
use event_ops::CoverageRowRecord;
mod ingest;
mod outbox_ops;
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

    fn query_newest_observed_by(
        &self,
        filter: &Filter,
        relays: &BTreeSet<RelayUrl>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_observed_by(self, filter, relays, limit)
    }

    fn query_newest_before(
        &self,
        filter: &Filter,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before(self, filter, before, limit)
    }

    fn query_newest_before_observed_by(
        &self,
        filter: &Filter,
        relays: &BTreeSet<RelayUrl>,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before_observed_by(self, filter, relays, before, limit)
    }

    fn query_newest_before_any(
        &self,
        filters: &[Filter],
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before_any(self, filters, before, limit)
    }

    fn query_newest_before_any_observed_by(
        &self,
        filters: &[Filter],
        relays: &BTreeSet<RelayUrl>,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        event_ops::query_newest_before_any_observed_by(self, filters, relays, before, limit)
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
        atom: &ContextualAtom,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) -> Result<(), PersistenceError> {
        event_ops::record_coverage(self, atom, relay, proven)
    }

    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        event_ops::get_coverage(self, key, relay)
    }

    fn gc(&mut self, claims: &ClaimSet) -> Result<GcReport, PersistenceError> {
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

    fn cancel_ephemeral_receipt(
        &mut self,
        receipt_id: u64,
    ) -> Result<crate::CancelEphemeralOutcome, PersistenceError> {
        write_ops::cancel_ephemeral_receipt(self, receipt_id)
    }

    fn mark_ephemeral_signed(&mut self, receipt_id: u64) -> Result<bool, PersistenceError> {
        write_ops::mark_ephemeral_signed(self, receipt_id)
    }

    fn recover_outbox(&self) -> Vec<RecoveredIntent> {
        outbox_ops::recover_outbox(self)
    }

    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError> {
        outbox_ops::reattach_receipt(self, receipt_id)
    }

    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError> {
        outbox_ops::lookup_correlation(self, token)
    }

    fn record_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        outbox_ops::record_route_revision(self, intent_id, relays)
    }

    fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
        outbox_ops::recover_route_revisions(self, intent_id)
    }

    fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        outbox_ops::recover_attempts(self, intent_id)
    }

    fn bootstrap_outbox_lanes(
        &mut self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        outbox_ops::bootstrap_outbox_lanes(self, intent_id)
    }

    fn recover_outbox_lanes(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        outbox_ops::recover_outbox_lanes(self, intent_id)
    }

    fn due_outbox_deadlines(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<LaneDeadline>, PersistenceError> {
        outbox_ops::due_outbox_deadlines(self, now, limit)
    }

    fn next_outbox_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        outbox_ops::next_outbox_deadline(self)
    }

    fn set_lane_waiting(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        outbox_ops::set_lane_waiting(self, key, expected_revision, auth)
    }

    fn set_lane_eligible(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        since: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        outbox_ops::set_lane_eligible(self, key, expected_revision, since)
    }

    fn set_lane_transient(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: TransientCause,
        raw_reason: Option<String>,
    ) -> Result<RecoveredLane, PersistenceError> {
        outbox_ops::set_lane_transient(
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
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        at: Timestamp,
        cause: TransientCause,
        raw_reason: Option<String>,
        auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        outbox_ops::suspend_lane_attempt(
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
        key: &LaneKey,
        expected_revision: u64,
        event: Event,
        started_at: Timestamp,
    ) -> Result<(RecoveredAttempt, RecoveredLane), PersistenceError> {
        outbox_ops::start_lane_attempt(self, key, expected_revision, event, started_at)
    }

    fn record_lane_handoff(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        detail: AttemptHandoffDetail,
        next: PostHandoffState,
    ) -> Result<RecoveredLane, PersistenceError> {
        outbox_ops::record_lane_handoff(self, key, expected_revision, ordinal, detail, next)
    }

    fn finish_lane_attempt(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        outcome: AttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        outbox_ops::finish_lane_attempt(self, key, expected_revision, ordinal, outcome, finished_at)
    }

    fn recover_attempt_details(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredAttemptDetails>, PersistenceError> {
        outbox_ops::recover_attempt_details(self, intent_id)
    }

    fn close_terminal_intent(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        outbox_ops::close_terminal_intent(self, intent_id)
    }

    fn accept_ephemeral(
        &mut self,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
    ) -> Result<u64, PersistenceError> {
        outbox_ops::accept_ephemeral(self, frozen_id, expected_pubkey)
    }
}

#[cfg(test)]
mod crash_atomicity_tests;

#[cfg(test)]
mod tests;
