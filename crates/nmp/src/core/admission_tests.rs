//! Admission-window and surgical lifecycle falsifiers for #1340/#1341.

use super::*;
use nmp_grammar::{Binding, Demand, Filter, IndexedTagName};
use nmp_store::MemoryStore;

fn query(relay: &RelayUrl, value: &str, freshness: Freshness) -> LiveQuery {
    let mut demand = Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([0u16])),
        tags: BTreeMap::from([(
            IndexedTagName::new('p').unwrap(),
            Binding::Literal(BTreeSet::from([value.to_owned()])),
        )]),
        ..Filter::default()
    });
    demand.source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    demand.freshness = freshness;
    LiveQuery::single(demand)
}

fn observation_id(effects: &[Effect]) -> ObservationId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, _, _) => Some(*id),
            _ => None,
        })
        .expect("an observation open returns its immediate cache seed")
}

fn wire_ops(effects: &[Effect]) -> Vec<&WireOp> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| delta.ops.iter().flat_map(|(_, ops)| ops))
        .collect()
}

fn flush(core: &mut EngineCore<MemoryStore>) -> Vec<Effect> {
    core.handle(EngineMsg::FlushWireAdmission)
}

#[test]
fn cache_seed_is_immediate_while_wire_execution_waits_for_admission_flush() {
    let relay = RelayUrl::parse("wss://admission-seed.example").unwrap();
    let mut core = EngineCore::new(MemoryStore::new(), 20);

    let opened = core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    )));

    observation_id(&opened);
    assert!(wire_ops(&opened).is_empty(), "open must not execute a REQ");
    assert!(opened
        .iter()
        .any(|effect| matches!(effect, Effect::ArmWireAdmission)));

    let flushed = flush(&mut core);
    assert_eq!(
        wire_ops(&flushed)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        1
    );
}

#[test]
fn compatible_pending_observations_compile_once_into_one_relay_request() {
    let relay = RelayUrl::parse("wss://admission-group.example").unwrap();
    let mut core = EngineCore::new(MemoryStore::new(), 20);

    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    )));
    core.handle(EngineMsg::Subscribe(query(&relay, "bob", Freshness::Live)));
    assert_eq!(core.router_compiles.get(), 0);

    let flushed = flush(&mut core);
    assert_eq!(core.router_compiles.get(), 1);
    let filters: Vec<_> = wire_ops(&flushed)
        .into_iter()
        .filter_map(|op| match op {
            WireOp::Req(_, filter) => Some(filter),
            WireOp::Close(_) => None,
        })
        .collect();
    assert_eq!(filters.len(), 1);
    assert_eq!(
        filters[0].tags[&IndexedTagName::new('p').unwrap()],
        BTreeSet::from(["alice".to_owned(), "bob".to_owned()])
    );
}

#[test]
fn later_uncovered_demand_opens_a_second_req_without_replacing_the_running_one() {
    let relay = RelayUrl::parse("wss://admission-immutable.example").unwrap();
    let mut core = EngineCore::new(MemoryStore::new(), 20);
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    )));
    let first = flush(&mut core);
    let first_id = wire_ops(&first)
        .into_iter()
        .find_map(|op| match op {
            WireOp::Req(id, _) => Some(id.clone()),
            WireOp::Close(_) => None,
        })
        .unwrap();

    let later = core.handle(EngineMsg::Subscribe(query(&relay, "bob", Freshness::Live)));
    assert!(wire_ops(&later).is_empty());
    let second = flush(&mut core);
    let second_ops = wire_ops(&second);
    assert!(second_ops.iter().all(|op| !matches!(op, WireOp::Close(_))));
    let second_id = second_ops
        .into_iter()
        .find_map(|op| match op {
            WireOp::Req(id, _) => Some(id.clone()),
            WireOp::Close(_) => None,
        })
        .unwrap();
    assert_ne!(first_id, second_id);
    assert_eq!(
        core.router.plan().reqs[&RelaySessionKey::public(relay)].len(),
        2
    );
}

