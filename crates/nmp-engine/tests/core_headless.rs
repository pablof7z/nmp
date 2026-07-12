//! Headless `EngineCore` tests (M3 plan §5 tier A, re-expressed at the
//! `EngineCore` level per the M3-B build brief) + the coverage-attribution
//! ruling's falsifiers
//! (`docs/consults/2026-07-11-fable-coverage-attribution.md`). Zero I/O:
//! every "relay" interaction here is a scripted `EngineMsg::RelayConnected`/
//! `RelayFrame` fed directly to `EngineCore::handle`, exactly as the ruling's
//! own reasoning demands (send-time snapshots, the EOSE intersection rule,
//! `limit` poisoning, and per-query scoped acquisition evidence).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nmp_engine::core::{
    AcquisitionEvidence, Effect, EngineCore, EngineMsg, ReceiptId, RowDelta, RowSink,
    ShortfallFact, SourceEvidence, SourceStatus,
};
use nmp_engine::outbox::{ReceiptSink, WriteStatus};
use nmp_grammar::{
    AccessContext, Binding, ConcreteFilter, ContextualAtom, Durability, Filter, NarrowOnly,
    PrivateRoute, SourceAuthority, WriteIntent, WritePayload, WriteRouting,
};
use nmp_resolver::{HandleId, LiveQuery};
use nmp_router::{FixtureDirectory, SubId, WireOp};
use nmp_store::{
    AcceptOutcome, AcceptWrite, AttemptOutcome, ClaimSet, CompensateOutcome, CoverageInterval,
    CoverageKey, EventStore, FinishAttemptOutcome, GcReport, InsertOutcome, MemoryStore,
    PersistenceError, PromoteOutcome, RecoveredAttempt, RecoveredIntent, RecoveredReceipt,
    RecoveredRouteRevision, RedbStore, RelayObserved, RetractReason, StoredEvent,
};
use nmp_transport::{HandoffResult, RelayFrame, RelayHandle};
use nostr::{
    JsonUtil, Keys, Kind, RelayMessage, RelayUrl, SubscriptionId, Timestamp, UnsignedEvent,
};

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
    fn recover_outbox(&self) -> Vec<RecoveredIntent> {
        self.inner.recover_outbox()
    }
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError> {
        self.inner.reattach_receipt(receipt_id)
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
    fn start_attempt(
        &mut self,
        intent_id: nmp_store::IntentId,
        relay: RelayUrl,
        event: nostr::Event,
    ) -> Result<RecoveredAttempt, PersistenceError> {
        self.inner.start_attempt(intent_id, relay, event)
    }
    fn finish_attempt(
        &mut self,
        intent_id: nmp_store::IntentId,
        relay: &RelayUrl,
        ordinal: u64,
        outcome: AttemptOutcome,
    ) -> Result<FinishAttemptOutcome, PersistenceError> {
        if self.fail_next_attempt_finish {
            self.fail_next_attempt_finish = false;
            return Err(PersistenceError("injected attempt finish failure".into()));
        }
        self.inner
            .finish_attempt(intent_id, relay, ordinal, outcome)
    }
    fn recover_attempts(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }
    fn accept_ephemeral(
        &mut self,
        frozen_id: nostr::EventId,
        expected_pubkey: nostr::PublicKey,
    ) -> Result<u64, PersistenceError> {
        self.inner.accept_ephemeral(frozen_id, expected_pubkey)
    }
}

#[derive(Clone)]
struct SharedFailStartStore {
    inner: Arc<Mutex<MemoryStore>>,
    failed_relays: Arc<Mutex<BTreeSet<RelayUrl>>>,
}

impl SharedFailStartStore {
    fn new(failed_relays: impl IntoIterator<Item = RelayUrl>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MemoryStore::new())),
            failed_relays: Arc::new(Mutex::new(failed_relays.into_iter().collect())),
        }
    }
}

impl EventStore for SharedFailStartStore {
    fn insert(
        &mut self,
        event: nostr::Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        self.inner.lock().unwrap().insert(event, from)
    }
    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.lock().unwrap().query(filter)
    }
    fn remove(
        &mut self,
        id: nostr::EventId,
        reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        self.inner.lock().unwrap().remove(id, reason)
    }
    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.lock().unwrap().expire_due(now)
    }
    fn next_expiration(&self) -> Option<Timestamp> {
        self.inner.lock().unwrap().next_expiration()
    }
    fn record_coverage(
        &mut self,
        atom: &nmp_grammar::ContextualAtom,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) -> Result<(), PersistenceError> {
        self.inner
            .lock()
            .unwrap()
            .record_coverage(atom, relay, proven)
    }
    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        self.inner.lock().unwrap().get_coverage(key, relay)
    }
    fn gc(&mut self, claims: &ClaimSet) -> Result<GcReport, PersistenceError> {
        self.inner.lock().unwrap().gc(claims)
    }
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        self.inner.lock().unwrap().accept_write(accept)
    }
    fn promote_signed(
        &mut self,
        intent_id: nmp_store::IntentId,
        sig: nostr::secp256k1::schnorr::Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner.lock().unwrap().promote_signed(intent_id, sig)
    }
    fn compensate_write(
        &mut self,
        intent_id: nmp_store::IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.lock().unwrap().compensate_write(intent_id)
    }
    fn recover_outbox(&self) -> Vec<RecoveredIntent> {
        self.inner.lock().unwrap().recover_outbox()
    }
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError> {
        self.inner.lock().unwrap().reattach_receipt(receipt_id)
    }
    fn record_route_revision(
        &mut self,
        intent_id: nmp_store::IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        self.inner
            .lock()
            .unwrap()
            .record_route_revision(intent_id, relays)
    }
    fn recover_route_revisions(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
        self.inner
            .lock()
            .unwrap()
            .recover_route_revisions(intent_id)
    }
    fn start_attempt(
        &mut self,
        intent_id: nmp_store::IntentId,
        relay: RelayUrl,
        event: nostr::Event,
    ) -> Result<RecoveredAttempt, PersistenceError> {
        if self.failed_relays.lock().unwrap().contains(&relay) {
            return Err(PersistenceError("injected attempt start failure".into()));
        }
        self.inner
            .lock()
            .unwrap()
            .start_attempt(intent_id, relay, event)
    }
    fn finish_attempt(
        &mut self,
        intent_id: nmp_store::IntentId,
        relay: &RelayUrl,
        ordinal: u64,
        outcome: AttemptOutcome,
    ) -> Result<FinishAttemptOutcome, PersistenceError> {
        self.inner
            .lock()
            .unwrap()
            .finish_attempt(intent_id, relay, ordinal, outcome)
    }
    fn recover_attempts(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        self.inner.lock().unwrap().recover_attempts(intent_id)
    }
    fn accept_ephemeral(
        &mut self,
        frozen_id: nostr::EventId,
        expected_pubkey: nostr::PublicKey,
    ) -> Result<u64, PersistenceError> {
        self.inner
            .lock()
            .unwrap()
            .accept_ephemeral(frozen_id, expected_pubkey)
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
    fn start_attempt(
        &mut self,
        intent_id: nmp_store::IntentId,
        relay: RelayUrl,
        event: nostr::Event,
    ) -> Result<RecoveredAttempt, PersistenceError> {
        if self.failed_relays.contains(&relay) {
            return Err(PersistenceError("injected attempt start failure".into()));
        }
        self.inner.start_attempt(intent_id, relay, event)
    }
    fn finish_attempt(
        &mut self,
        intent_id: nmp_store::IntentId,
        relay: &RelayUrl,
        ordinal: u64,
        outcome: AttemptOutcome,
    ) -> Result<FinishAttemptOutcome, PersistenceError> {
        self.inner
            .finish_attempt(intent_id, relay, ordinal, outcome)
    }
    fn recover_attempts(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
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
                if r == relay {
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

fn wire_sub_string(sub_id: &SubId) -> String {
    format!("{}", sub_id.1)
}

fn connect(core: &mut EngineCore<MemoryStore>, slot: u32, url: &RelayUrl) -> Vec<Effect> {
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot,
            generation: 1,
        },
        url.clone(),
    ))
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
    RelayFrame::Text(RelayMessage::event(SubscriptionId::new(sub), event).as_json())
}

fn eose_frame(sub: &str) -> RelayFrame {
    RelayFrame::Text(RelayMessage::eose(SubscriptionId::new(sub)).as_json())
}

// ---- test 1 analog: subscribe -> Wire; ingest -> Wire + EmitRows --------

#[test]
fn subscribe_opens_wire_for_resolved_demand() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));

    let (_sub_id, filter) = req_for(&effects, &relay0);
    assert_eq!(filter, &cf(&[1], &[&a.public_key().to_hex()]));
}

#[test]
fn ingest_frame_recompiles_wire_and_emits_rows() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(b.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    connect(&mut core, 0, &relay0);

    // $myFollows shape: kinds:[1], authors := Derived(inner=kind:3 by me,
    // project=#p) -- exactly nmp-resolver's M1 contract-test shape.
    let my_follows = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(nmp_grammar::Derived {
            inner: nmp_grammar::Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                ..Filter::default()
            }),
            project: nmp_grammar::Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    });

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::SetActivePubkey(Some(a.public_key())));
    let _ = core.handle(EngineMsg::Subscribe(my_follows, Box::new(sink.clone())));

    // B's kind:1 post arrives UNSOLICITED (before B is ever followed) --
    // the store holds it, but it matches no handle's root atoms yet.
    let b_post = nmp_resolver::testkit::kind1(&b, "hello from b", 50);
    let pre_effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", b_post.clone()),
    ));
    assert!(
        !pre_effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _) if !rows.is_empty())),
        "b's post must not be visible before b is followed"
    );

    // Now `a` follows `b`: root atoms fan out to include {kind:1,
    // authors:{b}} -- demand changes (Wire opens b's write relay) AND the
    // handle's row set changes (b's pre-existing post is now in scope).
    let contact_list = nmp_resolver::testkit::kind3(&a, &[b.public_key()], 100);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", contact_list),
    ));

    assert!(
        effects.iter().any(|e| matches!(e, Effect::Wire(_))),
        "ingest must recompile and open the new author's atom on the wire"
    );
    let emitted = effects.iter().find_map(|e| match e {
        Effect::EmitRows(_, rows, _) => Some(rows),
        _ => None,
    });
    let rows = emitted.expect("ingest must emit rows for the affected handle");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].event().map(|e| e.id),
        Some(b_post.id),
        "the single delta must be an Added(b_post), never a Removed or a re-delivered full set"
    );

    // The sink was also called synchronously with the same rows.
    let captured = sink.0.lock().unwrap();
    assert!(captured
        .iter()
        .any(|batch| batch.len() == 1 && batch[0].event().map(|e| e.id) == Some(b_post.id)));
}

// ---- P0 load test (docs/known-gaps.md): redelivery must be O(distinct
// rows), never O(rows^2) --------------------------------------------------

/// The falsifier for the P0 dogfooding bug: before the `RowDelta::Added`/
/// `Removed` delta fix, `EngineCore::refresh_handle` re-emitted the FULL
/// current row set on every single ingested event (because
/// `rows_and_coverage_for` always recomputed -- and `EmitRows` always
/// carried -- every currently-matching row, not just what changed). N
/// distinct matching events therefore delivered ~N*(N+1)/2 total rows
/// across the run -- O(N^2) -- confirmed live against real relays as a
/// 635-1294x redelivery ratio (~3.35M raw row deliveries for ~2,587
/// distinct notes in 20s). This test subscribes once, then ingests N=2,000
/// distinct matching events ONE AT A TIME through the real
/// `EngineMsg::RelayFrame` ingest path (exactly what a live relay stream
/// does -- `on_relay_frame`'s `Event` arm always calls `recompile` +
/// `refresh_all_handles`), and asserts the TOTAL number of row-delta
/// entries delivered across every `EmitRows` batch stays close to N (each
/// distinct row delivered ~once), nowhere near the O(N^2) blow-up the old
/// full-set-re-emit behavior produced. Bounded/deterministic: a fixed N,
/// no network, and a generous wall-clock ceiling so an O(N^2) regression
/// fails loudly instead of hanging.
#[test]
fn ingesting_n_distinct_events_delivers_order_n_row_entries_not_order_n_squared() {
    let start = Instant::now();
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(sink.clone()),
    ));

    const N: u64 = 2_000;
    let mut total_delta_entries = 0usize;
    for i in 0..N {
        let event = nmp_resolver::testkit::kind1(&a, &format!("load-test post #{i}"), 1_000 + i);
        let effects = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            event_frame("s", event),
        ));
        for effect in &effects {
            if let Effect::EmitRows(_, rows, _) = effect {
                total_delta_entries += rows.len();
            }
        }
    }

    // The fix must not have traded over-delivery for under-delivery: every
    // one of the N distinct events actually reaches the sink at least once
    // (as an `Added`), or this "load test" would be vacuous.
    let captured = sink.0.lock().unwrap();
    let distinct_delivered: BTreeSet<nostr::EventId> = captured
        .iter()
        .flatten()
        .filter_map(RowDelta::event)
        .map(|e| e.id)
        .collect();
    assert_eq!(
        distinct_delivered.len(),
        N as usize,
        "every one of the N distinct ingested events must be delivered at least once"
    );

    // THE falsifier: total delivered row-delta entries stays ~O(N) (a small
    // constant multiple covers the initial empty-subscribe batch and any
    // coverage-only re-emits), nowhere near the O(N^2) blow-up a full-set
    // re-emit would produce (~N*(N+1)/2 = 2,001,000 for N=2,000 -- 500x+
    // this bound).
    let quadratic_blowup = (N * (N + 1)) / 2;
    assert!(
        total_delta_entries < (N as usize) * 2,
        "total delivered row-delta entries ({total_delta_entries}) must stay ~O(N) -- the \
         old full-set-re-emit bug would have delivered ~{quadratic_blowup} (O(N^2))"
    );

    assert!(
        start.elapsed() < Duration::from_secs(30),
        "load test must complete quickly -- an O(N^2) regression would blow this budget \
         (elapsed: {:?})",
        start.elapsed()
    );
}

// ---- #124: a demand's NIP-01 `limit:N` projects only the N newest rows ---

