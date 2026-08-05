use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{mpsc, Arc, Condvar, Mutex};

use nmp_grammar::{Binding, Derived, Filter, IdentityField, Selector};
use nmp_router::FixtureRoutingFacts;
use nmp_store::{
    AcceptOutcome, AcceptWrite, CompensateOutcome, CompensationReason, CoverageInterval,
    CoverageKey, EventCursor, EventStore, GcReport, GcRetentionSet, InsertOutcome, MemoryStore,
    PersistenceError, PromoteOutcome, PublishQueueAttempt, PublishQueueIntent, PublishQueueReceipt,
    PublishQueueRouteRevision, RefuseReason, RelayObserved, RemoveQueueEntryOutcome, RetractReason,
    StoredEvent,
};
use nostr::{Event, EventBuilder, EventId, Keys, Kind, RelayUrl, Tag, Timestamp};

use super::*;

#[derive(Debug)]
enum FailRead {
    Query {
        message: String,
        block: Option<BlockedRead>,
    },
    NewestBefore(String),
}

#[derive(Debug)]
struct BlockedRead {
    entered: mpsc::SyncSender<()>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

struct BlockedReadControl {
    entered: mpsc::Receiver<()>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl BlockedReadControl {
    fn wait_until_entered(&self) {
        self.entered
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("runtime must reach the controlled store read");
    }

    fn release(&self) {
        let (released, wake) = &*self.release;
        *released.lock().unwrap() = true;
        wake.notify_all();
    }
}

#[derive(Clone, Default)]
struct ReadFailureControl(Arc<Mutex<Option<FailRead>>>);

impl ReadFailureControl {
    fn fail_query(&self, message: &str) {
        *self.0.lock().unwrap() = Some(FailRead::Query {
            message: message.to_owned(),
            block: None,
        });
    }

    fn block_then_fail_query(&self, message: &str) -> BlockedReadControl {
        let (entered_tx, entered) = mpsc::sync_channel(0);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        *self.0.lock().unwrap() = Some(FailRead::Query {
            message: message.to_owned(),
            block: Some(BlockedRead {
                entered: entered_tx,
                release: Arc::clone(&release),
            }),
        });
        BlockedReadControl { entered, release }
    }

    fn fail_newest_before(&self, message: &str) {
        *self.0.lock().unwrap() = Some(FailRead::NewestBefore(message.to_owned()));
    }

    fn take_query_failure(&self) -> Option<PersistenceError> {
        let mut failure = self.0.lock().unwrap();
        if matches!(failure.as_ref(), Some(FailRead::Query { .. })) {
            let Some(FailRead::Query { message, block }) = failure.take() else {
                unreachable!()
            };
            drop(failure);
            if let Some(block) = block {
                block
                    .entered
                    .send(())
                    .expect("controlled read witness remains alive");
                let (released, wake) = &*block.release;
                let mut released = released.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
            }
            Some(PersistenceError::invariant(message))
        } else {
            None
        }
    }

    fn take_newest_before_failure(&self) -> Option<PersistenceError> {
        let mut failure = self.0.lock().unwrap();
        if matches!(failure.as_ref(), Some(FailRead::NewestBefore(_))) {
            let Some(FailRead::NewestBefore(message)) = failure.take() else {
                unreachable!()
            };
            Some(PersistenceError::invariant(message))
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
        claims: &[(ContextualAtom, RelayUrl, CoverageInterval)],
    ) -> Result<(), PersistenceError> {
        self.inner.record_coverage(claims)
    }

    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
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
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<PublishQueueRouteRevision, PersistenceError> {
        self.inner.record_route_revision(intent_id, relays)
    }

    fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueRouteRevision>, PersistenceError> {
        self.inner.recover_route_revisions(intent_id)
    }

    fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }

