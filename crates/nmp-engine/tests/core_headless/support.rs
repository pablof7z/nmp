//! Headless `EngineCore` tests (M3 plan §5 tier A, re-expressed at the
//! `EngineCore` level per the M3-B build brief) + the coverage-attribution
//! ruling's falsifiers
//! (`docs/consults/2026-07-11-fable-coverage-attribution.md`). Zero I/O:
//! every "relay" interaction here is a scripted `EngineMsg::RelayConnected`/
//! `RelayFrame` fed directly to `EngineCore::handle`, exactly as the ruling's
//! own reasoning demands (send-time snapshots, the EOSE intersection rule,
//! `limit` poisoning, and per-query scoped acquisition evidence).

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nmp_engine::core::{
    AcquisitionEvidence, AuthCapability, AuthCapabilityInstance, AuthEffect, AuthPolicyOutcome,
    AuthSendOutcome, AuthSignerOutcome, Effect, EngineCore, EngineMsg, ReceiptId, RowDelta,
    RowSink, ShortfallFact, SourceEvidence, SourceStatus,
};
use nmp_engine::outbox::{ReceiptSink, WriteStatus};
use nmp_grammar::{
    AccessContext, Binding, ConcreteFilter, ContextualAtom, Durability, Filter, NarrowOnly,
    PrivateRoute, RelaySessionKey, SourceAuthority, WriteIntent, WritePayload, WriteRouting,
};
use nmp_resolver::{HandleId, LiveQuery};
use nmp_router::{FixtureDirectory, SubId, WireOp};
use nmp_store::{
    AcceptOutcome, AcceptWrite, AttemptOutcome, CancelEphemeralOutcome, ClaimSet,
    CompensateOutcome, CompensationReason, CoverageInterval, CoverageKey, EventStore, GcReport,
    InsertOutcome, MemoryStore, PersistenceError, PromoteOutcome, RecoveredAttempt,
    RecoveredIntent, RecoveredReceipt, RecoveredRouteRevision, RedbStore, RelayObserved,
    RetractReason, StoredEvent,
};
use nmp_transport::{DisconnectReason, HandoffResult, RelayFrame, RelayHandle};
use nostr::{Keys, Kind, RelayMessage, RelayUrl, SubscriptionId, Timestamp, UnsignedEvent};

use std::collections::BTreeSet;

/// A `RowSink` that just records every batch it is handed, for assertions.
#[derive(Clone, Default)]
struct CapturingSink(Arc<Mutex<Vec<Vec<RowDelta>>>>);

impl RowSink for CapturingSink {
    fn on_rows(&self, rows: Vec<RowDelta>) {
        self.0.lock().unwrap().push(rows);
    }
}

/// A `ReceiptSink` that just records every status it is handed, for
/// assertions (mirrors `CapturingSink` on the write side).
#[derive(Clone, Default)]
struct CapturingReceiptSink(Arc<Mutex<Vec<WriteStatus>>>);

impl ReceiptSink for CapturingReceiptSink {
    fn on_status(&self, status: WriteStatus) {
        self.0.lock().unwrap().push(status);
    }
}

fn unsigned(author: &Keys, seq: u64, content: &str) -> UnsignedEvent {
    UnsignedEvent::new(
        author.public_key(),
        Timestamp::from(seq),
        Kind::TextNote,
        Vec::new(),
        content,
    )
}

fn cf(kinds: &[u16], authors: &[&str]) -> ConcreteFilter {
    ConcreteFilter {
        kinds: Some(kinds.iter().copied().collect()),
        authors: Some(authors.iter().map(|s| s.to_string()).collect()),
        ..ConcreteFilter::default()
    }
}

/// An `AuthorOutboxes`-sourced atom (#118): every `cf(...)` fixture in this
/// file is author-bearing, so this is the exact true context each one was
/// actually acquired under -- `EngineCore::get_coverage` now takes the
/// atom's real `ContextualAtom`, never a reconstruction.
fn ctx_atom(filter: ConcreteFilter) -> ContextualAtom {
    ctx_atom_with(filter, SourceAuthority::AuthorOutboxes)
}