/// A literal-author query carrying an explicit NIP-01 `limit:N`.
fn limited_literal_query(kinds: &[u16], author_hex: &str, limit: usize) -> LiveQuery {
    LiveQuery::from_filter(Filter {
        kinds: Some(kinds.iter().copied().collect()),
        authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
        limit: Some(limit),
        ..Filter::default()
    })
}

/// Fold one delivered `RowDelta` batch into a running "current row set" of
/// event ids, exactly as an app consuming the reactive stream would.
fn apply_deltas(current: &mut BTreeSet<nostr::EventId>, batch: &[RowDelta]) {
    for delta in batch {
        match delta {
            RowDelta::Added(row) => {
                current.insert(row.event.id);
            }
            RowDelta::Removed(id) => {
                current.remove(id);
            }
            RowDelta::SourcesGrew { .. } => {}
        }
    }
}

/// (a) With M > N matching cached events, the handle projects EXACTLY the N
/// newest by `created_at` DESC (id ASC tie-break) -- never every cached
/// match. Feeds five kind:1 events (created_at 10..50) one at a time into a
/// `limit:3` handle and asserts the folded current set is precisely the three
/// newest, and that it never grew past N at any point along the way.
#[test]
fn limited_handle_projects_only_the_n_newest_of_m_matches() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        limited_literal_query(&[1], &a.public_key().to_hex(), 3),
        Box::new(sink.clone()),
    ));

    let mut ids_by_time: Vec<(u64, nostr::EventId)> = Vec::new();
    for created_at in [10u64, 20, 30, 40, 50] {
        let event = nmp_resolver::testkit::kind1(&a, &format!("note @{created_at}"), created_at);
        ids_by_time.push((created_at, event.id));
        let _ = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            event_frame("s", event),
        ));
    }

    // Replay the delivered stream; assert it never exceeds N mid-flight.
    let mut current = BTreeSet::new();
    let mut high_water = 0usize;
    for batch in sink.0.lock().unwrap().iter() {
        apply_deltas(&mut current, batch);
        high_water = high_water.max(current.len());
    }
    assert!(
        high_water <= 3,
        "a limit:3 handle must never accumulate more than 3 rows (peak was {high_water})"
    );

    let expected: BTreeSet<nostr::EventId> = ids_by_time
        .iter()
        .rev()
        .take(3)
        .map(|(_, id)| *id)
        .collect();
    assert_eq!(
        current, expected,
        "the projected set must be exactly the 3 newest (created_at 30/40/50), not all 5"
    );
}

/// (b) A newer matching event entering the top-N evicts the oldest of the N:
/// the ingest emits Added(new) + Removed(oldest) and the set stays at N,
/// proving the reactive DELTA path (not just a fresh snapshot) maintains the
/// window.
#[test]
fn newer_event_evicts_oldest_of_top_n_via_delta() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        limited_literal_query(&[1], &a.public_key().to_hex(), 2),
        Box::new(sink.clone()),
    ));

    let oldest = nmp_resolver::testkit::kind1(&a, "oldest", 100);
    let middle = nmp_resolver::testkit::kind1(&a, "middle", 200);
    for event in [oldest.clone(), middle.clone()] {
        let _ = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            event_frame("s", event),
        ));
    }

    // The top-2 is now {oldest, middle}. A strictly newer event arrives.
    let newest = nmp_resolver::testkit::kind1(&a, "newest", 300);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", newest.clone()),
    ));
    let batch = effects
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(_, rows, _) => Some(rows.clone()),
            _ => None,
        })
        .expect("the newer event must emit a row delta");

    assert!(
        batch
            .iter()
            .any(|d| matches!(d, RowDelta::Added(row) if row.event.id == newest.id)),
        "the newer event must be Added: {batch:?}"
    );
    assert!(
        batch
            .iter()
            .any(|d| matches!(d, RowDelta::Removed(id) if *id == oldest.id)),
        "the evicted oldest of the top-N must be Removed: {batch:?}"
    );
    assert!(
        !batch.iter().any(|d| d.id() == middle.id),
        "the surviving middle row must not churn (no delta for it): {batch:?}"
    );

    let mut current = BTreeSet::new();
    for b in sink.0.lock().unwrap().iter() {
        apply_deltas(&mut current, b);
    }
    assert_eq!(
        current,
        BTreeSet::from([middle.id, newest.id]),
        "the window must hold exactly the 2 newest after the churn"
    );
}

/// (c) Retracting a member of the current top-N pulls the next-newest
/// (previously excluded) match IN: the retraction emits Removed(retracted) +
/// Added(next-newest), and the set stays at N.
#[test]
fn retracting_top_n_member_pulls_in_next_newest() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        limited_literal_query(&[1], &a.public_key().to_hex(), 2),
        Box::new(sink.clone()),
    ));

    // Three matches; the top-2 is {second, third}, `first` is excluded.
    let first = nmp_resolver::testkit::kind1(&a, "first", 100);
    let second = nmp_resolver::testkit::kind1(&a, "second", 200);
    let third = nmp_resolver::testkit::kind1(&a, "third", 300);
    for event in [first.clone(), second.clone(), third.clone()] {
        let _ = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            event_frame("s", event),
        ));
    }
    {
        let mut current = BTreeSet::new();
        for b in sink.0.lock().unwrap().iter() {
            apply_deltas(&mut current, b);
        }
        assert_eq!(
            current,
            BTreeSet::from([second.id, third.id]),
            "precondition: the window holds the 2 newest, excluding `first`"
        );
    }

    // Retract `third` (a current top-N member) via a NIP-09 kind:5 delete.
    let deletion = nmp_resolver::testkit::deletion(&a, &[third.id], 400);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", deletion),
    ));
    let batch = effects
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(_, rows, _) => Some(rows.clone()),
            _ => None,
        })
        .expect("retracting a held row must emit a row delta");
    assert!(
        batch
            .iter()
            .any(|d| matches!(d, RowDelta::Removed(id) if *id == third.id)),
        "the retracted top-N member must be Removed: {batch:?}"
    );
    assert!(
        batch
            .iter()
            .any(|d| matches!(d, RowDelta::Added(row) if row.event.id == first.id)),
        "the next-newest previously-excluded match must be pulled IN as Added: {batch:?}"
    );

    let mut current = BTreeSet::new();
    for b in sink.0.lock().unwrap().iter() {
        apply_deltas(&mut current, b);
    }
    assert_eq!(
        current,
        BTreeSet::from([first.id, second.id]),
        "after retraction the window refills to the next 2 newest"
    );
}

/// (d) `limit: None` is unchanged -- every matching row is projected, with no
/// truncation.
#[test]
fn unlimited_handle_projects_every_match() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    // `literal_query` carries no limit (limit: None).
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(sink.clone()),
    ));

    let mut all_ids = BTreeSet::new();
    for created_at in [10u64, 20, 30, 40, 50] {
        let event = nmp_resolver::testkit::kind1(&a, &format!("note @{created_at}"), created_at);
        all_ids.insert(event.id);
        let _ = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            event_frame("s", event),
        ));
    }

    let mut current = BTreeSet::new();
    for b in sink.0.lock().unwrap().iter() {
        apply_deltas(&mut current, b);
    }
    assert_eq!(
        current, all_ids,
        "with no limit, every one of the 5 matching rows must be projected"
    );
}

// ---- test 2 analog: EOSE records a watermark; a bare EVENT never does ---

#[test]
fn eose_records_coverage_watermark_and_non_eose_does_not() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let atom = cf(&[3], &[&a.public_key().to_hex()]);
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[3], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (sub_id, _filter) = req_for(&effects, &relay0);
    let wire = wire_sub_string(sub_id);

    // A bare EVENT frame (no EOSE yet) must record nothing.
    let e = nmp_resolver::testkit::kind3(&a, &[], 10);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame(&wire, e),
    ));
    assert_eq!(
        core.get_coverage(&ctx_atom(atom.clone()), &relay0),
        None,
        "presence != coverage"
    );

    // The EOSE proves the (unfloored) window up to the engine clock.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire),
    ));

    let interval = core
        .get_coverage(&ctx_atom(atom.clone()), &relay0)
        .expect("EOSE must record a coverage row");
    assert_eq!(interval.from, Timestamp::from(0u64));
    assert_eq!(interval.through, Timestamp::from(500u64));
}

/// #118's headline falsifier (fixed ahead of #107): a `Demand` explicitly
/// declared `Public` over an author-bearing selection (#106's "new
/// expressible behavior" -- "these authors, generic facts only, no outbox
/// chase") is a genuinely DIFFERENT coverage identity than the SAME
/// selection under the static-default `AuthorOutboxes` guess. Proves
/// `get_coverage` now reads the atom's TRUE declared context: querying
/// under the correct (`Public`) context finds the recorded coverage;
/// querying under the static default's WRONG guess (`AuthorOutboxes`,
/// since the filter IS author-bearing) does not -- exactly the silent
/// re-alias #118 describes, now provably closed.
#[test]
fn get_coverage_distinguishes_true_context_from_the_static_default_guess() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let filter = cf(&[1], &[&a.public_key().to_hex()]);
    // A directory fact so the Public-sourced atom (classify() sends
    // `Public` straight to the pinned/directory lookup, never the outbox
    // solver) actually routes somewhere.
    let dir = FixtureDirectory::new().with_group_host(filter.clone(), relay0.clone());
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let demand = nmp_grammar::Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Literal(BTreeSet::from([a.public_key().to_hex()]))),
            ..Filter::default()
        },
        SourceAuthority::Public,
        AccessContext::Public,
    )
    .expect("Public over an author-bearing selection is legal (#106)");

    let effects = core.handle(EngineMsg::Subscribe(
        LiveQuery(demand),
        Box::new(CapturingSink::default()),
    ));
    let (sub_id, _f) = req_for(&effects, &relay0);
    let wire = wire_sub_string(sub_id);

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire),
    ));

    assert!(
        core.get_coverage(
            &ctx_atom_with(filter.clone(), SourceAuthority::Public),
            &relay0
        )
        .is_some(),
        "the TRUE declared context (Public) must find the recorded coverage"
    );
    assert!(
        core.get_coverage(&ctx_atom(filter), &relay0).is_none(),
        "the static-default's WRONG guess (AuthorOutboxes, since the filter is \
         author-bearing) must NOT find coverage recorded under a genuinely \
         different declared context"
    );
}

/// #107's core Done-when trio, exercised as one flow since they compose
/// naturally: (1) Agnostic pinned-R1 returns a matching cached R2-only row
/// while wire contacts only R1; (2) Strict pinned-R1 excludes that same row
/// until it is observed from R1 too; (6) same-filter Agnostic and Strict
/// handles remain distinct even though they share ONE wire subscription
/// (`AcquisitionKey` excludes `cache`, #106/#107's ratified shape -- two
/// handles differing ONLY in `cache` dedup onto the identical graph node/
/// wire/coverage, per `nmp-resolver::Engine::subscribe`'s own doc).
#[test]
fn agnostic_and_strict_pinned_handles_project_distinct_rows_from_one_shared_wire() {
    let a = Keys::generate();
    let relay_other = RelayUrl::parse("wss://other.example.com").unwrap();
    let relay_pinned = RelayUrl::parse("wss://pinned.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay_other.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay_other);
    connect(&mut core, 1, &relay_pinned);

    // Seed the store: an ordinary AuthorOutboxes subscribe pulls the event
    // in from relay_other, giving it Row.sources == {relay_other}.
    let outbox_effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (outbox_sub, _f) = req_for(&outbox_effects, &relay_other);
    let outbox_wire = wire_sub_string(outbox_sub);
    let event = unsigned(&a, 1, "seeded via relay_other")
        .sign_with_keys(&a)
        .expect("sign fixture event");
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame(&outbox_wire, event.clone()),
    ));

    // Two NEW handles over the IDENTICAL selection, both declared
    // SourceAuthority::Pinned({relay_pinned}) -- the SAME AcquisitionKey --
    // but one Agnostic (the default), one Strict.
    let filter = Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([a.public_key().to_hex()]))),
        ..Filter::default()
    };
    let pinned_relays = BTreeSet::from([relay_pinned.clone()]);
    let agnostic_demand = nmp_grammar::Demand::new(
        filter,
        SourceAuthority::Pinned(pinned_relays),
        AccessContext::Public,
    )
    .expect("a nonempty pinned relay set is legal (#107)");
    let mut strict_demand = agnostic_demand.clone();
    strict_demand.cache = nmp_grammar::CacheMode::Strict;

    let effects_agnostic = core.handle(EngineMsg::Subscribe(
        LiveQuery(agnostic_demand),
        Box::new(CapturingSink::default()),
    ));

    // Wire contacts ONLY the declared pinned relay for this new atom --
    // never relay_other (no re-req there at all: nothing about that atom
    // changed), and (since this fixture directory configures no app/
    // fallback/indexer/group-host facts) there is nowhere else it even
    // COULD leak to.
    let (pinned_sub, _f) = req_for(&effects_agnostic, &relay_pinned);
    let pinned_wire = wire_sub_string(pinned_sub);
    assert!(
        !effects_agnostic.iter().any(|effect| matches!(
            effect,
            Effect::Wire(delta) if delta.ops.iter().any(|(r, _)| r == &relay_other)
        )),
        "an ExplicitPinned atom's subscribe must never recompile a Req/Close at any \
         relay but its own declared set"
    );
    assert!(
        all_row_deltas(&effects_agnostic)
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == event.id)),
        "Agnostic must return a matching cached row regardless of its recorded provenance"
    );

    // The Strict handle dedups onto the SAME graph/wire (no new Req at
    // relay_pinned), yet must NOT see the row: its provenance ({relay_other})
    // is disjoint from the pinned set ({relay_pinned}).
    let effects_strict = core.handle(EngineMsg::Subscribe(
        LiveQuery(strict_demand),
        Box::new(CapturingSink::default()),
    ));
    assert!(
        !effects_strict
            .iter()
            .any(|effect| matches!(effect, Effect::Wire(_))),
        "a Strict handle sharing the identical AcquisitionKey must dedup onto the \
         existing wire subscription, never open a second one"
    );
    assert!(
        !all_row_deltas(&effects_strict)
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == event.id)),
        "Strict must exclude a row whose recorded provenance is disjoint from the \
         pinned relay set"
    );

    // The SAME event now arrives from the pinned relay too: the Strict
    // handle must pick it up the instant its own provenance intersects the
    // pinned set, and the Agnostic handle (which already had it) must still
    // record the provenance growth -- both are the SAME underlying
    // `Row.sources` growing, projected differently per handle's `cache`.
    let after = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        event_frame(&pinned_wire, event.clone()),
    ));
    let deltas = all_row_deltas(&after);
    assert!(
        deltas.iter().any(|delta| matches!(
            delta,
            RowDelta::Added(row) if row.event.id == event.id && row.sources.contains(&relay_pinned)
        )),
        "the Strict handle must newly Add the row once its provenance includes the \
         pinned relay: {deltas:?}"
    );
    assert!(
        deltas.iter().any(|delta| matches!(
            delta,
            RowDelta::SourcesGrew { id, sources } if *id == event.id && sources.contains(&relay_pinned)
        )),
        "the Agnostic handle's already-visible row must still record the provenance \
         growth: {deltas:?}"
    );
}