    fn enumerate_publish_queue_receipts(
        &self,
    ) -> Result<Vec<PublishQueueReceipt>, PersistenceError> {
        self.inner.enumerate_publish_queue_receipts()
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

#[test]
fn observation_open_failures_are_typed_leak_free_and_leave_runtime_usable() {
    let control = ReadFailureControl::default();
    let store = FailingReadStore::new(MemoryStore::new(), control.clone());
    let (engine, handle) = crate::runtime::EngineThread::spawn(
        store,
        4,
        nmp_transport::PoolConfig::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("runtime starts before injected canonical-store read failures");
    assert_eq!(
        handle.observation_ownership_census(),
        crate::runtime::ObservationOwnershipCensus::default()
    );

    let ordinary = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1])),
        ..Filter::default()
    });
    control.fail_query("canonical ordinary projection failed");
    assert!(matches!(
        handle.subscribe(ordinary.clone()),
        Err(crate::runtime::EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("canonical ordinary projection failed")
    ));
    assert_eq!(
        handle.observation_ownership_census(),
        crate::runtime::ObservationOwnershipCensus::default(),
        "post-handle ordinary projection refusal must roll back every owner"
    );
    let (ordinary_handle, ordinary_rows) = handle
        .subscribe(ordinary)
        .expect("a healthy ordinary open proves the engine thread survived");
    ordinary_rows
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("healthy empty ordinary query still receives its initial frame");
    handle.unsubscribe(ordinary_handle);
    assert_eq!(
        handle.observation_ownership_census(),
        crate::runtime::ObservationOwnershipCensus::default()
    );

    let history = HistoryQuery::new(
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([2])),
            ..Filter::default()
        }),
        1,
        2,
    );
    control.fail_query("canonical history projection failed");
    assert!(matches!(
        handle.subscribe_history(history.clone()),
        Err(crate::runtime::EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("canonical history projection failed")
    ));
    assert_eq!(
        handle.observation_ownership_census(),
        crate::runtime::ObservationOwnershipCensus::default(),
        "post-handle history projection refusal must roll back every owner"
    );
    let (history_handle, history_rows) = handle
        .subscribe_history(history)
        .expect("a healthy history open proves the engine thread survived");
    history_rows
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("healthy empty history query still receives its initial frame");
    handle.unsubscribe_history(history_handle);
    assert_eq!(
        handle.observation_ownership_census(),
        crate::runtime::ObservationOwnershipCensus::default()
    );

    let derived = LiveQuery::from_filter(Filter {
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: nmp_grammar::Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([3])),
                ..Filter::default()
            }),
            project: Selector::Tag("p".to_owned()),
        }))),
        kinds: Some(BTreeSet::from([1])),
        ..Filter::default()
    });
    control.fail_query("derived resolver construction failed");
    assert!(matches!(
        handle.subscribe(derived.clone()),
        Err(crate::runtime::EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("derived resolver construction failed")
    ));
    assert_eq!(
        handle.observation_ownership_census(),
        crate::runtime::ObservationOwnershipCensus::default(),
        "pre-handle ordinary refusal must discard partial resolver nodes"
    );

    control.fail_query("derived history construction failed");
    assert!(matches!(
        handle.subscribe_history(HistoryQuery::new(derived, 1, 2)),
        Err(crate::runtime::EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("derived history construction failed")
    ));
    assert_eq!(
        handle.observation_ownership_census(),
        crate::runtime::ObservationOwnershipCensus::default(),
        "pre-handle history refusal must discard partial resolver nodes"
    );

    handle.shutdown();
    engine.join();
}

/// #1108's "branch K open failure rolls back 0..K-1", for the failure class
/// the caller-visible census above cannot see.
///
/// A branch's graph is built BEFORE any handle is registered, so a branch
/// opened and then abandoned mid-open leaves residue only in the resolver.
/// `active_demand` is computed from `self.handles` and would report that
/// residue as absent; only the ownership census counts the graph nodes
/// themselves. Deleting the withdrawal loop in `open_observation`'s branch
/// construction must turn this red -- the union's FIRST branch is fully built
/// when its second one fails, and nothing else in the suite opens a branch
/// that is later abandoned.
#[test]
fn a_union_branch_whose_graph_fails_withdraws_the_branches_opened_before_it() {
    let control = ReadFailureControl::default();
    let mut core = EngineCore::new(
        FailingReadStore::new(MemoryStore::new(), control.clone()),
        20,
    );
    let baseline = core.observation_ownership_census();

    // The first branch resolves without touching the store; the second must
    // read it to resolve its `Derived` inner query, so the injected failure
    // can only strike the LATER branch. Canonical branch order sorts on the
    // selection first, and 1 < 2.
    let first = nmp_grammar::Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([1])),
        ..Filter::default()
    });
    let failing = nmp_grammar::Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([2])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: nmp_grammar::Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([3])),
                ..Filter::default()
            }),
            project: Selector::Tag("p".to_owned()),
        }))),
        ..Filter::default()
    });
    let query = LiveQuery::union(
        [
            LiveQuery::single(first.clone()),
            LiveQuery::single(failing.clone()),
        ],
        None,
    )
    .expect("a two-branch union is constructible");
    assert_eq!(
        query
            .branches()
            .iter()
            .position(|branch| branch == &failing),
        Some(1),
        "the fault must be injectable at a LATER branch; a failure at branch 0 \
         has nothing to roll back"
    );

    control.fail_query("second branch graph construction failed");
    let effects = match core.open_observation(query) {
        ObservationOpen::Refused { reason, effects } => {
            assert!(
                reason.contains("second branch graph construction failed"),
                "unexpected refusal: {reason}"
            );
            effects
        }
        ObservationOpen::Opened { .. } => panic!("injected resolution failure was ignored"),
    };

    assert_only_refusal_diagnostic(
        &effects,
        "durable-store persistence failure: second branch graph construction failed",
    );
    assert_eq!(
        core.observation_ownership_census(),
        baseline,
        "the branch built BEFORE the failing one must be withdrawn too; its \
         graph nodes are the residue nothing else observes"
    );

    assert!(matches!(
        core.open_observation(LiveQuery::single(first)),
        ObservationOpen::Opened { .. }
    ));
}

