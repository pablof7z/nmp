use std::{borrow::Cow, collections::BTreeSet};

use nmp_engine::core::{
    AcquisitionEvidence, Effect, EngineCore, EngineMsg, HistoryQuery, ObservationId, RowDelta,
    ShortfallFact, SourceStatus,
};
use nmp_grammar::Derived;
use nmp_grammar::LiveQuery;
use nmp_grammar::{
    Binding, CacheMode, ConcreteFilter, ContextualAtom, Demand, Filter, Freshness,
    ReadRouting, RelaySessionKey, Selector,
};
use nmp_router::WireOp;
use nmp_router_testkit::FixtureRoutingFacts;
use nmp_store::{CoverageInterval, RedbStore, RelayObserved};
use nmp_transport::{RelayFrame, RelayHandle};
use nostr::{Event, Keys, Kind, RelayMessage, RelayUrl, SubscriptionId, Timestamp, UnsignedEvent};

fn event(keys: &Keys, at: u64) -> Event {
    UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(at),
        Kind::Metadata,
        Vec::new(),
        "{}",
    )
    .sign_with_keys(keys)
    .unwrap()
}

fn reaction(keys: &Keys, at: u64) -> Event {
    UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(at),
        Kind::from(7u16),
        Vec::new(),
        "+",
    )
    .sign_with_keys(keys)
    .unwrap()
}

fn filter(keys: &Keys) -> Filter {
    Filter {
        kinds: Some(BTreeSet::from([0])),
        authors: Some(Binding::Literal(BTreeSet::from([keys
            .public_key()
            .to_hex()]))),
        ..Filter::default()
    }
}

fn concrete(keys: &Keys) -> ConcreteFilter {
    ConcreteFilter {
        kinds: Some(BTreeSet::from([0])),
        authors: Some(BTreeSet::from([keys.public_key().to_hex()])),
        ..ConcreteFilter::default()
    }
}

fn atom(keys: &Keys, routing: ReadRouting) -> ContextualAtom {
    ContextualAtom {
        filter: concrete(keys),
        routing,
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    }
}

fn query(keys: &Keys, freshness: Freshness) -> LiveQuery {
    let mut demand = Demand {
        selection: filter(keys),
        ..Demand::default()
    };
    demand.freshness = freshness;
    LiveQuery::single(demand)
}

fn nested_query(
    keys: &Keys,
    inner_relay: &RelayUrl,
    inner_freshness: Freshness,
    outer_relay: &RelayUrl,
    outer_freshness: Freshness,
) -> LiveQuery {
    let mut inner = Demand::new(
        filter(keys),
        ReadRouting::Explicit(vec![inner_relay.clone()])
    )
    .unwrap();
    inner.freshness = inner_freshness;
    let outer_selection = Filter {
        kinds: Some(BTreeSet::from([7u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner,
            project: Selector::Authors,
        }))),
        ..Filter::default()
    };
    let mut outer = Demand::new(
        outer_selection,
        ReadRouting::Explicit(vec![outer_relay.clone()])
    )
    .unwrap();
    outer.freshness = outer_freshness;
    LiveQuery::single(outer)
}

fn pinned_query(keys: &Keys, relay: &RelayUrl, freshness: Freshness) -> LiveQuery {
    let mut demand = Demand::new(
        filter(keys),
        ReadRouting::Explicit(vec![relay.clone()])
    )
    .unwrap();
    demand.freshness = freshness;
    LiveQuery::single(demand)
}

fn seeded_nested_store(keys: &Keys, inner_relay: &RelayUrl) -> RedbStore {
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .insert(
            event(keys, 90_000),
            RelayObserved::new(inner_relay.clone(), Timestamp::from(90_000u64)),
        )
        .unwrap();
    store
}

fn core(store: RedbStore, keys: &Keys, relay: &RelayUrl) -> EngineCore {
    EngineCore::new_with_fixture_routing_facts(
        store,
        FixtureRoutingFacts::new().with_outbound_routes(keys.public_key(), [relay.clone()]),
        10,
    )
}

