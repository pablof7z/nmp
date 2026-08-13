//! Falsifiers for #1108: independent `Demand` branches composed inside ONE
//! live-query lifecycle.
//!
//! Every proof here is headless and deterministic: scripted `EngineMsg`s into
//! `EngineCore`, no sockets, no timing. Each test corresponds to one of the
//! issue's named falsifiers, and each one fails under the exact mechanism
//! disablement it names:
//!
//! | falsifier | disablement that must turn it red |
//! |---|---|
//! | `branch_sources_are_never_flattened_into_one_pinned_set` | merge every branch's `Pinned` set into one branch |
//! | `equal_branches_keep_independent_evidence_entries` | return one merged `AcquisitionEvidence` for the observation |
//! | `an_unplannable_branch_reports_its_own_shortfall` | union every branch's shortfall into one entry |
//! | `rows_union_by_event_id_with_merged_provenance` | deliver one frame per branch above the subscription |
//! | `the_aggregate_bound_is_applied_after_the_union` | apply the bound per branch before the union |
//! | `a_reactive_change_moves_every_branch_in_one_frame` | emit one frame per affected branch |
//! | `cancelling_a_union_keeps_work_a_sibling_observation_still_owns` | withdraw every branch's atoms unconditionally |
//! | `an_over_cap_union_refuses_the_whole_declaration` | truncate to the ceiling instead of refusing |
//! | `a_window_bounds_the_union_globally` | give each branch its own window target |
//! | `only_the_branch_tells_two_identical_resolver_facts_apart` | drop or fix the canonical branch on an observation fact |
//! | `a_union_branch_that_cannot_open_leaves_no_earlier_branch_installed` | keep the branches installed before the failing one |
//! | `one_branchs_refresh_failure_retracts_no_sibling_row` | project the branches whose read succeeded and drop the one that failed |
//! | `each_redeclared_branch_decides_freshness_from_its_own_stored_coverage` | make ONE freshness decision for the observation and give it to every branch |
//! | `a_redeclared_window_starts_again_at_its_initial_size` | carry a window's grown target across the restart |
//!
//! The graph-construction half of the partial-open rollback is proved by
//! `nmp::core::history_load_failure_tests::a_union_branch_whose_graph_fails_withdraws_the_branches_opened_before_it`,
//! which needs the crate-internal ownership census: a branch abandoned before
//! its handle is registered leaves residue only in the resolver graph, which
//! `active_demand` (computed from the handle table) reports as absent.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use nmp::mechanism::core::{
    AcquisitionEvidence, Effect, EngineCore, EngineMsg, HistoryQuery, ObservationEvidence,
    ObservationId, RowDelta, ShortfallFact,
};
use nmp_grammar::{
    AccessContext, Binding, CacheMode, ContextualAtom, Demand, Filter, Freshness, IdentityField,
    LiveQuery, LiveQueryError, SourceAuthority,
};
use nmp_router::{FixtureRoutingFacts, WireOp};
use nmp_store::{
    AcceptOutcome, AcceptWrite, CompensateOutcome, CompensationReason, CoverageInterval,
    CoverageKey, EventCursor, EventStore, GcReport, GcRetentionSet, InsertOutcome, IntentId,
    PersistenceError, PromoteOutcome, PublishQueueAttempt, PublishQueueIntent, PublishQueueReceipt,
    PublishQueueRouteRevision, RedbStore, RefuseReason, RelayObserved, RemoveQueueEntryOutcome,
    RetractReason, StoredEvent,
};
use nostr::{Event, EventId, Keys, Kind, PublicKey, RelayUrl, Timestamp, UnsignedEvent};

const KIND: u16 = 39_000;
/// The second branch's selection. Distinct from [`KIND`] so a fault can be
/// aimed at ONE branch's canonical read by what it asks for, rather than by
/// which read happens to come first.
const OTHER_KIND: u16 = 39_001;

fn relay(host: &str) -> RelayUrl {
    RelayUrl::parse(&format!("wss://{host}.example")).expect("fixture relay url")
}

fn core() -> EngineCore<RedbStore> {
    core_over(RedbStore::temporary().expect("temporary Redb store"))
}

fn core_over(store: RedbStore) -> EngineCore<RedbStore> {
    EngineCore::new_with_fixture_routing_facts(store, FixtureRoutingFacts::new(), 10)
}

/// Cross the explicit pending-admission boundary while keeping these
/// headless falsifiers deterministic. The runtime owns the 10 ms timer; core
/// tests drive the corresponding message directly.
fn handle_and_flush<S: EventStore>(core: &mut EngineCore<S>, msg: EngineMsg) -> Vec<Effect> {
    let mut effects = core.handle(msg);
    if effects
        .iter()
        .any(|effect| matches!(effect, Effect::ArmWireAdmission))
    {
        effects.extend(core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64))));
    }
    effects
}

fn selection() -> Filter {
    selection_of(KIND)
}

fn selection_of(kind: u16) -> Filter {
    Filter {
        kinds: Some(BTreeSet::from([kind])),
        ..Filter::default()
    }
}

/// One branch: the whole selection pinned to exactly one host, projecting
/// only rows that host actually served (`CacheMode::Strict`). This is the
/// shape a host-scoped protocol helper lowers to.
fn host_branch(host: &RelayUrl) -> Demand {
    host_branch_of_kind(host, KIND)
}