#[test]
fn shutdown_queued_during_each_refusal_keeps_the_typed_reply_and_never_panics() {
    {
        let control = ReadFailureControl::default();
        let blocked = control.block_then_fail_query("ordinary refusal won the shutdown race");
        let (engine, handle) = crate::runtime::EngineThread::spawn(
            FailingReadStore::new(MemoryStore::new(), control),
            4,
            nmp_transport::PoolConfig::default(),
            RelayAdmissionPolicy::default(),
        )
        .unwrap();
        let caller_handle = handle.clone();
        let caller = std::thread::spawn(move || {
            caller_handle.subscribe(LiveQuery::from_filter(Filter {
                kinds: Some(BTreeSet::from([1])),
                ..Filter::default()
            }))
        });
        blocked.wait_until_entered();
        handle.shutdown();
        blocked.release();
        assert!(matches!(
            caller.join().expect("ordinary caller must not panic"),
            Err(crate::runtime::EngineThreadError::ObservationUnavailable { reason })
                if reason.contains("ordinary refusal won the shutdown race")
        ));
        engine.join();
    }

    {
        let control = ReadFailureControl::default();
        let blocked = control.block_then_fail_query("history refusal won the shutdown race");
        let (engine, handle) = crate::runtime::EngineThread::spawn(
            FailingReadStore::new(MemoryStore::new(), control),
            4,
            nmp_transport::PoolConfig::default(),
            RelayAdmissionPolicy::default(),
        )
        .unwrap();
        let caller_handle = handle.clone();
        let caller = std::thread::spawn(move || {
            caller_handle.subscribe_history(HistoryQuery::new(
                LiveQuery::from_filter(Filter {
                    kinds: Some(BTreeSet::from([2])),
                    ..Filter::default()
                }),
                1,
                2,
            ))
        });
        blocked.wait_until_entered();
        handle.shutdown();
        blocked.release();
        assert!(matches!(
            caller.join().expect("history caller must not panic"),
            Err(crate::runtime::EngineThreadError::ObservationUnavailable { reason })
                if reason.contains("history refusal won the shutdown race")
        ));
        engine.join();
    }
}

fn routed_query(author: PublicKey, kind: u16) -> LiveQuery {
    LiveQuery::from_filter(Filter {
        authors: Some(Binding::Literal(BTreeSet::from([author.to_hex()]))),
        kinds: Some(BTreeSet::from([kind])),
        ..Filter::default()
    })
}

fn assert_only_refusal_diagnostic(effects: &[Effect], expected: &str) {
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::EmitDiagnostics(snapshot)
                    if snapshot.store_degraded.as_deref() == Some(expected)
            ))
            .count(),
        1,
        "one durable-store degradation fact survives the rolled-back open"
    );
    assert!(
        effects
            .iter()
            .all(|effect| matches!(effect, Effect::EmitDiagnostics(_))),
        "a refused open must stage no wire, relay, attribution, or sibling-frame effect"
    );
}

