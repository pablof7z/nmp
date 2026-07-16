use std::{borrow::Cow, collections::BTreeSet};

use nmp_engine::core::{
    AcquisitionEvidence, Effect, EngineCore, EngineMsg, HistoryBatch, HistoryQuery, HistorySink,
    RowDelta, RowSink,
};
use nmp_grammar::{
    AccessContext, Binding, CacheMode, ConcreteFilter, ContextualAtom, Demand, Filter, Freshness,
    RelaySessionKey, SourceAuthority,
};
use nmp_resolver::{HandleId, LiveQuery};
use nmp_router::{FixtureDirectory, WireOp};
use nmp_store::{CoverageInterval, EventStore, MemoryStore, RelayObserved};
use nmp_transport::{RelayFrame, RelayHandle};
use nostr::{Event, Keys, Kind, RelayMessage, RelayUrl, SubscriptionId, Timestamp, UnsignedEvent};

struct Sink;
impl RowSink for Sink {
    fn on_rows(&self, _: Vec<RowDelta>) {}
}

struct WindowSink;
impl HistorySink for WindowSink {
    fn on_history(&self, _: HistoryBatch) {}
}

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

fn atom(keys: &Keys, source: SourceAuthority) -> ContextualAtom {
    ContextualAtom {
        filter: concrete(keys),
        source,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn query(keys: &Keys, freshness: Freshness) -> LiveQuery {
    let mut demand = Demand::from_filter(filter(keys));
    demand.freshness = freshness;
    LiveQuery(demand)
}

fn core(store: MemoryStore, keys: &Keys, relay: &RelayUrl) -> EngineCore<MemoryStore> {
    EngineCore::new(
        store,
        Box::new(FixtureDirectory::new().with_write(keys.public_key().to_hex(), [relay.clone()])),
        10,
    )
}

fn core_with_relays(
    store: MemoryStore,
    keys: &Keys,
    relays: impl IntoIterator<Item = RelayUrl>,
) -> EngineCore<MemoryStore> {
    EngineCore::new(
        store,
        Box::new(FixtureDirectory::new().with_write(keys.public_key().to_hex(), relays)),
        10,
    )
}

fn subscribe(core: &mut EngineCore<MemoryStore>, query: LiveQuery) -> Vec<Effect> {
    core.handle(EngineMsg::Subscribe(query, Box::new(Sink)))
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

fn initial(effects: &[Effect]) -> (HandleId, Vec<RowDelta>, AcquisitionEvidence) {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, rows, evidence) => Some((*id, rows.clone(), evidence.clone())),
            _ => None,
        })
        .unwrap()
}

fn record(store: &mut MemoryStore, atom: &ContextualAtom, relay: &RelayUrl, through: u64) {
    store
        .record_coverage(
            atom,
            relay,
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(through)),
        )
        .unwrap();
}

fn tick(core: &mut EngineCore<MemoryStore>, now: u64) {
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(now)));
}

#[test]
fn fresh_cached_profile_uses_coverage_and_zero_wire() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://fresh.example").unwrap();
    let profile = event(&keys, 90_000);
    let mut store = MemoryStore::new();
    store
        .insert(
            profile.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(96_400u64)),
        )
        .unwrap();
    record(
        &mut store,
        &atom(&keys, SourceAuthority::AuthorOutboxes),
        &relay,
        96_400,
    );
    let mut core = core(store, &keys, &relay);
    tick(&mut core, 100_000);

    let effects = subscribe(
        &mut core,
        query(&keys, Freshness::MaxAge { seconds: 14_400 }),
    );
    let (id, rows, evidence) = initial(&effects);
    assert_eq!(reqs(&effects), 0);
    assert!(rows
        .iter()
        .any(|row| matches!(row, RowDelta::Added(row) if row.event.id == profile.id)));
    assert_eq!(evidence.sources.len(), 1);
    assert_eq!(evidence.sources[0].relay, relay);
    assert_eq!(
        evidence.sources[0].reconciled_through,
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
    let demand_atom = atom(&keys, SourceAuthority::AuthorOutboxes);
    let mut stale_store = MemoryStore::new();
    record(&mut stale_store, &demand_atom, &relay, 82_000);
    let mut stale = core(stale_store, &keys, &relay);
    tick(&mut stale, 100_000);
    let stale_effects = subscribe(
        &mut stale,
        query(&keys, Freshness::MaxAge { seconds: 14_400 }),
    );
    assert_eq!(reqs(&stale_effects), 1);
    let mut live = core(MemoryStore::new(), &keys, &relay);
    tick(&mut live, 100_000);
    let live_effects = subscribe(&mut live, query(&keys, Freshness::Live));
    assert_eq!(
        requested_filters(&stale_effects),
        requested_filters(&live_effects),
        "stale MaxAge must use the exact ordinary Live plan"
    );

    let mut empty_store = MemoryStore::new();
    record(&mut empty_store, &demand_atom, &relay, 96_400);
    let mut empty = core(empty_store, &keys, &relay);
    tick(&mut empty, 100_000);
    let empty_effects = subscribe(
        &mut empty,
        query(&keys, Freshness::MaxAge { seconds: 14_400 }),
    );
    let (_, rows, evidence) = initial(&empty_effects);
    assert_eq!(reqs(&empty_effects), 0);
    assert!(
        rows.is_empty(),
        "absence is fresh when its question is covered"
    );
    assert_eq!(evidence.sources.len(), 1);
}