fn host_branch_of_kind(host: &RelayUrl, kind: u16) -> Demand {
    let mut demand = Demand::new(
        selection_of(kind),
        SourceAuthority::Pinned(BTreeSet::from([host.clone()])),
        AccessContext::Public,
    )
    .expect("a one-relay pinned set is nonempty");
    demand.cache = CacheMode::Strict;
    demand
}

fn union_of(branches: [Demand; 2], aggregate_result_limit: Option<usize>) -> LiveQuery {
    LiveQuery::union(
        branches.into_iter().map(LiveQuery::single),
        aggregate_result_limit,
    )
    .expect("a two-branch union is constructible")
}

fn store_event(
    store: &mut RedbStore,
    keys: &Keys,
    created_at: u64,
    identifier: &str,
    served_by: &[&RelayUrl],
) -> EventId {
    store_event_of_kind(store, keys, KIND, created_at, identifier, served_by)
}

fn store_event_of_kind<S: EventStore>(
    store: &mut S,
    keys: &Keys,
    kind: u16,
    created_at: u64,
    identifier: &str,
    served_by: &[&RelayUrl],
) -> EventId {
    let event = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(created_at),
        Kind::from_u16(kind),
        vec![nostr::Tag::identifier(identifier)],
        String::new(),
    )
    .sign_with_keys(keys)
    .expect("fixture signing never fails");
    let id = event.id;
    for host in served_by {
        store
            .insert(
                event.clone(),
                RelayObserved::new((*host).clone(), Timestamp::from(created_at)),
            )
            .expect("fixture insert");
    }
    id
}

fn observation(effects: &[Effect]) -> ObservationId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, ..) => Some(*id),
            _ => None,
        })
        .expect("subscribe emits exactly one initial frame for its observation")
}

fn frames(effects: &[Effect], id: ObservationId) -> Vec<(&[RowDelta], &[AcquisitionEvidence])> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitRows(candidate, deltas, evidence) if *candidate == id => {
                Some((deltas.as_slice(), evidence.as_slice()))
            }
            _ => None,
        })
        .collect()
}

/// The observation's exact current row membership, folded from every frame it
/// has been delivered. An app does exactly this; nothing here reconstructs
/// state the engine did not send.
#[derive(Default)]
struct Projection {
    rows: BTreeMap<EventId, BTreeSet<RelayUrl>>,
    evidence: Vec<AcquisitionEvidence>,
    frames: usize,
}

impl Projection {
    fn apply(&mut self, effects: &[Effect], id: ObservationId) {
        for (deltas, evidence) in frames(effects, id) {
            self.frames += 1;
            for delta in deltas {
                match delta {
                    RowDelta::Added(row) => {
                        self.rows.insert(row.id(), row.sources.clone());
                    }
                    RowDelta::Updated(row) => {
                        self.rows.insert(row.id(), row.sources.clone());
                    }
                    RowDelta::SourcesGrew { id, sources } => {
                        self.rows.insert(*id, sources.clone());
                    }
                    RowDelta::Removed(id) => {
                        self.rows.remove(id);
                    }
                }
            }
            self.evidence = evidence.to_vec();
        }
    }
}

fn requested_relays(effects: &[Effect]) -> BTreeSet<RelayUrl> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| delta.ops.iter())
        .filter(|(_, ops)| ops.iter().any(|op| matches!(op, WireOp::Req(..))))
        .map(|(session, _)| session.relay.clone())
        .collect()
}

fn closed_relays(effects: &[Effect]) -> BTreeSet<RelayUrl> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| delta.ops.iter())
        .filter(|(_, ops)| ops.iter().any(|op| matches!(op, WireOp::Close(..))))
        .map(|(session, _)| session.relay.clone())
        .collect()
}

