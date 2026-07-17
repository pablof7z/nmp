use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use nmp_grammar::{Binding, Derived, Filter, IdentityField, Selector};
use nmp_router::FixtureDirectory;
use nmp_store::{
    AcceptOutcome, AcceptWrite, CancelEphemeralOutcome, ClaimSet, CompensateOutcome,
    CompensationReason, CoverageInterval, CoverageKey, EventCursor, EventStore, GcReport,
    InsertOutcome, MemoryStore, PersistenceError, PromoteOutcome, RecoveredAttempt,
    RecoveredIntent, RecoveredReceipt, RecoveredRouteRevision, RelayObserved, RetractReason,
    StoredEvent,
};
use nostr::{Event, EventBuilder, EventId, Keys, Kind, RelayUrl, Tag, Timestamp};

use super::*;

#[derive(Debug)]
enum FailRead {
    Query(String),
    NewestBefore(String),
}

#[derive(Clone, Default)]
struct ReadFailureControl(Rc<RefCell<Option<FailRead>>>);

impl ReadFailureControl {
    fn fail_query(&self, message: &str) {
        *self.0.borrow_mut() = Some(FailRead::Query(message.to_owned()));
    }

    fn fail_newest_before(&self, message: &str) {
        *self.0.borrow_mut() = Some(FailRead::NewestBefore(message.to_owned()));
    }

    fn take_query_failure(&self) -> Option<PersistenceError> {
        let mut failure = self.0.borrow_mut();
        if matches!(failure.as_ref(), Some(FailRead::Query(_))) {
            let Some(FailRead::Query(message)) = failure.take() else {
                unreachable!()
            };
            Some(PersistenceError(message))
        } else {
            None
        }
    }

    fn take_newest_before_failure(&self) -> Option<PersistenceError> {
        let mut failure = self.0.borrow_mut();
        if matches!(failure.as_ref(), Some(FailRead::NewestBefore(_))) {
            let Some(FailRead::NewestBefore(message)) = failure.take() else {
                unreachable!()
            };
            Some(PersistenceError(message))
        } else {
            None
        }
    }
}

struct FailingReadStore {
    inner: MemoryStore,
    control: ReadFailureControl,
}

impl FailingReadStore {
    fn new(inner: MemoryStore, control: ReadFailureControl) -> Self {
        Self { inner, control }
    }
}

impl EventStore for FailingReadStore {
    fn compensate_write_with_state(
        &mut self,
        intent_id: IntentId,
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
        event: Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        self.inner.insert(event, from)
    }

    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        if let Some(error) = self.control.take_query_failure() {
            return Err(error);
        }
        self.inner.query(filter)
    }

    fn query_newest_before(
        &self,
        filter: &nostr::Filter,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        if let Some(error) = self.control.take_newest_before_failure() {
            return Err(error);
        }
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

    fn next_expiration(&self) -> Option<Timestamp> {
        self.inner.next_expiration()
    }

    fn record_coverage(
        &mut self,
        atom: &ContextualAtom,
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
        intent_id: IntentId,
        sig: nostr::secp256k1::schnorr::Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner.promote_signed(intent_id, sig)
    }

    fn compensate_write(
        &mut self,
        intent_id: IntentId,
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
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        self.inner.record_route_revision(intent_id, relays)
    }

    fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
        self.inner.recover_route_revisions(intent_id)
    }

    fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }

    fn accept_ephemeral(
        &mut self,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
    ) -> Result<u64, PersistenceError> {
        self.inner.accept_ephemeral(frozen_id, expected_pubkey)
    }
}

#[derive(Clone, Default)]
struct CapturingHistorySink(Arc<Mutex<Vec<HistoryBatch>>>);