fn core_with_relays(
    store: RedbStore,
    keys: &Keys,
    relays: impl IntoIterator<Item = RelayUrl>,
) -> EngineCore {
    EngineCore::new_with_fixture_routing_facts(
        store,
        FixtureRoutingFacts::new().with_outbound_routes(keys.public_key(), relays),
        10,
    )
}

fn subscribe(core: &mut EngineCore, query: LiveQuery, admission_at: u64) -> Vec<Effect> {
    let mut effects = core.handle(EngineMsg::Subscribe(query));
    effects.extend(core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(admission_at))));
    effects
}

fn reqs(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(
                delta
                    .ops
                    .iter()
                    .flat_map(|(_, ops)| ops)
                    .filter(|op| matches!(op, WireOp::Req(..)))
                    .count(),
            ),
            _ => None,
        })
        .sum()
}

fn closes(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(
                delta
                    .ops
                    .iter()
                    .flat_map(|(_, ops)| ops)
                    .filter(|op| matches!(op, WireOp::Close(..)))
                    .count(),
            ),
            _ => None,
        })
        .sum()
}

fn requested_filters(effects: &[Effect]) -> BTreeSet<(RelaySessionKey, ConcreteFilter)> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(&delta.ops),
            _ => None,
        })
        .flatten()
        .flat_map(|(session, ops)| {
            ops.iter().filter_map(move |op| match op {
                WireOp::Req(_, filter) => Some((session.clone(), filter.clone())),
                WireOp::Close(_) => None,
            })
        })
        .collect()
}

fn wire_id(effects: &[Effect]) -> String {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Wire(delta) => delta.ops.iter().find_map(|(_, ops)| {
                ops.iter().find_map(|op| match op {
                    WireOp::Req(id, _) => Some(id.1.to_string()),
                    WireOp::Close(_) => None,
                })
            }),
            _ => None,
        })
        .unwrap()
}

fn initial(effects: &[Effect]) -> (ObservationId, Vec<RowDelta>, Vec<AcquisitionEvidence>) {
    let (id, rows, _) = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, rows, evidence) => Some((*id, rows.clone(), evidence.clone())),
            _ => None,
        })
        .unwrap();
    let evidence = effects
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::EmitRows(effect_id, _, evidence) if *effect_id == id => Some(evidence.clone()),
            _ => None,
        })
        .unwrap();
    (id, rows, evidence)
}

fn record(store: &mut RedbStore, atom: &ContextualAtom, relay: &RelayUrl, through: u64) {
    store
        .record_coverage(&[(
            atom.clone(),
            relay.clone(),
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(through)),
        )])
        .unwrap();
}

fn tick(core: &mut EngineCore, now: u64) {
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(now)));
}

#[test]
fn fresh_cached_profile_uses_coverage_and_zero_wire() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://fresh.example").unwrap();
    let profile = event(&keys, 90_000);
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .insert(
            profile.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(96_400u64)),
        )
        .unwrap();
    record(&mut store, &atom(&keys, ReadRouting::Auto), &relay, 96_400);
    let mut core = core(store, &keys, &relay);
    tick(&mut core, 100_000);

    let effects = subscribe(
        &mut core,
        query(&keys, Freshness::MaxAge { seconds: 14_400 }),
        100_000,
    );
    let (id, rows, evidence) = initial(&effects);
    assert_eq!(reqs(&effects), 0);
    assert!(rows
        .iter()
        .any(|row| matches!(row, RowDelta::Added(row) if row.id() == profile.id)));
    assert_eq!(evidence[0].sources.len(), 1);
    assert_eq!(evidence[0].sources[0].relay, relay);
    assert_eq!(
        evidence[0].sources[0].reconciled_through,
        Some(Timestamp::from(96_400u64))
    );
    let aged = core.handle(EngineMsg::Tick(Timestamp::from(200_000u64)));
    assert_eq!(reqs(&aged), 0, "a satisfied handle is not re-evaluated");
    assert_eq!(closes(&core.handle(EngineMsg::Unsubscribe(id))), 0);
}