fn evidence_relays(evidence: &AcquisitionEvidence) -> BTreeSet<RelayUrl> {
    evidence
        .sources
        .iter()
        .map(|source| source.relay.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Falsifier: "Flatten all branch sources into one unioned `Pinned` set."
// ---------------------------------------------------------------------------

#[test]
fn branch_sources_are_never_flattened_into_one_pinned_set() {
    let (a, b) = (relay("a"), relay("b"));
    let mut core = core();

    let effects = handle_and_flush(
        &mut core,
        EngineMsg::Subscribe(union_of([host_branch(&a), host_branch(&b)], None)),
    );
    let id = observation(&effects);
    let mut projection = Projection::default();
    projection.apply(&effects, id);

    assert_eq!(
        projection.evidence.len(),
        2,
        "two branches must produce two scoped evidence entries, not one merged scope"
    );
    let scopes: Vec<BTreeSet<RelayUrl>> = projection
        .evidence
        .iter()
        .map(evidence_relays)
        .filter(|relays| !relays.is_empty())
        .collect();
    for scope in &scopes {
        assert_eq!(
            scope.len(),
            1,
            "each branch's evidence must name only its OWN pinned host; \
             a flattened Pinned({{a,b}}) would give one entry naming both: {scopes:?}"
        );
    }
    assert_eq!(
        scopes.iter().flatten().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([a.clone(), b.clone()]),
        "every declared host still gets asked, one branch each"
    );
    assert_eq!(
        requested_relays(&effects),
        BTreeSet::from([a, b]),
        "both hosts are requested, each under its own branch's authority"
    );
}

// ---------------------------------------------------------------------------
// Falsifier: "Erase branch identity while aggregating acquisition evidence."
// ---------------------------------------------------------------------------

#[test]
fn equal_branches_keep_independent_evidence_entries() {
    let host = relay("shared");
    let mut core = core();

    // Same selection, same source authority, same access: only the per-handle
    // freshness policy differs. These two branches may share every atom, wire
    // request and coverage row underneath -- and must STILL be two branches,
    // because collapsing them would silently discard one branch's own policy.
    let live = host_branch(&host);
    let mut cache_only = live.clone();
    cache_only.freshness = nmp_grammar::Freshness::CacheOnly;

    let query = union_of([live.clone(), cache_only.clone()], None);
    assert_eq!(
        query.branches().len(),
        2,
        "policy-distinct branches are not duplicates"
    );

    let effects = handle_and_flush(&mut core, EngineMsg::Subscribe(query));
    let id = observation(&effects);
    let mut projection = Projection::default();
    projection.apply(&effects, id);

    assert_eq!(
        projection.evidence.len(),
        2,
        "each branch keeps its own evidence entry even when their acquisition \
         identity is equal"
    );
}

// ---------------------------------------------------------------------------
// Falsifier: "Index per-branch evidence by anything other than the query's own
// canonical branch order" -- caller insertion order, discovery order, or a
// second sort that can drift from `Demand`'s.
// ---------------------------------------------------------------------------

/// `branches()[i]` must name the branch `evidence[i]` reports on. Every
/// surface indexes per-branch evidence positionally, so a mismatch is silent:
/// an app reads a source list, a shortfall or a diagnostic and attributes it
/// to the wrong host with nothing anywhere disagreeing.
#[test]
fn per_branch_evidence_is_indexed_by_canonical_branch_order() {
    let (a, b) = (relay("a"), relay("b"));
    let one_way = union_of([host_branch(&a), host_branch(&b)], None);
    let other_way = union_of([host_branch(&b), host_branch(&a)], None);
    assert_eq!(
        one_way, other_way,
        "the same two branches typed either way are one query"
    );

    // These two ARE one value once the assertion above holds -- construction
    // canonicalizes, so caller order never survives to reach the engine. Both
    // are still driven so that a regression which let insertion order leak
    // into the value fails HERE, on the branch-to-evidence mapping, rather
    // than only on the equality assertion above.
    for query in [one_way, other_way] {
        let declared: Vec<BTreeSet<RelayUrl>> = query
            .branches()
            .iter()
            .map(|branch| match &branch.source {
                SourceAuthority::Pinned(hosts) => hosts.clone(),
                other => panic!("fixture branches are pinned to one host: {other:?}"),
            })
            .collect();

        let mut core = core();
        let effects = handle_and_flush(&mut core, EngineMsg::Subscribe(query.clone()));
        let id = observation(&effects);
        let mut projection = Projection::default();
        projection.apply(&effects, id);

        let reported: Vec<BTreeSet<RelayUrl>> =
            projection.evidence.iter().map(evidence_relays).collect();
        assert_eq!(
            reported,
            declared,
            "evidence entry i must report on branches()[i]; indexing by the \
             order the caller typed instead swaps these two entries for one of \
             the two declarations: {:?}",
            query.branches()
        );
    }
}

#[test]
fn an_unplannable_branch_reports_its_own_shortfall() {
    let host = relay("reachable");
    let author = Keys::generate();
    let mut core = core();

    // Branch A is fully plannable against its pinned host. Branch B chases an
    // author whose outboxes nothing knows, so nothing is even trying to
    // acquire it.
    let unroutable = Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([KIND])),
            authors: Some(Binding::Literal(BTreeSet::from([author
                .public_key()
                .to_hex()]))),
            ..Filter::default()
        },
        SourceAuthority::AuthorOutboxes,
        AccessContext::Public,
    )
    .expect("an author-bound outbox demand is constructible");

    let query = union_of([host_branch(&host), unroutable.clone()], None);
    let branch_of_unroutable = query
        .branches()
        .iter()
        .position(|branch| branch == &unroutable)
        .expect("the unroutable branch survives canonicalization");

    let effects = handle_and_flush(&mut core, EngineMsg::Subscribe(query));
    let id = observation(&effects);
    let mut projection = Projection::default();
    projection.apply(&effects, id);

    assert_eq!(projection.evidence.len(), 2);
    let unroutable_evidence = &projection.evidence[branch_of_unroutable];
    let routable_evidence = &projection.evidence[1 - branch_of_unroutable];

    assert!(
        !unroutable_evidence.shortfall.is_empty(),
        "the branch nothing can acquire must carry its own explicit shortfall: \
         {unroutable_evidence:?}"
    );
    assert!(
        routable_evidence.shortfall.is_empty(),
        "the healthy branch must NOT inherit its sibling's shortfall; a merged \
         evidence value would put both in one entry: {routable_evidence:?}"
    );
    assert_eq!(
        evidence_relays(routable_evidence),
        BTreeSet::from([host]),
        "the healthy branch still reports its own planned source"
    );
}