/// #107's remaining Done-when trio item: "Equal filters pinned to R1 and R2
/// retain distinct row projections, evidence, EOSE facts, and teardown."
/// Unlike the Agnostic/Strict test above (same pinned set, different cache
/// mode, sharing ONE wire subscription), this is the OTHER axis: the
/// IDENTICAL filter pinned to two DIFFERENT relay sets is a genuinely
/// different `SourceAuthority::Pinned` value, hence a different
/// `AcquisitionKey` -- two fully independent handles, subs, and EOSE
/// watermarks, never sharing so much as a wire request.
#[test]
fn identical_filter_pinned_to_different_relays_stays_fully_independent() {
    let a = Keys::generate();
    let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
    let relay2 = RelayUrl::parse("wss://relay2.example.com").unwrap();
    let mut core = new_core(FixtureDirectory::new());
    connect(&mut core, 0, &relay1);
    connect(&mut core, 1, &relay2);

    let filter = Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([a.public_key().to_hex()]))),
        ..Filter::default()
    };
    let demand1 = nmp_grammar::Demand::new(
        filter.clone(),
        SourceAuthority::Pinned(BTreeSet::from([relay1.clone()])),
        AccessContext::Public,
    )
    .expect("nonempty pinned relay set is legal");
    let demand2 = nmp_grammar::Demand::new(
        filter,
        SourceAuthority::Pinned(BTreeSet::from([relay2.clone()])),
        AccessContext::Public,
    )
    .expect("nonempty pinned relay set is legal");

    let effects1 = core.handle(EngineMsg::Subscribe(
        LiveQuery(demand1),
        Box::new(CapturingSink::default()),
    ));
    let id1 = effects1
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(hid, ..) => Some(*hid),
            _ => None,
        })
        .expect("subscribe must emit an initial EmitRows for its own handle");
    let (sub1, _) = req_for(&effects1, &relay1);
    let wire1 = wire_sub_string(sub1);
    assert!(
        !effects1.iter().any(
            |e| matches!(e, Effect::Wire(delta) if delta.ops.iter().any(|(r, _)| r == &relay2))
        ),
        "demand1's Pinned({{relay1}}) atom must never touch relay2"
    );

    let effects2 = core.handle(EngineMsg::Subscribe(
        LiveQuery(demand2),
        Box::new(CapturingSink::default()),
    ));
    let id2 = effects2
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(hid, ..) => Some(*hid),
            _ => None,
        })
        .expect("subscribe must emit an initial EmitRows for its own handle");
    let (sub2, _) = req_for(&effects2, &relay2);
    let _wire2 = wire_sub_string(sub2);
    assert_ne!(
        id1, id2,
        "two distinct subscribe calls must yield distinct handles"
    );
    assert_ne!(
        sub1, sub2,
        "distinct pinned relay sets over an identical filter must never share a SubId"
    );
    assert!(
        !effects2.iter().any(
            |e| matches!(e, Effect::Wire(delta) if delta.ops.iter().any(|(r, _)| r == &relay1))
        ),
        "demand2's Pinned({{relay2}}) atom must never touch relay1 -- and must not even \
         re-touch relay1's already-open sub, since these are independent graph nodes"
    );

    // Distinct EOSE facts: only relay1's sub finishes -- handle1's OWN
    // relay1 entry advances; handle2's relay2 entry (a DIFFERENT handle
    // entirely) must stay unproven.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire1),
    ));
    let evidence1 = evidence_from(&effects, id1).expect("relay1's EOSE must refresh handle1");
    let r1 = source_for(evidence1, &relay1).expect("relay1 must be a source for handle1");
    assert_eq!(r1.reconciled_through, Some(Timestamp::from(10u64)));
    assert!(
        evidence_from(&effects, id2).is_none()
            || source_for(evidence_from(&effects, id2).unwrap(), &relay2)
                .is_none_or(|r2| r2.reconciled_through.is_none()),
        "handle2's relay2 entry must NOT advance off handle1's relay1 EOSE"
    );

    // Distinct teardown: unsubscribing handle1 closes ONLY relay1's sub;
    // handle2's relay2 subscription is untouched.
    let teardown = core.handle(EngineMsg::Unsubscribe(id1));
    let closed_relays: BTreeSet<RelayUrl> = teardown
        .iter()
        .filter_map(|e| match e {
            Effect::Wire(delta) => {
                Some(delta.ops.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>())
            }
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        closed_relays,
        BTreeSet::from([relay1]),
        "unsubscribing handle1 must close exactly relay1's sub, never touch relay2's"
    );
}

// ---- the EOSE-overwrite-race rule (ruling §2) ---------------------------

#[test]
fn eose_overwrite_race_credits_only_the_intersection() {
    let a = Keys::generate();
    let e_key = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(e_key.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    // First subscribe: sends REQ(sub, {authors:{a}}) -- snapshot1 absorbs
    // {h_a} only.
    let effects1 = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (sub_id, _f) = req_for(&effects1, &relay0);
    let sub_id = sub_id.clone();
    let wire = wire_sub_string(&sub_id);

    // Second subscribe (same skeleton, same relay): AuthorUnion widens the
    // SAME sub_id's filter to {a, e} -- an OVERWRITING REQ, snapshot2
    // absorbs {h_a, h_e}, pushed onto the SAME FIFO alongside snapshot1.
    let effects2 = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &e_key.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (sub_id2, filter2) = req_for(&effects2, &relay0);
    assert_eq!(sub_id2, &sub_id, "same skeleton must reuse the sub id");
    assert_eq!(
        filter2.authors,
        Some(BTreeSet::from([
            a.public_key().to_hex(),
            e_key.public_key().to_hex()
        ]))
    );

    // A straggler EOSE for the sub now arrives, while BOTH snapshots are
    // outstanding.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(100u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire),
    ));

    let atom_a = cf(&[1], &[&a.public_key().to_hex()]);
    let atom_e = cf(&[1], &[&e_key.public_key().to_hex()]);
    assert!(
        core.get_coverage(&ctx_atom(atom_a.clone()), &relay0)
            .is_some(),
        "a is in BOTH outstanding snapshots -- must be credited"
    );
    assert!(
        core.get_coverage(&ctx_atom(atom_e.clone()), &relay0)
            .is_none(),
        "e is only in the newer snapshot -- the straggler EOSE must NOT credit it"
    );

    // The next EOSE (for the newer, still-outstanding snapshot) credits e.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(200u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire),
    ));
    assert!(
        core.get_coverage(&ctx_atom(atom_e.clone()), &relay0)
            .is_some(),
        "the second EOSE must credit the still-outstanding snapshot's atoms"
    );
}

// ---- limit poisons coverage ----------------------------------------------

#[test]
fn limited_fetch_never_records_coverage() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let limited_query = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([a.public_key().to_hex()]))),
        limit: Some(500),
        ..Filter::default()
    });
    let effects = core.handle(EngineMsg::Subscribe(
        limited_query,
        Box::new(CapturingSink::default()),
    ));
    let (sub_id, filter) = req_for(&effects, &relay0);
    assert_eq!(filter.limit, Some(500));
    let wire = wire_sub_string(sub_id);

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire),
    ));

    let atom = cf(&[1], &[&a.public_key().to_hex()]);
    assert_eq!(
        core.get_coverage(&ctx_atom(atom.clone()), &relay0),
        None,
        "a limited REQ's EOSE must poison -- never record a watermark"
    );
}

// ---- per-source acquisition evidence (docs/design/
// scoped-evidence-49-12-plan.md §2/§3, folding #12 into #49) -------------

/// Find `relay`'s [`SourceEvidence`] entry, if any, inside `evidence`.
fn source_for<'a>(
    evidence: &'a AcquisitionEvidence,
    relay: &RelayUrl,
) -> Option<&'a SourceEvidence> {
    evidence.sources.iter().find(|s| &s.relay == relay)
}

fn evidence_from(effects: &[Effect], id: HandleId) -> Option<&AcquisitionEvidence> {
    effects.iter().find_map(|e| match e {
        Effect::EmitRows(hid, _, ev) if *hid == id => Some(ev),
        _ => None,
    })
}

#[test]
fn zero_atom_query_reports_no_resolved_demand_instead_of_vacuous_evidence() {
    let mut core = new_core(FixtureDirectory::new());
    let unresolved = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([9999u16])),
        authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
        ..Filter::default()
    });

    let effects = core.handle(EngineMsg::Subscribe(
        unresolved,
        Box::new(CapturingSink::default()),
    ));
    let evidence = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(_, _, evidence) => Some(evidence),
            _ => None,
        })
        .expect("a new subscription must emit its initial evidence");

    assert!(evidence.sources.is_empty());
    assert_eq!(evidence.shortfall, vec![ShortfallFact::NoResolvedDemand]);
}

#[test]
fn resolved_atom_without_a_planned_relay_reports_no_planned_source() {
    let a = Keys::generate();
    let atom = cf(&[9999], &[&a.public_key().to_hex()]);
    let mut core = new_core(FixtureDirectory::new());

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[9999], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let evidence = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(_, _, evidence) => Some(evidence),
            _ => None,
        })
        .expect("a new subscription must emit its initial evidence");

    assert!(evidence.sources.is_empty());
    assert_eq!(
        evidence.shortfall,
        vec![ShortfallFact::NoPlannedSource { atom }]
    );
}

#[test]
fn equal_evidence_on_reconnect_does_not_spuriously_emit_rows() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://stable-evidence.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);

    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[9999], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let first_connect = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 7,
            generation: 1,
        },
        relay.clone(),
    ));
    assert!(
        first_connect
            .iter()
            .any(|effect| matches!(effect, Effect::EmitRows(..))),
        "Connecting -> Requesting is a real evidence change"
    );

    let unchanged_reconnect = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 7,
            generation: 2,
        },
        relay,
    ));
    assert!(
        unchanged_reconnect
            .iter()
            .all(|effect| !matches!(effect, Effect::EmitRows(..))),
        "deterministically equal source evidence must not produce a duplicate row batch"
    );
}

#[test]
fn surviving_handle_evidence_tracks_plan_changes_from_other_handle_lifetimes() {
    let a = Keys::generate();
    let b = Keys::generate();
    let r1 = RelayUrl::parse("wss://r1.example.com").unwrap();
    let r2 = RelayUrl::parse("wss://r2.example.com").unwrap();
    let r3 = RelayUrl::parse("wss://r3.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [r2.clone(), r3.clone()])
        .with_write(b.public_key().to_hex(), [r1.clone(), r2.clone()]);
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(dir), 2);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[9999], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let a_id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, _, _) => Some(*id),
            _ => None,
        })
        .unwrap();
    let a_initial = evidence_from(&effects, a_id).unwrap();
    assert_eq!(
        a_initial
            .sources
            .iter()
            .map(|source| source.relay.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([r2.clone(), r3.clone()])
    );

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[9999], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let b_id = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitRows(id, _, _) if *id != a_id => Some(*id),
            _ => None,
        })
        .next()
        .expect("the second subscription must emit its own initial batch");
    let a_while_b_is_live = evidence_from(&effects, a_id)
        .expect("adding B changes A's capped current plan and must refresh A");
    assert_eq!(
        a_while_b_is_live
            .sources
            .iter()
            .map(|source| source.relay.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([r2.clone()]),
        "the shared r2 plus lexicographically earlier r1 exhaust the cap while B is live"
    );

    let effects = core.handle(EngineMsg::Unsubscribe(b_id));
    let a_after_b_is_removed = evidence_from(&effects, a_id)
        .expect("removing B frees cap for r3 and must refresh surviving A");
    assert_eq!(
        a_after_b_is_removed
            .sources
            .iter()
            .map(|source| source.relay.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([r2, r3])
    );
}

/// The direct #12 fix falsifier: two independently-covering relays for the
/// SAME query never collapse into one verdict -- each relay's own proof (or
/// lack of it) is visible on its own `SourceEvidence` entry. Replaces the
/// deleted `QueryCoverage::CompleteUpTo`/`Unknown` unanimity test: there is
/// no aggregate here for either relay to jointly satisfy or fail.
#[test]
fn per_source_evidence_reflects_each_relays_own_proof_independently() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone(), relay1.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);
    connect(&mut core, 1, &relay1);

    let sink = CapturingSink::default();
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(sink.clone()),
    ));
    let id = effects
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(hid, ..) => Some(*hid),
            _ => None,
        })
        .expect("subscribe must emit an initial EmitRows for its own handle");
    let (sub0, _) = req_for(&effects, &relay0);
    let (sub1, _) = req_for(&effects, &relay1);
    let wire0 = wire_sub_string(sub0);
    let wire1 = wire_sub_string(sub1);

    // Only relay0 finishes: its OWN source flips to a proven watermark;
    // relay1's source stays unproven -- independently, no joint verdict.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire0),
    ));
    let evidence = evidence_from(&effects, id).expect("watermark advance must emit EmitRows");
    let r0 = source_for(evidence, &relay0).expect("relay0 must be a source");
    assert_eq!(r0.reconciled_through, Some(Timestamp::from(10u64)));
    let r1 = source_for(evidence, &relay1).expect("relay1 must be a source");
    assert_eq!(
        r1.reconciled_through, None,
        "relay1 has proven nothing yet -- its OWN entry must say so independently of relay0"
    );

    // relay1 also finishes: NOW its own entry advances too, still separate.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(20u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        eose_frame(&wire1),
    ));
    let evidence = evidence_from(&effects, id).expect("watermark advance must emit EmitRows");
    let r1 = source_for(evidence, &relay1).expect("relay1 must be a source");
    assert_eq!(r1.reconciled_through, Some(Timestamp::from(20u64)));
}

