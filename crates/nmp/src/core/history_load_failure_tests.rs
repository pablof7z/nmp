use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use nmp_grammar::{Binding, Derived, Filter, IdentityField, Selector};
use nmp_router::FixtureRoutingFacts;
use nmp_store::{
    testing, AcceptOutcome, AcceptWrite, CompensateOutcome, CompensationReason, CoverageInterval,
    CoverageKey, EventCursor, EventStore, GcReport, GcRetentionSet, InsertOutcome,
    PersistenceError, PromoteOutcome, PublishQueueAttempt, PublishQueueIntent, PublishQueueReceipt,
    PublishQueueRouteRevision, RedbStore, RefuseReason, RelayObserved, RemoveQueueEntryOutcome,
    RetractReason, StoredEvent,
};
use nostr::{Event, EventBuilder, EventId, Keys, Kind, RelayUrl, Tag, Timestamp};

use super::*;

#[derive(Debug)]
enum StoreFailure {
    Query(String),
    CoverageWrite(String),
}

#[derive(Clone, Default)]
pub(super) struct StoreFailureControl(Arc<Mutex<Option<StoreFailure>>>);

impl StoreFailureControl {
    fn fail_query(&self, message: &str) {
        *self.0.lock().unwrap() = Some(StoreFailure::Query(message.to_owned()));
    }

    pub(super) fn fail_coverage_write(&self, message: &str) {
        *self.0.lock().unwrap() = Some(StoreFailure::CoverageWrite(message.to_owned()));
    }

    fn take_coverage_write_failure(&self) -> Option<PersistenceError> {
        let mut failure = self.0.lock().unwrap();
        if matches!(failure.as_ref(), Some(StoreFailure::CoverageWrite(_))) {
            let Some(StoreFailure::CoverageWrite(message)) = failure.take() else {
                unreachable!()
            };
            Some(PersistenceError::invariant(message))
        } else {
            None
        }
    }

    fn take_query_failure(&self) -> Option<PersistenceError> {
        let mut failure = self.0.lock().unwrap();
        if matches!(failure.as_ref(), Some(StoreFailure::Query(_))) {
            let Some(StoreFailure::Query(message)) = failure.take() else {
                unreachable!()
            };
            Some(PersistenceError::invariant(message))
        } else {
            None
        }
    }
}

pub(super) struct ControlledFailureStore {
    inner: RedbStore,
    control: StoreFailureControl,
}

impl ControlledFailureStore {
    pub(super) fn new(inner: RedbStore, control: StoreFailureControl) -> Self {
        Self { inner, control }
    }
}