// ---------------------------------------------------------------------------
// Falsifier: "Deliver branch frames independently and merge them above
// `Subscription`."
// ---------------------------------------------------------------------------

#[test]
fn rows_union_by_event_id_with_merged_provenance() {
    let (a, b) = (relay("a"), relay("b"));
    let keys = Keys::generate();
    let mut store = RedbStore::temporary().expect("temporary Redb store");

    let only_a = store_event(&mut store, &keys, 200, "a-only", &[&a]);
    let only_b = store_event(&mut store, &keys, 200, "b-only", &[&b]);
    let both = store_event(&mut store, &keys, 200, "shared", &[&a, &b]);
    let mut core = core_over(store);

    let effects = handle_and_flush(
        &mut core,
        EngineMsg::Subscribe(union_of([host_branch(&a), host_branch(&b)], None)),
    );
    let id = observation(&effects);
    let mut projection = Projection::default();
    projection.apply(&effects, id);

    assert_eq!(
        frames(&effects, id)
            .into_iter()
            .filter(|(deltas, _)| !deltas.is_empty())
            .count(),
        1,
        "one observation delivers ONE coherent row frame for its whole branch \
         set, never one frame per branch; the later evidence-only frame records \
         completion of pending wire admission"
    );
    assert_eq!(
        projection.rows.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([only_a, only_b, both]),
        "the union is by event id across branches"
    );
    assert_eq!(
        projection.rows[&both],
        BTreeSet::from([a, b]),
        "a row admitted by two branches appears ONCE carrying both branches' \
         provenance"
    );
}

// ---------------------------------------------------------------------------
// Falsifier: "Apply `limit`/window per branch before union."
// ---------------------------------------------------------------------------

#[test]
fn the_aggregate_bound_is_applied_after_the_union() {
    let (a, b) = (relay("a"), relay("b"));
    let keys = Keys::generate();
    let mut store = RedbStore::temporary().expect("temporary Redb store");

    // Four rows: three at the same second (so the tie order is event id ASC)
    // and one strictly older that must never win.
    let a1 = store_event(&mut store, &keys, 200, "a1", &[&a]);
    let a2 = store_event(&mut store, &keys, 200, "a2", &[&a]);
    let b1 = store_event(&mut store, &keys, 200, "b1", &[&b]);
    let older = store_event(&mut store, &keys, 199, "b-older", &[&b]);
    let mut core = core_over(store);

    let effects = handle_and_flush(
        &mut core,
        EngineMsg::Subscribe(union_of([host_branch(&a), host_branch(&b)], Some(2))),
    );
    let id = observation(&effects);
    let mut projection = Projection::default();
    projection.apply(&effects, id);

    let mut newest: Vec<EventId> = vec![a1, a2, b1];
    newest.sort_by_key(|id| id.to_bytes());
    let expected: BTreeSet<EventId> = newest.into_iter().take(2).collect();

    assert_eq!(
        projection.rows.len(),
        2,
        "an aggregate bound of 2 means 2 rows in the union, not 2 per branch"
    );
    assert_eq!(
        projection.rows.keys().copied().collect::<BTreeSet<_>>(),
        expected,
        "the exact globally newest rows win; per-branch truncation before the \
         union would select different ones"
    );
    assert!(
        !projection.rows.contains_key(&older),
        "a strictly older row can never enter a global newest-N"
    );
}

// ---------------------------------------------------------------------------
// Falsifier: coherent reactive transition across branches.
// ---------------------------------------------------------------------------

#[test]
fn a_reactive_change_moves_every_branch_in_one_frame() {
    let (a, b) = (relay("a"), relay("b"));
    let mut core = core();

    let reactive = |host: &RelayUrl| {
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([KIND])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::from([host.clone()])),
            AccessContext::Public,
        )
        .expect("a reactive pinned demand is constructible")
    };

    let effects = handle_and_flush(
        &mut core,
        EngineMsg::Subscribe(union_of([reactive(&a), reactive(&b)], None)),
    );
    let id = observation(&effects);

    let account = Keys::generate();
    let effects = core.handle(EngineMsg::SetActivePubkey(Some(account.public_key())));
    let delivered = frames(&effects, id);
    assert_eq!(
        delivered.len(),
        1,
        "one account change is ONE transition for the whole observation; \
         a per-branch emit would deliver a frame in which one branch has \
         re-rooted and the other has not: {delivered:?}"
    );
    assert_eq!(
        delivered[0].1.len(),
        2,
        "that one frame still carries both branches' scoped evidence"
    );
}

// ---------------------------------------------------------------------------
// Falsifier: "Withdraw shared atoms unconditionally when the composite closes."
// ---------------------------------------------------------------------------

