//! Fresh-id request replacement and NIP-77 transition ownership.

use super::*;

#[test]
fn changed_filter_uses_fresh_id_keeps_old_on_refusal_and_retires_it_only_after_accept() {
    let relay = RelayUrl::parse("wss://fresh-request-id.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let handle = TransportRelayHandle {
        slot: 82,
        generation: 1,
    };
    let first = atom(&relay, &"11".repeat(32));
    let second = atom(&relay, &"22".repeat(32));
    let first_claim = coverage_key(&first);
    let second_claim = coverage_key(&second);
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
    core.slot_to_relay
        .insert(handle.slot, (handle, session.clone()));
    core.connected_relays.insert(session.clone());
    core.ever_connected_relays.insert(session.clone());
    core.attribution.observe_atom(&first);

    let opened = apply_compile(&mut core, BTreeSet::from([first.clone()]));
    let (_, first_sub_id, first_filter, first_attempt) = only_request(&opened);
    core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
        attempt_id: first_attempt,
        handle,
    });
    assert!(core
        .live_wire_requests
        .contains_key(&(session.clone(), first_sub_id.clone())));

    core.attribution.observe_atom(&second);
    core.attribution.release_atom(&first);
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

    let due = core.next_deadline().unwrap().unwrap();
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

    core.clock = Timestamp::from(200u64);
    for sub_id in [&first_sub_id, &second_sub_id] {
        core.on_relay_frame(
            handle,
            session.clone(),
            RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
                nostr::SubscriptionId::new(wire_sub_id_string(sub_id)),
            ))),
        );
    }
    assert!(core
        .store
        .get_coverage(first_claim, &relay)
        .unwrap()
        .is_none());
    assert!(core
        .store
        .get_coverage(second_claim, &relay)
        .unwrap()
        .is_some());

    let closed = apply_compile(&mut core, BTreeSet::new());
    assert!(wire_ops(&closed)
        .iter()
        .any(|op| matches!(op, WireOp::Close(sub_id) if sub_id == &second_sub_id)));
    core.attribution.release_atom(&second);
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
    assert_ne!(first_filter, second_filter);
}

#[test]
fn nip77_replacement_keeps_old_child_through_local_accept_and_commits_at_candidate_eose() {
    let relay = RelayUrl::parse("wss://fresh-nip77-id.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let handle = TransportRelayHandle {
        slot: 83,
        generation: 1,
    };
    let first = atom(&relay, &"33".repeat(32));
    let second = atom(&relay, &"44".repeat(32));
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
    core.slot_to_relay
        .insert(handle.slot, (handle, session.clone()));
    core.connected_relays.insert(session.clone());
    core.ever_connected_relays.insert(session.clone());
    core.attribution.observe_atom(&first);

    let opened = apply_compile(&mut core, BTreeSet::from([first.clone()]));
    let (_, first_plan_sub, first_filter, first_attempt) = only_request(&opened);
    core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
        attempt_id: first_attempt,
        handle,
    });

    core.prober
        .states
        .insert(relay.clone(), crate::negentropy::ProbeState::Supported);
    let probed = core.prober.probed(&relay).unwrap();
    let mut handoff_effects = Vec::new();
    core.begin_neg_handoff(
        probed,
        first_plan_sub.clone(),
        Some(first_plan_sub.clone()),
        first_filter,
        &mut handoff_effects,
    );
    let (_, old_child, _, old_child_attempt) = only_request(&handoff_effects);
    core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
        attempt_id: old_child_attempt,
        handle,
    });
    core.on_relay_frame(
        handle,
        session.clone(),
        RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
            nostr::SubscriptionId::new(wire_sub_id_string(&old_child)),
        ))),
    );
    assert_eq!(
        core.active_nip77_live.get(&first_plan_sub),
        Some(&old_child)
    );

    core.attribution.observe_atom(&second);
    core.attribution.release_atom(&first);
    let replacement = apply_compile(&mut core, BTreeSet::from([second]));
    let (_, second_plan_child, _, second_attempt) = only_request(&replacement);
    assert_ne!(old_child, second_plan_child);
    assert!(wire_ops(&replacement)
        .iter()
        .all(|op| !matches!(op, WireOp::Close(sub_id) if sub_id == &old_child)));
    assert_eq!(core.bench_ownership_census().request_replacement_jobs, 1);
    assert_eq!(
        core.active_nip77_live.get(&first_plan_sub),
        Some(&old_child)
    );

    let accepted = core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
        attempt_id: second_attempt,
        handle,
    });
    assert!(wire_ops(&accepted)
        .iter()
        .all(|op| !matches!(op, WireOp::Close(sub_id) if sub_id == &old_child)));
    assert_eq!(
        core.active_nip77_live.get(&first_plan_sub),
        Some(&old_child)
    );
    assert_eq!(core.bench_ownership_census().request_replacement_jobs, 1);

    let promoted = core.on_relay_frame(
        handle,
        session.clone(),
        RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
            nostr::SubscriptionId::new(wire_sub_id_string(&second_plan_child)),
        ))),
    );
    assert_eq!(
        wire_ops(&promoted)
            .iter()
            .filter(|op| matches!(op, WireOp::Close(sub_id) if sub_id == &old_child))
            .count(),
        1
    );
    assert!(!core.active_nip77_live.contains_key(&first_plan_sub));
    assert_eq!(core.bench_ownership_census().request_replacement_jobs, 0);
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