#[test]
fn duplicate_running_demand_attaches_without_compile_or_sibling_projection() {
    let relay = RelayUrl::parse("wss://admission-covered.example").unwrap();
    let mut core = EngineCore::new(MemoryStore::new(), 20);
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    )));
    flush(&mut core);
    core.projection_store_queries.set(0);
    core.router_compiles.set(0);

    let duplicate = core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    )));

    observation_id(&duplicate);
    assert_eq!(
        core.projection_store_queries.get(),
        1,
        "only the new observation may read its canonical cache projection"
    );
    assert_eq!(core.router_compiles.get(), 0);
    assert!(!duplicate
        .iter()
        .any(|effect| matches!(effect, Effect::ArmWireAdmission)));
    assert!(wire_ops(&duplicate).is_empty());
}

#[test]
fn cache_only_open_reads_only_its_own_projection_and_never_arms_wire() {
    let relay = RelayUrl::parse("wss://admission-cache-only.example").unwrap();
    let mut core = EngineCore::new(MemoryStore::new(), 20);
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    )));
    flush(&mut core);
    core.projection_store_queries.set(0);
    core.router_compiles.set(0);

    let cached = core.handle(EngineMsg::Subscribe(query(
        &relay,
        "bob",
        Freshness::CacheOnly,
    )));

    observation_id(&cached);
    assert_eq!(core.projection_store_queries.get(), 1);
    assert_eq!(core.router_compiles.get(), 0);
    assert!(!cached
        .iter()
        .any(|effect| matches!(effect, Effect::ArmWireAdmission)));
    assert!(wire_ops(&cached).is_empty());
}

#[test]
fn cancelling_a_pending_observation_before_flush_sends_nothing() {
    let relay = RelayUrl::parse("wss://admission-cancel.example").unwrap();
    let mut core = EngineCore::new(MemoryStore::new(), 20);
    let opened = core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    )));
    let id = observation_id(&opened);

    let closed = core.handle(EngineMsg::Unsubscribe(id));
    assert!(wire_ops(&closed).is_empty());
    assert!(wire_ops(&flush(&mut core)).is_empty());
}

#[test]
fn a_large_open_and_close_burst_never_reprojects_sibling_rows() {
    let relay = RelayUrl::parse("wss://admission-scale.example").unwrap();
    let mut core = EngineCore::new(MemoryStore::new(), 20);
    let mut observations = Vec::new();
    core.projection_store_queries.set(0);
    core.router_compiles.set(0);

    for index in 0..207 {
        let opened = core.handle(EngineMsg::Subscribe(query(
            &relay,
            &format!("person-{index:03}"),
            Freshness::Live,
        )));
        observations.push(observation_id(&opened));
    }

    assert_eq!(core.projection_store_queries.get(), 207);
    assert_eq!(core.router_compiles.get(), 0);
    let admitted = flush(&mut core);
    assert_eq!(core.router_compiles.get(), 1);
    assert_eq!(core.projection_store_queries.get(), 207);
    assert_eq!(
        wire_ops(&admitted)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        1
    );

    core.projection_store_queries.set(0);
    let mut diagnostics = 0;
    for observation in observations {
        diagnostics += core
            .handle(EngineMsg::Unsubscribe(observation))
            .iter()
            .filter(|effect| matches!(effect, Effect::EmitDiagnostics(_)))
            .count();
    }
    assert_eq!(core.projection_store_queries.get(), 0);
    assert_eq!(
        diagnostics, 1,
        "only the final owner changes the immutable relay plan"
    );
}

#[test]
fn history_open_waits_for_the_same_flush_without_refreshing_an_ordinary_sibling() {
    let relay = RelayUrl::parse("wss://admission-history.example").unwrap();
    let mut core = EngineCore::new(MemoryStore::new(), 20);
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    )));
    core.projection_store_queries.set(0);
    core.history_store_queries.set(0);
    core.router_compiles.set(0);

    let opened = core.handle(EngineMsg::SubscribeHistory(HistoryQuery::new(
        query(&relay, "bob", Freshness::Live),
        1,
        2,
    )));

    assert!(opened
        .iter()
        .any(|effect| matches!(effect, Effect::EmitHistory(_, _))));
    assert_eq!(core.projection_store_queries.get(), 0);
    assert_eq!(core.history_store_queries.get(), 1);
    assert_eq!(core.router_compiles.get(), 0);
    let admitted = flush(&mut core);
    assert_eq!(core.router_compiles.get(), 1);
    assert_eq!(core.projection_store_queries.get(), 0);
    assert_eq!(core.history_store_queries.get(), 1);
    assert_eq!(
        wire_ops(&admitted)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        2,
        "the bounded history filter cannot merge with an unlimited ordinary filter"
    );
}