impl EventStore for ControlledFailureStore {
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
        if let Some(error) = self.control.take_coverage_write_failure() {
            return Err(error);
        }
        self.inner.record_coverage(claims)
    }

    fn get_coverage(
        &self,
        key: CoverageKey,
        relay: &RelayUrl,
    ) -> Result<Option<CoverageInterval>, PersistenceError> {
        self.inner.get_coverage(key, relay)
    }

    fn gc(&mut self, claims: &GcRetentionSet) -> Result<GcReport, PersistenceError> {
        self.inner.gc(claims)
    }

    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        self.inner.accept_write(accept)
    }
    fn replaceable_operation_snapshot(
        &self,
        coordinate: &nostr::nips::nip01::Coordinate,
    ) -> Result<Option<nmp_store::RecoveredSemanticResource>, PersistenceError> {
        self.inner.replaceable_operation_snapshot(coordinate)
    }
    fn install_replaceable_materialization(
        &mut self,
        rematerialize: nmp_store::SemanticRematerialize,
    ) -> Result<nmp_store::SemanticInstallOutcome, PersistenceError> {
        self.inner
            .install_replaceable_materialization(rematerialize)
    }
    fn install_replaceable_source_materialization(
        &mut self,
        install: nmp_store::SemanticSourceInstall,
    ) -> Result<nmp_store::SemanticInstallOutcome, PersistenceError> {
        self.inner
            .install_replaceable_source_materialization(install)
    }

    fn promote_signed(
        &mut self,
        target: nmp_store::PromotionTarget,
        verified: nmp_store::VerifiedSignature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner.promote_signed(target, verified)
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

fn canonical_corruption(kind: u16, filename: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("canonical corruption directory");
    let path = directory.path().join(filename);
    let corrupt_id = {
        let keys = Keys::generate();
        let corrupt = event(&keys, kind, 1_000);
        let corrupt_id = corrupt.id;
        let mut store = RedbStore::open(&path).expect("create persistent Redb fixture");
        store
            .insert(
                corrupt,
                RelayObserved::new(
                    RelayUrl::parse("wss://canonical-corruption.example").unwrap(),
                    Timestamp::from(1_001u64),
                ),
            )
            .expect("seed canonical event");
        corrupt_id
    };
    testing::corrupt_canonical_event(&path, corrupt_id)
        .expect("store-owned canonical-event corruption");
    (directory, path)
}

#[test]
fn observation_open_failures_are_typed_leak_free_and_leave_runtime_usable() {
    let (_directory, path) = canonical_corruption(1, "observation-open-corruption.redb");
    let store = RedbStore::open(&path).expect("reopen corrupted Redb fixture");
    let (engine, handle) =
        crate::runtime::EngineThread::spawn(store, 4, nmp_transport::PoolConfig::default())
            .expect("runtime starts over targeted canonical corruption");
    assert_eq!(
        handle.observation_ownership_census(),
        crate::runtime::ObservationOwnershipCensus::default()
    );

    let ordinary = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1])),
        ..Filter::default()
    });
    assert!(matches!(
        handle.subscribe(ordinary),
        Err(crate::runtime::EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("decode canonical event view")
    ));
    assert_eq!(
        handle.observation_ownership_census(),
        crate::runtime::ObservationOwnershipCensus::default(),
        "post-handle ordinary projection refusal must roll back every owner"
    );
    let healthy = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([2])),
        ..Filter::default()
    });
    let (ordinary_handle, ordinary_rows) = handle.subscribe(healthy.clone()).expect(
        "a disjoint healthy ordinary filter proves corruption is targeted and runtime survived",
    );
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
            kinds: Some(BTreeSet::from([1])),
            ..Filter::default()
        }),
        1,
        2,
    );
    assert!(matches!(
        handle.subscribe_history(history),
        Err(crate::runtime::EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("decode canonical event view")
    ));
    assert_eq!(
        handle.observation_ownership_census(),
        crate::runtime::ObservationOwnershipCensus::default(),
        "post-handle history projection refusal must roll back every owner"
    );
    let (history_handle, history_rows) = handle
        .subscribe_history(HistoryQuery::new(healthy, 1, 2))
        .expect("the same disjoint filter remains usable through history");
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
                kinds: Some(BTreeSet::from([1])),
                ..Filter::default()
            }),
            project: Selector::Tag("p".to_owned()),
        }))),
        kinds: Some(BTreeSet::from([4])),
        ..Filter::default()
    });
    assert!(matches!(
        handle.subscribe(derived.clone()),
        Err(crate::runtime::EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("decode canonical event view")
    ));
    assert_eq!(
        handle.observation_ownership_census(),
        crate::runtime::ObservationOwnershipCensus::default(),
        "pre-handle ordinary refusal must discard partial resolver nodes"
    );

    assert!(matches!(
        handle.subscribe_history(HistoryQuery::new(derived, 1, 2)),
        Err(crate::runtime::EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("decode canonical event view")
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
    let (_directory, path) = canonical_corruption(3, "union-later-branch-corruption.redb");
    let mut core = EngineCore::new(
        RedbStore::open(&path).expect("reopen corrupted Redb fixture"),
        20,
    );
    let baseline = core.observation_ownership_census();

    // The first branch resolves without touching the store; the second must
    // read it to resolve its `Derived` inner query, so the corrupt kind-3 row
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

    let (reason, effects) = match core.open_observation(query, Timestamp::from(0u64)) {
        ObservationOpen::Refused { reason, effects } => {
            assert!(
                reason.contains("decode canonical event view"),
                "unexpected refusal: {reason}"
            );
            (reason, effects)
        }
        ObservationOpen::Opened { .. } => panic!("corrupt later-branch row was ignored"),
    };

    let store_error = reason
        .strip_prefix("canonical query resolution failed: ")
        .expect("branch construction refusal names its store error");
    assert_only_refusal_diagnostic(&effects, store_error);
    assert_eq!(
        core.observation_ownership_census(),
        baseline,
        "the branch built BEFORE the failing one must be withdrawn too; its \
         graph nodes are the residue nothing else observes"
    );

    assert!(matches!(
        core.open_observation(LiveQuery::single(first), Timestamp::from(0u64)),
        ObservationOpen::Opened { .. }
    ));
}

#[test]
fn opening_freshness_refusal_leaves_no_candidate_request_target_index() {
    let directory = tempfile::tempdir().expect("coverage corruption directory");
    let path = directory.path().join("opening-freshness-corruption.redb");
    let relay = RelayUrl::parse("wss://request-target-refusal.example").unwrap();
    let filter = Filter {
        kinds: Some(BTreeSet::from([1])),
        ..Filter::default()
    };
    let mut demand = nmp_grammar::Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([1])),
        ..Filter::default()
    });
    demand.source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    demand.freshness = Freshness::MaxAge { seconds: 60 };
    let atom = ContextualAtom {
        filter: ConcreteFilter {
            kinds: filter.kinds.clone(),
            ..ConcreteFilter::default()
        },
        source: demand.source.clone(),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let coverage_key = nmp_store::coverage_key(&atom);
    {
        let mut store = RedbStore::open(&path).expect("create persistent Redb fixture");
        store
            .record_coverage(&[(
                atom,
                relay.clone(),
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
            )])
            .expect("seed coverage row");
    }
    testing::corrupt_coverage(&path, coverage_key, &relay)
        .expect("store-owned coverage corruption");
    let mut core = EngineCore::new(
        RedbStore::open(&path).expect("reopen corrupted Redb fixture"),
        20,
    );
    core.handle(EngineMsg::Tick(Timestamp::from(100u64)));
    // MaxAge now uses this one read for both its freshness decision and the
    // opening frame. Corrupt that sole authority and prove the candidate graph
    // unwinds before any request-target ownership can escape.

    let refusal = core.open_observation(LiveQuery::single(demand), Timestamp::from(100u64));
    match refusal {
        ObservationOpen::Refused { reason, .. } => assert!(
            reason.contains("decode coverage row"),
            "unexpected refusal: {reason}"
        ),
        ObservationOpen::Opened { .. } => panic!("corrupt coverage row was ignored"),
    }
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

/// #1165/#1342: a failed graph build must not swallow handle drops drained
/// before the failing read, and reporting the refusal consumes that batch
/// exactly once.
#[test]
fn resolver_refusal_carries_the_pending_drop_delta_exactly_once() {
    let (_directory, path) = canonical_corruption(3, "pending-drop-corruption.redb");
    let mut resolver =
        ResolverEngine::new(RedbStore::open(&path).expect("reopen corrupted Redb fixture"));
    let first = nmp_grammar::Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([1])),
        ..Filter::default()
    });
    let first_handle = match resolver.subscribe(first) {
        SubscribeOutcome::Opened { handle, .. } => handle,
        SubscribeOutcome::Refused { error, .. } => panic!("fixture open refused: {error}"),
    };
    drop(first_handle);

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
    let delta = match resolver.subscribe(failing) {
        SubscribeOutcome::Refused { error, delta } => {
            assert!(error.to_string().contains("decode canonical event view"));
            delta
        }
        SubscribeOutcome::Opened { .. } => panic!("corrupt derived-query row was ignored"),
    };
    assert_eq!(delta.ops.len(), 1);
    assert!(matches!(
        &delta.ops[0],
        DemandOp::Close(atom) if atom.filter.kinds == Some(BTreeSet::from([1]))
    ));
    assert!(
        resolver.poll_pending_drops().is_empty(),
        "the refused outcome owns the drained close; polling cannot report it twice"
    );
}