fn ctx_atom_with(filter: ConcreteFilter, source: SourceAuthority) -> ContextualAtom {
    ContextualAtom {
        filter,
        source,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn literal_query(kinds: &[u16], author_hex: &str) -> LiveQuery {
    LiveQuery::from_filter(Filter {
        kinds: Some(kinds.iter().copied().collect()),
        authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
        ..Filter::default()
    })
}

fn new_core(dir: FixtureDirectory) -> EngineCore<MemoryStore> {
    EngineCore::new(MemoryStore::new(), Box::new(dir), 10)
}

fn activate<S: EventStore>(core: &mut EngineCore<S>, keys: &Keys) {
    core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
}

struct FailOnceCompensationStore {
    inner: MemoryStore,
    fail_next_compensation: bool,
    fail_next_attempt_finish: bool,
}

macro_rules! delegate_lane_methods {
    ($inner:ident) => {
        fn bootstrap_outbox_lanes(
            &mut self,
            intent_id: nmp_store::IntentId,
        ) -> Result<Vec<nmp_store::RecoveredLane>, PersistenceError> {
            self.$inner.bootstrap_outbox_lanes(intent_id)
        }
        fn recover_outbox_lanes(
            &self,
            intent_id: nmp_store::IntentId,
        ) -> Result<Vec<nmp_store::RecoveredLane>, PersistenceError> {
            self.$inner.recover_outbox_lanes(intent_id)
        }
        fn due_outbox_deadlines(
            &self,
            now: Timestamp,
            limit: usize,
        ) -> Result<Vec<nmp_store::LaneDeadline>, PersistenceError> {
            self.$inner.due_outbox_deadlines(now, limit)
        }
        fn next_outbox_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
            self.$inner.next_outbox_deadline()
        }
        fn set_lane_waiting(
            &mut self,
            key: &nmp_store::LaneKey,
            revision: u64,
            auth: bool,
        ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
            self.$inner.set_lane_waiting(key, revision, auth)
        }
        fn set_lane_eligible(
            &mut self,
            key: &nmp_store::LaneKey,
            revision: u64,
            since: Timestamp,
        ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
            self.$inner.set_lane_eligible(key, revision, since)
        }
        fn set_lane_transient(
            &mut self,
            key: &nmp_store::LaneKey,
            revision: u64,
            ordinal: u64,
            eligible_at: Timestamp,
            cause: nmp_store::TransientCause,
            raw_reason: Option<String>,
        ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
            self.$inner
                .set_lane_transient(key, revision, ordinal, eligible_at, cause, raw_reason)
        }
        fn suspend_lane_attempt(
            &mut self,
            key: &nmp_store::LaneKey,
            revision: u64,
            ordinal: u64,
            at: Timestamp,
            cause: nmp_store::TransientCause,
            raw_reason: Option<String>,
            auth: bool,
        ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
            self.$inner
                .suspend_lane_attempt(key, revision, ordinal, at, cause, raw_reason, auth)
        }
        fn record_lane_handoff(
            &mut self,
            key: &nmp_store::LaneKey,
            revision: u64,
            ordinal: u64,
            detail: nmp_store::AttemptHandoffDetail,
            next: nmp_store::PostHandoffState,
        ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
            self.$inner
                .record_lane_handoff(key, revision, ordinal, detail, next)
        }
        fn recover_attempt_details(
            &self,
            intent_id: nmp_store::IntentId,
        ) -> Result<Vec<nmp_store::RecoveredAttemptDetails>, PersistenceError> {
            self.$inner.recover_attempt_details(intent_id)
        }
        fn close_terminal_intent(
            &mut self,
            intent_id: nmp_store::IntentId,
        ) -> Result<nmp_store::CloseIntentOutcome, PersistenceError> {
            self.$inner.close_terminal_intent(intent_id)
        }
    };
}

impl FailOnceCompensationStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            fail_next_compensation: true,
            fail_next_attempt_finish: false,
        }
    }

    fn failing_attempt_finish() -> Self {
        Self {
            inner: MemoryStore::new(),
            fail_next_compensation: false,
            fail_next_attempt_finish: true,
        }
    }
}

