//! cohort admission proofs.

use super::*;

#[test]
fn cache_seed_is_immediate_while_wire_execution_waits_for_admission_flush() {
    let relay = RelayUrl::parse("wss://admission-seed.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);

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
fn unbounded_profile_observations_group_without_losing_independent_owners() {
    let relay = RelayUrl::parse("wss://admission-group.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let alice = Keys::generate().public_key();
    let bob = Keys::generate().public_key();

    let alice_opened = core.handle(EngineMsg::Subscribe(profile_query(&relay, alice)));
    let bob_opened = core.handle(EngineMsg::Subscribe(profile_query(&relay, bob)));
    let alice_observation = observation_id(&alice_opened);
    let bob_observation = observation_id(&bob_opened);
    assert_ne!(alice_observation, bob_observation);
    for opened in [&alice_opened, &bob_opened] {
        assert!(opened.iter().any(|effect| matches!(
            effect,
            Effect::EmitRows(_, _, evidence) if evidence.len() == 1
        )));
    }
    assert_eq!(core.router_compiles.get(), 0);

    core.request_target_demand_keys_touched.set(0);
    core.request_target_candidates_examined.set(0);
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
        filters[0].authors,
        Some(BTreeSet::from([alice.to_hex(), bob.to_hex()]))
    );
    assert_eq!(filters[0].kinds, Some(BTreeSet::from([0])));
    assert_eq!(filters[0].limit, None);
    let session = RelaySessionKey::unauthenticated(relay.clone());
    assert_eq!(
        relay_request_observations(&accept_first_request(&mut core, &session, 1)),
        BTreeSet::from([alice_observation, bob_observation])
    );
    assert_eq!(core.request_target_demand_keys_touched.get(), 2);
    assert_eq!(core.request_target_candidates_examined.get(), 2);

    assert!(wire_ops(&core.handle(EngineMsg::Unsubscribe(alice_observation))).is_empty());
    assert_eq!(
        wire_ops(&core.handle(EngineMsg::Unsubscribe(bob_observation)))
            .into_iter()
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn later_uncovered_demand_opens_a_second_req_without_replacing_the_running_one() {
    let relay = RelayUrl::parse("wss://admission-immutable.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
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
        core.router.plan().reqs[&RelaySessionKey::unauthenticated(relay)].len(),
        2
    );
}

#[test]
fn duplicate_running_demand_attaches_without_compile_or_sibling_projection() {
    let relay = RelayUrl::parse("wss://admission-covered.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
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
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
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
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
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
fn reattaching_a_covered_atom_keeps_its_shared_immutable_request_active() {
    let relay = RelayUrl::parse("wss://admission-reattach.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let first_a = observation_id(&core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    ))));
    let b =
        observation_id(&core.handle(EngineMsg::Subscribe(query(&relay, "bob", Freshness::Live))));
    assert_eq!(
        wire_ops(&flush(&mut core))
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        1
    );
    let (session, immutable_request) = core
        .router
        .plan()
        .reqs
        .iter()
        .next()
        .map(|(session, requests)| (session.clone(), requests[0].clone()))
        .unwrap();
    let handle = TransportRelayHandle {
        slot: 7,
        generation: 1,
    };
    accept_request(
        &mut core,
        &session,
        &immutable_request.sub_id,
        immutable_request.filter.hash(),
        handle,
    );

    assert!(wire_ops(&core.handle(EngineMsg::Unsubscribe(first_a))).is_empty());
    let reopened = core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    )));
    let second_a = observation_id(&reopened);
    assert!(wire_ops(&reopened).is_empty());
    assert!(!reopened
        .iter()
        .any(|effect| matches!(effect, Effect::ArmWireAdmission)));

    let b_closed = core.handle(EngineMsg::Unsubscribe(b));
    assert!(
        wire_ops(&b_closed).is_empty(),
        "reattached A still owns the shared physical request"
    );
    // The claim is that the reattached A is the one owner the shared request
    // keeps -- so name it. `owner_demands.len() == 1` and
    // `coverage_claims.len() == 1` were cardinalities where identities were
    // available, and both pass just as well if the DEPARTED B is the
    // survivor; `source` and `provenance` were not checked at all (#1850).
    // The request's identity (`sub_id`, `filter`, `source`, `provenance`) is
    // byte-identical across B's departure; its local metadata is exactly A's
    // contribution, which is the whole point of the immutable-request rule.
    let reattached = core
        .wire
        .atoms_for_handle(core.observations[&second_a].branches[0]);
    assert_eq!(
        core.router.plan().reqs.get(&session).unwrap(),
        &vec![nmp_router::WireReq {
            owner_demands: reattached.iter().map(DemandKey::for_atom).collect(),
            coverage_claims: reattached.iter().map(coverage_key).collect(),
            ..immutable_request.clone()
        }],
        "B's departure leaves the request's identity byte-identical and its \
         local metadata exactly A's"
    );

    let final_closed = core.handle(EngineMsg::Unsubscribe(second_a));
    assert_eq!(
        wire_ops(&final_closed)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn delayed_accepted_handoff_cannot_resurrect_a_fully_withdrawn_request() {
    let relay = RelayUrl::parse("wss://admission-delayed-handoff.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let observation = observation_id(&core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    ))));
    let flushed = flush(&mut core);
    assert_eq!(
        wire_ops(&flushed)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        1
    );
    let (session, request) = core
        .router
        .plan()
        .reqs
        .iter()
        .next()
        .map(|(session, requests)| (session.clone(), requests[0].clone()))
        .unwrap();
    let delayed_attempt = current_attempt(&core, &session, &request.sub_id, request.filter.hash());

    assert_eq!(
        wire_ops(&core.handle(EngineMsg::Unsubscribe(observation)))
            .into_iter()
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );

    let delayed = core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
        attempt_id: delayed_attempt,
        handle: TransportRelayHandle {
            slot: 9,
            generation: 1,
        },
    });
    assert!(delayed.is_empty());
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn pending_handoff_resolves_the_current_exact_owner_set() {
    for close_first_owner in [false, true] {
        let relay = RelayUrl::parse("wss://admission-pending-request-owners.example").unwrap();
        let session = RelaySessionKey::unauthenticated(relay.clone());
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
        let first = observation_id(&core.handle(EngineMsg::Subscribe(query(
            &relay,
            "same",
            Freshness::Live,
        ))));
        flush(&mut core);
        let request = core.router.plan().reqs[&session][0].clone();
        let second = observation_id(&core.handle(EngineMsg::Subscribe(query(
            &relay,
            "same",
            Freshness::Live,
        ))));
        let (departing, survivor) = if close_first_owner {
            (first, second)
        } else {
            (second, first)
        };
        assert!(wire_ops(&core.handle(EngineMsg::Unsubscribe(departing))).is_empty());
        assert_eq!(
            relay_request_observations(&accept_request(
                &mut core,
                &session,
                &request.sub_id,
                request.filter.hash(),
                TransportRelayHandle {
                    slot: 9,
                    generation: 1,
                },
            )),
            BTreeSet::from([survivor]),
            "a delayed callback cannot target a detached owner or omit a later exact attachment"
        );
        assert_eq!(
            wire_ops(&core.handle(EngineMsg::Unsubscribe(survivor)))
                .into_iter()
                .filter(|op| matches!(op, WireOp::Close(_)))
                .count(),
            1
        );
        assert_eq!(
            core.bench_ownership_census(),
            CoreOwnershipCensus::default()
        );
    }
}

#[test]
fn pending_execution_census_counts_every_revision_queued_under_one_wire_key() {
    let relay = RelayUrl::parse("wss://admission-pending-census.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let observation =
        observation_id(&core.handle(EngineMsg::Subscribe(query(&relay, "same", Freshness::Live))));
    flush(&mut core);
    let request = core.router.plan().reqs[&session][0].clone();
    core.white_box("record_observed_request", |s| {
        s.record_observed_request(RequestSend {
            session: &session,
            sub_id: &request.sub_id,
            filter: &request.filter,
            coverage_claims: request.coverage_claims.clone(),
            owner_demands: request.owner_demands.clone(),
            lanes: BTreeSet::new(),
            replay: true,
            event_failure_target: EventFailureTarget::ThisSend,
        })
    });

    assert_eq!(core.pending_request_evidence.len(), 1);
    assert_eq!(
        core.pending_request_evidence[&(session.clone(), request.sub_id.clone())].len(),
        2
    );
    assert_eq!(core.bench_ownership_census().pending_execution_owners, 2);
    assert_eq!(
        core.bench_ownership_census().pending_execution_owner_keys,
        1
    );
    assert_eq!(
        core.observation_ownership_census().pending_execution_owners,
        2
    );
    assert_eq!(
        core.observation_ownership_census()
            .pending_execution_owner_keys,
        1
    );

    core.white_box("abandon_sub", |s| s.abandon_sub(&request.sub_id));
    core.handle(EngineMsg::Unsubscribe(observation));
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}