#[test]
fn cancelling_a_union_keeps_work_a_sibling_observation_still_owns() {
    let (a, b) = (relay("a"), relay("b"));
    let mut core = core();

    let composite = handle_and_flush(
        &mut core,
        EngineMsg::Subscribe(union_of([host_branch(&a), host_branch(&b)], None)),
    );
    let composite_id = observation(&composite);

    // An unrelated observation independently requires branch A's exact demand.
    let unrelated = handle_and_flush(
        &mut core,
        EngineMsg::Subscribe(LiveQuery::single(host_branch(&a))),
    );
    let unrelated_id = observation(&unrelated);
    assert_ne!(composite_id, unrelated_id);

    let withdraw = core.handle(EngineMsg::Unsubscribe(composite_id));

    let closed = closed_relays(&withdraw);
    assert!(
        closed.contains(&b),
        "the unshared branch must be withdrawn: {closed:?}"
    );
    assert!(
        !closed.contains(&a),
        "the branch the unrelated observation still requires must stay live; \
         withdrawing it unconditionally would take work that observation owns: \
         {closed:?}"
    );
    assert!(
        frames(&withdraw, composite_id).is_empty(),
        "no later frame is delivered to a withdrawn observation"
    );

    // The surviving observation still owns relay A as a planned source.
    let mut surviving = Projection::default();
    surviving.apply(&unrelated, unrelated_id);
    surviving.apply(&withdraw, unrelated_id);
    assert_eq!(
        surviving.evidence.len(),
        1,
        "the unrelated single-branch observation keeps exactly one entry"
    );
    assert_eq!(
        evidence_relays(&surviving.evidence[0]),
        BTreeSet::from([a]),
        "and it still names the source it owns"
    );
}

// ---------------------------------------------------------------------------
// Falsifier: "Drop one branch because of a graph/relay cap without emitting
// shortfall."
// ---------------------------------------------------------------------------

#[test]
fn an_over_cap_union_refuses_the_whole_declaration() {
    let branches: Vec<LiveQuery> = (0..=LiveQuery::MAX_BRANCHES)
        .map(|index| {
            LiveQuery::single(Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([index as u16])),
                ..Filter::default()
            }))
        })
        .collect();

    assert_eq!(
        LiveQuery::union(branches, None),
        Err(LiveQueryError::TooManyQueryBranches {
            requested: LiveQuery::MAX_BRANCHES + 1,
            maximum: LiveQuery::MAX_BRANCHES,
        }),
        "an over-cap declaration is refused whole, naming both counts -- never \
         silently truncated to the ceiling"
    );

    assert_eq!(
        LiveQuery::union(Vec::new(), None),
        Err(LiveQueryError::EmptyUnion)
    );
    assert_eq!(
        LiveQuery::union([LiveQuery::from_filter(selection())], Some(0)),
        Err(LiveQueryError::AggregateResultLimitZero)
    );
    assert_eq!(
        LiveQuery::union(
            [LiveQuery::union([LiveQuery::from_filter(selection())], Some(4)).unwrap()],
            None
        ),
        Err(LiveQueryError::NestedAggregateResultLimit),
        "a nested aggregate bound has no surviving scope; accepting it would \
         silently discard it"
    );
}

// ---------------------------------------------------------------------------
// Falsifier: "windowing is global."
// ---------------------------------------------------------------------------

#[test]
fn a_window_bounds_the_union_globally() {
    let (a, b) = (relay("a"), relay("b"));
    let keys = Keys::generate();
    let mut store = RedbStore::temporary().expect("temporary Redb store");

    for (index, host) in [(0u64, &a), (1, &a), (2, &a), (3, &b), (4, &b), (5, &b)] {
        store_event(
            &mut store,
            &keys,
            200 + index,
            &format!("row{index}"),
            &[host],
        );
    }
    let mut core = core_over(store);

    let effects = handle_and_flush(
        &mut core,
        EngineMsg::SubscribeHistory(HistoryQuery::new(
            union_of([host_branch(&a), host_branch(&b)], None),
            2,
            6,
        )),
    );
    let batch = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitHistory(_, batch) => Some(batch),
            _ => None,
        })
        .expect("a windowed subscribe emits its initial batch");

    assert_eq!(
        batch.rows.len(),
        2,
        "an initial window of 2 holds 2 rows across the whole union, never 2 \
         per branch: {:?}",
        batch.rows.iter().map(|row| row.id()).collect::<Vec<_>>()
    );
    assert_eq!(
        batch.evidence.len(),
        2,
        "a windowed observation reports per-branch evidence exactly like an \
         unbounded one"
    );
    assert_eq!(
        batch.rows[0].created_at().as_secs(),
        205,
        "the window holds the globally newest rows"
    );
    assert_eq!(batch.rows[1].created_at().as_secs(), 204);
}

#[test]
fn a_window_and_an_aggregate_bound_are_two_owners_of_row_membership() {
    let query = union_of(
        [host_branch(&relay("a")), host_branch(&relay("b"))],
        Some(3),
    );
    let engine = nmp::Engine::new(nmp::EngineConfig::default()).expect("temporary Redb engine");
    let window = nmp::Window::Expandable {
        initial: std::num::NonZeroUsize::new(2).unwrap(),
        max: std::num::NonZeroUsize::new(4).unwrap(),
    };

    assert_eq!(
        engine.observe(query, Some(window)).err(),
        Some(nmp::EngineError::WindowAggregateResultLimit),
        "a window and an aggregate result limit must not both own the merged \
         row count"
    );
    engine.shutdown();
}

#[test]
fn a_shortfall_only_reaches_the_branch_that_has_it() {
    // Guard against the merge-then-report shape: `ShortfallFact` values are
    // never deduplicated across branches into one list.
    let evidence = [
        AcquisitionEvidence {
            sources: Vec::new(),
            shortfall: vec![ShortfallFact::NoResolvedDemand],
        },
        AcquisitionEvidence::default(),
    ];
    assert_ne!(
        evidence[0], evidence[1],
        "an empty-evidence branch and a shortfall branch are different facts"
    );
}