#[test]
fn each_refused_open_arm_consumes_a_pending_drop_into_one_same_call_wire_close() {
    for graph_refusal in [true, false] {
        let corrupt_kind = if graph_refusal { 3 } else { 2 };
        let (_directory, path) =
            canonical_corruption(corrupt_kind, "refused-open-withdrawal-corruption.redb");
        let relay = RelayUrl::parse("wss://refused-open-withdrawal.example").unwrap();
        let mut core = EngineCore::new(
            RedbStore::open(&path).expect("reopen corrupted Redb fixture"),
            4,
        );
        let mut first = nmp_grammar::Demand::from_filter(Filter {
            kinds: Some(BTreeSet::from([1])),
            ..Filter::default()
        });
        first.source = SourceAuthority::Pinned(BTreeSet::from([relay]));
        first.freshness = Freshness::Live;
        let observation =
            match core.open_observation(LiveQuery::single(first), Timestamp::from(0u64)) {
                ObservationOpen::Opened { id, .. } => id,
                ObservationOpen::Refused { reason, .. } => panic!("fixture open refused: {reason}"),
            };
        let opened = core.flush_wire_admission(Timestamp::from(0u64));
        let sub_id = opened
            .iter()
            .filter_map(|effect| match effect {
                Effect::Wire(delta) => Some(delta),
                _ => None,
            })
            .flat_map(|delta| delta.ops.iter().flat_map(|(_, ops)| ops))
            .find_map(|op| match op {
                WireOp::Req(sub_id, _) => Some(sub_id.clone()),
                WireOp::Close(_) => None,
            })
            .expect("fixture admission opens one wire request");

        // Model the exact #1165 seam: the owning resolver handle is dropped,
        // so its close is pending inside the resolver, while core's exact wire
        // index still records the request the next outcome must withdraw.
        let branch = core.observations[&observation].branches[0];
        core.observations.remove(&observation);
        drop(core.handles.remove(&branch));

        let failing = if graph_refusal {
            nmp_grammar::Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([2])),
                authors: Some(Binding::Derived(Box::new(Derived {
                    inner: nmp_grammar::Demand::from_filter(Filter {
                        kinds: Some(BTreeSet::from([3])),
                        ..Filter::default()
                    }),
                    project: Selector::Tag("p".to_owned()),
                }))),
                ..Filter::default()
            })
        } else {
            nmp_grammar::Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([2])),
                ..Filter::default()
            })
        };
        let refusal = if graph_refusal { "graph" } else { "projection" };
        let effects = match core.open_observation(LiveQuery::single(failing), Timestamp::from(0u64))
        {
            ObservationOpen::Refused { reason, effects } => {
                assert!(
                    reason.contains("decode canonical event view"),
                    "unexpected {refusal} refusal: {reason}"
                );
                effects
            }
            ObservationOpen::Opened { .. } => panic!("corrupt {refusal} row was ignored"),
        };
        let closes: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Wire(delta) => Some(delta),
                _ => None,
            })
            .flat_map(|delta| delta.ops.iter().flat_map(|(_, ops)| ops))
            .filter_map(|op| match op {
                WireOp::Close(closed) => Some(closed),
                WireOp::Req(_, _) => None,
            })
            .collect();
        assert_eq!(closes, vec![&sub_id], "{refusal} refusal");
        assert!(core.router.plan().reqs.is_empty());
        assert!(core.resolver.poll_pending_drops().is_empty());
        assert!(core.flush_wire_admission(Timestamp::from(0u64)).is_empty());
    }
}