/// #12's own falsifier, reshaped for the deleted-collapse model: a
/// `Derived` query ($myFollows shape) whose OUTER atom (kind:1 by the
/// followed author) has a proven coverage row, while the INNER atom (kind:3
/// -- the follow list itself, by the active identity) has none. The old
/// `query_coverage` consulted `root_atoms` ONLY, so the inner atom was
/// invisible to it and the query could report itself `CompleteUpTo` while
/// the follow-list expansion was entirely unproven. Under
/// `AcquisitionEvidence` (built over `subtree_atoms`, #12), the inner atom's
/// covering relay is its OWN source entry, unproven independently of the
/// outer relay's proof -- no field anywhere implies the feed is settled.
#[test]
fn derived_query_evidence_surfaces_the_unproven_inner_atom_independently_of_the_outer() {
    let a = Keys::generate();
    let b = Keys::generate();
    // relay0 hosts `a`'s own kind:3 (the inner/follow-list atom); relay1
    // hosts `b`'s kind:1 posts (the outer/root atom, once `a` follows `b`).
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(b.public_key().to_hex(), [relay1.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);
    connect(&mut core, 1, &relay1);

    let my_follows = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(nmp_grammar::Derived {
            inner: nmp_grammar::Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                ..Filter::default()
            }),
            project: nmp_grammar::Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    });

    let _ = core.handle(EngineMsg::SetActivePubkey(Some(a.public_key())));
    let effects = core.handle(EngineMsg::Subscribe(
        my_follows,
        Box::new(CapturingSink::default()),
    ));
    let id = effects
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(hid, ..) => Some(*hid),
            _ => None,
        })
        .expect("subscribe must emit an initial EmitRows for its own handle");
    // Only the inner atom (kind:3 by `a`) is resolvable at subscribe time --
    // the outer author set is still empty (no wildcard), so relay0 is the
    // only wire sub open right now.
    let (sub0, _) = req_for(&effects, &relay0);
    let wire0 = wire_sub_string(sub0);

    // `a` follows `b`: the outer atom {kind:1, authors:{b}} now resolves and
    // opens relay1.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10u64)));
    let contact_list = nmp_resolver::testkit::kind3(&a, &[b.public_key()], 10);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame(&wire0, contact_list),
    ));
    let (sub1, _) = req_for(&effects, &relay1);
    let wire1 = wire_sub_string(sub1);

    // The OUTER atom's relay (relay1) proves its window; the INNER atom's
    // relay (relay0, the follow-list itself) never gets an EOSE.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(20u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        eose_frame(&wire1),
    ));
    let evidence = evidence_from(&effects, id).expect("watermark advance must emit EmitRows");
    let outer = source_for(evidence, &relay1).expect("relay1 (outer) must be a source");
    assert_eq!(
        outer.reconciled_through,
        Some(Timestamp::from(20u64)),
        "the outer atom's own relay proved its own window"
    );
    let inner = source_for(evidence, &relay0).expect(
        "relay0 (the INNER kind:3 atom's covering relay) must be PRESENT in evidence.sources -- \
         the whole point of #12 is that interior atoms are consulted, never invisible",
    );
    assert_eq!(
        inner.reconciled_through, None,
        "the inner atom (the follow-list itself) has proven nothing -- no source anywhere may \
         imply this feed is settled while the follow-list expansion is unproven"
    );

    // Now the inner atom's own EOSE arrives: ONLY relay0's entry flips.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(30u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire0),
    ));
    let evidence = evidence_from(&effects, id).expect("watermark advance must emit EmitRows");
    let inner = source_for(evidence, &relay0).expect("relay0 must still be a source");
    assert_eq!(inner.reconciled_through, Some(Timestamp::from(30u64)));
}

/// The orthogonality proof (docs/design/scoped-evidence-49-12-plan.md Q3):
/// a relay's durable watermark and its current link status are
/// INDEPENDENT fields, never one enum. A source that proved its window and
/// then dropped must keep reporting BOTH facts in the SAME snapshot --
/// `reconciled_through: Some(_)` (the #49 "offline cached rows remain
/// usable" acceptance criterion) AND `status: Disconnected`, simultaneously.
#[test]
fn source_watermark_survives_disconnect_alongside_the_disconnected_status() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let id = effects
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(hid, ..) => Some(*hid),
            _ => None,
        })
        .expect("subscribe must emit an initial EmitRows for its own handle");
    let (sub0, _) = req_for(&effects, &relay0);
    let wire0 = wire_sub_string(sub0);

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        eose_frame(&wire0),
    ));
    let evidence = evidence_from(&effects, id).expect("watermark advance must emit EmitRows");
    let r0 = source_for(evidence, &relay0).expect("relay0 must be a source");
    assert_eq!(r0.reconciled_through, Some(Timestamp::from(10u64)));
    assert_eq!(r0.status, SourceStatus::Requesting);

    // relay0 drops. Its watermark must survive; its status must flip.
    let effects = core.handle(EngineMsg::RelayDisconnected(0));
    let evidence = evidence_from(&effects, id).expect("a link-status flip must emit EmitRows");
    let r0 = source_for(evidence, &relay0).expect("relay0 must still be a source");
    assert_eq!(
        r0.reconciled_through,
        Some(Timestamp::from(10u64)),
        "the prior watermark must survive a disconnect -- offline cached rows remain usable"
    );
    assert_eq!(
        r0.status,
        SourceStatus::Disconnected,
        "the link status must independently reflect the drop"
    );
}

// ---- set-active-pubkey re-root ------------------------------------------

#[test]
fn set_active_pubkey_reroots_and_recompiles() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay_a = RelayUrl::parse("wss://relay-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://relay-b.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay_a.clone()])
        .with_write(b.public_key().to_hex(), [relay_b.clone()]);
    let mut core = new_core(dir);

    let whoami = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([0u16])),
        authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
        ..Filter::default()
    });

    let _ = core.handle(EngineMsg::SetActivePubkey(Some(a.public_key())));
    let effects = core.handle(EngineMsg::Subscribe(
        whoami,
        Box::new(CapturingSink::default()),
    ));
    req_for(&effects, &relay_a); // demand is currently for `a`.

    let effects = core.handle(EngineMsg::SetActivePubkey(Some(b.public_key())));
    let closed_a = effects.iter().any(|e| {
        matches!(e, Effect::Wire(d) if d.ops.iter().any(|(r, ops)| r == &relay_a && ops.iter().any(|op| matches!(op, WireOp::Close(_)))))
    });
    assert!(closed_a, "re-root must close a's demand");
    req_for(&effects, &relay_b); // and open b's.
}

// ---- write outbox (M3 plan §5 tests 4, 5, 11) ---------------------------

fn find_sign_request(effects: &[Effect]) -> (nmp_engine::core::ReceiptId, u64, UnsignedEvent) {
    effects
        .iter()
        .find_map(|e| match e {
            Effect::RequestSign(id, generation, u) => Some((*id, *generation, u.clone())),
            _ => None,
        })
        .expect("expected a RequestSign effect")
}

/// Test 4 analog: `enqueue_is_not_converged` (ledger #9). A durable
/// publish's FIRST status is `Accepted`, never a terminal; an `Ephemeral`
/// intent gets a receipt-only record (still fires onto the wire once
/// signed, but never gains a pending row); an `AtMostOnce` intent sends exactly once and a relay dropping
/// before it acks never produces a retry `PublishEvent`.
#[test]
fn enqueue_is_not_converged() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    connect(&mut core, 0, &relay0);

    // -- Durable: first status is Accepted, never a bool/terminal. --
    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "durable write")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(sink.clone()),
    ));
    assert!(
        matches!(
            effects.first(),
            Some(Effect::EmitReceipt(_, WriteStatus::Accepted))
        ),
        "the first emitted status for a durable publish must be Accepted, never a terminal"
    );
    assert_eq!(sink.0.lock().unwrap().first(), Some(&WriteStatus::Accepted));

    // -- Ephemeral: receipt-only, no durable delivery obligation. --
    let eph_sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 2, "ephemeral write")),
            durability: Durability::Ephemeral,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(eph_sink.clone()),
    ));
    assert!(matches!(
        effects.first(),
        Some(Effect::EmitReceipt(_, WriteStatus::Accepted))
    ));
    assert_eq!(
        eph_sink.0.lock().unwrap().as_slice(),
        [WriteStatus::Accepted]
    );
    let (eph_id, eph_generation, eph_unsigned) = find_sign_request(&effects);
    let eph_signed = eph_unsigned.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        eph_id,
        eph_generation,
        Ok(eph_signed),
    ));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(r, _, _) if r == &relay0)),
        "an ephemeral write is fire-and-forget -- it still reaches the wire"
    );
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::EmitReceipt(_, WriteStatus::Signed(_)))));

    // -- AtMostOnce: sends exactly once; a dropped relay never retries. --
    let amo_sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 3, "at most once write")),
            durability: Durability::AtMostOnce,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(amo_sink.clone()),
    ));
    let (amo_id, amo_generation, amo_unsigned) = find_sign_request(&effects);
    let amo_signed = amo_unsigned.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        amo_id,
        amo_generation,
        Ok(amo_signed),
    ));
    let publish_count = effects
        .iter()
        .filter(|e| matches!(e, Effect::PublishEvent(r, _, _) if r == &relay0))
        .count();
    assert_eq!(publish_count, 1, "at-most-once sends exactly once");

    let effects = core.handle(EngineMsg::RelayDisconnected(0));
    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::EmitReceipt(rid, WriteStatus::GaveUp(r)) if *rid == amo_id && r == &relay0)
        ),
        "a relay dropping before it acks must surface as a terminal GaveUp"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(..))),
        "no retry Effect::PublishEvent after a failure -- no blind retry"
    );
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

#[test]
fn durable_pending_row_is_visible_before_signer_and_tamper_compensates() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    let row_sink = CapturingSink::default();
    core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(row_sink),
    ));

    let receipt_sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 10, "accepted body")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(receipt_sink.clone()),
    ));
    let (id, generation, accepted_template) = find_sign_request(&effects);
    let accepted_id = accepted_template.clone().sign_with_keys(&a).unwrap().id;
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == accepted_id)));
    assert!(matches!(
        receipt_sink.0.lock().unwrap().as_slice(),
        [WriteStatus::Accepted]
    ));

    let tampered = unsigned(&a, 10, "different signer output")
        .sign_with_keys(&a)
        .unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(tampered)));
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(event_id) if *event_id == accepted_id)));
    assert!(matches!(
        receipt_sink.0.lock().unwrap().last(),
        Some(WriteStatus::Failed(_))
    ));
}

#[test]
fn cancellation_restores_replaceable_predecessor_through_query_reactivity() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[0], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));

    let older_unsigned = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(1),
        Kind::Metadata,
        Vec::new(),
        "older",
    );
    let older = older_unsigned.sign_with_keys(&a).unwrap();
    core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(older.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(CapturingReceiptSink::default()),
    ));

    let newer_unsigned = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(2),
        Kind::Metadata,
        Vec::new(),
        "newer",
    );
    let newer_id = newer_unsigned.clone().sign_with_keys(&a).unwrap().id;
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(newer_unsigned),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
    let (newer_receipt, _, _) = find_sign_request(&effects);
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == newer_id)));
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == older.id)));

    let effects = core.handle(EngineMsg::CancelWrite(newer_receipt));
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == newer_id)));
    assert!(all_row_deltas(&effects)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == older.id)));
}

#[test]
fn signer_unavailable_keeps_accepted_row_visible() {
    let a = Keys::generate();
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &a);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "awaiting signer")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, template) = find_sign_request(&effects);
    let expected_id = template.sign_with_keys(&a).unwrap().id;
    let effects = core.handle(EngineMsg::SignerUnavailable(id, generation));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(rid, WriteStatus::AwaitingCapability) if *rid == id
    )));
    let fresh = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == expected_id)));
}

#[test]
fn ephemeral_is_receipt_only_and_never_creates_a_pending_row() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "ephemeral")),
            durability: Durability::Ephemeral,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(sink.clone()),
    ));
    assert!(matches!(
        effects.first(),
        Some(Effect::EmitReceipt(_, WriteStatus::Accepted))
    ));
    assert!(all_row_deltas(&effects).is_empty());
    let fresh = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(all_row_deltas(&fresh).is_empty());
}

#[test]
fn relay_rejection_after_promotion_does_not_retract_the_signed_row() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        relay.clone(),
    ));
    let signed = unsigned(&a, 1, "signed cache truth")
        .sign_with_keys(&a)
        .unwrap();
    core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(signed.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
    let rejected = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        RelayFrame::Text(RelayMessage::ok(signed.id, false, "policy rejection").as_json()),
    ));
    assert!(!all_row_deltas(&rejected)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == signed.id)));
    let fresh = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == signed.id)));
}