#[test]
fn stale_max_age_is_live_but_recent_empty_coverage_is_fresh() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://age.example").unwrap();
    let demand_atom = atom(&keys, ReadRouting::Auto);
    let mut stale_store = RedbStore::temporary().expect("temporary Redb store");
    record(&mut stale_store, &demand_atom, &relay, 82_000);
    let mut stale = core(stale_store, &keys, &relay);
    tick(&mut stale, 100_000);
    let stale_effects = subscribe(
        &mut stale,
        query(&keys, Freshness::MaxAge { seconds: 14_400 }),
        100_000,
    );
    assert_eq!(reqs(&stale_effects), 1);
    let mut live = core(
        RedbStore::temporary().expect("temporary Redb store"),
        &keys,
        &relay,
    );
    tick(&mut live, 100_000);
    let live_effects = subscribe(&mut live, query(&keys, Freshness::Live), 100_000);
    assert_eq!(
        requested_filters(&stale_effects),
        requested_filters(&live_effects),
        "stale MaxAge must use the exact ordinary Live plan"
    );

    let mut empty_store = RedbStore::temporary().expect("temporary Redb store");
    record(&mut empty_store, &demand_atom, &relay, 96_400);
    let mut empty = core(empty_store, &keys, &relay);
    tick(&mut empty, 100_000);
    let empty_effects = subscribe(
        &mut empty,
        query(&keys, Freshness::MaxAge { seconds: 14_400 }),
        100_000,
    );
    let (_, rows, evidence) = initial(&empty_effects);
    assert_eq!(reqs(&empty_effects), 0);
    assert!(
        rows.is_empty(),
        "absence is fresh when its question is covered"
    );
    assert_eq!(evidence[0].sources.len(), 1);
}

#[test]
fn cache_only_does_not_borrow_live_sibling_wire_or_evidence() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://cache-only.example").unwrap();
    let mut core = core(
        RedbStore::temporary().expect("temporary Redb store"),
        &keys,
        &relay,
    );
    tick(&mut core, 100_000);
    let live = subscribe(&mut core, query(&keys, Freshness::Live), 100_000);
    let (live_id, _, _) = initial(&live);
    assert_eq!(reqs(&live), 1);

    let cached = subscribe(&mut core, query(&keys, Freshness::CacheOnly), 100_000);
    let (cached_id, _, evidence) = initial(&cached);
    assert_eq!(reqs(&cached), 0);
    assert!(evidence[0].sources.is_empty());
    assert_eq!(evidence[0].shortfall.len(), 1);
    assert_eq!(closes(&core.handle(EngineMsg::Unsubscribe(cached_id))), 0);
    assert_eq!(closes(&core.handle(EngineMsg::Unsubscribe(live_id))), 1);
}

#[test]
fn cache_only_never_opens_wire_with_populated_cache_and_coverage() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://cache-only-populated.example").unwrap();
    let cached = event(&keys, 99_000);
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .insert(
            cached.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(99_000u64)),
        )
        .unwrap();
    record(&mut store, &atom(&keys, ReadRouting::Auto), &relay, 99_000);
    let mut core = core(store, &keys, &relay);
    tick(&mut core, 100_000);
    let effects = subscribe(&mut core, query(&keys, Freshness::CacheOnly), 100_000);
    let (_, rows, evidence) = initial(&effects);
    assert_eq!(reqs(&effects), 0);
    assert!(rows
        .iter()
        .any(|row| matches!(row, RowDelta::Added(row) if row.id() == cached.id)));
    assert!(
        evidence[0].sources.is_empty(),
        "CacheOnly claims no acquisition"
    );
}