#[test]
fn cache_only_does_not_borrow_live_sibling_wire_or_evidence() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://cache-only.example").unwrap();
    let mut core = core(MemoryStore::new(), &keys, &relay);
    tick(&mut core, 100_000);
    let live = subscribe(&mut core, query(&keys, Freshness::Live));
    let (live_id, _, _) = initial(&live);
    assert_eq!(reqs(&live), 1);

    let cached = subscribe(&mut core, query(&keys, Freshness::CacheOnly));
    let (cached_id, _, evidence) = initial(&cached);
    assert_eq!(reqs(&cached), 0);
    assert!(evidence.sources.is_empty());
    assert_eq!(evidence.shortfall.len(), 1);
    assert_eq!(closes(&core.handle(EngineMsg::Unsubscribe(cached_id))), 0);
    assert_eq!(closes(&core.handle(EngineMsg::Unsubscribe(live_id))), 1);
}

#[test]
fn cache_only_never_opens_wire_with_populated_cache_and_coverage() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://cache-only-populated.example").unwrap();
    let cached = event(&keys, 99_000);
    let mut store = MemoryStore::new();
    store
        .insert(
            cached.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(99_000u64)),
        )
        .unwrap();
    record(
        &mut store,
        &atom(&keys, SourceAuthority::AuthorOutboxes),
        &relay,
        99_000,
    );
    let mut core = core(store, &keys, &relay);
    tick(&mut core, 100_000);
    let effects = subscribe(&mut core, query(&keys, Freshness::CacheOnly));
    let (_, rows, evidence) = initial(&effects);
    assert_eq!(reqs(&effects), 0);
    assert!(rows
        .iter()
        .any(|row| matches!(row, RowDelta::Added(row) if row.event.id == cached.id)));
    assert!(
        evidence.sources.is_empty(),
        "CacheOnly claims no acquisition"
    );
}