impl EventStore for FailOnceCompensationStore {
    fn insert(
        &mut self,
        event: nostr::Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        self.inner.insert(event, from)
    }
    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.query(filter)
    }
    fn remove(
        &mut self,
        id: nostr::EventId,
        reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        self.inner.remove(id, reason)
    }
    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.expire_due(now)
    }
    fn next_expiration(&self) -> Option<Timestamp> {
        self.inner.next_expiration()
    }
    fn record_coverage(
        &mut self,
        atom: &nmp_grammar::ContextualAtom,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) -> Result<(), PersistenceError> {
        self.inner.record_coverage(atom, relay, proven)
    }
    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        self.inner.get_coverage(key, relay)
    }
    fn gc(&mut self, claims: &ClaimSet) -> Result<GcReport, PersistenceError> {
        self.inner.gc(claims)
    }
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        self.inner.accept_write(accept)
    }
    fn promote_signed(
        &mut self,
        intent_id: nmp_store::IntentId,
        sig: nostr::secp256k1::schnorr::Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner.promote_signed(intent_id, sig)
    }
    fn compensate_write(
        &mut self,
        intent_id: nmp_store::IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        if self.fail_next_compensation {
            self.fail_next_compensation = false;
            Err(PersistenceError(
                "injected compensation failure".to_string(),
            ))
        } else {
            self.inner.compensate_write(intent_id)
        }
    }
    fn compensate_write_with_state(
        &mut self,
        intent_id: nmp_store::IntentId,
        reason: CompensationReason,
    ) -> Result<CompensateOutcome, PersistenceError> {
        if self.fail_next_compensation {
            self.fail_next_compensation = false;
            Err(PersistenceError(
                "injected compensation failure".to_string(),
            ))
        } else {
            self.inner.compensate_write_with_state(intent_id, reason)
        }
    }
    fn cancel_ephemeral_receipt(
        &mut self,
        receipt_id: u64,
    ) -> Result<CancelEphemeralOutcome, PersistenceError> {
        self.inner.cancel_ephemeral_receipt(receipt_id)
    }
    fn mark_ephemeral_signed(&mut self, receipt_id: u64) -> Result<bool, PersistenceError> {
        self.inner.mark_ephemeral_signed(receipt_id)
    }
    fn recover_outbox(&self) -> Vec<RecoveredIntent> {
        self.inner.recover_outbox()
    }
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError> {
        self.inner.reattach_receipt(receipt_id)
    }
    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError> {
        self.inner.lookup_correlation(token)
    }
    fn record_route_revision(
        &mut self,
        intent_id: nmp_store::IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        self.inner.record_route_revision(intent_id, relays)
    }
    fn recover_route_revisions(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
        self.inner.recover_route_revisions(intent_id)
    }
    fn recover_attempts(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }
    delegate_lane_methods!(inner);
    fn start_lane_attempt(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        event: nostr::Event,
        started_at: Timestamp,
    ) -> Result<(RecoveredAttempt, nmp_store::RecoveredLane), PersistenceError> {
        self.inner
            .start_lane_attempt(key, revision, event, started_at)
    }
    fn finish_lane_attempt(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        ordinal: u64,
        outcome: AttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
        if self.fail_next_attempt_finish {
            self.fail_next_attempt_finish = false;
            return Err(PersistenceError("injected attempt finish failure".into()));
        }
        self.inner
            .finish_lane_attempt(key, revision, ordinal, outcome, finished_at)
    }
    fn accept_ephemeral(
        &mut self,
        frozen_id: nostr::EventId,
        expected_pubkey: nostr::PublicKey,
    ) -> Result<u64, PersistenceError> {
        self.inner.accept_ephemeral(frozen_id, expected_pubkey)
    }
}