#[test]
fn cancelling_displaced_pending_then_newest_never_resurrects_cancelled_row() {
    let a = Keys::generate();
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &a);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[0], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));

    let base = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(1),
        Kind::Metadata,
        Vec::new(),
        "base",
    )
    .sign_with_keys(&a)
    .unwrap();
    core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(base.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(CapturingReceiptSink::default()),
    ));

    let middle = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(2),
        Kind::Metadata,
        Vec::new(),
        "middle",
    );
    let middle_id = middle.clone().sign_with_keys(&a).unwrap().id;
    let middle_effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(middle),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
    let (middle_receipt, _, _) = find_sign_request(&middle_effects);

    let newest = UnsignedEvent::new(
        a.public_key(),
        Timestamp::from(3),
        Kind::Metadata,
        Vec::new(),
        "newest",
    );
    let newest_id = newest.clone().sign_with_keys(&a).unwrap().id;
    let newest_effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(newest),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
    let (newest_receipt, _, _) = find_sign_request(&newest_effects);

    let older_cancel = core.handle(EngineMsg::CancelWrite(middle_receipt));
    assert!(!all_row_deltas(&older_cancel).iter().any(|delta| {
        matches!(delta, RowDelta::Removed(id) if *id == newest_id)
            || matches!(delta, RowDelta::Added(row) if row.event.id == middle_id)
    }));

    let newest_cancel = core.handle(EngineMsg::CancelWrite(newest_receipt));
    assert!(all_row_deltas(&newest_cancel)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == newest_id)));
    assert!(!all_row_deltas(&newest_cancel)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == middle_id)));
    let fresh = core.handle(EngineMsg::Subscribe(
        literal_query(&[0], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(all_row_deltas(&fresh).is_empty());
}

#[test]
fn expired_local_acceptance_is_first_and_only_failed_with_no_side_effects() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    core.handle(EngineMsg::Tick(Timestamp::from(200)));
    let expired = nmp_resolver::testkit::expiring_kind1(&a, "expired", 100, 150);
    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(expired),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(sink.clone()),
    ));
    assert!(matches!(
        effects.as_slice(),
        [Effect::EmitReceipt(_, WriteStatus::Failed(_))]
    ));
    assert!(matches!(
        sink.0.lock().unwrap().as_slice(),
        [WriteStatus::Failed(_)]
    ));
}

#[test]
fn exact_duplicate_intents_get_distinct_store_ids_and_one_promotion_advances_both() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://write.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    let template = unsigned(&a, 1, "same body");

    let first = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
    let (first_id, first_generation, first_template) = find_sign_request(&first);
    let second = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
    let (second_id, second_generation, second_template) = find_sign_request(&second);
    assert_ne!(
        first_id, second_id,
        "each accepted obligation owns one store id"
    );

    let signed = first_template.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        first_id,
        first_generation,
        Ok(signed.clone()),
    ));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteStatus::Signed(event_id))
            if *id == first_id && *event_id == signed.id
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteStatus::Signed(event_id))
            if *id == second_id && *event_id == signed.id
    )));

    // The co-owner was atomically promoted by the first completion; its
    // delayed signer result is ignored and cannot publish a second time.
    let delayed = second_template.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        second_id,
        second_generation,
        Ok(delayed),
    ));
    assert!(effects.is_empty());
}

#[test]
fn duplicate_coowners_keep_independent_routes_and_terminal_receipts() {
    let a = Keys::generate();
    let ack = RelayUrl::parse("wss://ack.example.com").unwrap();
    let nack = RelayUrl::parse("wss://nack.example.com").unwrap();
    let drop_relay = RelayUrl::parse("wss://drop.example.com").unwrap();
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &a);
    connect(&mut core, 0, &ack);
    connect(&mut core, 1, &nack);
    connect(&mut core, 2, &drop_relay);
    let template = unsigned(&a, 1, "same bytes, separate obligations");
    let sink_a = CapturingReceiptSink::default();
    let sink_b = CapturingReceiptSink::default();

    let first = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new([ack.clone(), drop_relay.clone()]),
            }),
        },
        Box::new(sink_a.clone()),
    ));
    let (id_a, generation_a, to_sign) = find_sign_request(&first);
    let second = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template),
            durability: Durability::Durable,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new([nack.clone()]),
            }),
        },
        Box::new(sink_b.clone()),
    ));
    let (id_b, _, _) = find_sign_request(&second);
    let signed = to_sign.sign_with_keys(&a).unwrap();
    let routed = core.handle(EngineMsg::SignerCompleted(
        id_a,
        generation_a,
        Ok(signed.clone()),
    ));
    assert!(routed
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(relay, _, _) if relay == &ack)));
    assert!(routed
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(relay, _, _) if relay == &drop_relay)));
    assert!(routed
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(relay, _, _) if relay == &nack)));

    let acked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        RelayFrame::Text(RelayMessage::ok(signed.id, true, "").as_json()),
    ));
    assert!(acked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteStatus::Acked(relay)) if *id == id_a && relay == &ack
    )));
    assert!(!acked
        .iter()
        .any(|effect| matches!(effect, Effect::EmitReceipt(id, _) if *id == id_b)));

    let nacked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        RelayFrame::Text(RelayMessage::ok(signed.id, false, "no").as_json()),
    ));
    assert!(nacked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteStatus::Rejected(relay, _)) if *id == id_b && relay == &nack
    )));

    let dropped = core.handle(EngineMsg::RelayDisconnected(2));
    assert!(dropped.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(id, WriteStatus::GaveUp(relay)) if *id == id_a && relay == &drop_relay
    )));
}

#[test]
fn relay_signature_satisfies_all_pending_coowners_and_late_signers_are_ignored() {
    let a = Keys::generate();
    let source = RelayUrl::parse("wss://source.example.com").unwrap();
    let out = RelayUrl::parse("wss://out.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [out.clone()]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    connect(&mut core, 0, &source);
    let template = unsigned(&a, 1, "relay wins signing race");
    let sink_a = CapturingReceiptSink::default();
    let sink_b = CapturingReceiptSink::default();
    let first = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(sink_a.clone()),
    ));
    let (id_a, generation_a, signer_a) = find_sign_request(&first);
    let second = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(template),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(sink_b.clone()),
    ));
    let (id_b, generation_b, signer_b) = find_sign_request(&second);
    let signed = signer_a.clone().sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("unsolicited", signed.clone()),
    ));
    for id in [id_a, id_b] {
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(receipt, WriteStatus::Signed(event_id))
                if *receipt == id && *event_id == signed.id
        )));
    }
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(relay, _, _) if relay == &out))
            .count(),
        2
    );
    assert!(core
        .handle(EngineMsg::SignerCompleted(
            id_a,
            generation_a,
            Ok(signer_a.sign_with_keys(&a).unwrap()),
        ))
        .is_empty());
    assert!(core
        .handle(EngineMsg::SignerCompleted(
            id_b,
            generation_b,
            Ok(signer_b.sign_with_keys(&a).unwrap()),
        ))
        .is_empty());
}

#[test]
fn repeated_signer_notifications_never_start_concurrent_operations() {
    let a = Keys::generate();
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &a);
    let sink = CapturingReceiptSink::default();
    let published = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "one operation")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, template) = find_sign_request(&published);
    assert!(core
        .handle(EngineMsg::SignerAttached(a.public_key()))
        .is_empty());
    assert!(core
        .handle(EngineMsg::SignerAttached(a.public_key()))
        .is_empty());

    core.handle(EngineMsg::SignerUnavailable(id, generation));
    let rearmed = core.handle(EngineMsg::SignerAttached(a.public_key()));
    assert_eq!(
        rearmed
            .iter()
            .filter(|effect| matches!(effect, Effect::RequestSign(..)))
            .count(),
        1
    );
    let (_, next_generation, _) = find_sign_request(&rearmed);
    assert!(next_generation > generation);
    let signed = template.sign_with_keys(&a).unwrap();
    assert!(core
        .handle(EngineMsg::SignerCompleted(
            id,
            generation,
            Ok(signed.clone())
        ))
        .is_empty());
    assert!(core
        .handle(EngineMsg::SignerAttached(a.public_key()))
        .is_empty());
    let completed = core.handle(EngineMsg::SignerCompleted(id, next_generation, Ok(signed)));
    assert!(completed.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(rid, WriteStatus::Signed(_)) if *rid == id
    )));
}

#[test]
fn compensation_persistence_failure_is_nonterminal_and_retryable() {
    let a = Keys::generate();
    let mut core = EngineCore::new(
        FailOnceCompensationStore::new(),
        Box::new(FixtureDirectory::new()),
        10,
    );
    activate(&mut core, &a);
    core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let sink = CapturingReceiptSink::default();
    let published = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "must remain pending")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, template) = find_sign_request(&published);
    let event_id = template.sign_with_keys(&a).unwrap().id;

    let failed_compensation = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Err(nmp_signer::SignerError::Unavailable),
    ));
    assert!(failed_compensation.is_empty(), "no terminal fact committed");
    assert_eq!(sink.0.lock().unwrap().as_slice(), [WriteStatus::Accepted]);
    let fresh = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(all_row_deltas(&fresh)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == event_id)));

    let retried = core.handle(EngineMsg::CancelWrite(id));
    assert!(retried.iter().any(
        |effect| matches!(effect, Effect::EmitReceipt(rid, WriteStatus::Failed(_)) if *rid == id)
    ));
    assert!(all_row_deltas(&retried)
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(removed) if *removed == event_id)));
}

/// #52 Q2 smoking gun: `EngineCore::on_publish` is the ONE place every
/// publish converges (FFI, direct-Rust, `nmp-bdd`'s `EngineThread`), so a
/// `WritePayload::Signed` whose content was tampered with after signing
/// (id/sig stale relative to the new content) must be rejected there,
/// before `WriteStatus::Accepted` is ever emitted and before any
/// `Effect::PublishEvent` is produced -- regardless of caller, with no FFI
/// verify layer anywhere in the loop.
#[test]
fn direct_publish_of_forged_signed_event_is_rejected_before_acceptance() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let genuine = unsigned(&a, 1, "genuine content")
        .sign_with_keys(&a)
        .unwrap();
    // Forge: reuse the genuine id/signature but swap in different content --
    // exactly the "reconstructed from caller-supplied fields verbatim"
    // shape the FFI boundary's own `signed_event_from_ffi` guards against,
    // now driven straight through `Handle::publish` with no FFI in the loop.
    let forged = nostr::Event::new(
        genuine.id,
        genuine.pubkey,
        genuine.created_at,
        genuine.kind,
        genuine.tags.clone(),
        "forged content -- attacker tampered after signing",
        genuine.sig,
    );
    assert!(
        forged.verify().is_err(),
        "test fixture sanity: the forged event must not verify"
    );

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(forged),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(sink.clone()),
    ));

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::EmitReceipt(_, WriteStatus::Failed(_))]
        ),
        "a forged Signed publish must terminate as the ONLY effect, as Failed -- got {effects:?}"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(..))),
        "a forged Signed publish must never produce Effect::PublishEvent"
    );
    let statuses = sink.0.lock().unwrap();
    assert!(
        matches!(statuses.as_slice(), [WriteStatus::Failed(_)]),
        "the sink must see Failed and nothing else -- never Accepted -- got {statuses:?}"
    );
}

/// Companion to the forged-event smoking gun: a properly-signed `Signed`
/// payload is unaffected by the acceptance-boundary verify and flows to
/// `Effect::PublishEvent` exactly as before -- no `RequestSign` (VISION P:
/// a caller that already holds a valid signature skips signing entirely).
#[test]
fn direct_publish_of_valid_signed_event_still_publishes() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let genuine = unsigned(&a, 1, "genuine content")
        .sign_with_keys(&a)
        .unwrap();

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Signed(genuine.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(sink.clone()),
    ));

    assert!(
        matches!(
            effects.first(),
            Some(Effect::EmitReceipt(_, WriteStatus::Accepted))
        ),
        "a valid Signed publish must still be Accepted first"
    );
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::RequestSign(..))),
        "an already-signed payload must never request the signer"
    );
    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::PublishEvent(r, ev, _) if r == &relay0 && ev.id == genuine.id)
        ),
        "a valid Signed publish must still reach the wire -- got {effects:?}"
    );
}

/// Test 5 analog: `private_route_fails_closed` (ledger #6). A
/// `PrivateNarrow` route whose relay set is empty (unroutable) fails CLOSED
/// with a typed `WriteStatus::Failed` -- it never reaches a public relay.
/// `NarrowOnly` exposes no widen/insert method by construction (compile-
/// level: there is no method this test -- or any caller -- could call to
/// grow the set after `NarrowOnly::new`).
#[test]
fn private_route_fails_closed() {
    let a = Keys::generate();
    // Deliberately empty directory: even if `PrivateNarrow` DID consult it
    // (it must not), there would be no public write relay to fall back to.
    let dir = FixtureDirectory::new();
    let mut core = new_core(dir);
    activate(&mut core, &a);

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "private dm")),
            durability: Durability::Durable,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new(std::iter::empty::<RelayUrl>()),
            }),
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, u) = find_sign_request(&effects);
    let signed = u.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));

    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(..))),
        "an unroutable private recipient must never reach ANY relay, public or otherwise"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(rid, WriteStatus::Failed(_)) if *rid == id)),
        "must fail CLOSED with a typed error, not silently drop the write"
    );
    assert!(matches!(
        sink.0.lock().unwrap().last(),
        Some(WriteStatus::Failed(_))
    ));
}

/// Test 11 analog: `write_ack_per_relay`. A durable publish to two relays,
/// one OKs and one NACKs -- the receipt stream reaches `Acked(R_ok)` and
/// `Rejected(R_bad, reason)` independently; "is it sent?" is only readable
/// from the stream, never a single bool.
#[test]
fn one_attempt_start_failure_is_owned_nonterminal_and_never_hits_the_wire() {
    let author = Keys::generate();
    let good = RelayUrl::parse("wss://persisted.example").unwrap();
    let blocked = RelayUrl::parse("wss://blocked.example").unwrap();
    let store = SharedFailStartStore::new([blocked.clone()]);
    let sink = CapturingReceiptSink::default();
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);

    let (id, _, effects) = publish_private(
        &mut core,
        &author,
        [good.clone(), blocked.clone()],
        sink.clone(),
    );
    assert!(effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(relay, _, _) if relay == &good)));
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(relay, _, _) if relay == &blocked)));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(receipt, WriteStatus::PersistenceBlocked(relay))
            if *receipt == id && relay == &blocked
    )));
    let replay = CapturingReceiptSink::default();
    assert!(core
        .reattach_receipt(id, Box::new(replay.clone()))
        .is_attached());
    assert!(replay
        .0
        .lock()
        .unwrap()
        .contains(&WriteStatus::PersistenceBlocked(blocked)));
}

// ---- issue #93: durable EVENT handoff -----------------------------------

