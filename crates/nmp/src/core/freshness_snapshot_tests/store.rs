use std::collections::BTreeSet;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use nmp_grammar::ContextualAtom;
use nmp_store::{
    AcceptOutcome, AcceptWrite, CompensateOutcome, CompensationReason, CoverageInterval,
    CoverageKey, EventCursor, EventStore, GcReport, GcRetentionSet, InsertOutcome, MemoryStore,
    PersistenceError, PromoteOutcome, PublishQueueAttempt, PublishQueueIntent, PublishQueueReceipt,
    PublishQueueRouteRevision, RefuseReason, RelayObserved, RemoveQueueEntryOutcome, RetractReason,
    StoredEvent,
};
use nostr::{Event, EventId, PublicKey, RelayUrl, Timestamp};

#[derive(Clone, Default)]
pub(super) struct CoverageReadCounter(Arc<AtomicU64>);

impl CoverageReadCounter {
    pub(super) fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    pub(super) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

pub(super) struct CountingCoverageStore {
    inner: MemoryStore,
    reads: CoverageReadCounter,
}

impl CountingCoverageStore {
    pub(super) fn new(inner: MemoryStore, reads: CoverageReadCounter) -> Self {
        Self { inner, reads }
    }
}

impl EventStore for CountingCoverageStore {
    fn compensate_write_with_state(
        &mut self,
        intent_id: nmp_store::IntentId,
        reason: CompensationReason,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write_with_state(intent_id, reason)
    }

    fn insert(
        &mut self,
        event: Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        self.inner.insert(event, from)
    }

    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.query(filter)
    }

    fn query_newest_before(
        &self,
        filter: &nostr::Filter,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.query_newest_before(filter, before, limit)
    }

    fn remove(
        &mut self,
        id: EventId,
        reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        self.inner.remove(id, reason)
    }

    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.expire_due(now)
    }

    fn next_expiration(&self) -> Result<Option<Timestamp>, PersistenceError> {
        self.inner.next_expiration()
    }

    fn record_coverage(
        &mut self,
        claims: &[(ContextualAtom, RelayUrl, CoverageInterval)],
    ) -> Result<(), PersistenceError> {
        self.inner.record_coverage(claims)
    }

    fn get_coverage(
        &self,
        key: CoverageKey,
        relay: &RelayUrl,
    ) -> Result<Option<CoverageInterval>, PersistenceError> {
        self.reads.0.fetch_add(1, Ordering::Relaxed);
        self.inner.get_coverage(key, relay)
    }

    fn gc(&mut self, claims: &GcRetentionSet) -> Result<GcReport, PersistenceError> {
        self.inner.gc(claims)
    }

    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        self.inner.accept_write(accept)
    }

    fn promote_signed(
        &mut self,
        intent_id: nmp_store::IntentId,
        verified: nmp_store::VerifiedSignature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner
            .promote_signed(crate::PromotionTarget::Event(intent_id), verified)
    }

    fn compensate_write(
        &mut self,
        intent_id: nmp_store::IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write(intent_id)
    }

    fn recover_publish_queue(&self) -> Result<Vec<PublishQueueIntent>, PersistenceError> {
        self.inner.recover_publish_queue()
    }

    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<PublishQueueReceipt>, PersistenceError> {
        self.inner.reattach_receipt(receipt_id)
    }

    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError> {
        self.inner.lookup_correlation(token)
    }

    fn record_route_revision(
        &mut self,
        intent_id: nmp_store::IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<PublishQueueRouteRevision, PersistenceError> {
        self.inner.record_route_revision(intent_id, relays)
    }

    fn recover_route_revisions(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<PublishQueueRouteRevision>, PersistenceError> {
        self.inner.recover_route_revisions(intent_id)
    }

    fn recover_attempts(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<PublishQueueAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }

    fn enumerate_publish_queue_receipts(
        &self,
    ) -> Result<Vec<PublishQueueReceipt>, PersistenceError> {
        self.inner.enumerate_publish_queue_receipts()
    }

    fn publish_queue_receipts_after(
        &self,
        after: Option<u64>,
        limit: u8,
    ) -> Result<Vec<PublishQueueReceipt>, PersistenceError> {
        self.inner.publish_queue_receipts_after(after, limit)
    }

    fn remove_publish_queue_entry(
        &mut self,
        receipt_id: u64,
    ) -> Result<RemoveQueueEntryOutcome, PersistenceError> {
        self.inner.remove_publish_queue_entry(receipt_id)
    }

    fn accept_refused(
        &mut self,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
        reason: RefuseReason,
    ) -> Result<u64, PersistenceError> {
        self.inner
            .accept_refused(frozen_id, expected_pubkey, reason)
    }
}