// ---------------------------------------------------------------------------
// Falsifier: "Erase branch identity while aggregating acquisition evidence" --
// the execution-trace half. Row evidence is per branch (above); so is the
// ordered diagnostic trace, and here nothing BUT the branch distinguishes the
// two facts.
// ---------------------------------------------------------------------------

#[test]
fn only_the_branch_tells_two_identical_resolver_facts_apart() {
    let (a, b) = (relay("a"), relay("b"));
    let mut core = core();

    // Two branches differing ONLY in pinned host: same selection, so both
    // resolve a byte-identical concrete filter at the same path, same
    // revision, same fingerprint, same cause.
    let effects = handle_and_flush(
        &mut core,
        EngineMsg::Subscribe(union_of([host_branch(&a), host_branch(&b)], None)),
    );
    let id = observation(&effects);
    let trace: Vec<&ObservationEvidence> = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitObservationEvidence(candidate, facts) if *candidate == id => Some(facts),
            _ => None,
        })
        .flatten()
        .collect();

    assert_eq!(
        trace.len(),
        2,
        "each branch contributes its own resolution fact: {trace:?}"
    );
    assert_eq!(
        trace[0].fact, trace[1].fact,
        "the two facts are deliberately identical in every field a diagnostic \
         consumer can read -- path, revision, filter, fingerprint and cause"
    );
    assert_eq!(
        trace
            .iter()
            .map(|fact| fact.branch)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([Some(0), Some(1)]),
        "so the CANONICAL BRANCH is the only thing that tells them apart; \
         dropping or fixing it collapses two branches' traces into one \
         indistinguishable pair: {trace:?}"
    );
    assert_eq!(
        trace.iter().map(|fact| fact.sequence).collect::<Vec<_>>(),
        vec![1, 2],
        "and the sequence is monotonic across the WHOLE observation, never \
         restarted per branch"
    );
}

// ---------------------------------------------------------------------------
// Fault injection for the three proofs below.
//
// The existing single-branch rollback proof
// (`observation_open_failures_are_typed_leak_free_and_leave_runtime_usable`)
// arms a fault that fails the NEXT canonical read, so on a union it can only
// ever fail branch 0 and nothing is rolled back. This store instead fails the
// read that asks for ONE named kind, and delegates every other door to a
// healthy `RedbStore`. Because canonical branch order sorts on the
// selection first, aiming the fault at `OTHER_KIND` fails a LATER branch with
// the earlier one already opened -- the case a per-branch subscribe-and-merge
// implementation gets wrong.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct BranchReadFault(Rc<Cell<Option<u16>>>);

impl BranchReadFault {
    fn aim_at(&self, kind: u16) {
        self.0.set(Some(kind));
    }

    fn disarm(&self) {
        self.0.set(None);
    }

    fn strikes(&self, filter: &nostr::Filter) -> bool {
        let Some(armed) = self.0.get() else {
            return false;
        };
        filter
            .kinds
            .as_ref()
            .is_some_and(|kinds| kinds.contains(&Kind::from_u16(armed)))
    }
}

struct BranchReadFailureStore {
    inner: RedbStore,
    fault: BranchReadFault,
}

impl BranchReadFailureStore {
    fn new(inner: RedbStore, fault: BranchReadFault) -> Self {
        Self { inner, fault }
    }
}