/// `Sent` must never fire synchronously at enqueue time -- the moment this
/// call returns effects for a signed publish is not the same fact as
/// transport confirming the write. Only `EngineMsg::EventHandoff(_,
/// Written)` may ever produce it (asserted below by actually driving that
/// message and observing exactly one `Sent`).
#[test]
fn sent_never_fires_synchronously_and_only_written_handoff_produces_it() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(author.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);
    let sink = CapturingReceiptSink::default();

    let (id, _signed, effects) = publish_private(&mut core, &author, [relay.clone()], sink.clone());

    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(_, WriteStatus::Sent(_)))),
        "Sent must never fire synchronously at enqueue time, got {effects:?}"
    );
    assert!(
        !sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|s| matches!(s, WriteStatus::Sent(_))),
        "the sink must not have observed Sent before any handoff result arrives"
    );

    let correlation = effects
        .iter()
        .find_map(|e| match e {
            Effect::PublishEvent(r, _, c) if r == &relay => Some(*c),
            _ => None,
        })
        .expect("a PublishEvent effect must have been emitted for this relay");

    let reattached = CapturingReceiptSink::default();
    assert!(core
        .reattach_receipt(id, Box::new(reattached.clone()))
        .is_attached());
    assert!(
        !reattached
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|status| matches!(status, WriteStatus::Sent(_))),
        "a persisted Started row is pre-wire and must not replay as Sent"
    );

    let handoff_effects = core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    assert!(
        handoff_effects.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(receipt, WriteStatus::Sent(r)) if *receipt == id && r == &relay
        )),
        "a Written handoff must emit exactly one Sent, got {handoff_effects:?}"
    );
    assert!(sink
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|s| matches!(s, WriteStatus::Sent(r) if r == &relay)));
    assert!(reattached
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|s| matches!(s, WriteStatus::Sent(r) if r == &relay)));

    // The SAME correlation resolving a second time (a defensive duplicate
    // delivery, which transport itself never actually produces) must be a
    // complete no-op -- the correlation was already consumed above.
    let repeat = core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    assert!(
        repeat.is_empty(),
        "an already-resolved correlation must never re-fire Sent, got {repeat:?}"
    );
}

#[test]
fn ephemeral_observer_survives_until_every_handoff_result_then_sees_written_sent() {
    let author = Keys::generate();
    let relay_a = RelayUrl::parse("wss://ephemeral-a.example").unwrap();
    let relay_b = RelayUrl::parse("wss://ephemeral-b.example").unwrap();
    let mut core = new_core(FixtureDirectory::new());
    activate(&mut core, &author);
    let sink = CapturingReceiptSink::default();
    let accepted = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&author, 93, "ephemeral handoff")),
            durability: Durability::Ephemeral,
            routing: WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new([relay_a.clone(), relay_b.clone()]),
            }),
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, unsigned) = find_sign_request(&accepted);
    let signed = unsigned.sign_with_keys(&author).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    assert!(!sink
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|status| matches!(status, WriteStatus::Sent(_))));
    let correlation_for = |relay: &RelayUrl| {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::PublishEvent(found, _, correlation) if found == relay => Some(*correlation),
                _ => None,
            })
            .unwrap()
    };

    assert!(core
        .handle(EngineMsg::EventHandoff(
            correlation_for(&relay_a),
            HandoffResult::NotHandedOff,
        ))
        .is_empty());
    let written = core.handle(EngineMsg::EventHandoff(
        correlation_for(&relay_b),
        HandoffResult::Written,
    ));
    assert!(written.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(found, WriteStatus::Sent(relay))
            if *found == id && relay == &relay_b
    )));
    assert_eq!(
        sink.0
            .lock()
            .unwrap()
            .iter()
            .filter(|status| matches!(status, WriteStatus::Sent(relay) if relay == &relay_b))
            .count(),
        1
    );
}

/// `NotHandedOff`/`Ambiguous` are typed INTERNAL facts only (issue #93
/// scope): neither ever emits `WriteStatus::Sent`, any other receipt
/// status, or any effect at all -- #96 wires governed visibility, #95
/// wires the scheduler that acts on them; this unit's job stops at
/// correlating the fact correctly.
#[test]
fn not_handed_off_and_ambiguous_never_emit_any_receipt_status() {
    let author = Keys::generate();
    let relay_a = RelayUrl::parse("wss://relay-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://relay-b.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(
        author.public_key().to_hex(),
        [relay_a.clone(), relay_b.clone()],
    );
    let mut core = new_core(dir);
    let sink = CapturingReceiptSink::default();

    let (_id, _signed, effects) = publish_private(
        &mut core,
        &author,
        [relay_a.clone(), relay_b.clone()],
        sink.clone(),
    );
    let correlation_for = |relay: &RelayUrl| {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::PublishEvent(r, _, c) if r == relay => Some(*c),
                _ => None,
            })
            .expect("a PublishEvent effect must have been emitted for this relay")
    };

    let not_handed_off = core.handle(EngineMsg::EventHandoff(
        correlation_for(&relay_a),
        HandoffResult::NotHandedOff,
    ));
    assert!(
        not_handed_off.is_empty(),
        "NotHandedOff must produce no effects, got {not_handed_off:?}"
    );
    let ambiguous = core.handle(EngineMsg::EventHandoff(
        correlation_for(&relay_b),
        HandoffResult::Ambiguous,
    ));
    assert!(
        ambiguous.is_empty(),
        "Ambiguous must produce no effects, got {ambiguous:?}"
    );
    assert!(
        !sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|s| matches!(s, WriteStatus::Sent(_))),
        "neither NotHandedOff nor Ambiguous may ever surface as Sent"
    );
}

/// An `EventHandoff` for a correlation this reducer never minted (unknown,
/// or belonging to a different process entirely) is a structural no-op --
/// never a panic, never a stray effect.
#[test]
fn event_handoff_for_an_unknown_correlation_is_inert() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(author.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);
    let _ = publish_private(&mut core, &author, [relay], CapturingReceiptSink::default());

    let unknown = nmp_transport::AttemptCorrelation(u64::MAX);
    let effects = core.handle(EngineMsg::EventHandoff(unknown, HandoffResult::Written));
    assert!(effects.is_empty());
}

#[test]
fn all_attempt_start_failures_retain_every_lane_without_empty_terminal_sentinel() {
    let author = Keys::generate();
    let a = RelayUrl::parse("wss://blocked-a.example").unwrap();
    let b = RelayUrl::parse("wss://blocked-b.example").unwrap();
    let store = SharedFailStartStore::new([a.clone(), b.clone()]);
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    let sink = CapturingReceiptSink::default();

    let (id, _, effects) =
        publish_private(&mut core, &author, [a.clone(), b.clone()], sink.clone());
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(..)))
            .count(),
        0
    );
    let statuses = sink.0.lock().unwrap();
    assert!(statuses.contains(&WriteStatus::PersistenceBlocked(a.clone())));
    assert!(statuses.contains(&WriteStatus::PersistenceBlocked(b.clone())));
    drop(statuses);
    let replay = CapturingReceiptSink::default();
    assert!(core
        .reattach_receipt(id, Box::new(replay.clone()))
        .is_attached());
    let replayed = replay.0.lock().unwrap();
    assert!(replayed.contains(&WriteStatus::PersistenceBlocked(a)));
    assert!(replayed.contains(&WriteStatus::PersistenceBlocked(b)));
}

#[test]
fn ack_of_persisted_lane_does_not_terminalize_mixed_blocked_obligation() {
    let author = Keys::generate();
    let good = RelayUrl::parse("wss://ack-persisted.example").unwrap();
    let blocked = RelayUrl::parse("wss://still-blocked.example").unwrap();
    let store = SharedFailStartStore::new([blocked.clone()]);
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        good.clone(),
    ));
    let (id, signed, _) = publish_private(
        &mut core,
        &author,
        [good.clone(), blocked.clone()],
        CapturingReceiptSink::default(),
    );
    let acked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        RelayFrame::Text(RelayMessage::ok(signed.id, true, "").as_json()),
    ));
    assert!(acked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(receipt, WriteStatus::Acked(relay))
            if *receipt == id && relay == &good
    )));
    let replay = CapturingReceiptSink::default();
    assert!(core
        .reattach_receipt(id, Box::new(replay.clone()))
        .is_attached());
    assert!(replay
        .0
        .lock()
        .unwrap()
        .contains(&WriteStatus::PersistenceBlocked(blocked)));
}

#[test]
fn restart_rediscovers_unstarted_lane_and_persists_it_before_recovery_publish() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://recover-blocked.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("start-failure.redb");
    let receipt = {
        let mut first = EngineCore::new(
            RedbFailStartStore::open(&path, [relay.clone()]),
            Box::new(FixtureDirectory::new()),
            10,
        );
        let (id, _, effects) = publish_private(
            &mut first,
            &author,
            [relay.clone()],
            CapturingReceiptSink::default(),
        );
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        id
    };

    let mut still_blocked = EngineCore::new(
        RedbFailStartStore::open(&path, [relay.clone()]),
        Box::new(FixtureDirectory::new()),
        10,
    );
    assert!(still_blocked.recover_on_boot().is_empty());
    let replay = CapturingReceiptSink::default();
    assert!(still_blocked
        .reattach_receipt(receipt, Box::new(replay.clone()))
        .is_attached());
    assert!(replay
        .0
        .lock()
        .unwrap()
        .contains(&WriteStatus::PersistenceBlocked(relay.clone())));
    drop(still_blocked);

    let mut recovered = EngineCore::new(
        RedbFailStartStore::open(&path, []),
        Box::new(FixtureDirectory::new()),
        10,
    );
    let effects = recovered.recover_on_boot();
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(r, _, _) if r == &relay))
            .count(),
        1
    );
    drop(recovered);
    let store = RedbStore::open(&path).expect("inspect recovered redb");
    let intent = store.recover_outbox()[0].intent_id;
    let attempts = store.recover_attempts(intent).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].relay, relay);
    assert_eq!(attempts[0].outcome, AttemptOutcome::Started);
}

#[test]
fn author_outbox_failed_attempt_survives_restart_with_empty_directory() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://durable-author-route.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("author-route.redb");
    let receipt = {
        let directory =
            FixtureDirectory::new().with_write(author.public_key().to_hex(), [relay.clone()]);
        let mut core = EngineCore::new(
            RedbFailStartStore::open(&path, [relay.clone()]),
            Box::new(directory),
            10,
        );
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned(&author, 86, "dynamic author route")),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            },
            Box::new(CapturingReceiptSink::default()),
        ));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteStatus::PersistenceBlocked(r)) if r == &relay
        )));
        id
    };

    {
        let store = RedbStore::open(&path).unwrap();
        let intent = store.recover_outbox()[0].intent_id;
        let revisions = store.recover_route_revisions(intent).unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].relays, BTreeSet::from([relay.clone()]));
        assert!(store.recover_attempts(intent).unwrap().is_empty());
    }

    let mut recovered = EngineCore::new(
        RedbFailStartStore::open(&path, []),
        Box::new(FixtureDirectory::new()),
        10,
    );
    let effects = recovered.recover_on_boot();
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(r, _, _) if r == &relay))
            .count(),
        1
    );
    assert!(recovered
        .reattach_receipt(receipt, Box::new(CapturingReceiptSink::default()))
        .is_attached());
}

#[test]
fn inbox_route_removal_cannot_erase_durable_lane_and_new_revision_failure_is_volatile() {
    let author = Keys::generate();
    let recipient = Keys::generate();
    let old = RelayUrl::parse("wss://old-inbox.example").unwrap();
    let new = RelayUrl::parse("wss://new-inbox.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("inbox-route.redb");
    let receipt = {
        let directory =
            FixtureDirectory::new().with_read(recipient.public_key().to_hex(), [old.clone()]);
        let mut core = EngineCore::new(
            RedbFailStartStore::open(&path, [old.clone()]),
            Box::new(directory),
            10,
        );
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned(&author, 87, "dynamic inbox route")),
                durability: Durability::Durable,
                routing: WriteRouting::ToInboxes(vec![recipient.public_key()]),
            },
            Box::new(CapturingReceiptSink::default()),
        ));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        id
    };

    // Directory removal/replacement cannot subtract `old`. Failure to append
    // the newly resolved `new` revision blocks only that volatile lane; the
    // already-durable old obligation may still start and publish.
    {
        let changed =
            FixtureDirectory::new().with_read(recipient.public_key().to_hex(), [new.clone()]);
        let mut core = EngineCore::new(
            RedbFailStartStore::open_with_route_failure(&path),
            Box::new(changed),
            10,
        );
        let effects = core.recover_on_boot();
        let old_event = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::PublishEvent(relay, event, _) if relay == &old => Some(event.clone()),
                _ => None,
            })
            .expect("durable old lane publishes");
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(r, _, _) if r == &old)));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(r, _, _) if r == &new)));
        core.handle(EngineMsg::RelayConnected(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            old.clone(),
        ));
        let acked = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            RelayFrame::Text(RelayMessage::ok(old_event.id, true, "").as_json()),
        ));
        assert!(acked.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteStatus::Acked(r)) if r == &old
        )));
        let replay = CapturingReceiptSink::default();
        assert!(core
            .reattach_receipt(receipt, Box::new(replay.clone()))
            .is_attached());
        assert!(replay
            .0
            .lock()
            .unwrap()
            .contains(&WriteStatus::RoutePersistenceBlocked(new.clone())));
    }

    {
        let store = RedbStore::open(&path).unwrap();
        let intent = store.recover_outbox()[0].intent_id;
        let durable = store
            .recover_route_revisions(intent)
            .unwrap()
            .into_iter()
            .flat_map(|revision| revision.relays)
            .collect::<BTreeSet<_>>();
        assert_eq!(durable, BTreeSet::from([old.clone()]));
    }

    // Once a later boot can persist the changed revision, `new` starts. The
    // old lane is retained in route history but is already terminal (Acked),
    // so it is correctly not published again.
    let changed = FixtureDirectory::new().with_read(recipient.public_key().to_hex(), [new.clone()]);
    let mut core = EngineCore::new(RedbFailStartStore::open(&path, []), Box::new(changed), 10);
    let effects = core.recover_on_boot();
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(r, _, _) if r == &old)));
    assert!(effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(r, _, _) if r == &new)));
}