impl HistorySink for CapturingHistorySink {
    fn on_history(&self, batch: HistoryBatch) {
        self.0.lock().unwrap().push(batch);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HistorySnapshot {
    target_rows: usize,
    acquired_tie_seconds: BTreeSet<u64>,
    last_rows: BTreeMap<EventId, Row>,
    order: BTreeSet<(Reverse<u64>, EventId)>,
    last_evidence: Option<AcquisitionEvidence>,
    projection_complete: bool,
    load: WindowLoad,
    handle_ids: BTreeSet<HandleId>,
    history_by_handle: HashMap<HandleId, HistorySessionId>,
}

fn snapshot(core: &EngineCore<FailingReadStore>, id: HistorySessionId) -> HistorySnapshot {
    let state = &core.histories[&id];
    assert!(state.pending_load.is_none());
    HistorySnapshot {
        target_rows: state.target_rows,
        acquired_tie_seconds: state.acquired_tie_seconds.clone(),
        last_rows: state.last_rows.clone(),
        order: state.order.clone(),
        last_evidence: state.last_evidence.clone(),
        projection_complete: state.projection_complete,
        load: state.load,
        handle_ids: state.handle_ids.clone(),
        history_by_handle: core.history_by_handle.clone(),
    }
}

fn event(keys: &Keys, kind: u16, created_at: u64) -> Event {
    EventBuilder::new(Kind::from(kind), format!("row-{kind}-{created_at}"))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .unwrap()
}

fn seeded_store(events: impl IntoIterator<Item = Event>, relay: &RelayUrl) -> MemoryStore {
    let mut store = MemoryStore::new();
    store
        .insert_batch(
            events
                .into_iter()
                .map(|event| {
                    (
                        event,
                        RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                    )
                })
                .collect(),
        )
        .unwrap();
    store
}

fn derived_history_query() -> HistoryQuery {
    HistoryQuery::new(
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Derived(Box::new(Derived {
                inner: nmp_grammar::Demand::from_filter(Filter {
                    kinds: Some(BTreeSet::from([3u16])),
                    authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                    ..Filter::default()
                }),
                project: Selector::Tag("p".to_owned()),
            }))),
            ..Filter::default()
        }),
        2,
        4,
    )
}

fn literal_history_query() -> HistoryQuery {
    HistoryQuery::new(
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([9u16])),
            ..Filter::default()
        }),
        2,
        4,
    )
}

/// The oldest retained row's second: the boundary an advance would fetch
/// behind. Derived from state now that windows carry no continuation token.
fn boundary_second(core: &EngineCore<FailingReadStore>, id: HistorySessionId) -> u64 {
    core.histories[&id]
        .last_rows
        .values()
        .map(|row| row.event.created_at.as_secs())
        .min()
        .expect("an opened window holds at least one row")
}

fn open_history(
    store: MemoryStore,
    control: ReadFailureControl,
    query: HistoryQuery,
    active_pubkey: Option<PublicKey>,
) -> (
    EngineCore<FailingReadStore>,
    HistorySessionId,
    CapturingHistorySink,
) {
    let mut core = EngineCore::new(
        FailingReadStore::new(store, control),
        Box::new(FixtureDirectory::new()),
        20,
    );
    if let Some(active_pubkey) = active_pubkey {
        core.handle(EngineMsg::SetActivePubkey(Some(active_pubkey)));
    }
    let sink = CapturingHistorySink::default();
    let effects = core.handle(EngineMsg::SubscribeHistory(query, Box::new(sink.clone())));
    let id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitHistory(id, _) => Some(*id),
            _ => None,
        })
        .expect("fixture must open a history frame");
    sink.0.lock().unwrap().clear();
    (core, id, sink)
}

fn assert_failed_load(
    core: &EngineCore<FailingReadStore>,
    id: HistorySessionId,
    sink: &CapturingHistorySink,
    before: &HistorySnapshot,
    effects: &[Effect],
    first_error: &str,
) {
    let diagnostic_index = effects
        .iter()
        .position(|effect| {
            matches!(effect, Effect::EmitDiagnostics(diagnostics)
                if diagnostics.store_degraded.as_deref() == Some(first_error))
        })
        .expect("store failure must immediately emit the latched diagnostic");
    let result_index = effects
        .iter()
        .position(|effect| {
            matches!(effect,
                Effect::HistoryLoadResult(session, Err(HistoryAdvanceError::StoreUnavailable))
                    if *session == id)
        })
        .expect("store failure must retain its typed load result");
    assert!(diagnostic_index < result_index);
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::EmitHistory(session, _) if *session == id)));
    assert!(
        sink.0.lock().unwrap().is_empty(),
        "last frame must not change"
    );
    assert_eq!(&snapshot(core, id), before, "rollback must be exact");
}

fn derived_fixture() -> (
    EngineCore<FailingReadStore>,
    HistorySessionId,
    CapturingHistorySink,
    ReadFailureControl,
) {
    let me = Keys::generate();
    let followed = Keys::generate();
    let relay = RelayUrl::parse("wss://history-read-failure.example").unwrap();
    let contact_list = EventBuilder::new(Kind::ContactList, "")
        .tag(Tag::public_key(followed.public_key()))
        .custom_created_at(Timestamp::from(500u64))
        .sign_with_keys(&me)
        .unwrap();
    let rows = (100..106).map(|created_at| event(&followed, 1, created_at));
    let store = seeded_store(std::iter::once(contact_list).chain(rows), &relay);
    let control = ReadFailureControl::default();
    let (core, id, sink) = open_history(
        store,
        control.clone(),
        derived_history_query(),
        Some(me.public_key()),
    );
    (core, id, sink, control)
}

