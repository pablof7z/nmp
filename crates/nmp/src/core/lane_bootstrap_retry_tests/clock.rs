use super::*;

/// A durable lane parked on `WaitingConnection` may wake after a long idle.
/// The attempt starts at current command time, without running maintenance.
#[test]
fn waiting_connection_attempt_is_anchored_to_connect_time_without_maintenance() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://waiting-connection-clock.example.com").unwrap();
    let old_time = Timestamp::from(100u64);
    let connect_time = Timestamp::from(10_000u64);
    let store = RedbStore::temporary().expect("temporary Redb store");
    let mut core = EngineCore::new(store, 10);
    core.advance_clock(old_time);

    let (receipt, _, parked) =
        publish_narrow(&mut core, &author, std::slice::from_ref(&relay), 706);
    assert!(!parked
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    let intent = core.pending[&receipt].intent_id;
    let session = session_for(&relay, &author);
    let handle = TransportRelayHandle {
        slot: 9,
        generation: 1,
    };

    core.advance_clock(connect_time);
    let connected = core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    assert!(!connected
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    let ready = core.handle(EngineMsg::AuthProbeReleased(handle, session.clone()));
    assert!(ready.iter().any(
        |effect| matches!(effect, Effect::PublishEvent(candidate, _, _) if candidate == &session)
    ));
    let attempts = core
        .resolver
        .store()
        .recover_attempt_details(intent)
        .expect("attempt-detail recovery");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].started_at, Some(connect_time));
    assert_eq!(core.maintenance_turn_count(), 0);
}
