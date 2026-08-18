//! Protected-session request-attempt ownership.

use super::*;

#[test]
fn protected_retry_cannot_cross_to_a_fresh_unauthenticated_transport_generation() {
    let relay = RelayUrl::parse("wss://protected-request-retry.example").unwrap();
    let session = RelaySessionKey::new(
        relay.clone(),
        Some(nostr::Keys::generate().public_key()),
    );
    let filter = ConcreteFilter {
        kinds: Some(BTreeSet::from([1u16])),
        ..ConcreteFilter::default()
    };
    let atom = ContextualAtom {
        filter: filter.clone(),
        routing: ReadRouting::Explicit(vec![relay]),
        authenticate_as: session.authenticate_as,
        routing_evidence: BTreeSet::new(),
    };
    let sub_id = SubId::allocate(session.relay.clone(), &atom.routing, atom.authenticate_as, 1007);
    let claims = BTreeSet::from([nmp_store::coverage_key(&atom)]);
    let owners = BTreeSet::from([nmp_router::DemandKey::for_atom(&atom)]);
    let first_handle = TransportRelayHandle {
        slot: 95,
        generation: 1,
    };
    let next_handle = TransportRelayHandle {
        slot: 95,
        generation: 2,
    };
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
    core.set_active_demand(&BTreeSet::from([atom.clone()]));
    core.white_box("attribution.retain_live_request_claims", |s| {
        s.attribution
            .retain_live_request_claims(&sub_id, claims.clone())
    });
    core.white_box("install_plan_execution_metadata", |s| {
        s.install_plan_execution_metadata(
            sub_id.clone(),
            filter.clone(),
            claims.clone(),
            owners.clone(),
            BTreeSet::new(),
        )
    });
    core.white_box("slot_to_relay.insert", |s| {
        s.slot_to_relay
            .insert(first_handle.slot, (first_handle, session.clone()))
    });
    core.white_box("record_observed_request", |s| {
        s.record_observed_request(RequestSend {
            session: &session,
            sub_id: &sub_id,
            filter: &filter,
            coverage_claims: claims,
            owner_demands: owners,
            lanes: BTreeSet::new(),
            replay: false,
            event_failure_target: EventFailureTarget::ThisSend,
        })
    });
    let attempt_id = core.pending_request_evidence[&(session.clone(), sub_id.clone())]
        .back()
        .unwrap()
        .attempt_id;
    core.on_wire_request_handoff(RequestHandoffOutcome::Refused {
        attempt_id,
        cause: LocalSendRefusal::WorkerAdmissionRefused {
            handle: first_handle,
        },
    });
    assert_eq!(core.bench_ownership_census().request_retry_jobs, 1);

    core.white_box("on_relay_connected", |s| {
        s.on_relay_connected(next_handle, session.clone())
    });
    let census = core.bench_ownership_census();
    assert_eq!(census.request_retry_jobs, 0);
    assert_eq!(census.request_retry_sub_keys, 0);
    assert_eq!(census.request_retry_session_keys, 0);
    assert_eq!(census.request_attempts, 0);

    core.white_box("on_relay_disconnected", |s| {
        s.on_relay_disconnected(next_handle, session, DisconnectReason::Closed)
    });
    core.white_box("retire_plan_execution_metadata", |s| {
        s.retire_plan_execution_metadata(&sub_id)
    });
    core.set_active_demand(&BTreeSet::new());
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}