/// QUERIES-DERIVED-FRESHNESS-001: a root Live decision must not overwrite
/// the independently declared CacheOnly policy at the nested Demand boundary.
#[test]
fn nested_cache_only_opens_no_inner_wire_under_live_outer() {
    let keys = Keys::generate();
    let inner_relay = RelayUrl::parse("wss://nested-cache-only.example").unwrap();
    let outer_relay = RelayUrl::parse("wss://outer-live.example").unwrap();
    let store = seeded_nested_store(&keys, &inner_relay);
    let mut core = EngineCore::new(store, 10);

    let effects = subscribe(
        &mut core,
        nested_query(
            &keys,
            &inner_relay,
            Freshness::CacheOnly,
            &outer_relay,
            Freshness::Live,
        ),
        0,
    );

    assert_eq!(
        requested_filters(&effects),
        BTreeSet::from([(
            RelaySessionKey::unauthenticated(outer_relay.clone()),
            ConcreteFilter {
                kinds: Some(BTreeSet::from([7u16])),
                authors: Some(BTreeSet::from([keys.public_key().to_hex()])),
                ..ConcreteFilter::default()
            },
        )]),
        "the outer Live request remains, while the inner CacheOnly atom contributes no wire work"
    );
    let (_, _, evidence) = initial(&effects);
    assert_eq!(
        evidence[0]
            .sources
            .iter()
            .map(|source| source.relay.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([outer_relay]),
        "CacheOnly inner evidence must not borrow the outer Live plan"
    );
    assert_eq!(
        evidence[0].shortfall,
        vec![ShortfallFact::NoPlannedSource {
            atom: concrete(&keys),
        }],
        "the locally readable inner Demand remains an explicit acquisition shortfall"
    );
}

/// The nested Demand's pinned Strict cache policy must not become the root
/// Demand's cache policy. Strict over Public remains the documented
/// Agnostic-equivalent root projection.
#[test]
fn nested_strict_pins_do_not_contaminate_public_root_cache_projection() {
    let inner_author = Keys::generate();
    let inner_relay = RelayUrl::parse("wss://nested-root-isolation-a.example").unwrap();
    let root_relay = RelayUrl::parse("wss://nested-root-isolation-b.example").unwrap();
    let inner_row = event(&inner_author, 100);
    let root_row = reaction(&inner_author, 200);
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .insert(
            inner_row,
            RelayObserved::new(inner_relay.clone(), Timestamp::from(100u64)),
        )
        .unwrap();
    store
        .insert(
            root_row.clone(),
            RelayObserved::new(root_relay, Timestamp::from(200u64)),
        )
        .unwrap();

    let mut inner = Demand::new(
        filter(&inner_author),
        ReadRouting::Explicit(vec![inner_relay])
    )
    .unwrap();
    inner.cache = CacheMode::Strict;
    inner.freshness = Freshness::CacheOnly;
    let mut root = Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([7u16])),
            authors: Some(Binding::Derived(Box::new(Derived {
                inner,
                project: Selector::Authors,
            }))),
            ..Filter::default()
        },
        ReadRouting::Auto
    )
    .unwrap();
    root.cache = CacheMode::Strict;
    root.freshness = Freshness::CacheOnly;
    let mut core = EngineCore::new(store, 10);
    let effects = subscribe(&mut core, LiveQuery::single(root), 0);
    let (_, rows, _) = initial(&effects);

    assert!(
        rows.iter().any(
            |delta| matches!(delta, RowDelta::Added(row) if row.id() == root_row.id)
        ),
        "Public root Strict is a no-op and must keep the B-observed row; the nested A pin belongs only to the inner projection"
    );
}

/// QUERIES-DERIVED-FRESHNESS-002: a root CacheOnly decision must not suppress
/// an independently Live nested Demand.
#[test]
fn nested_live_opens_wire_under_cache_only_outer() {
    let keys = Keys::generate();
    let inner_relay = RelayUrl::parse("wss://nested-live.example").unwrap();
    let outer_relay = RelayUrl::parse("wss://outer-cache-only.example").unwrap();
    let store = seeded_nested_store(&keys, &inner_relay);
    let mut core = EngineCore::new(store, 10);

    let effects = subscribe(
        &mut core,
        nested_query(
            &keys,
            &inner_relay,
            Freshness::Live,
            &outer_relay,
            Freshness::CacheOnly,
        ),
        0,
    );

    assert_eq!(
        requested_filters(&effects),
        BTreeSet::from([(RelaySessionKey::unauthenticated(inner_relay), concrete(&keys),)]),
        "the inner Live request remains, while the outer CacheOnly atom contributes no wire work"
    );
}