struct SharedFailStartStore {
    inner: MemoryStore,
    failed_relays: BTreeSet<RelayUrl>,
}

impl SharedFailStartStore {
    fn new(failed_relays: impl IntoIterator<Item = RelayUrl>) -> Self {
        Self {
            inner: MemoryStore::new(),
            failed_relays: failed_relays.into_iter().collect(),
        }
    }
}

impl EventStore for SharedFailStartStore {
    fn compensate_write_with_state(
        &mut self,
        intent_id: nmp_store::IntentId,
        reason: CompensationReason,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write_with_state(intent_id, reason)
    }
    fn cancel_ephemeral_receipt(
        &mut self,
        receipt_id: u64,
    ) -> Result<CancelEphemeralOutcome, PersistenceError> {
        self.inner.cancel_ephemeral_receipt(receipt_id)
    }
    fn mark_ephemeral_signed(&mut self, receipt_id: u64) -> Result<bool, PersistenceError> {
        self.inner.mark_ephemeral_signed(receipt_id)
    }
    fn insert(
        &mut self,
        event: nostr::Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        self.inner.insert(event, from)
    }
    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.query(filter)
    }
    fn remove(
        &mut self,
        id: nostr::EventId,
        reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        self.inner.remove(id, reason)
    }
    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.expire_due(now)
    }
    fn next_expiration(&self) -> Option<Timestamp> {
        self.inner.next_expiration()
    }
    fn record_coverage(
        &mut self,
        atom: &nmp_grammar::ContextualAtom,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) -> Result<(), PersistenceError> {
        self.inner.record_coverage(atom, relay, proven)
    }
    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        self.inner.get_coverage(key, relay)
    }
    fn gc(&mut self, claims: &ClaimSet) -> Result<GcReport, PersistenceError> {
        self.inner.gc(claims)
    }
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        self.inner.accept_write(accept)
    }
    fn promote_signed(
        &mut self,
        intent_id: nmp_store::IntentId,
        sig: nostr::secp256k1::schnorr::Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner.promote_signed(intent_id, sig)
    }
    fn compensate_write(
        &mut self,
        intent_id: nmp_store::IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write(intent_id)
    }
    fn recover_outbox(&self) -> Vec<RecoveredIntent> {
        self.inner.recover_outbox()
    }
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError> {
        self.inner.reattach_receipt(receipt_id)
    }
    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError> {
        self.inner.lookup_correlation(token)
    }
    fn record_route_revision(
        &mut self,
        intent_id: nmp_store::IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        self.inner.record_route_revision(intent_id, relays)
    }
    fn recover_route_revisions(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
        self.inner.recover_route_revisions(intent_id)
    }
    fn recover_attempts(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }
    delegate_lane_methods!(inner);
    fn start_lane_attempt(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        event: nostr::Event,
        started_at: Timestamp,
    ) -> Result<(RecoveredAttempt, nmp_store::RecoveredLane), PersistenceError> {
        if self.failed_relays.contains(&key.relay) {
            return Err(PersistenceError("injected attempt start failure".into()));
        }
        self.inner
            .start_lane_attempt(key, revision, event, started_at)
    }
    fn finish_lane_attempt(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        ordinal: u64,
        outcome: AttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
        self.inner
            .finish_lane_attempt(key, revision, ordinal, outcome, finished_at)
    }
    fn accept_ephemeral(
        &mut self,
        frozen_id: nostr::EventId,
        expected_pubkey: nostr::PublicKey,
    ) -> Result<u64, PersistenceError> {
        self.inner.accept_ephemeral(frozen_id, expected_pubkey)
    }
}

struct RedbFailStartStore {
    inner: RedbStore,
    failed_relays: BTreeSet<RelayUrl>,
    fail_route_revisions: bool,
}