#[test]
fn shutdown_queued_during_each_refusal_keeps_the_typed_reply_and_never_panics() {
    {
        let (_directory, path) = canonical_corruption(1, "ordinary-shutdown-race-corruption.redb");
        let (store, blocked) = RedbStore::open_with_ordered_event_read_pause(&path)
            .expect("reopen corrupted Redb fixture with one ordered-read pause");
        let (engine, handle) =
            crate::runtime::EngineThread::spawn(store, 4, nmp_transport::PoolConfig::default())
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
                if reason.contains("decode canonical event view")
        ));
        engine.join();
    }

    {
        let (_directory, path) = canonical_corruption(2, "history-shutdown-race-corruption.redb");
        let (store, blocked) = RedbStore::open_with_ordered_event_read_pause(&path)
            .expect("reopen corrupted Redb fixture with one ordered-read pause");
        let (engine, handle) =
            crate::runtime::EngineThread::spawn(store, 4, nmp_transport::PoolConfig::default())
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
                if reason.contains("decode canonical event view")
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
    assert_eq!(actual.limited_demands, expected.limited_demands);
    assert_eq!(actual.limited_demands, expected.limited_demands);
    assert_eq!(actual.refused_sessions, expected.refused_sessions);
    assert_eq!(
        actual.subscription_shortfalls,
        expected.subscription_shortfalls
    );
}