/// QUERIES-DERIVED-FRESHNESS-003/004: MaxAge is decided over the nested
/// Demand's own atom and pinned source coverage, independently from the outer
/// Demand's Live participation.
#[test]
fn nested_max_age_uses_inner_scoped_coverage_only() {
    let keys = Keys::generate();
    let inner_relay = RelayUrl::parse("wss://nested-max-age.example").unwrap();
    let outer_relay = RelayUrl::parse("wss://outer-live-max-age.example").unwrap();
    let inner_source = ReadRouting::Explicit(vec![inner_relay.clone()]);
    let mut fresh_store = seeded_nested_store(&keys, &inner_relay);
    record(
        &mut fresh_store,
        &atom(&keys, inner_source.clone()),
        &inner_relay,
        99_000,
    );
    let mut fresh = EngineCore::new(fresh_store, 10);
    tick(&mut fresh, 100_000);

    let fresh_effects = subscribe(
        &mut fresh,
        nested_query(
            &keys,
            &inner_relay,
            Freshness::MaxAge { seconds: 3_600 },
            &outer_relay,
            Freshness::Live,
        ),
        100_000,
    );
    assert_eq!(
        requested_filters(&fresh_effects),
        BTreeSet::from([(
            RelaySessionKey::unauthenticated(outer_relay.clone()),
            ConcreteFilter {
                kinds: Some(BTreeSet::from([7u16])),
                authors: Some(BTreeSet::from([keys.public_key().to_hex()])),
                ..ConcreteFilter::default()
            },
        )]),
        "fresh inner coverage suppresses only the nested request"
    );
    let (_, _, fresh_evidence) = initial(&fresh_effects);
    let inner_evidence = fresh_evidence[0]
        .sources
        .iter()
        .find(|source| source.relay == inner_relay)
        .expect("the nested source retains its own evidence");
    assert_eq!(
        inner_evidence.reconciled_through,
        Some(Timestamp::from(99_000u64)),
        "the nested source exposes only its own fresh durable watermark"
    );
    assert_eq!(inner_evidence.status, SourceStatus::CoverageSatisfied);
    let outer_evidence = fresh_evidence[0]
        .sources
        .iter()
        .find(|source| source.relay == outer_relay)
        .expect("the independently live outer source remains visible");
    assert_eq!(
        outer_evidence.reconciled_through, None,
        "the nested watermark must not become a global completion claim"
    );
    assert_eq!(outer_evidence.status, SourceStatus::Connecting);
    assert!(
        fresh_evidence[0].shortfall.is_empty(),
        "both Demand boundaries have an honest source"
    );

    let mut stale_store = seeded_nested_store(&keys, &inner_relay);
    record(
        &mut stale_store,
        &atom(&keys, inner_source),
        &inner_relay,
        90_000,
    );
    let mut stale = EngineCore::new(stale_store, 10);
    tick(&mut stale, 100_000);
    let sibling_keys = Keys::generate();
    let sibling_relay = RelayUrl::parse("wss://unrelated-live-sibling.example").unwrap();
    let sibling_opened = subscribe(
        &mut stale,
        pinned_query(&sibling_keys, &sibling_relay, Freshness::Live),
        100_000,
    );
    let (sibling_id, _, sibling_evidence) = initial(&sibling_opened);
    assert_eq!(
        requested_filters(&sibling_opened),
        BTreeSet::from([(
            RelaySessionKey::unauthenticated(sibling_relay.clone()),
            concrete(&sibling_keys),
        )]),
        "the independent live sibling starts with exactly its own wire request"
    );
    assert_eq!(sibling_evidence[0].sources.len(), 1);
    assert_eq!(sibling_evidence[0].sources[0].relay, sibling_relay);
    let stale_effects = subscribe(
        &mut stale,
        nested_query(
            &keys,
            &inner_relay,
            Freshness::MaxAge { seconds: 3_600 },
            &outer_relay,
            Freshness::Live,
        ),
        100_000,
    );
    assert_eq!(
        requested_filters(&stale_effects),
        BTreeSet::from([
            (RelaySessionKey::unauthenticated(inner_relay), concrete(&keys)),
            (
                RelaySessionKey::unauthenticated(outer_relay),
                ConcreteFilter {
                    kinds: Some(BTreeSet::from([7u16])),
                    authors: Some(BTreeSet::from([keys.public_key().to_hex()])),
                    ..ConcreteFilter::default()
                },
            ),
        ]),
        "stale inner coverage degrades only the nested request to ordinary Live"
    );
    assert_eq!(
        closes(&stale_effects),
        0,
        "opening the stale nested query must not replace the unrelated live sibling"
    );
    assert!(
        !stale_effects
            .iter()
            .any(|effect| matches!(effect, Effect::EmitRows(id, _, _) if *id == sibling_id)),
        "the unrelated live sibling's rows and evidence remain unchanged"
    );
}