impl RedbFailStartStore {
    fn open(path: &std::path::Path, failed_relays: impl IntoIterator<Item = RelayUrl>) -> Self {
        Self {
            inner: RedbStore::open(path).expect("open redb failure fixture"),
            failed_relays: failed_relays.into_iter().collect(),
            fail_route_revisions: false,
        }
    }

    fn open_with_route_failure(path: &std::path::Path) -> Self {
        Self {
            inner: RedbStore::open(path).expect("open redb route-failure fixture"),
            failed_relays: BTreeSet::new(),
            fail_route_revisions: true,
        }
    }
}

impl EventStore for RedbFailStartStore {
    fn compensate_write_with_state(
        &mut self,
        intent_id: nmp_store::IntentId,
        reason: CompensationReason,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write_with_state(intent_id, reason)
    }
    fn cancel_ephemeral_receipt(
        &mut self,
        receipt_id: u64,
    ) -> Result<CancelEphemeralOutcome, PersistenceError> {
        self.inner.cancel_ephemeral_receipt(receipt_id)
    }
    fn mark_ephemeral_signed(&mut self, receipt_id: u64) -> Result<bool, PersistenceError> {
        self.inner.mark_ephemeral_signed(receipt_id)
    }
    fn insert(
        &mut self,
        event: nostr::Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        self.inner.insert(event, from)
    }
    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.query(filter)
    }
    fn remove(
        &mut self,
        id: nostr::EventId,
        reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        self.inner.remove(id, reason)
    }
    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.expire_due(now)
    }
    fn next_expiration(&self) -> Option<Timestamp> {
        self.inner.next_expiration()
    }
    fn record_coverage(
        &mut self,
        atom: &nmp_grammar::ContextualAtom,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) -> Result<(), PersistenceError> {
        self.inner.record_coverage(atom, relay, proven)
    }
    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        self.inner.get_coverage(key, relay)
    }
    fn gc(&mut self, claims: &ClaimSet) -> Result<GcReport, PersistenceError> {
        self.inner.gc(claims)
    }
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        self.inner.accept_write(accept)
    }
    fn promote_signed(
        &mut self,
        intent_id: nmp_store::IntentId,
        sig: nostr::secp256k1::schnorr::Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner.promote_signed(intent_id, sig)
    }
    fn compensate_write(
        &mut self,
        intent_id: nmp_store::IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write(intent_id)
    }
    fn recover_outbox(&self) -> Vec<RecoveredIntent> {
        self.inner.recover_outbox()
    }
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError> {
        self.inner.reattach_receipt(receipt_id)
    }
    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError> {
        self.inner.lookup_correlation(token)
    }
    fn record_route_revision(
        &mut self,
        intent_id: nmp_store::IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        if self.fail_route_revisions {
            return Err(PersistenceError("injected route revision failure".into()));
        }
        self.inner.record_route_revision(intent_id, relays)
    }
    fn recover_route_revisions(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
        self.inner.recover_route_revisions(intent_id)
    }
    fn recover_attempts(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }
    delegate_lane_methods!(inner);
    fn start_lane_attempt(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        event: nostr::Event,
        started_at: Timestamp,
    ) -> Result<(RecoveredAttempt, nmp_store::RecoveredLane), PersistenceError> {
        if self.failed_relays.contains(&key.relay) {
            return Err(PersistenceError("injected attempt start failure".into()));
        }
        self.inner
            .start_lane_attempt(key, revision, event, started_at)
    }
    fn finish_lane_attempt(
        &mut self,
        key: &nmp_store::LaneKey,
        revision: u64,
        ordinal: u64,
        outcome: AttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<nmp_store::RecoveredLane, PersistenceError> {
        self.inner
            .finish_lane_attempt(key, revision, ordinal, outcome, finished_at)
    }
    fn accept_ephemeral(
        &mut self,
        frozen_id: nostr::EventId,
        expected_pubkey: nostr::PublicKey,
    ) -> Result<u64, PersistenceError> {
        self.inner.accept_ephemeral(frozen_id, expected_pubkey)
    }
}