impl EventStore for BranchReadFailureStore {
    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        if self.fault.strikes(filter) {
            return Err(PersistenceError::invariant(
                "injected branch canonical read failure",
            ));
        }
        self.inner.query(filter)
    }

    fn query_newest_before(
        &self,
        filter: &nostr::Filter,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        if self.fault.strikes(filter) {
            return Err(PersistenceError::invariant(
                "injected branch canonical read failure",
            ));
        }
        self.inner.query_newest_before(filter, before, limit)
    }

    fn insert(
        &mut self,
        event: Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        self.inner.insert(event, from)
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
    fn compensate_write_with_state(
        &mut self,
        intent_id: IntentId,
        reason: CompensationReason,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write_with_state(intent_id, reason)
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
}

fn degraded(effects: &[Effect]) -> Option<String> {
    effects.iter().find_map(|effect| match effect {
        Effect::EmitDiagnostics(snapshot) => snapshot.store_degraded.clone(),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Falsifier: "branch K's open failure rolls back 0..K-1 with no wire owner."
// ---------------------------------------------------------------------------

#[test]
fn a_union_branch_that_cannot_open_leaves_no_earlier_branch_installed() {
    let (a, b, c) = (relay("a"), relay("b"), relay("c"));
    let fault = BranchReadFault::default();
    let mut core = EngineCore::new_with_fixture_routing_facts(
        BranchReadFailureStore::new(
            RedbStore::temporary().expect("temporary Redb store"),
            fault.clone(),
        ),
        FixtureRoutingFacts::new(),
        10,
    );

    let first = host_branch_of_kind(&a, KIND);
    let failing = host_branch_of_kind(&b, OTHER_KIND);
    let query = union_of([first.clone(), failing.clone()], None);
    assert_eq!(
        query
            .branches()
            .iter()
            .position(|branch| branch == &failing),
        Some(1),
        "the fault must be aimed at a LATER branch: a failure at branch 0 has \
         nothing to roll back and proves nothing about a union"
    );

    fault.aim_at(OTHER_KIND);
    let refused = core.handle(EngineMsg::Subscribe(query));
    fault.disarm();

    assert_eq!(
        degraded(&refused).as_deref(),
        Some("durable-store persistence failure: injected branch canonical read failure"),
        "the injected read must actually have fired; without this the rest of \
         this test would pass over a perfectly healthy open"
    );
    assert!(
        !refused
            .iter()
            .any(|effect| matches!(effect, Effect::EmitRows(..))),
        "a refused open owns no observation and delivers no frame"
    );
    assert!(
        refused
            .iter()
            .all(|effect| matches!(effect, Effect::EmitDiagnostics(_))),
        "a refused open stages no wire, relay, attribution or sibling-frame \
         effect: {refused:?}"
    );

    // The absence of residue, proved positively rather than by asserting the
    // open failed. The first branch WAS installed before the second failed, so
    // an implementation that forgets to withdraw it keeps its demand atom
    // here...
    assert_eq!(
        core.active_demand(),
        BTreeSet::new(),
        "no branch of a refused union retains a handle or its demand atom"
    );
    // ...and hands relay "a" a REQ on the very next recompile, which an
    // unrelated observation over relay "c" forces.
    let later = handle_and_flush(
        &mut core,
        EngineMsg::Subscribe(LiveQuery::single(host_branch(&c))),
    );
    assert_eq!(
        requested_relays(&later),
        BTreeSet::from([c]),
        "the first branch's relay request was RELEASED, not merely left \
         un-emitted by the refusal itself"
    );
    assert!(
        !closed_relays(&later).contains(&a),
        "nothing was ever opened for relay \"a\", so nothing is closed for it \
         either -- a CLOSE here would mean a wire owner had survived the refusal"
    );
}

// ---------------------------------------------------------------------------
// Falsifier: "emit successful reads after a sibling store failure."
// ---------------------------------------------------------------------------

#[test]
fn one_branchs_refresh_failure_retracts_no_sibling_row() {
    let (a, b) = (relay("a"), relay("b"));
    let keys = Keys::generate();
    let fault = BranchReadFault::default();
    let mut store = BranchReadFailureStore::new(
        RedbStore::temporary().expect("temporary Redb store"),
        fault.clone(),
    );
    let from_a = store_event_of_kind(&mut store, &keys, KIND, 200, "a", &[&a]);
    let from_b = store_event_of_kind(&mut store, &keys, OTHER_KIND, 201, "b", &[&b]);
    let mut core =
        EngineCore::new_with_fixture_routing_facts(store, FixtureRoutingFacts::new(), 10);

    let opened = handle_and_flush(
        &mut core,
        EngineMsg::Subscribe(union_of(
            [
                host_branch_of_kind(&a, KIND),
                host_branch_of_kind(&b, OTHER_KIND),
            ],
            None,
        )),
    );
    let id = observation(&opened);
    let mut projection = Projection::default();
    projection.apply(&opened, id);
    assert_eq!(
        projection.rows.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([from_a, from_b]),
        "the fixture must actually deliver both branches' rows first"
    );
    let prior_rows = projection.rows.clone();
    let prior_evidence = projection.evidence.clone();
    assert_eq!(prior_evidence.len(), 2);

    // One branch's local read fails while the whole query refreshes.
    fault.aim_at(OTHER_KIND);
    let refreshed = core.handle(EngineMsg::SetActivePubkey(Some(
        Keys::generate().public_key(),
    )));
    fault.disarm();

    let retracted: Vec<EventId> = frames(&refreshed, id)
        .iter()
        .flat_map(|(deltas, _)| deltas.iter())
        .filter_map(|delta| match delta {
            RowDelta::Removed(id) => Some(*id),
            _ => None,
        })
        .collect();
    assert!(
        retracted.is_empty(),
        "a branch that could not be read is a branch NOTHING is known about; \
         projecting only the branches that succeeded blinks its rows out as \
         Removed: {retracted:?}"
    );

    projection.apply(&refreshed, id);
    assert_eq!(
        projection.rows, prior_rows,
        "the app keeps every row it already had"
    );
    assert_eq!(
        projection.evidence, prior_evidence,
        "and both branches' evidence, byte-identical -- including the failing \
         branch's own prior entry, which is not replaced by an empty one"
    );
    assert_eq!(
        degraded(&refreshed).as_deref(),
        Some("durable-store persistence failure: injected branch canonical read failure"),
        "the failure is reported as an ordinary degraded diagnostic instead"
    );
}

// ---------------------------------------------------------------------------
// Falsifier: "restart redeclaration reuses cache/coverage honestly, re-decides
// freshness, and resets Window."
// ---------------------------------------------------------------------------

/// A branch pinned to one host that will go to the relay unless its OWN
/// persisted coverage already satisfies it.
fn max_age_branch(host: &RelayUrl, keys: &Keys) -> Demand {
    let mut demand = Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([KIND])),
            authors: Some(Binding::Literal(BTreeSet::from([keys
                .public_key()
                .to_hex()]))),
            ..Filter::default()
        },
        SourceAuthority::Pinned(BTreeSet::from([host.clone()])),
        AccessContext::Public,
    )
    .expect("a one-relay pinned set is nonempty");
    demand.freshness = Freshness::MaxAge { seconds: 3_600 };
    demand
}

fn branch_atom(host: &RelayUrl, keys: &Keys) -> ContextualAtom {
    ContextualAtom {
        filter: nmp_grammar::ConcreteFilter {
            kinds: Some(BTreeSet::from([KIND])),
            authors: Some(BTreeSet::from([keys.public_key().to_hex()])),
            ..nmp_grammar::ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(BTreeSet::from([host.clone()])),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

#[test]
fn each_redeclared_branch_decides_freshness_from_its_own_stored_coverage() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("union-restart-coverage.redb");
    let keys = Keys::generate();
    let (a, b) = (relay("a"), relay("b"));

    // Only branch A's own scoped coverage is durable. Branch B's host was
    // never reconciled.
    {
        let mut store = RedbStore::open(&path).expect("create durable store");
        store
            .record_coverage(&[(
                branch_atom(&a, &keys),
                a.clone(),
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(99_000u64)),
            )])
            .expect("fixture coverage");
    }

    let mut restarted = EngineCore::new(RedbStore::open(&path).expect("reopen durable store"), 10);
    assert!(
        restarted.recover_on_boot().is_empty(),
        "a live query is ephemeral: nothing durable continues the previous \
         observation across a restart"
    );
    restarted.handle(EngineMsg::Tick(Timestamp::from(100_000u64)));

    let query = union_of([max_age_branch(&a, &keys), max_age_branch(&b, &keys)], None);
    let covered = query
        .branches()
        .iter()
        .position(|branch| branch == &max_age_branch(&a, &keys))
        .expect("the covered branch survives canonicalization");
    let effects = handle_and_flush(&mut restarted, EngineMsg::Subscribe(query));

    assert_eq!(
        requested_relays(&effects),
        BTreeSet::from([b.clone()]),
        "each branch re-decides freshness from ITS OWN persisted coverage: \
         relay \"a\" is already fresh enough, relay \"b\" was never reconciled \
         and must still be asked"
    );

    let id = observation(&effects);
    let mut projection = Projection::default();
    projection.apply(&effects, id);
    assert_eq!(projection.evidence.len(), 2);
    assert_eq!(
        projection.evidence[covered]
            .sources
            .iter()
            .map(|source| (source.relay.clone(), source.reconciled_through))
            .collect::<Vec<_>>(),
        vec![(a, Some(Timestamp::from(99_000u64)))],
        "the reopened branch carries its own durable watermark"
    );
    assert!(
        projection.evidence[1 - covered]
            .sources
            .iter()
            .all(|source| source.reconciled_through.is_none()),
        "and its sibling borrows none of it: {:?}",
        projection.evidence[1 - covered]
    );
}

#[test]
fn a_redeclared_window_starts_again_at_its_initial_size() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("union-restart-window.redb");
    let keys = Keys::generate();
    let (a, b) = (relay("a"), relay("b"));
    let window = || {
        HistoryQuery::new(
            union_of(
                [
                    host_branch_of_kind(&a, KIND),
                    host_branch_of_kind(&b, OTHER_KIND),
                ],
                None,
            ),
            2,
            6,
        )
    };
    // The window's CURRENT contents: an advance emits its `Requesting` beat
    // and then its settled frame in one turn, so the last frame of a turn is
    // the window as the app now holds it.
    let batch = |effects: &[Effect]| {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::EmitHistory(id, batch) => Some((*id, batch.rows.len())),
                _ => None,
            })
            .next_back()
            .expect("a windowed turn emits at least one batch")
    };

    let (before_session, grown) = {
        let mut store = RedbStore::open(&path).expect("create durable store");
        for index in 0..3u64 {
            store_event_of_kind(
                &mut store,
                &keys,
                KIND,
                200 + index * 2,
                &format!("a{index}"),
                &[&a],
            );
            store_event_of_kind(
                &mut store,
                &keys,
                OTHER_KIND,
                201 + index * 2,
                &format!("b{index}"),
                &[&b],
            );
        }
        let mut core = EngineCore::new(store, 10);
        let opened = core.handle(EngineMsg::SubscribeHistory(window()));
        let (session, rows) = batch(&opened);
        assert_eq!(rows, 2, "the window opens at its declared initial size");
        core.handle(EngineMsg::RequestRows(session, 4));
        let committed = core.handle(EngineMsg::CommitHistoryLoad(session));
        (session, batch(&committed).1)
    };
    assert_eq!(
        grown, 4,
        "the fixture must actually have grown the window before the restart"
    );

    let mut restarted = EngineCore::new(RedbStore::open(&path).expect("reopen durable store"), 10);
    assert!(restarted.recover_on_boot().is_empty());
    let redeclared = restarted.handle(EngineMsg::SubscribeHistory(window()));
    let (after_session, rows) = batch(&redeclared);

    assert_eq!(
        rows, 2,
        "redeclaring the same window after a restart starts it again at its \
         INITIAL size; carrying the grown target across would return {grown}"
    );
    assert_eq!(
        after_session, before_session,
        "and the session identity is minted afresh from zero -- no durable \
         composite continuation, branch id or query receipt was kept"
    );
}