/// QUERIES-DERIVED-FRESHNESS-005: the coverage fact consulted by a nested
/// MaxAge Demand is durable store truth, not volatile reducer state.
#[test]
fn nested_max_age_scoped_coverage_survives_redb_restart() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("nested-max-age.redb");
    let keys = Keys::generate();
    let inner_relay = RelayUrl::parse("wss://nested-restart-inner.example").unwrap();
    let outer_relay = RelayUrl::parse("wss://nested-restart-outer.example").unwrap();
    let inner_atom = atom(&keys, ReadRouting::Explicit(vec![inner_relay.clone()]));

    {
        let mut store = RedbStore::open(&path).expect("create durable store");
        store
            .insert(
                event(&keys, 90_000),
                RelayObserved::new(inner_relay.clone(), Timestamp::from(90_000u64)),
            )
            .unwrap();
        record(&mut store, &inner_atom, &inner_relay, 99_000);
    }

    let mut reopened = EngineCore::new(RedbStore::open(&path).expect("reopen durable store"), 10);
    assert!(reopened.recover_on_boot().is_empty());
    tick(&mut reopened, 100_000);
    let effects = subscribe(
        &mut reopened,
        nested_query(
            &keys,
            &inner_relay,
            Freshness::MaxAge { seconds: 3_600 },
            &outer_relay,
            Freshness::CacheOnly,
        ),
        100_000,
    );

    assert!(
        requested_filters(&effects).is_empty(),
        "reopened inner coverage satisfies nested MaxAge while the root independently remains CacheOnly"
    );
    let (_, _, evidence) = initial(&effects);
    assert_eq!(
        evidence[0].sources.len(),
        1,
        "only the nested source owns persisted coverage"
    );
    assert_eq!(evidence[0].sources[0].relay, inner_relay);
    assert_eq!(
        evidence[0].sources[0].reconciled_through,
        Some(Timestamp::from(99_000u64)),
        "the reopened snapshot retains the nested durable watermark"
    );
    assert_eq!(
        evidence[0].sources[0].status,
        SourceStatus::CoverageSatisfied
    );
    assert_eq!(
        evidence[0].shortfall,
        vec![ShortfallFact::NoPlannedSource {
            atom: ConcreteFilter {
                kinds: Some(BTreeSet::from([7u16])),
                authors: Some(BTreeSet::from([keys.public_key().to_hex()])),
                ..ConcreteFilter::default()
            },
        }],
        "the root CacheOnly boundary stays an explicit, separate no-source fact"
    );
}

#[test]
fn live_and_satisfied_max_age_drop_independently() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://siblings.example").unwrap();
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    record(&mut store, &atom(&keys, ReadRouting::Auto), &relay, 99_000);
    let mut forward = core(store, &keys, &relay);
    tick(&mut forward, 100_000);
    let live = subscribe(&mut forward, query(&keys, Freshness::Live), 100_000);
    let (live_id, _, _) = initial(&live);
    let fresh = subscribe(
        &mut forward,
        query(&keys, Freshness::MaxAge { seconds: 3_600 }),
        100_000,
    );
    let (fresh_id, _, _) = initial(&fresh);
    assert_eq!(reqs(&live), 1);
    assert_eq!(reqs(&fresh), 0);
    let live_drop = forward.handle(EngineMsg::Unsubscribe(live_id));
    assert_eq!(closes(&live_drop), 1);
    assert_eq!(
        reqs(&live_drop),
        0,
        "fresh handle never reopens sibling wire"
    );
    assert_eq!(closes(&forward.handle(EngineMsg::Unsubscribe(fresh_id))), 0);

    let mut store = RedbStore::temporary().expect("temporary Redb store");
    record(&mut store, &atom(&keys, ReadRouting::Auto), &relay, 99_000);
    let mut reverse = core(store, &keys, &relay);
    tick(&mut reverse, 100_000);
    let live = subscribe(&mut reverse, query(&keys, Freshness::Live), 100_000);
    let (live_id, _, _) = initial(&live);
    let fresh = subscribe(
        &mut reverse,
        query(&keys, Freshness::MaxAge { seconds: 3_600 }),
        100_000,
    );
    let (fresh_id, _, _) = initial(&fresh);
    assert_eq!(closes(&reverse.handle(EngineMsg::Unsubscribe(fresh_id))), 0);
    assert_eq!(closes(&reverse.handle(EngineMsg::Unsubscribe(live_id))), 1);
}