/// Find the single `WireOp::Req` for `relay` inside `effects`, panicking if
/// there isn't exactly one (test-fixture convenience, not production code).
fn req_for<'a>(effects: &'a [Effect], relay: &RelayUrl) -> (&'a SubId, &'a ConcreteFilter) {
    for effect in effects {
        if let Effect::Wire(delta) = effect {
            for (r, ops) in &delta.ops {
                if &r.relay == relay {
                    for op in ops {
                        if let WireOp::Req(sub_id, filter) = op {
                            return (sub_id, filter);
                        }
                    }
                }
            }
        }
    }
    panic!("expected a WireOp::Req for {relay:?} in {effects:?}");
}

fn req_for_kind<'a>(
    effects: &'a [Effect],
    relay: &RelayUrl,
    kind: u16,
) -> (&'a SubId, &'a ConcreteFilter) {
    for effect in effects {
        if let Effect::Wire(delta) = effect {
            for (r, ops) in &delta.ops {
                if &r.relay != relay {
                    continue;
                }
                for op in ops {
                    if let WireOp::Req(sub_id, filter) = op {
                        if filter
                            .kinds
                            .as_ref()
                            .is_some_and(|kinds| kinds.contains(&kind))
                        {
                            return (sub_id, filter);
                        }
                    }
                }
            }
        }
    }
    panic!("expected a kind:{kind} WireOp::Req for {relay:?} in {effects:?}");
}

fn wire_sub_string(sub_id: &SubId) -> String {
    format!("{}", sub_id.1)
}

fn public_session(relay: &RelayUrl) -> RelaySessionKey {
    RelaySessionKey::public(relay.clone())
}

// With the #8 AUTH reducer landed, the write plane rides the signing
// identity's authenticated session again: every durable/ephemeral write
// demands `AccessContext::Nip42(signing pubkey)`, so tests that expect
// attempts must connect exactly this session.
fn signer_session(relay: &RelayUrl, signer: nostr::PublicKey) -> RelaySessionKey {
    RelaySessionKey::new(relay.clone(), AccessContext::Nip42(signer))
}

fn protected_pinned_query(relay: &RelayUrl, signer: nostr::PublicKey, kind: u16) -> LiveQuery {
    LiveQuery(
        nmp_grammar::Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([kind])),
                ..Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            AccessContext::Nip42(signer),
        )
        .expect("protected pinned demand is valid"),
    )
}

fn subscribed_handle(effects: &[Effect]) -> HandleId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, ..) => Some(*id),
            _ => None,
        })
        .expect("subscribe emits its initial row snapshot")
}

fn assert_no_protected_req(effects: &[Effect], session: &RelaySessionKey) {
    assert!(
        !effects.iter().any(|effect| match effect {
            Effect::Replay(candidate, reqs) => candidate == session && !reqs.is_empty(),
            Effect::Wire(delta) => delta.ops.iter().any(|(candidate, ops)| {
                candidate == session && ops.iter().any(|op| matches!(op, WireOp::Req(..)))
            }),
            _ => false,
        }),
        "protected REQs must remain parked before current AUTH readiness: {effects:?}"
    );
}

fn connect<S: EventStore>(core: &mut EngineCore<S>, slot: u32, url: &RelayUrl) -> Vec<Effect> {
    let mut effects = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot,
            generation: 1,
        },
        public_session(url),
    ));
    // Most legacy headless tests model a relay with no NIP-11 support list.
    // Resolve that one-shot explicitly now that connection and HTTP
    // capability acquisition are separate reducer inputs.
    effects.extend(core.handle(EngineMsg::RelayInformationResolved(url.clone(), None)));
    effects
}

fn connect_signer<S: EventStore>(
    core: &mut EngineCore<S>,
    slot: u32,
    url: &RelayUrl,
    signer: nostr::PublicKey,
) -> Vec<Effect> {
    let mut effects = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot,
            generation: 1,
        },
        signer_session(url, signer),
    ));
    effects.extend(core.handle(EngineMsg::RelayInformationResolved(url.clone(), None)));
    effects
}

