//! Fresh-id request replacement transition ownership.

use nmp_grammar::RelaySessionKey;
use super::*;

#[test]
fn changed_filter_uses_fresh_id_keeps_old_on_refusal_and_retires_it_only_after_accept() {
    let relay = RelayUrl::parse("wss://fresh-request-id.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let handle = TransportRelayHandle {
        slot: 82,
        generation: 1,
    };
    let first = atom(&relay, &"11".repeat(32));
    let second = atom(&relay, &"22".repeat(32));
    let first_claim = coverage_key(&first);
    let second_claim = coverage_key(&second);
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
    core.white_box("slot_to_relay.insert", |s| {
        s.slot_to_relay
            .insert(handle.slot, (handle, session.clone()))
    });
    core.white_box("connected_relays.insert", |s| {
        s.connected_relays.insert(session.clone())
    });
    core.white_box("ever_connected_relays.insert", |s| {
        s.ever_connected_relays.insert(session.clone())
    });

    let opened = apply_compile(&mut core, BTreeSet::from([first.clone()]));
    let (_, first_sub_id, first_filter, first_attempt) = only_request(&opened);
    core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
        attempt_id: first_attempt,
        handle,
    });
    assert!(core
        .live_wire_requests
        .contains_key(&(session.clone(), first_sub_id.clone())));

    let replacement = apply_compile(&mut core, BTreeSet::from([second.clone()]));
    let (_, second_sub_id, second_filter, second_attempt) = only_request(&replacement);
    assert_ne!(first_sub_id, second_sub_id);
    assert!(wire_ops(&replacement)
        .iter()
        .all(|op| { !matches!(op, WireOp::Close(sub_id) if sub_id == &first_sub_id) }));
    assert!(core
        .live_wire_requests
        .contains_key(&(session.clone(), first_sub_id.clone())));

    let refused = core.on_wire_request_handoff(RequestHandoffOutcome::Refused {
        attempt_id: second_attempt,
        cause: LocalSendRefusal::SessionUnavailable,
    });
    assert!(wire_ops(&refused)
        .iter()
        .all(|op| !matches!(op, WireOp::Close(_))));
    assert!(core
        .live_wire_requests
        .contains_key(&(session.clone(), first_sub_id.clone())));
    assert_eq!(core.bench_ownership_census().request_retry_jobs, 1);

    let due = core.next_deadline().unwrap();
    let retried = core.handle(EngineMsg::Tick(due));
    let (_, retried_sub_id, retried_filter, retry_attempt) = only_request(&retried);
    assert_eq!(retried_sub_id, second_sub_id);
    assert_eq!(retried_filter, second_filter);
    let accepted = core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
        attempt_id: retry_attempt,
        handle,
    });
    assert_eq!(
        wire_ops(&accepted)
            .iter()
            .filter(|op| matches!(op, WireOp::Close(sub_id) if sub_id == &first_sub_id))
            .count(),
        1
    );
    assert!(!core
        .live_wire_requests
        .contains_key(&(session.clone(), first_sub_id.clone())));
    assert!(core
        .live_wire_requests
        .contains_key(&(session.clone(), second_sub_id.clone())));
    assert_eq!(core.active_request_evidence.len(), 1);

    core.white_box("clock", |s| s.clock = Timestamp::from(200u64));
    for sub_id in [&first_sub_id, &second_sub_id] {
        core.white_box("on_relay_frame", |s| {
            s.on_relay_frame(
                handle,
                session.clone(),
                RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
                    nostr::SubscriptionId::new(wire_sub_id_string(sub_id)),
                ))),
            )
        });
    }
    assert!(core
        .store
        .get_coverage(first_claim, &RelaySessionKey::unauthenticated(relay.clone()))
        .unwrap()
        .is_none());
    assert!(core
        .store
        .get_coverage(second_claim, &RelaySessionKey::unauthenticated(relay.clone()))
        .unwrap()
        .is_some());

    let closed = apply_compile(&mut core, BTreeSet::new());
    assert!(wire_ops(&closed)
        .iter()
        .any(|op| matches!(op, WireOp::Close(sub_id) if sub_id == &second_sub_id)));
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
    assert_ne!(first_filter, second_filter);
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