#[test]
fn max_age_requires_fresh_coverage_from_every_assigned_outbox() {
    let keys = Keys::generate();
    let first = RelayUrl::parse("wss://first-outbox.example").unwrap();
    let second = RelayUrl::parse("wss://second-outbox.example").unwrap();
    let demand_atom = atom(&keys, ReadRouting::Auto);

    let mut partial_store = RedbStore::temporary().expect("temporary Redb store");
    record(&mut partial_store, &demand_atom, &first, 99_000);
    let mut partial = core_with_relays(partial_store, &keys, [first.clone(), second.clone()]);
    tick(&mut partial, 100_000);
    let partial_effects = subscribe(
        &mut partial,
        query(&keys, Freshness::MaxAge { seconds: 3_600 }),
        100_000,
    );
    assert_eq!(reqs(&partial_effects), 2, "one fresh relay is insufficient");

    let mut complete_store = RedbStore::temporary().expect("temporary Redb store");
    record(&mut complete_store, &demand_atom, &first, 99_000);
    record(&mut complete_store, &demand_atom, &second, 99_000);
    let mut complete = core_with_relays(complete_store, &keys, [first.clone(), second.clone()]);
    tick(&mut complete, 100_000);
    let complete_effects = subscribe(
        &mut complete,
        query(&keys, Freshness::MaxAge { seconds: 3_600 }),
        100_000,
    );
    let (_, _, evidence) = initial(&complete_effects);
    assert_eq!(reqs(&complete_effects), 0);
    assert_eq!(evidence[0].sources.len(), 2);
    assert!(evidence[0]
        .sources
        .iter()
        .all(|source| source.reconciled_through == Some(Timestamp::from(99_000u64))));
}

#[test]
fn stale_max_age_refreshes_coverage_once_and_remains_live() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://refresh.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let handle = RelayHandle {
        slot: 1,
        generation: 1,
    };
    let mut core = core(
        RedbStore::temporary().expect("temporary Redb store"),
        &keys,
        &relay,
    );
    let _ = core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let _ = core.handle(EngineMsg::RelayInformationResolved(relay.clone(), None));
    tick(&mut core, 100_000);
    let opened = subscribe(
        &mut core,
        query(&keys, Freshness::MaxAge { seconds: 3_600 }),
        100_000,
    );
    let (id, _, _) = initial(&opened);
    assert_eq!(reqs(&opened), 1);
    let completed = core.handle(EngineMsg::RelayFrame(
        handle,
        session,
        RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
            SubscriptionId::new(wire_id(&opened)),
        ))),
    ));
    assert_eq!(reqs(&completed), 0, "EOSE does not reopen the handle");
    assert_eq!(
        closes(&completed),
        0,
        "EOSE does not suppress the live tail"
    );
    assert_eq!(
        core.get_coverage(&atom(&keys, ReadRouting::Auto), &relay)
            .expect("coverage peek")
            .expect("a proven row")
            .through,
        Timestamp::from(100_000u64)
    );
    let aged = core.handle(EngineMsg::Tick(Timestamp::from(200_000u64)));
    assert_eq!(reqs(&aged), 0, "no mid-handle freshness loop exists");
    assert_eq!(closes(&core.handle(EngineMsg::Unsubscribe(id))), 1);
}