fn release_author_probe<S: EventStore>(
    core: &mut EngineCore<S>,
    handle: RelayHandle,
    url: &RelayUrl,
    signer: nostr::PublicKey,
) -> Vec<Effect> {
    core.handle(EngineMsg::AuthProbeReleased(
        handle,
        signer_session(url, signer),
    ))
}

/// Complete the canonical NIP-42 handshake for one exact signer session.
///
/// Protected-write tests call this explicitly after `connect_signer`; the
/// returned effects are the matching AUTH `OK` wake, so callers can still
/// assert any write scheduling caused by readiness.
fn authenticate_signer<S: EventStore>(
    core: &mut EngineCore<S>,
    slot: u32,
    url: &RelayUrl,
    signer: &Keys,
) -> Vec<Effect> {
    authenticate_signer_generation(
        core,
        RelayHandle {
            slot,
            generation: 1,
        },
        url,
        signer,
    )
}

fn authenticate_signer_generation<S: EventStore>(
    core: &mut EngineCore<S>,
    handle: RelayHandle,
    url: &RelayUrl,
    signer: &Keys,
) -> Vec<Effect> {
    let session = signer_session(url, signer.public_key());
    let challenge = core.handle(EngineMsg::RelayFrame(
        handle,
        session.clone(),
        RelayFrame::from(RelayMessage::Auth {
            challenge: Cow::Owned(format!(
                "core-headless-{}-{}",
                handle.slot, handle.generation
            )),
        }),
    ));
    let policy_token = challenge
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::RequestPolicy { token, .. }) => Some(token),
            _ => None,
        })
        .expect("AUTH challenge requests policy for the exact session");
    assert_eq!(policy_token.epoch.session, session);
    assert_eq!(policy_token.epoch.handle, handle);

    finish_authentication(core, handle, session, signer, policy_token)
}

fn finish_authentication<S: EventStore>(
    core: &mut EngineCore<S>,
    handle: RelayHandle,
    session: RelaySessionKey,
    signer: &Keys,
    policy_token: nmp_engine::core::AuthOpToken,
) -> Vec<Effect> {
    let policy_instance = AuthCapabilityInstance(1);
    core.handle(EngineMsg::AuthCapabilityBound {
        token: policy_token.clone(),
        capability: AuthCapability::Policy,
        instance: policy_instance,
    });
    let signature = core.handle(EngineMsg::AuthPolicyCompleted(
        policy_token,
        Some(policy_instance),
        AuthPolicyOutcome::Allow,
    ));
    let (sign_token, unsigned) = signature
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::RequestSignature { token, unsigned }) => {
                Some((token, unsigned))
            }
            _ => None,
        })
        .expect("allowed AUTH policy requests the frozen event signature");
    assert_eq!(sign_token.epoch.session, session);
    assert_eq!(sign_token.epoch.handle, handle);
    assert_eq!(unsigned.kind, Kind::Authentication);
    assert_eq!(unsigned.pubkey, signer.public_key());

    let signed = unsigned
        .sign_with_keys(signer)
        .expect("sign deterministic AUTH fixture");
    let signer_instance = AuthCapabilityInstance(2);
    core.handle(EngineMsg::AuthCapabilityBound {
        token: sign_token.clone(),
        capability: AuthCapability::Signer,
        instance: signer_instance,
    });
    let send = core.handle(EngineMsg::AuthSignerCompleted(
        sign_token,
        Some(signer_instance),
        AuthSignerOutcome::Signed(signed),
    ));
    let (send_token, auth_event) = send
        .into_iter()
        .find_map(|effect| match effect {
            Effect::RelayAuth(AuthEffect::Send {
                token,
                epoch,
                event,
            }) => {
                assert_eq!(epoch.session, session);
                assert_eq!(epoch.handle, handle);
                Some((token, event))
            }
            _ => None,
        })
        .expect("signed AUTH requests an exact-generation send");
    core.handle(EngineMsg::AuthSendCompleted(
        send_token,
        AuthSendOutcome::Accepted,
    ));
    core.handle(EngineMsg::RelayFrame(
        handle,
        session,
        RelayFrame::from(RelayMessage::ok(auth_event.id, true, "authenticated")),
    ))
}

