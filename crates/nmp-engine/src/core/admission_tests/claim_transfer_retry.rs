//! claim transfer retry admission proofs.

use super::*;

#[test]
fn post_eose_claim_transfer_retries_the_exact_generation_after_one_store_failure() {
    let relay = RelayUrl::parse("wss://post-eose-transfer-retry.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let incumbent = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1, 2])),
            since: Some(100),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![relay.clone()]),
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    };
    let added = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            since: Some(100),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![relay.clone()]),
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    };
    let incumbent_claim = coverage_key(&incumbent);
    let added_claim = coverage_key(&added);
    let old = CoverageInterval::new(Timestamp::from(0), Timestamp::from(99));
    let generation = CoverageInterval::new(Timestamp::from(100), Timestamp::from(200));
    let directory = tempfile::tempdir().expect("coverage retry directory");
    let path = directory.path().join("post-eose-transfer-retry.redb");
    {
        let mut store = RedbStore::open(&path).expect("create persistent Redb fixture");
        store
            .record_coverage(&[(added.clone(), relay.clone(), old)])
            .unwrap();
    }
    let store = RedbStore::open_with_failed_coverage_write(&path, added_claim, relay.clone())
        .expect("reopen exact coverage-write failure fixture");
    let mut core = EngineCore::new(store, 20);
    core.set_active_demand(&BTreeSet::from([incumbent.clone(), added.clone()]));
    let sub_id = SubId::for_wire(
        relay.clone(),
        &incumbent.filter,
        &incumbent.routing,
        incumbent.access,
    );
    core.white_box("attribution.retain_live_request_claims", |s| {
        s.attribution
            .retain_live_request_claims(&sub_id, BTreeSet::from([incumbent_claim]))
    });
    core.white_box("live_wire_requests.insert", |s| {
        s.live_wire_requests.insert(
            (session.clone(), sub_id.clone()),
            LiveWireRequest {
                filter: incumbent.filter.clone(),
                evidence_sub_id: sub_id.clone(),
                handle: TransportRelayHandle {
                    slot: 77,
                    generation: 1,
                },
                stored_events: super::observation::StoredEvents::Finished {
                    request_revision: 9,
                    committed_interval: Some(generation),
                },
                returns: Default::default(),
            },
        )
    });

    let mut failed = Vec::new();
    core.white_box("apply_request_metadata_updates", |s| {
        s.apply_request_metadata_updates(
            &[nmp_router::RequestMetadataUpdate {
                session: session.clone(),
                sub_id: sub_id.clone(),
                filter_hash: incumbent.filter.hash(),
                added_coverage_claims: BTreeSet::from([added_claim]),
                added_owner_demands: BTreeSet::from([DemandKey::for_atom(&added)]),
            }],
            &mut failed,
        )
    });
    assert_eq!(core.pending_request_claim_transfers.len(), 1);
    assert_eq!(
        core.bench_ownership_census()
            .pending_request_claim_transfer_jobs,
        1
    );
    assert_eq!(
        core.bench_ownership_census()
            .pending_request_claim_transfer_claims,
        1
    );
    assert!(!failed
        .iter()
        .any(|effect| matches!(effect, Effect::EmitObservationEvidence(..))));
    assert_eq!(
        core.store.get_coverage(added_claim, &relay).unwrap(),
        Some(old),
        "failure cannot mutate durable coverage or publish freshness"
    );
    assert_eq!(core.request_claim_transfer_attempts.get(), 1);
    assert_eq!(core.request_claim_transfer_claims_attempted.get(), 1);
    assert_eq!(core.request_claim_transfer_failures.get(), 1);
    assert_eq!(core.request_claim_transfer_commits.get(), 0);
    core.set_active_demand(&BTreeSet::from([incumbent.clone()]));

    core.white_box("retry_scheduler_blocked", |s| {
        s.retry_scheduler_blocked = true
    });
    let due = core
        .next_deadline()
        .unwrap()
        .expect("the failed transfer owns a bounded retry deadline");
    let retried = core.tick(due);
    assert!(core.pending_request_claim_transfers.is_empty());
    assert_eq!(core.request_claim_transfer_attempts.get(), 2);
    assert_eq!(core.request_claim_transfer_claims_attempted.get(), 2);
    assert_eq!(core.request_claim_transfer_failures.get(), 1);
    assert_eq!(core.request_claim_transfer_commits.get(), 1);
    assert!(!retried
        .iter()
        .any(|effect| matches!(effect, Effect::EmitObservationEvidence(..))));
    assert_eq!(
        core.store.get_coverage(added_claim, &relay).unwrap(),
        Some(CoverageInterval::new(
            Timestamp::from(0),
            Timestamp::from(200)
        )),
        "the store may merge rows, but the transfer itself records only the current generation"
    );
    assert!(core.pending_request_claim_transfers.is_empty());

    core.white_box("live_wire_requests.remove", |s| {
        s.live_wire_requests.remove(&(session, sub_id.clone()))
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

#[test]
fn successful_same_filter_eose_supersedes_an_older_pending_claim_transfer() {
    let relay = RelayUrl::parse("wss://post-eose-transfer-superseded.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let incumbent = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1, 2])),
            since: Some(100),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![relay.clone()]),
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    };
    let added = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            since: Some(100),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![relay.clone()]),
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    };
    let incumbent_claim = coverage_key(&incumbent);
    let added_claim = coverage_key(&added);
    let directory = tempfile::tempdir().expect("coverage supersession directory");
    let path = directory.path().join("post-eose-transfer-superseded.redb");
    let store = RedbStore::open_with_failed_coverage_write(&path, added_claim, relay.clone())
        .expect("persistent exact coverage-write failure fixture");
    let mut core = EngineCore::new(store, 20);
    core.set_active_demand(&BTreeSet::from([incumbent.clone(), added.clone()]));
    let sub_id = SubId::for_wire(
        relay.clone(),
        &incumbent.filter,
        &incumbent.routing,
        incumbent.access,
    );
    core.white_box("attribution.retain_live_request_claims", |s| {
        s.attribution
            .retain_live_request_claims(&sub_id, BTreeSet::from([incumbent_claim]))
    });
    core.white_box("record_observed_request", |s| {
        s.record_observed_request(RequestSend {
            session: &session,
            sub_id: &sub_id,
            filter: &incumbent.filter,
            coverage_claims: BTreeSet::from([incumbent_claim]),
            owner_demands: BTreeSet::from([DemandKey::for_atom(&incumbent)]),
            lanes: BTreeSet::new(),
            replay: false,
            event_failure_target: EventFailureTarget::ThisSend,
        })
    });
    let first_transport = TransportRelayHandle {
        slot: 80,
        generation: 1,
    };
    core.white_box("slot_to_relay.insert", |s| {
        s.slot_to_relay
            .insert(first_transport.slot, (first_transport, session.clone()))
    });
    accept_request(
        &mut core,
        &session,
        &sub_id,
        incumbent.filter.hash(),
        first_transport,
    );
    core.white_box("clock", |s| s.clock = Timestamp::from(200u64));
    core.white_box("on_relay_frame", |s| {
        s.on_relay_frame(
            first_transport,
            session.clone(),
            RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
                nostr::SubscriptionId::new(wire_sub_id_string(&sub_id)),
            ))),
        )
    });

    core.white_box("apply_request_metadata_updates", |s| {
        s.apply_request_metadata_updates(
            &[nmp_router::RequestMetadataUpdate {
                session: session.clone(),
                sub_id: sub_id.clone(),
                filter_hash: incumbent.filter.hash(),
                added_coverage_claims: BTreeSet::from([added_claim]),
                added_owner_demands: BTreeSet::from([DemandKey::for_atom(&added)]),
            }],
            &mut Vec::new(),
        )
    });
    assert_eq!(core.pending_request_claim_transfers.len(), 1);
    assert_eq!(core.request_claim_transfer_attempts.get(), 1);

    core.white_box("record_observed_request", |s| {
        s.record_observed_request(RequestSend {
            session: &session,
            sub_id: &sub_id,
            filter: &incumbent.filter,
            coverage_claims: BTreeSet::from([incumbent_claim, added_claim]),
            owner_demands: BTreeSet::from([
                DemandKey::for_atom(&incumbent),
                DemandKey::for_atom(&added),
            ]),
            lanes: BTreeSet::new(),
            replay: true,
            event_failure_target: EventFailureTarget::ThisSend,
        })
    });
    let transport = TransportRelayHandle {
        slot: 80,
        generation: 2,
    };
    core.white_box("slot_to_relay.insert", |s| {
        s.slot_to_relay
            .insert(transport.slot, (transport, session.clone()))
    });
    accept_request(
        &mut core,
        &session,
        &sub_id,
        incumbent.filter.hash(),
        transport,
    );
    core.white_box("clock", |s| s.clock = Timestamp::from(250u64));
    let _ = core.white_box("on_relay_frame", |s| {
        s.on_relay_frame(
            transport,
            session.clone(),
            RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
                nostr::SubscriptionId::new(wire_sub_id_string(&sub_id)),
            ))),
        )
    });

    assert!(core.pending_request_claim_transfers.is_empty());
    assert_eq!(core.request_claim_transfer_attempts.get(), 1);
    let census = core.bench_ownership_census();
    assert_eq!(census.attribution_live_shape_keys, 2);
    assert_eq!(census.attribution_live_shape_refs, 2);
    core.white_box("retry_scheduler_blocked", |s| {
        s.retry_scheduler_blocked = true
    });
    assert_eq!(core.next_deadline().unwrap(), None);
    core.tick(Timestamp::from(300u64));
    assert_eq!(core.request_claim_transfer_attempts.get(), 1);
    assert_eq!(
        core.store.get_coverage(added_claim, &relay).unwrap(),
        Some(CoverageInterval::new(
            Timestamp::from(100),
            Timestamp::from(250)
        ))
    );

    core.white_box("live_wire_requests.remove", |s| {
        s.live_wire_requests
            .remove(&(session.clone(), sub_id.clone()))
    });
    core.white_box("retire_plan_execution_metadata", |s| {
        s.retire_plan_execution_metadata(&sub_id)
    });
    core.white_box("abandon_sub", |s| s.abandon_sub(&sub_id));
    core.white_box("slot_to_relay.remove", |s| {
        s.slot_to_relay.remove(&transport.slot)
    });
    core.set_active_demand(&BTreeSet::new());
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}
