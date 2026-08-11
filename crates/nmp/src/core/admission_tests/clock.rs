use std::borrow::Cow;

use super::*;
use crate::lane_fault_store::{FaultyLaneStore, LaneFaults};
use nmp_store::{EventStore, MemoryStore};
use nostr::SubscriptionId;

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
            Effect::StartProbe(_, url, sub_id, ..) if url == relay => Some(sub_id.clone()),
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

/// A delayed admission transition owns the timestamp of any NIP-77 liveness
/// state it creates. Advancing that stamp is not permission to run expiry,
/// retry, or liveness maintenance.
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
        Some(admission_time + NEG_LIVENESS_DEADLINE_SECS)
    );
    assert_eq!(faults.maintenance_sweeps(), 0);
}

/// Reconnect replay can enter the same live-first NIP-77 handoff without an
/// admission flush, so the new generation must inherit connect time rather
/// than the reducer's last maintenance time.
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
        Some(connect_time + NEG_LIVENESS_DEADLINE_SECS)
    );
    assert_eq!(faults.maintenance_sweeps(), 0);
}