#[test]
fn route_revision_failure_emits_no_attempt_or_wire_and_claims_no_crash_durable_url() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://volatile-route.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("route-failure.redb");
    {
        let directory =
            FixtureDirectory::new().with_write(author.public_key().to_hex(), [relay.clone()]);
        let mut core = EngineCore::new(
            RedbFailStartStore::open_with_route_failure(&path),
            Box::new(directory),
            10,
        );
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned(&author, 88, "volatile route")),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            },
            Box::new(CapturingReceiptSink::default()),
        ));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteStatus::RoutePersistenceBlocked(r)) if r == &relay
        )));
    }
    let store = RedbStore::open(&path).unwrap();
    let intent = store.recover_outbox()[0].intent_id;
    assert!(store.recover_route_revisions(intent).unwrap().is_empty());
    assert!(store.recover_attempts(intent).unwrap().is_empty());
    drop(store);

    let mut recovered = EngineCore::new(
        RedbFailStartStore::open(&path, []),
        Box::new(FixtureDirectory::new()),
        10,
    );
    assert!(recovered.recover_on_boot().is_empty());
}

#[test]
fn write_ack_per_relay() {
    let a = Keys::generate();
    let relay_ok = RelayUrl::parse("wss://relay-ok.example.com").unwrap();
    let relay_bad = RelayUrl::parse("wss://relay-bad.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(
        a.public_key().to_hex(),
        [relay_ok.clone(), relay_bad.clone()],
    );
    let mut core = new_core(dir);
    activate(&mut core, &a);
    connect(&mut core, 0, &relay_ok);
    connect(&mut core, 1, &relay_bad);

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 1, "durable ack test")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, u) = find_sign_request(&effects);
    let signed = u.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Ok(signed.clone()),
    ));
    assert_eq!(
        effects
            .iter()
            .filter(|e| matches!(e, Effect::PublishEvent(..)))
            .count(),
        2,
        "a durable AuthorOutbox write reaches both of the author's write relays"
    );

    let ok_frame = RelayFrame::Text(RelayMessage::ok(signed.id, true, "").as_json());
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        ok_frame,
    ));
    assert!(effects.iter().any(
        |e| matches!(e, Effect::EmitReceipt(rid, WriteStatus::Acked(r)) if *rid == id && r == &relay_ok)
    ));

    let nack_frame =
        RelayFrame::Text(RelayMessage::ok(signed.id, false, "blocked: spam").as_json());
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        nack_frame,
    ));
    assert!(effects.iter().any(
        |e| matches!(e, Effect::EmitReceipt(rid, WriteStatus::Rejected(r, msg)) if *rid == id && r == &relay_bad && msg.contains("blocked"))
    ));

    let statuses = sink.0.lock().unwrap();
    assert!(statuses
        .iter()
        .any(|s| matches!(s, WriteStatus::Acked(r) if r == &relay_ok)));
    assert!(statuses
        .iter()
        .any(|s| matches!(s, WriteStatus::Rejected(r, _) if r == &relay_bad)));
}

#[test]
fn uncommitted_attempt_terminal_emits_no_receipt_and_keeps_lane_live() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://finish-failure.example").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]);
    let mut core = EngineCore::new(
        FailOnceCompensationStore::failing_attempt_finish(),
        Box::new(dir),
        10,
    );
    activate(&mut core, &a);
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        relay.clone(),
    ));
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&a, 2, "finish persistence")),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(CapturingReceiptSink::default()),
    ));
    let (id, generation, unsigned) = find_sign_request(&effects);
    let signed = unsigned.sign_with_keys(&a).unwrap();
    core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Ok(signed.clone()),
    ));
    let frame = || RelayFrame::Text(RelayMessage::ok(signed.id, true, "").as_json());
    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        frame(),
    ));
    assert!(!failed
        .iter()
        .any(|effect| matches!(effect, Effect::EmitReceipt(_, WriteStatus::Acked(_)))));
    let retried = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        frame(),
    ));
    assert!(retried.iter().any(
        |effect| matches!(effect, Effect::EmitReceipt(receipt, WriteStatus::Acked(r)) if *receipt == id && r == &relay)
    ));
}

#[test]
fn unaccepted_failure_ids_are_distinct_and_disjoint_from_store_receipts() {
    let a = Keys::generate();
    let mut core = new_core(FixtureDirectory::new());
    let fail = |core: &mut EngineCore<MemoryStore>, seq| {
        core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned(&a, seq, "unaccepted")),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            },
            Box::new(CapturingReceiptSink::default()),
        ))
        .into_iter()
        .find_map(|effect| match effect {
            Effect::EmitReceipt(id, WriteStatus::Failed(_)) => Some(id),
            _ => None,
        })
        .unwrap()
    };
    let first = fail(&mut core, 200);
    let second = fail(&mut core, 201);
    assert_ne!(first, second);
    assert!(first.0 >= (1u64 << 63));
    assert!(second.0 >= (1u64 << 63));
}

// ---- negentropy (M3 plan §6 E): ledger #8 structural gate + REQ fallback
// selection --------------------------------------------------------------

fn neg_msg_frame(sub: &str, message_hex: &str) -> RelayFrame {
    RelayFrame::Text(
        RelayMessage::NegMsg {
            subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(sub)),
            message: std::borrow::Cow::Owned(message_hex.to_string()),
        }
        .as_json(),
    )
}

fn neg_err_frame(sub: &str) -> RelayFrame {
    RelayFrame::Text(
        RelayMessage::NegErr {
            subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(sub)),
            message: std::borrow::Cow::Owned("blocked: unsupported".to_string()),
        }
        .as_json(),
    )
}

/// Test 3 (ledger #8) first half: an unprobed relay (never even connected,
/// so its `Prober` state stays `Unknown`) must never see `Effect::NegOpen`
/// -- only a plain REQ.
#[test]
fn unprobed_relay_never_routes_to_negentropy() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));

    assert!(
        !effects.iter().any(|e| matches!(e, Effect::NegOpen(..))),
        "an unprobed relay must never receive Effect::NegOpen -- only a plain REQ"
    );
    req_for(&effects, &relay0); // panics if there is no plain REQ.
}

/// Test 3 (ledger #8) second half + test 10's routing half: drives the
/// Prober FSM to a real `Supported` verdict via a scripted NEG-MSG (exactly
/// what a real relay's probe response looks like from `EngineCore`'s point
/// of view), then proves a broad/unlimited demand change on that relay
/// routes negentropy-first while a small/limited query on the SAME relay
/// still stays on plain REQ.
#[test]
fn probed_relay_routes_broad_demand_to_negentropy_but_limited_demand_stays_on_req() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(b.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    // Bootstrap: a's kind:1 atom -- the relay is `Unknown` at this point
    // (probing can only start once SOME demand causes a connection), so
    // this is unavoidably a plain REQ.
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    req_for(&effects, &relay0);

    let connect_effects = connect(&mut core, 0, &relay0);
    let (probe_sub, ..) = connect_effects
        .iter()
        .find_map(|e| match e {
            Effect::StartProbe(url, sub_id, filter, hex) if url == &relay0 => {
                Some((sub_id.clone(), filter.clone(), hex.clone()))
            }
            _ => None,
        })
        .expect("connecting a never-probed relay must start a capability probe");
    let probe_wire = wire_sub_string(&probe_sub);

    // The relay answers the probe with a NEG-MSG -- any valid response
    // classifies NIP-77 support; the payload's content is never inspected
    // by the prober.
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        neg_msg_frame(&probe_wire, "6100"),
    ));

    // b's kind:1 atom widens the SAME (kind:1) skeleton -- same sub-id,
    // now the relay is Supported and the widened filter is broad
    // (unlimited), so it routes through negentropy instead of a plain REQ.
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(
        effects.iter().any(|e| matches!(e, Effect::NegOpen(..))),
        "a probed relay's broad demand change must route negentropy-first"
    );
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::Wire(d)
            if d.ops.iter().any(|(r, ops)| r == &relay0
                && ops.iter().any(|op| matches!(op, WireOp::Req(..)))))),
        "the widened atom must NOT ALSO reach the relay as a plain REQ"
    );

    // A LIMITED (small-exact-result) query on the SAME relay stays on plain
    // REQ even though the relay is Supported -- ledger #8's REQ-fallback
    // selection rule (a different skeleton -- kind:7 -- so it is a brand
    // new, independent sub-id, unaffected by kind:1's negentropy routing).
    let limited = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([7u16])),
        authors: Some(Binding::Literal(BTreeSet::from([a.public_key().to_hex()]))),
        limit: Some(1),
        ..Filter::default()
    });
    let effects = core.handle(EngineMsg::Subscribe(
        limited,
        Box::new(CapturingSink::default()),
    ));
    req_for(&effects, &relay0); // must still be a plain REQ.
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::NegOpen(..))),
        "a small/limited exact-result query must stay on REQ even for a Supported relay"
    );
}

/// A relay that answers the capability probe with `NEG-ERR` is classified
/// `Unsupported` and cached -- its demand stays on plain REQ forever after,
/// same as an unprobed relay.
#[test]
fn relay_that_rejects_the_probe_is_classified_unsupported_and_stays_on_req() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    req_for(&effects, &relay0);

    let connect_effects = connect(&mut core, 0, &relay0);
    let (probe_sub, ..) = connect_effects
        .iter()
        .find_map(|e| match e {
            Effect::StartProbe(url, sub_id, filter, hex) if url == &relay0 => {
                Some((sub_id.clone(), filter.clone(), hex.clone()))
            }
            _ => None,
        })
        .expect("connecting a never-probed relay must start a capability probe");
    let probe_wire = wire_sub_string(&probe_sub);

    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        neg_err_frame(&probe_wire),
    ));

    let b = Keys::generate();
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::NegOpen(..))),
        "an Unsupported-classified relay must never route to negentropy"
    );
}

/// Structural grep-guard (ledger #8, "not a runtime `if`"): the ONLY place
/// in `core/mod.rs` that constructs a `ProbedRelay` value is inside
/// `negentropy/mod.rs` (`Prober::probed`/`Prober::on_neg_msg`) -- reading
/// `core/mod.rs`'s own source confirms it never spells the constructor
/// itself, so the only way it can ever hold one is by receiving it back
/// from `Prober`, exactly the compile-fence the plan asks for.
#[test]
fn core_never_constructs_a_probed_relay_directly() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/core/mod.rs"))
        .expect("read core/mod.rs");
    let code_lines: Vec<&str> = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//"))
        .collect();
    assert!(
        !code_lines.iter().any(|l| l.contains("ProbedRelay(")),
        "core/mod.rs must never construct a ProbedRelay literal itself -- only `negentropy::Prober` may"
    );
}

/// Test 10's liveness half (bounded, headless): a reconciliation open past
/// [`NEG_LIVENESS_DEADLINE_SECS`]'s worth of synthetic clock advance is
/// abandoned and falls back to a plain REQ -- driven entirely via
/// `EngineCore::tick`'s own clock parameter, never a real sleep.
#[test]
fn stale_negentropy_session_falls_back_to_req_after_the_liveness_deadline() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(b.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    req_for(&effects, &relay0);

    let connect_effects = connect(&mut core, 0, &relay0);
    let (probe_sub, ..) = connect_effects
        .iter()
        .find_map(|e| match e {
            Effect::StartProbe(url, sub_id, filter, hex) if url == &relay0 => {
                Some((sub_id.clone(), filter.clone(), hex.clone()))
            }
            _ => None,
        })
        .expect("connecting a never-probed relay must start a capability probe");
    let probe_wire = wire_sub_string(&probe_sub);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        neg_msg_frame(&probe_wire, "6100"),
    ));

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let neg_sub_id = effects
        .iter()
        .find_map(|e| match e {
            Effect::NegOpen(_, sub_id, ..) => Some(sub_id.clone()),
            _ => None,
        })
        .expect("the widened broad demand must have opened a negentropy session");

    // No reply ever arrives; advance the clock past the liveness deadline.
    let effects = core.handle(EngineMsg::Tick(Timestamp::from(31u64)));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::NegClose(_, sub_id) if sub_id == &neg_sub_id)),
        "a stale session past the liveness deadline must be closed"
    );
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Wire(d)
            if d.ops.iter().any(|(r, ops)| r == &relay0
                && ops.iter().any(|op| matches!(op, WireOp::Req(sid, _) if sid == &neg_sub_id))))),
        "a stale session must fall back to a plain REQ for the same sub-id"
    );
}

// ---- #34 retraction seam (retraction-and-negative-deltas.md §1.3/§3) ----

/// `RowDelta::Removed` on kind:5 deletion (issue #34's `root_query_emits_
/// removed_on_delete` obligation, asserted explicitly here even though it
/// "may already pass via refresh's full-set diff" -- a root query has no
/// `Derived` node to seed at all, so the row simply leaving the store on
/// the next `refresh_handle` is enough; the resolver-level dirty-seed
/// wiring this issue adds is what makes the SAME delete also retract a
/// `Derived` member correctly, covered separately in
/// `nmp-resolver/tests/contract.rs`).
#[test]
fn root_query_emits_removed_on_delete() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(sink.clone()),
    ));

    let note = nmp_resolver::testkit::kind1(&a, "delete me", 100);
    let note_id = note.id;
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", note),
    ));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _)
            if rows.iter().any(|r| matches!(r, RowDelta::Added(row) if row.event.id == note_id)))),
        "the note must arrive as Added first"
    );

    let deletion = nmp_resolver::testkit::deletion(&a, &[note_id], 200);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", deletion),
    ));

    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _)
            if rows.iter().any(|r| matches!(r, RowDelta::Removed(id) if *id == note_id)))),
        "a kind:5 delete of a row the handle is currently holding must emit \
         RowDelta::Removed for it: {effects:?}"
    );
}