#[test]
fn tie_second_read_failure_dispatches_diagnostics_and_exact_rollback() {
    let (mut core, id, sink, control) = derived_fixture();
    let before = snapshot(&core, id);
    control.fail_query("tie-second read failed");

    let effects = core.handle(EngineMsg::RequestRows(id, 4));

    assert_failed_load(
        &core,
        id,
        &sink,
        &before,
        &effects,
        "durable-store persistence failure: tie-second read failed",
    );

    control.fail_query("later failure must not replace first");
    let repeated = core.handle(EngineMsg::RequestRows(id, 4));
    assert_failed_load(
        &core,
        id,
        &sink,
        &before,
        &repeated,
        "durable-store persistence failure: tie-second read failed",
    );
}

#[test]
fn older_window_read_failure_dispatches_diagnostics_and_exact_rollback() {
    let (mut core, id, sink, control) = derived_fixture();
    let boundary_secs = boundary_second(&core, id);
    core.histories
        .get_mut(&id)
        .unwrap()
        .acquired_tie_seconds
        .insert(boundary_secs);
    let before = snapshot(&core, id);
    control.fail_query("older-window read failed");

    let effects = core.handle(EngineMsg::RequestRows(id, 4));

    assert_failed_load(
        &core,
        id,
        &sink,
        &before,
        &effects,
        "durable-store persistence failure: older-window read failed",
    );
}

#[test]
fn projection_advance_read_failure_dispatches_diagnostics_and_exact_rollback() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://history-advance-failure.example").unwrap();
    let store = seeded_store(
        (100..106).map(|created_at| event(&keys, 9, created_at)),
        &relay,
    );
    let control = ReadFailureControl::default();
    let (mut core, id, sink) = open_history(store, control.clone(), literal_history_query(), None);
    let before = snapshot(&core, id);
    control.fail_newest_before("projection advance read failed");

    let effects = core.handle(EngineMsg::RequestRows(id, 4));

    assert_failed_load(
        &core,
        id,
        &sink,
        &before,
        &effects,
        "durable-store persistence failure: projection advance read failed",
    );
}

#[test]
fn under_return_keeps_limit_and_disconnect_evidence_without_false_end() {
    let keys = Keys::generate();
    let first = RelayUrl::parse("wss://history-limit-a.example").unwrap();
    let second = RelayUrl::parse("wss://history-limit-b.example").unwrap();
    let store = seeded_store(
        (101..104).map(|created_at| event(&keys, 1, created_at)),
        &first,
    );
    let query = HistoryQuery::new(
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Literal(BTreeSet::from([keys
                .public_key()
                .to_hex()]))),
            ..Filter::default()
        }),
        2,
        6,
    );
    let directory = FixtureDirectory::new().with_write(keys.public_key().to_hex(), [first, second]);
    let control = ReadFailureControl::default();
    let mut core = EngineCore::new(
        FailingReadStore::new(store, control),
        Box::new(directory),
        1,
    );
    let sink = CapturingHistorySink::default();
    let opened = core.handle(EngineMsg::SubscribeHistory(query, Box::new(sink.clone())));
    let id = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitHistory(id, _) => Some(*id),
            _ => None,
        })
        .unwrap();
    let selected = core.router.plan().reqs.keys().next().unwrap().clone();
    let relay_handle = TransportRelayHandle {
        slot: 7,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(relay_handle, selected.clone()));
    let disconnected = core.handle(EngineMsg::RelayDisconnected(
        relay_handle,
        selected.clone(),
        DisconnectReason::Error,
    ));
    assert!(
        disconnected
            .iter()
            .any(|effect| matches!(effect, Effect::EmitHistory(session, _) if *session == id)),
        "disconnect evidence refresh must issue a current frame"
    );
    sink.0.lock().unwrap().clear();

    let staged = core.handle(EngineMsg::RequestRows(id, 4));
    assert!(staged.iter().any(|effect| {
        matches!(effect, Effect::HistoryLoadResult(session, Ok(())) if *session == id)
    }));
    let committed = core.handle(EngineMsg::CommitHistoryLoad(id));
    let returned = committed
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitHistory(session, batch)
                if *session == id && matches!(batch.load, WindowLoad::Returned { added: 1 }) =>
            {
                Some(batch)
            }
            _ => None,
        })
        .expect("the short page must remain an explicit under-return fact");

    // A short local page is `Returned { added }`, never a synthetic "end":
    // there is no Complete/End variant, and the per-source evidence below
    // carries the real reason the page was short.
    assert!(returned
        .evidence
        .shortfall
        .iter()
        .any(|fact| { matches!(fact, ShortfallFact::LocalLimit { .. }) }));
    assert!(returned.evidence.sources.iter().any(|source| {
        source.relay == selected.relay && source.status == SourceStatus::Disconnected
    }));
}