#[test]
fn live_and_satisfied_max_age_drop_independently() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://siblings.example").unwrap();
    let mut store = MemoryStore::new();
    record(
        &mut store,
        &atom(&keys, SourceAuthority::AuthorOutboxes),
        &relay,
        99_000,
    );
    let mut forward = core(store, &keys, &relay);
    tick(&mut forward, 100_000);
    let live = subscribe(&mut forward, query(&keys, Freshness::Live));
    let (live_id, _, _) = initial(&live);
    let fresh = subscribe(
        &mut forward,
        query(&keys, Freshness::MaxAge { seconds: 3_600 }),
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

    let mut store = MemoryStore::new();
    record(
        &mut store,
        &atom(&keys, SourceAuthority::AuthorOutboxes),
        &relay,
        99_000,
    );
    let mut reverse = core(store, &keys, &relay);
    tick(&mut reverse, 100_000);
    let live = subscribe(&mut reverse, query(&keys, Freshness::Live));
    let (live_id, _, _) = initial(&live);
    let fresh = subscribe(
        &mut reverse,
        query(&keys, Freshness::MaxAge { seconds: 3_600 }),
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
    let demand_atom = atom(&keys, SourceAuthority::AuthorOutboxes);

    let mut partial_store = MemoryStore::new();
    record(&mut partial_store, &demand_atom, &first, 99_000);
    let mut partial = core_with_relays(partial_store, &keys, [first.clone(), second.clone()]);
    tick(&mut partial, 100_000);
    let partial_effects = subscribe(
        &mut partial,
        query(&keys, Freshness::MaxAge { seconds: 3_600 }),
    );
    assert_eq!(reqs(&partial_effects), 2, "one fresh relay is insufficient");

    let mut complete_store = MemoryStore::new();
    record(&mut complete_store, &demand_atom, &first, 99_000);
    record(&mut complete_store, &demand_atom, &second, 99_000);
    let mut complete = core_with_relays(complete_store, &keys, [first.clone(), second.clone()]);
    tick(&mut complete, 100_000);
    let complete_effects = subscribe(
        &mut complete,
        query(&keys, Freshness::MaxAge { seconds: 3_600 }),
    );
    let (_, _, evidence) = initial(&complete_effects);
    assert_eq!(reqs(&complete_effects), 0);
    assert_eq!(evidence.sources.len(), 2);
    assert!(evidence
        .sources
        .iter()
        .all(|source| source.reconciled_through == Some(Timestamp::from(99_000u64))));
}

#[test]
fn stale_max_age_refreshes_coverage_once_and_remains_live() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://refresh.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let handle = RelayHandle {
        slot: 1,
        generation: 1,
    };
    let mut core = core(MemoryStore::new(), &keys, &relay);
    let _ = core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let _ = core.handle(EngineMsg::RelayInformationResolved(relay.clone(), None));
    tick(&mut core, 100_000);
    let opened = subscribe(
        &mut core,
        query(&keys, Freshness::MaxAge { seconds: 3_600 }),
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
        core.get_coverage(&atom(&keys, SourceAuthority::AuthorOutboxes), &relay)
            .unwrap()
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
    let source = SourceAuthority::Pinned(BTreeSet::from([pinned.clone()]));
    let demand_atom = atom(&keys, source.clone());
    let mut store = MemoryStore::new();
    store
        .insert(
            event(&keys, 90_000),
            RelayObserved::new(other, Timestamp::from(99_000u64)),
        )
        .unwrap();
    record(&mut store, &demand_atom, &pinned, 99_000);
    let mut demand = Demand::new(filter(&keys), source, AccessContext::Public).unwrap();
    demand.cache = CacheMode::Strict;
    demand.freshness = Freshness::MaxAge { seconds: 3_600 };
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    tick(&mut core, 100_000);
    let effects = subscribe(&mut core, LiveQuery(demand));
    let (_, rows, evidence) = initial(&effects);
    assert_eq!(reqs(&effects), 0);
    assert!(rows.is_empty(), "Strict excludes non-pinned provenance");
    assert_eq!(evidence.sources[0].relay, pinned);
}

#[test]
fn future_event_time_never_inflates_coverage_or_freshness() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://future.example").unwrap();
    let mut core = core(MemoryStore::new(), &keys, &relay);
    let session = RelaySessionKey::public(relay.clone());
    let handle = RelayHandle {
        slot: 1,
        generation: 1,
    };
    let _ = core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let _ = core.handle(EngineMsg::RelayInformationResolved(relay.clone(), None));
    tick(&mut core, 100_000);
    let live = subscribe(&mut core, query(&keys, Freshness::Live));
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
        core.get_coverage(&atom(&keys, SourceAuthority::AuthorOutboxes), &relay)
            .unwrap()
            .through,
        Timestamp::from(100_001u64)
    );
    let _ = core.handle(EngineMsg::Unsubscribe(live_id));
    tick(&mut core, 120_000);
    let effects = subscribe(
        &mut core,
        query(&keys, Freshness::MaxAge { seconds: 1_000 }),
    );
    assert_eq!(
        reqs(&effects),
        1,
        "future event did not fake recent coverage"
    );
}

#[test]
fn satisfied_max_age_window_never_preflights_relays_when_it_grows() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://fresh-window.example").unwrap();
    let mut store = MemoryStore::new();
    store
        .insert(
            event(&keys, 99_000),
            RelayObserved::new(relay.clone(), Timestamp::from(99_000u64)),
        )
        .unwrap();
    record(
        &mut store,
        &atom(&keys, SourceAuthority::AuthorOutboxes),
        &relay,
        99_000,
    );
    let mut core = core(store, &keys, &relay);
    tick(&mut core, 100_000);
    let opened = core.handle(EngineMsg::SubscribeHistory(
        HistoryQuery::new(query(&keys, Freshness::MaxAge { seconds: 3_600 }), 1, 2),
        Box::new(WindowSink),
    ));
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
        |effect| matches!(effect, Effect::PreflightHistoryRelays(relays) if relays.is_empty())
    ));
}