#[test]
fn ordinary_projection_refusal_cannot_perturb_a_cap_sized_existing_plan() {
    let existing_author = Keys::generate().public_key();
    let candidate_keys = Keys::generate();
    let candidate_author = candidate_keys.public_key();
    let existing_relay = RelayUrl::parse("wss://open-existing.example").unwrap();
    let candidate_relay = RelayUrl::parse("wss://open-candidate.example").unwrap();
    let directory = tempfile::tempdir().expect("canonical corruption directory");
    let path = directory.path().join("ordinary-projection-corruption.redb");
    let (corrupt_id, healthy_id) = {
        let corrupt = event(&candidate_keys, 2, 1_000);
        let corrupt_id = corrupt.id;
        let healthy = event(&candidate_keys, 3, 1_001);
        let healthy_id = healthy.id;
        let mut store = RedbStore::open(&path).expect("create persistent Redb fixture");
        store
            .insert(
                corrupt,
                RelayObserved::new(candidate_relay.clone(), Timestamp::from(1_001u64)),
            )
            .expect("seed candidate canonical event");
        store
            .insert(
                healthy,
                RelayObserved::new(candidate_relay.clone(), Timestamp::from(1_002u64)),
            )
            .expect("seed disjoint healthy event");
        (corrupt_id, healthy_id)
    };
    testing::corrupt_canonical_event(&path, corrupt_id)
        .expect("store-owned canonical-event corruption");
    let facts = FixtureRoutingFacts::new()
        .with_outbound_routes(existing_author, [existing_relay])
        .with_outbound_routes(candidate_author, [candidate_relay]);
    let mut core = EngineCore::new_with_fixture_routing_facts(
        RedbStore::open(&path).expect("reopen corrupted Redb fixture"),
        facts,
        1,
    );
    let existing_id =
        match core.open_observation(routed_query(existing_author, 1), Timestamp::from(0u64)) {
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

    let (effects, diagnostic) =
        match core.open_observation(routed_query(candidate_author, 2), Timestamp::from(0u64)) {
            ObservationOpen::Refused { reason, effects } => {
                let store_error = reason
                    .strip_prefix("canonical row projection failed: ")
                    .expect("candidate refusal names the projection boundary");
                assert!(store_error.contains("decode canonical event view"));
                (effects, store_error.to_owned())
            }
            ObservationOpen::Opened { .. } => panic!("corrupt candidate projection was ignored"),
        };

    assert_only_refusal_diagnostic(&effects, &diagnostic);
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

    let healthy_query = LiveQuery::from_filter(Filter {
        authors: Some(Binding::Literal(BTreeSet::from(
            [candidate_author.to_hex()],
        ))),
        ids: Some(Binding::Literal(BTreeSet::from([healthy_id.to_hex()]))),
        kinds: Some(BTreeSet::from([3])),
        ..Filter::default()
    });
    if let ObservationOpen::Refused { reason, .. } =
        core.open_observation(healthy_query, Timestamp::from(0u64))
    {
        panic!("disjoint same-author projection refused: {reason}");
    }
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
    let control = StoreFailureControl::default();
    let mut core = EngineCore::new_with_fixture_routing_facts(
        ControlledFailureStore::new(
            RedbStore::temporary().expect("temporary Redb store"),
            control.clone(),
        ),
        facts,
        1,
    );
    let existing_id = match core.open_history_observation(
        HistoryQuery::new(routed_query(existing_author, 1), 1, 2),
        Timestamp::from(0u64),
    ) {
        ObservationOpen::Opened { id, .. } => id,
        ObservationOpen::Refused { reason, .. } => panic!("fixture open refused: {reason}"),
    };
    let baseline_census = core.observation_ownership_census();
    let baseline_demand = core.active_demand();
    let baseline_plan = core.router.plan().clone();
    let baseline_compiles = core.router_compiles.get();
    let baseline_history = snapshot(&core, existing_id);

    control.fail_query("candidate history projection failed");
    let effects = match core.open_history_observation(
        HistoryQuery::new(routed_query(candidate_author, 2), 1, 2),
        Timestamp::from(0u64),
    ) {
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
        core.open_history_observation(
            HistoryQuery::new(routed_query(candidate_author, 2), 1, 2),
            Timestamp::from(0u64),
        ),
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

fn snapshot<S: EventStore>(core: &EngineCore<S>, id: HistorySessionId) -> HistorySnapshot {
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

fn seeded_store(events: impl IntoIterator<Item = Event>, relay: &RelayUrl) -> RedbStore {
    let mut store = RedbStore::temporary().expect("temporary Redb store");
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
fn boundary_second(core: &EngineCore<ControlledFailureStore>, id: HistorySessionId) -> u64 {
    core.histories[&id]
        .last_rows
        .values()
        .map(|row| row.created_at().as_secs())
        .min()
        .expect("an opened window holds at least one row")
}

fn open_history(
    store: RedbStore,
    control: StoreFailureControl,
    query: HistoryQuery,
    active_pubkey: Option<PublicKey>,
) -> (EngineCore<ControlledFailureStore>, HistorySessionId) {
    let mut core = EngineCore::new(ControlledFailureStore::new(store, control), 20);
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

fn assert_failed_load<S: EventStore>(
    core: &EngineCore<S>,
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
    EngineCore<ControlledFailureStore>,
    HistorySessionId,
    StoreFailureControl,
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
    let control = StoreFailureControl::default();
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
    let mut store = RedbStore::temporary_with_failed_query_newest_before()
        .expect("temporary Redb query-newest-before failure fixture");
    store
        .insert_batch(
            (100..106)
                .map(|created_at| {
                    (
                        event(&keys, 9, created_at),
                        RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                    )
                })
                .collect(),
        )
        .unwrap();
    let mut core = EngineCore::new(store, 20);
    let opened = core.handle(EngineMsg::SubscribeHistory(literal_history_query()));
    let id = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitHistory(id, _) => Some(*id),
            _ => None,
        })
        .expect("fixture must open a history frame");
    let before = snapshot(&core, id);

    let effects = core.handle(EngineMsg::RequestRows(id, 4));

    assert_failed_load(
        &core,
        id,
        &before,
        &effects,
        "durable-store persistence failure: injected query-newest-before failure",
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
    let mut core = EngineCore::new_with_fixture_routing_facts(store, directory, 1);
    let opened = core.handle(EngineMsg::SubscribeHistory(query));
    let id = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitHistory(id, _) => Some(*id),
            _ => None,
        })
        .unwrap();
    core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)));
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