/// NIP-40 expiry retraction (issue #34's `expiry_emits_removed_via_manual_
/// tick`, retraction-and-negative-deltas.md §3.2): a manual/synthetic-clock
/// `EngineMsg::Tick` drains `store.expire_due`, routes the removed row
/// through `resolver.retract`, and the ordinary refresh diff emits
/// `RowDelta::Removed` -- with zero further input (no new event arrives,
/// only the clock advancing). This proves the mechanism directly, against a
/// synthetic clock, independent of who calls `tick` -- the `recv_timeout`
/// runtime driver that now fires this on its own live (#39, design §3.3) is
/// exercised separately in `runtime_integration.rs`.
#[test]
fn expiry_emits_removed_via_manual_tick() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(sink.clone()),
    ));

    let expiring = nmp_resolver::testkit::expiring_kind1(&a, "ephemeral", 100, 150);
    let expiring_id = expiring.id;
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", expiring),
    ));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _)
            if rows.iter().any(|r| matches!(r, RowDelta::Added(row) if row.event.id == expiring_id)))),
        "the expiring note must arrive as Added first"
    );

    // No further event arrives -- only the clock advances past its
    // expiration deadline (150).
    let effects = core.handle(EngineMsg::Tick(Timestamp::from(200u64)));

    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _)
            if rows.iter().any(|r| matches!(r, RowDelta::Removed(id) if *id == expiring_id)))),
        "tick() past the expiration deadline must emit RowDelta::Removed \
         with no new event: {effects:?}"
    );
}

/// #39 / retraction-and-negative-deltas.md §3.2: `EngineCore::next_deadline`
/// is the min over every deadline source this reducer currently tracks --
/// NIP-40 expiry (`store.next_expiration()`) and open negentropy sessions'
/// liveness deadlines (`started_at + NEG_LIVENESS_DEADLINE_SECS`, the same
/// 30s constant `stale_negentropy_session_falls_back_to_req_after_the_
/// liveness_deadline` exercises). Entirely against a synthetic clock -- no
/// real time elapses in this test -- so it is a pure function of `core`'s
/// tracked state, exactly what the `runtime::engine_loop` driver (tested
/// live in `runtime_integration.rs`) re-reads every iteration to arm its
/// `recv_timeout`.
#[test]
fn next_deadline_is_min_over_expiry_and_neg_liveness() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(b.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    assert_eq!(
        core.next_deadline(),
        None,
        "a fresh core tracks no expiring events and no open neg session"
    );

    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let connect_effects = connect(&mut core, 0, &relay0);

    // Ingest an event expiring at t=150 on the open sub -- the store's
    // expiration index is now the sole deadline source (no neg session
    // exists yet).
    let expiring = nmp_resolver::testkit::expiring_kind1(&a, "ephemeral", 100, 150);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", expiring),
    ));
    assert_eq!(
        core.next_deadline(),
        Some(Timestamp::from(150u64)),
        "with only an expiring event, next_deadline is the store's expiry"
    );

    // Drive the SAME probe-then-widen dance as
    // `probed_relay_routes_broad_demand_to_negentropy_but_limited_demand_
    // stays_on_req` to open a real neg session on relay0.
    let (probe_sub, ..) = connect_effects
        .iter()
        .find_map(|e| match e {
            Effect::StartProbe(url, sub_id, filter, hex) if url == &relay0 => {
                Some((sub_id.clone(), filter.clone(), hex.clone()))
            }
            _ => None,
        })
        .expect("connecting a never-probed relay must start a capability probe");
    let probe_wire = wire_sub_string(&probe_sub);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        neg_msg_frame(&probe_wire, "6100"),
    ));
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    assert!(
        effects.iter().any(|e| matches!(e, Effect::NegOpen(..))),
        "setup: b's widened demand must actually open a neg session"
    );

    // `NegSession::started_at` is `core`'s clock, which nothing above has
    // advanced past `EngineCore::new`'s default of 0 (only `Tick` ever
    // moves it) -- so the neg-liveness deadline lands at exactly
    // NEG_LIVENESS_DEADLINE_SECS (30), strictly nearer than the expiry at
    // 150, and must win the min.
    assert_eq!(
        core.next_deadline(),
        Some(Timestamp::from(30u64)),
        "an open neg session's liveness deadline (30) is nearer than the \
         expiry (150) and must win the min"
    );
}

// ---- issue #19: ToInboxes routes through NIP-65 READ relays -------------
//
// `EngineCore::resolve_routes`'s `ToInboxes` branch must fan a p-tagged
// inbox write out to each recipient's `read_relays` (lane `Nip65Read`) and
// NOTHING else — never a recipient's `write_relays`/`extra_relays`, and
// never a public fallback. A recipient whose inbox relays are unknown
// (never-seen kind:10002, or a write-only relay list) fails the WHOLE
// intent CLOSED with a typed `Failed`, before any `PublishEvent`. The
// read/write/unmarked *ingestion* split is proven at the parse+ingest
// level in `nmp_engine::core`'s `nip65_read_write_split_tests` (unmarked =
// both; write-marked excluded from read; one kind:10002 winner fills both
// sets); these tests own the *routing* half of the acceptance contract.

/// Read-only routing: a recipient advertising a distinct read relay, write
/// relay, AND extra relay routes an inbox write to ONLY the read relay. The
/// write/extra relays — the old flagged fallback — must never appear on the
/// wire. (Composed with the unmarked-parse tests, this also covers the
/// unmarked case: an unmarked `r` tag lands in the read set, which is
/// exactly what this branch consumes.)
#[test]
fn to_inboxes_routes_to_recipient_read_relays_only() {
    let author = Keys::generate();
    let recipient = Keys::generate();
    let read_relay = RelayUrl::parse("wss://recipient-inbox.example.com").unwrap();
    let write_relay = RelayUrl::parse("wss://recipient-outbox.example.com").unwrap();
    let extra_relay = RelayUrl::parse("wss://recipient-hint.example.com").unwrap();

    // The recipient's read set is DISTINCT from its write/extra sets, so a
    // wrong-lane read cannot masquerade as correct.
    let dir = FixtureDirectory::new()
        .with_read(recipient.public_key().to_hex(), [read_relay.clone()])
        .with_write(recipient.public_key().to_hex(), [write_relay.clone()])
        .with_extra(
            recipient.public_key().to_hex(),
            nmp_router::Lane::Hint,
            [extra_relay.clone()],
        );
    let mut core = new_core(dir);
    activate(&mut core, &author);

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&author, 1, "inbox dm")),
            durability: Durability::Durable,
            routing: WriteRouting::ToInboxes(vec![recipient.public_key()]),
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, u) = find_sign_request(&effects);
    let signed = u.sign_with_keys(&author).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));

    let published: BTreeSet<RelayUrl> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::PublishEvent(relay, _, _) => Some(relay.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        published,
        BTreeSet::from([read_relay.clone()]),
        "an inbox write must reach ONLY the recipient's NIP-65 read relay, \
         never its write/extra relays -- got {published:?}"
    );

    // The receipt's Routed status must carry the same read-only set.
    let routed = sink
        .0
        .lock()
        .unwrap()
        .iter()
        .find_map(|s| match s {
            WriteStatus::Routed(relays) => Some(relays.clone()),
            _ => None,
        })
        .expect("must reach a Routed status");
    assert_eq!(
        routed,
        BTreeSet::from([read_relay]),
        "Routed status must expose exactly the read-relay set"
    );
}

/// Write-only recipient: a recipient whose kind:10002 declares only
/// write-marked relays has an EMPTY read set, so an inbox write to it fails
/// CLOSED — no `PublishEvent` to the write relay, a typed `Failed` receipt.
#[test]
fn to_inboxes_write_only_recipient_fails_closed() {
    let author = Keys::generate();
    let recipient = Keys::generate();
    let write_relay = RelayUrl::parse("wss://recipient-outbox.example.com").unwrap();

    // Recipient is KNOWN, but only via write relays: read set is empty.
    let dir = FixtureDirectory::new().with_write(recipient.public_key().to_hex(), [write_relay]);
    let mut core = new_core(dir);
    activate(&mut core, &author);

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&author, 1, "inbox dm")),
            durability: Durability::Durable,
            routing: WriteRouting::ToInboxes(vec![recipient.public_key()]),
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, u) = find_sign_request(&effects);
    let signed = u.sign_with_keys(&author).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));

    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(..))),
        "a write-only recipient's inbox write must never reach a relay -- \
         especially not its write relay -- got {effects:?}"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(rid, WriteStatus::Failed(_)) if *rid == id)),
        "must fail CLOSED with a typed Failed, not silently drop the write"
    );
    assert!(matches!(
        sink.0.lock().unwrap().last(),
        Some(WriteStatus::Failed(_))
    ));
}

/// Unknown recipient: a recipient the directory has never seen a kind:10002
/// for fails CLOSED — the fail-closed status lands before any
/// `PublishEvent`, and one unknown recipient in a set poisons the whole
/// intent so a KNOWN co-recipient's relay is never written either (no
/// partial-leak inbox delivery).
#[test]
fn to_inboxes_unknown_recipient_fails_the_whole_intent_closed() {
    let author = Keys::generate();
    let known = Keys::generate();
    let unknown = Keys::generate();
    let known_inbox = RelayUrl::parse("wss://known-inbox.example.com").unwrap();

    // `known` has an inbox relay; `unknown` is absent entirely.
    let dir = FixtureDirectory::new().with_read(known.public_key().to_hex(), [known_inbox]);
    let mut core = new_core(dir);
    activate(&mut core, &author);

    let sink = CapturingReceiptSink::default();
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(unsigned(&author, 1, "group inbox dm")),
            durability: Durability::Durable,
            routing: WriteRouting::ToInboxes(vec![known.public_key(), unknown.public_key()]),
        },
        Box::new(sink.clone()),
    ));
    let (id, generation, u) = find_sign_request(&effects);
    let signed = u.sign_with_keys(&author).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));

    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(..))),
        "one unknown recipient must fail the WHOLE intent closed -- the \
         known co-recipient's relay must NOT be written either -- got {effects:?}"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitReceipt(rid, WriteStatus::Failed(_)) if *rid == id)),
        "must fail CLOSED with a typed Failed"
    );
    assert!(matches!(
        sink.0.lock().unwrap().last(),
        Some(WriteStatus::Failed(_))
    ));
}

// ---- issue #122: fallible ingest/read doors degrade, never panic ---------
//
// A fault-injecting `EventStore` whose ONE mutating ingest door (`insert`)
// returns a `PersistenceError` (a stand-in for disk-full / an I/O error on
// the real redb backend) while every OTHER door delegates to a healthy
// in-memory store. This isolates the ingest failure so the falsifiers below
// prove (a) the door surfaces `Err` rather than panicking, and (b) the
// engine degrades the local cache to read-only and emits a diagnostic
// instead of crashing the host app on a relay EVENT frame.
struct FailIngestStore {
    inner: MemoryStore,
    fail_insert: bool,
}

impl FailIngestStore {
    fn armed() -> Self {
        Self {
            inner: MemoryStore::new(),
            fail_insert: true,
        }
    }
}

impl EventStore for FailIngestStore {
    fn insert(
        &mut self,
        event: nostr::Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        if self.fail_insert {
            return Err(PersistenceError("injected ingest I/O failure".into()));
        }
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
    fn start_attempt(
        &mut self,
        intent_id: nmp_store::IntentId,
        relay: RelayUrl,
        event: nostr::Event,
    ) -> Result<RecoveredAttempt, PersistenceError> {
        self.inner.start_attempt(intent_id, relay, event)
    }
    fn finish_attempt(
        &mut self,
        intent_id: nmp_store::IntentId,
        relay: &RelayUrl,
        ordinal: u64,
        outcome: AttemptOutcome,
    ) -> Result<FinishAttemptOutcome, PersistenceError> {
        self.inner
            .finish_attempt(intent_id, relay, ordinal, outcome)
    }
    fn recover_attempts(
        &self,
        intent_id: nmp_store::IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }
    fn accept_ephemeral(
        &mut self,
        frozen_id: nostr::EventId,
        expected_pubkey: nostr::PublicKey,
    ) -> Result<u64, PersistenceError> {
        self.inner.accept_ephemeral(frozen_id, expected_pubkey)
    }
}

/// Door-level falsifier (issue #122): the `insert` ingest door surfaces a
/// realistic persistence I/O failure as `Err(PersistenceError)` rather than
/// panicking. `MemoryStore` never fails, so the fault is entirely the
/// injected one — this is the exact contract the redb backend now honors via
/// `.map_err(persist_err)?` on every real redb operation.
#[test]
fn ingest_door_surfaces_io_failure_as_persistence_error_not_panic() {
    let a = Keys::generate();
    let mut store = FailIngestStore::armed();
    let event = nmp_resolver::testkit::kind1(&a, "disk is full", 1_000);
    let from = RelayObserved::new(
        RelayUrl::parse("wss://relay.example.com").unwrap(),
        Timestamp::from(1_000u64),
    );
    let outcome = store.insert(event, from);
    assert!(
        matches!(outcome, Err(PersistenceError(_))),
        "an ingest-path I/O failure must surface as Err(PersistenceError), got {outcome:?}"
    );
}

/// Engine-level falsifier (issue #122): a relay EVENT frame whose store
/// `insert` fails on I/O DEGRADES the engine to read-only (a `store_degraded`
/// diagnostic is emitted) and never panics the reducer. The failed frame
/// delivers no phantom rows, and the engine stays usable for later messages.
#[test]
fn ingest_io_failure_degrades_read_only_without_panicking() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]);
    // `query`/coverage doors stay healthy; only `insert` fails — so the
    // subscribe/connect setup below (which reads, never inserts) succeeds,
    // proving the degrade is specific to the failing ingest door.
    let mut core = EngineCore::new(FailIngestStore::armed(), Box::new(dir), 10);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(sink.clone()),
    ));
    let _ = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        relay.clone(),
    ));

    // The real relay ingest path — the exact call that used to `.expect()`
    // panic on a disk-full redb `insert`.
    let event = nmp_resolver::testkit::kind1(&a, "disk is full", 1_000);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        event_frame("s", event),
    ));

    // Degrade, don't panic: the read-only signal reaches the diagnostics
    // surface.
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EmitDiagnostics(snap) if snap.store_degraded.is_some())),
        "an ingest I/O failure must surface a `store_degraded` diagnostic, got {effects:?}"
    );
    // A failed ingest fabricates no rows.
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _) if !rows.is_empty())),
        "a failed ingest must not deliver phantom rows, got {effects:?}"
    );
    // The reducer survives and keeps handling messages (no poisoned state).
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(1u64)));
}