fn assert_plan_unchanged(actual: &RelayPlan, expected: &RelayPlan) {
    assert_eq!(actual.reqs, expected.reqs);
    assert_eq!(actual.limited, expected.limited);
    assert_eq!(actual.refused_sessions, expected.refused_sessions);
    assert_eq!(
        actual.subscription_shortfalls,
        expected.subscription_shortfalls
    );
}

#[test]
fn ordinary_projection_refusal_cannot_perturb_a_cap_sized_existing_plan() {
    let existing_author = Keys::generate().public_key();
    let candidate_author = Keys::generate().public_key();
    let existing_relay = RelayUrl::parse("wss://open-existing.example").unwrap();
    let candidate_relay = RelayUrl::parse("wss://open-candidate.example").unwrap();
    let facts = FixtureRoutingFacts::new()
        .with_outbound_routes(existing_author, [existing_relay])
        .with_outbound_routes(candidate_author, [candidate_relay]);
    let control = ReadFailureControl::default();
    let mut core = EngineCore::new_with_fixture_routing_facts(
        FailingReadStore::new(MemoryStore::new(), control.clone()),
        facts,
        1,
    );
    let existing_id = match core.open_observation(routed_query(existing_author, 1)) {
        ObservationOpen::Opened { id, .. } => id,
        ObservationOpen::Refused { reason, .. } => panic!("fixture open refused: {reason}"),
    };
    let baseline_census = core.observation_ownership_census();
    let baseline_demand = core.active_demand();
    let baseline_plan = core.router.plan().clone();
    let baseline_compiles = core.router_compiles.get();
    let baseline_projection = {
        let state = &core.observations[&existing_id];
        (
            state.last_rows.clone(),
            state.last_evidence.clone(),
            state.projection_complete,
        )
    };

    control.fail_query("candidate ordinary projection failed");
    let effects = match core.open_observation(routed_query(candidate_author, 2)) {
        ObservationOpen::Refused { reason, effects } => {
            assert!(reason.contains("candidate ordinary projection failed"));
            effects
        }
        ObservationOpen::Opened { .. } => panic!("injected projection failure was ignored"),
    };

    assert_only_refusal_diagnostic(
        &effects,
        "durable-store persistence failure: candidate ordinary projection failed",
    );
    assert_eq!(core.observation_ownership_census(), baseline_census);
    assert_eq!(core.active_demand(), baseline_demand);
    assert_plan_unchanged(core.router.plan(), &baseline_plan);
    assert_eq!(
        core.router_compiles.get(),
        baseline_compiles,
        "the fallible canonical gate must run before speculative recompile"
    );
    let state = &core.observations[&existing_id];
    assert_eq!(
        (
            state.last_rows.clone(),
            state.last_evidence.clone(),
            state.projection_complete,
        ),
        baseline_projection,
        "existing rows and evidence stay byte-identical"
    );

    assert!(matches!(
        core.open_observation(routed_query(candidate_author, 2)),
        ObservationOpen::Opened { .. }
    ));
}