fn nip11_evidence(
    supported_nips: Option<Vec<u16>>,
) -> nmp_engine::relay_information::RelayInformationCapabilityEvidence {
    nip11_evidence_until(supported_nips, u64::MAX)
}

fn nip11_evidence_until(
    supported_nips: Option<Vec<u16>>,
    fresh_until: u64,
) -> nmp_engine::relay_information::RelayInformationCapabilityEvidence {
    nmp_engine::relay_information::RelayInformationCapabilityEvidence {
        supported_nips,
        document_revision: "test-revision".to_string(),
        fresh_until,
        last_error: None,
    }
}

fn mark_written<S: EventStore>(
    core: &mut EngineCore<S>,
    effects: &[Effect],
    relay: &RelayUrl,
) -> Vec<Effect> {
    let correlation = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PublishEvent(candidate, event, correlation)
                if &candidate.relay == relay
                    && candidate.access == AccessContext::Nip42(event.pubkey) =>
            {
                Some(*correlation)
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("expected a persisted scheduled publish for connected relay: {effects:?}")
        });
    core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written))
}

fn publish_private<S: EventStore>(
    core: &mut EngineCore<S>,
    author: &Keys,
    relays: impl IntoIterator<Item = RelayUrl>,
    sink: CapturingReceiptSink,
) -> (ReceiptId, nostr::Event, Vec<Effect>) {
    activate(core, author);
    let accepted = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(author, 85, "attempt-start failure")),
            durability: Durability::Durable,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new(relays),
            }),
            identity_override: None,
            correlation: None,
        },
        Box::new(sink),
    ));
    let (id, generation, unsigned) = find_sign_request(&accepted);
    let signed = unsigned.sign_with_keys(author).expect("sign fixture event");
    let effects = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Ok(signed.clone()),
    ));
    (id, signed, effects)
}

fn event_frame(sub: &str, event: nostr::Event) -> RelayFrame {
    RelayFrame::from(RelayMessage::event(SubscriptionId::new(sub), event))
}

fn eose_frame(sub: &str) -> RelayFrame {
    RelayFrame::from(RelayMessage::eose(SubscriptionId::new(sub)))
}

fn neg_msg_frame(sub: &str, message_hex: &str) -> RelayFrame {
    RelayFrame::from(RelayMessage::NegMsg {
        subscription_id: Cow::Owned(SubscriptionId::new(sub)),
        message: Cow::Owned(message_hex.to_string()),
    })
}

fn find_sign_request(effects: &[Effect]) -> (nmp_engine::core::ReceiptId, u64, UnsignedEvent) {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::RequestSign(id, generation, unsigned) => {
                Some((*id, *generation, unsigned.clone()))
            }
            _ => None,
        })
        .expect("expected a RequestSign effect")
}

fn all_row_deltas(effects: &[Effect]) -> Vec<&RowDelta> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitRows(_, rows, _) => Some(rows.iter()),
            _ => None,
        })
        .flatten()
        .collect()
}

#[path = "authentication.rs"]
mod authentication;
#[path = "live_queries.rs"]
mod live_queries;
#[path = "negentropy.rs"]
mod negentropy;
#[path = "persistence_failures.rs"]
mod persistence_failures;
#[path = "real_corpus_benchmark.rs"]
mod real_corpus_benchmark;
#[path = "state_maintenance.rs"]
mod state_maintenance;
#[path = "write_delivery.rs"]
mod write_delivery;
#[path = "write_scheduling.rs"]
mod write_scheduling;
#[path = "write_state.rs"]
mod write_state;
