//! Admission-window and surgical lifecycle falsifiers for #1340/#1341.

use std::borrow::Cow;

use super::*;
use crate::lane_fault_store::{FaultyLaneStore, LaneFaults};
use nmp_grammar::{Binding, Demand, Filter, IndexedTagName};
use nmp_store::MemoryStore;
use nostr::SubscriptionId;

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

fn prove_nip77<S: EventStore>(
    core: &mut EngineCore<S>,
    relay: &RelayUrl,
    transport: TransportRelayHandle,
    now: Timestamp,
) -> RelaySessionKey {
    let session = RelaySessionKey::public(relay.clone());
    core.advance_clock(now);
    let mut connected = core.handle(EngineMsg::RelayConnected(transport, session.clone()));
    connected.extend(core.handle(EngineMsg::RelayInformationResolved(relay.clone(), None)));
    let probe_sub_id = connected
        .iter()
        .find_map(|effect| match effect {
            Effect::StartProbe(url, sub_id, ..) if url == relay => Some(sub_id.clone()),
            _ => None,
        })
        .expect("the demanded relay starts a behavioral NIP-77 probe");
    core.handle(EngineMsg::RelayFrame(
        transport,
        session.clone(),
        RelayFrame::from(RelayMessage::NegMsg {
            subscription_id: Cow::Owned(SubscriptionId::new(probe_sub_id.1.to_string())),
            message: Cow::Borrowed("6100"),
        }),
    ));
    session
}

fn flush(core: &mut EngineCore<MemoryStore>) -> Vec<Effect> {
    core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)))
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

/// #1344 fix-up: the delayed admission boundary, rather than the app's
/// earlier subscribe call, owns the timestamp of any NIP-77 liveness state it
/// creates. Advancing that stamp must remain a cheap clock assignment: it is
/// not permission to run expiry, retry, or liveness maintenance.
#[test]
fn nip77_liveness_is_anchored_to_admission_time_without_maintenance() {
    let relay = RelayUrl::parse("wss://admission-clock.example").unwrap();
    let old_time = Timestamp::from(100u64);
    let admission_time = Timestamp::from(10_000u64);
    let faults = LaneFaults::default();
    let store = FaultyLaneStore::new(MemoryStore::new(), faults.clone());
    let mut core = EngineCore::new(store, 20);

    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    )));
    core.handle(EngineMsg::FlushWireAdmission(old_time));

    let transport = TransportRelayHandle {
        slot: 0,
        generation: 1,
    };
    prove_nip77(&mut core, &relay, transport, old_time);

    core.handle(EngineMsg::Subscribe(query(&relay, "bob", Freshness::Live)));
    let admitted = core.handle(EngineMsg::FlushWireAdmission(admission_time));
    assert!(wire_ops(&admitted)
        .into_iter()
        .any(|op| matches!(op, WireOp::Req(_, filter) if filter.limit == Some(0))));
    assert_eq!(
        core.next_deadline().expect("deadline read"),
        Some(admission_time + NEG_LIVENESS_DEADLINE_SECS),
        "the fresh handoff gets its full liveness window from actual admission"
    );
    assert_eq!(
        faults.maintenance_sweeps(),
        0,
        "stamping admission time must not execute deadline maintenance"
    );
}

/// #1344 fix-up: reconnect replay can enter the same live-first NIP-77
/// handoff without an admission flush. That transition owns its event time
/// explicitly too; a long idle period must not spend the new generation's
/// liveness window before the socket has even connected.
#[test]
fn nip77_reconnect_liveness_is_anchored_to_connect_time_without_maintenance() {
    let relay = RelayUrl::parse("wss://reconnect-clock.example").unwrap();
    let old_time = Timestamp::from(100u64);
    let connect_time = Timestamp::from(10_000u64);
    let faults = LaneFaults::default();
    let store = FaultyLaneStore::new(MemoryStore::new(), faults.clone());
    let mut core = EngineCore::new(store, 20);

    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    )));
    core.handle(EngineMsg::FlushWireAdmission(old_time));
    let first = TransportRelayHandle {
        slot: 0,
        generation: 1,
    };
    let session = prove_nip77(&mut core, &relay, first, old_time);
    core.handle(EngineMsg::RelayDisconnected(
        first,
        session.clone(),
        DisconnectReason::Error,
    ));

    core.advance_clock(connect_time);
    let reconnected = core.handle(EngineMsg::RelayConnected(
        TransportRelayHandle {
            slot: 0,
            generation: 2,
        },
        session,
    ));
    assert!(wire_ops(&reconnected)
        .into_iter()
        .any(|op| matches!(op, WireOp::Req(_, filter) if filter.limit == Some(0))));
    assert_eq!(
        core.next_deadline().expect("deadline read"),
        Some(connect_time + NEG_LIVENESS_DEADLINE_SECS),
        "the fresh generation gets its full liveness window from connect"
    );
    assert_eq!(
        faults.maintenance_sweeps(),
        0,
        "stamping connect time must not execute deadline maintenance"
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