#[test]
fn pinned_strict_max_age_uses_pinned_scope_for_coverage_and_rows() {
    let keys = Keys::generate();
    let pinned = RelayUrl::parse("wss://pinned.example").unwrap();
    let other = RelayUrl::parse("wss://other.example").unwrap();
    let source = ReadRouting::Explicit(vec![pinned.clone()]);
    let demand_atom = atom(&keys, source.clone());
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .insert(
            event(&keys, 90_000),
            RelayObserved::new(other, Timestamp::from(99_000u64)),
        )
        .unwrap();
    record(&mut store, &demand_atom, &pinned, 99_000);
    let mut demand = Demand::new(filter(&keys), source).unwrap();
    demand.cache = CacheMode::Strict;
    demand.freshness = Freshness::MaxAge { seconds: 3_600 };
    let mut core = EngineCore::new(store, 10);
    tick(&mut core, 100_000);
    let effects = subscribe(&mut core, LiveQuery::single(demand), 100_000);
    let (_, rows, evidence) = initial(&effects);
    assert_eq!(reqs(&effects), 0);
    assert!(rows.is_empty(), "Strict excludes non-pinned provenance");
    assert_eq!(evidence[0].sources[0].relay, pinned);
}

#[test]
fn future_event_time_never_inflates_coverage_or_freshness() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://future.example").unwrap();
    let mut core = core(
        RedbStore::temporary().expect("temporary Redb store"),
        &keys,
        &relay,
    );
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let handle = RelayHandle {
        slot: 1,
        generation: 1,
    };
    let _ = core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let _ = core.handle(EngineMsg::RelayInformationResolved(relay.clone(), None));
    tick(&mut core, 100_000);
    let live = subscribe(&mut core, query(&keys, Freshness::Live), 100_000);
    let (live_id, _, _) = initial(&live);
    let wire = wire_id(&live);
    let _ = core.handle(EngineMsg::RelayFrame(
        handle,
        session.clone(),
        RelayFrame::from_message(RelayMessage::Event {
            subscription_id: Cow::Owned(SubscriptionId::new(wire.clone())),
            event: Cow::Owned(event(&keys, 9_999_999)),
        }),
    ));
    tick(&mut core, 100_001);
    let _ = core.handle(EngineMsg::RelayFrame(
        handle,
        session,
        RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
            SubscriptionId::new(wire),
        ))),
    ));
    assert_eq!(
        core.get_coverage(&atom(&keys, ReadRouting::Auto), &relay)
            .expect("coverage peek")
            .expect("a proven row")
            .through,
        Timestamp::from(100_001u64)
    );
    let _ = core.handle(EngineMsg::Unsubscribe(live_id));
    tick(&mut core, 120_000);
    let effects = subscribe(
        &mut core,
        query(&keys, Freshness::MaxAge { seconds: 1_000 }),
        120_000,
    );
    assert_eq!(
        reqs(&effects),
        1,
        "future event did not fake recent coverage"
    );
}

#[test]
fn satisfied_max_age_window_growth_stays_store_only() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://fresh-window.example").unwrap();
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .insert(
            event(&keys, 99_000),
            RelayObserved::new(relay.clone(), Timestamp::from(99_000u64)),
        )
        .unwrap();
    record(&mut store, &atom(&keys, ReadRouting::Auto), &relay, 99_000);
    let mut core = core(store, &keys, &relay);
    tick(&mut core, 100_000);
    let opened = core.handle(EngineMsg::SubscribeHistory(HistoryQuery::new(
        query(&keys, Freshness::MaxAge { seconds: 3_600 }),
        1,
        2,
    )));
    assert_eq!(reqs(&opened), 0);
    let id = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitHistory(id, _) => Some(*id),
            _ => None,
        })
        .unwrap();
    let growth = core.handle(EngineMsg::RequestRows(id, 2));
    assert_eq!(reqs(&growth), 0);
    assert!(growth.iter().any(
        |effect| matches!(effect, Effect::HistoryLoadResult(session, Ok(())) if *session == id)
    ));
}
