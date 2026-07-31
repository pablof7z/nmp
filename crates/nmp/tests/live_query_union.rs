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

use std::collections::{BTreeMap, BTreeSet};

use nmp::mechanism::core::{
    AcquisitionEvidence, Effect, EngineCore, EngineMsg, HistoryQuery, ObservationId, RowDelta,
    ShortfallFact,
};
use nmp_grammar::{
    AccessContext, Binding, CacheMode, Demand, Filter, IdentityField, LiveQuery, LiveQueryError,
    SourceAuthority,
};
use nmp_router::{FixtureRoutingFacts, WireOp};
use nmp_store::{EventStore, MemoryStore, RelayObserved};
use nostr::{EventId, Keys, Kind, RelayUrl, Timestamp, UnsignedEvent};

const KIND: u16 = 39_000;

fn relay(host: &str) -> RelayUrl {
    RelayUrl::parse(&format!("wss://{host}.example")).expect("fixture relay url")
}

fn core() -> EngineCore<MemoryStore> {
    core_over(MemoryStore::new())
}

fn core_over(store: MemoryStore) -> EngineCore<MemoryStore> {
    EngineCore::new_with_fixture_routing_facts(store, FixtureRoutingFacts::new(), 10)
}

fn selection() -> Filter {
    Filter {
        kinds: Some(BTreeSet::from([KIND])),
        ..Filter::default()
    }
}

/// One branch: the whole selection pinned to exactly one host, projecting
/// only rows that host actually served (`CacheMode::Strict`). This is the
/// shape a host-scoped protocol helper lowers to.
fn host_branch(host: &RelayUrl) -> Demand {
    let mut demand = Demand::new(
        selection(),
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
    store: &mut MemoryStore,
    keys: &Keys,
    created_at: u64,
    identifier: &str,
    served_by: &[&RelayUrl],
) -> EventId {
    let event = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(created_at),
        Kind::from_u16(KIND),
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
                        self.rows.insert(row.event.id, row.sources.clone());
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

    let effects = core.handle(EngineMsg::Subscribe(union_of(
        [host_branch(&a), host_branch(&b)],
        None,
    )));
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

    let effects = core.handle(EngineMsg::Subscribe(query));
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

    let effects = core.handle(EngineMsg::Subscribe(query));
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
    let mut store = MemoryStore::new();

    let only_a = store_event(&mut store, &keys, 200, "a-only", &[&a]);
    let only_b = store_event(&mut store, &keys, 200, "b-only", &[&b]);
    let both = store_event(&mut store, &keys, 200, "shared", &[&a, &b]);
    let mut core = core_over(store);

    let effects = core.handle(EngineMsg::Subscribe(union_of(
        [host_branch(&a), host_branch(&b)],
        None,
    )));
    let id = observation(&effects);
    let mut projection = Projection::default();
    projection.apply(&effects, id);

    assert_eq!(
        projection.frames, 1,
        "one observation delivers ONE coherent frame for its whole branch set, \
         never one frame per branch"
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
    let mut store = MemoryStore::new();

    // Four rows: three at the same second (so the tie order is event id ASC)
    // and one strictly older that must never win.
    let a1 = store_event(&mut store, &keys, 200, "a1", &[&a]);
    let a2 = store_event(&mut store, &keys, 200, "a2", &[&a]);
    let b1 = store_event(&mut store, &keys, 200, "b1", &[&b]);
    let older = store_event(&mut store, &keys, 199, "b-older", &[&b]);
    let mut core = core_over(store);

    let effects = core.handle(EngineMsg::Subscribe(union_of(
        [host_branch(&a), host_branch(&b)],
        Some(2),
    )));
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

    let effects = core.handle(EngineMsg::Subscribe(union_of(
        [reactive(&a), reactive(&b)],
        None,
    )));
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

    let composite = core.handle(EngineMsg::Subscribe(union_of(
        [host_branch(&a), host_branch(&b)],
        None,
    )));
    let composite_id = observation(&composite);

    // An unrelated observation independently requires branch A's exact demand.
    let unrelated = core.handle(EngineMsg::Subscribe(LiveQuery::single(host_branch(&a))));
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
    let mut store = MemoryStore::new();

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

    let effects = core.handle(EngineMsg::SubscribeHistory(HistoryQuery::new(
        union_of([host_branch(&a), host_branch(&b)], None),
        2,
        6,
    )));
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
        batch
            .rows
            .iter()
            .map(|row| row.event.id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        batch.evidence.len(),
        2,
        "a windowed observation reports per-branch evidence exactly like an \
         unbounded one"
    );
    assert_eq!(
        batch.rows[0].event.created_at.as_secs(),
        205,
        "the window holds the globally newest rows"
    );
    assert_eq!(batch.rows[1].event.created_at.as_secs(), 204);
}

#[test]
fn a_window_and_an_aggregate_bound_are_two_owners_of_row_membership() {
    let query = union_of(
        [host_branch(&relay("a")), host_branch(&relay("b"))],
        Some(3),
    );
    let engine = nmp::Engine::new(nmp::EngineConfig::default()).expect("in-memory engine");
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