#[test]
fn history_projection_refusal_cannot_perturb_a_cap_sized_existing_window() {
    let existing_author = Keys::generate().public_key();
    let candidate_author = Keys::generate().public_key();
    let existing_relay = RelayUrl::parse("wss://history-open-existing.example").unwrap();
    let candidate_relay = RelayUrl::parse("wss://history-open-candidate.example").unwrap();
    let facts = FixtureRoutingFacts::new()
        .with_outbound_routes(existing_author, [existing_relay])
        .with_outbound_routes(candidate_author, [candidate_relay]);
    let control = ReadFailureControl::default();
    let mut core = EngineCore::new_with_fixture_routing_facts(
        FailingReadStore::new(MemoryStore::new(), control.clone()),
        facts,
        1,
    );
    let existing_id = match core.open_history_observation(HistoryQuery::new(
        routed_query(existing_author, 1),
        1,
        2,
    )) {
        ObservationOpen::Opened { id, .. } => id,
        ObservationOpen::Refused { reason, .. } => panic!("fixture open refused: {reason}"),
    };
    let baseline_census = core.observation_ownership_census();
    let baseline_demand = core.active_demand();
    let baseline_plan = core.router.plan().clone();
    let baseline_compiles = core.router_compiles.get();
    let baseline_history = snapshot(&core, existing_id);

    control.fail_query("candidate history projection failed");
    let effects = match core.open_history_observation(HistoryQuery::new(
        routed_query(candidate_author, 2),
        1,
        2,
    )) {
        ObservationOpen::Refused { reason, effects } => {
            assert!(reason.contains("candidate history projection failed"));
            effects
        }
        ObservationOpen::Opened { .. } => panic!("injected projection failure was ignored"),
    };

    assert_only_refusal_diagnostic(
        &effects,
        "durable-store persistence failure: candidate history projection failed",
    );
    assert_eq!(core.observation_ownership_census(), baseline_census);
    assert_eq!(core.active_demand(), baseline_demand);
    assert_plan_unchanged(core.router.plan(), &baseline_plan);
    assert_eq!(
        core.router_compiles.get(),
        baseline_compiles,
        "the fallible canonical gate must run before speculative recompile"
    );
    assert_eq!(
        snapshot(&core, existing_id),
        baseline_history,
        "existing history rows, evidence, and ownership stay byte-identical"
    );

    assert!(matches!(
        core.open_history_observation(HistoryQuery::new(routed_query(candidate_author, 2), 1, 2,)),
        ObservationOpen::Opened { .. }
    ));
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HistorySnapshot {
    target_rows: usize,
    acquired_tie_seconds: BTreeSet<u64>,
    last_rows: BTreeMap<EventId, Row>,
    order: BTreeSet<(Reverse<u64>, EventId)>,
    last_evidence: Option<Vec<AcquisitionEvidence>>,
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
) -> (EngineCore<FailingReadStore>, HistorySessionId) {
    let mut core = EngineCore::new(FailingReadStore::new(store, control), 20);
    if let Some(active_pubkey) = active_pubkey {
        core.handle(EngineMsg::SetActivePubkey(Some(active_pubkey)));
    }
    let effects = core.handle(EngineMsg::SubscribeHistory(query));
    let id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitHistory(id, _) => Some(*id),
            _ => None,
        })
        .expect("fixture must open a history frame");
    (core, id)
}

fn assert_failed_load(
    core: &EngineCore<FailingReadStore>,
    id: HistorySessionId,
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
    assert_eq!(&snapshot(core, id), before, "rollback must be exact");
}

fn derived_fixture() -> (
    EngineCore<FailingReadStore>,
    HistorySessionId,
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
    let (core, id) = open_history(
        store,
        control.clone(),
        derived_history_query(),
        Some(me.public_key()),
    );
    (core, id, control)
}

#[test]
fn tie_second_read_failure_dispatches_diagnostics_and_exact_rollback() {
    let (mut core, id, control) = derived_fixture();
    let before = snapshot(&core, id);
    control.fail_query("tie-second read failed");

    let effects = core.handle(EngineMsg::RequestRows(id, 4));

    assert_failed_load(
        &core,
        id,
        &before,
        &effects,
        "durable-store persistence failure: tie-second read failed",
    );

    control.fail_query("later failure must not replace first");
    let repeated = core.handle(EngineMsg::RequestRows(id, 4));
    assert_failed_load(
        &core,
        id,
        &before,
        &repeated,
        "durable-store persistence failure: tie-second read failed",
    );
}

#[test]
fn older_window_read_failure_dispatches_diagnostics_and_exact_rollback() {
    let (mut core, id, control) = derived_fixture();
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
    let (mut core, id) = open_history(store, control.clone(), literal_history_query(), None);
    let before = snapshot(&core, id);
    control.fail_newest_before("projection advance read failed");

    let effects = core.handle(EngineMsg::RequestRows(id, 4));

    assert_failed_load(
        &core,
        id,
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
    let directory =
        FixtureRoutingFacts::new().with_outbound_routes(keys.public_key(), [first, second]);
    let control = ReadFailureControl::default();
    let mut core = EngineCore::new_with_fixture_routing_facts(
        FailingReadStore::new(store, control),
        directory,
        1,
    );
    let opened = core.handle(EngineMsg::SubscribeHistory(query));
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
    assert!(returned.evidence[0]
        .shortfall
        .iter()
        .any(|fact| { matches!(fact, ShortfallFact::LocalLimit { .. }) }));
    assert!(returned.evidence[0].sources.iter().any(|source| {
        source.relay == selected.relay && source.status == SourceStatus::Disconnected
    }));
}
