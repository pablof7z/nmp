//! lifecycle admission proofs.

use nmp_grammar::RelaySessionKey;
use super::*;

#[test]
fn outstanding_request_terminals_follow_current_exact_owners_after_attachment_churn() {
    for close_first_owner in [false, true] {
        for relay_closes_before_eose in [false, true] {
            let relay = RelayUrl::parse("wss://admission-active-request-owners.example").unwrap();
            let session = RelaySessionKey::unauthenticated(relay.clone());
            let mut core =
                EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
            let first = observation_id(&core.handle(EngineMsg::Subscribe(query(
                &relay,
                "same",
                Freshness::Live,
            ))));
            flush(&mut core);
            let request = core.router.plan().reqs[&session][0].clone();
            let transport = TransportRelayHandle {
                slot: 10,
                generation: 1,
            };
            assert_eq!(
                relay_request_observations(&accept_request(
                    &mut core,
                    &session,
                    &request.sub_id,
                    request.filter.hash(),
                    transport,
                )),
                BTreeSet::from([first])
            );

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

            let mut terminal_effects = Vec::new();
            if relay_closes_before_eose {
                core.white_box("close_requests_for_sub", |s| {
                    s.close_requests_for_sub(
                        &session,
                        transport,
                        &request.sub_id,
                        "fixture relay close".to_string(),
                        &mut terminal_effects,
                    )
                });
            } else {
                let completed = core.white_box("attribution.attribute_eose_detailed", |s| {
                    s.attribution
                        .attribute_eose_detailed(
                            &session,
                            &wire_sub_id_string(&request.sub_id),
                            Timestamp::from(101u64),
                        )
                        .expect("the accepted request owns one outstanding EOSE")
                });
                core.white_box("emit_request_settled", |s| {
                    s.emit_request_settled(
                        completed.send_id(),
                        Timestamp::from(101u64),
                        RequestTerminal::Eose,
                        &mut terminal_effects,
                    )
                });
            }
            let terminal_owners: BTreeSet<_> = terminal_effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::EmitObservationEvidence(observation, evidence)
                        if evidence.iter().any(|fact| {
                            matches!(
                                fact.fact,
                                ObservationFact::RequestSettled { .. }
                                    | ObservationFact::RelayClosed { .. }
                            )
                        }) =>
                    {
                        Some(*observation)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                terminal_owners,
                BTreeSet::from([survivor]),
                "an outstanding request terminal resolves the current exact owner set"
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
}

#[test]
fn settled_departing_shape_remains_owned_by_the_shared_immutable_request() {
    let relay = RelayUrl::parse("wss://admission-attribution-prune.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let a = observation_id(&core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    ))));
    let b =
        observation_id(&core.handle(EngineMsg::Subscribe(query(&relay, "bob", Freshness::Live))));
    flush(&mut core);
    let (session, request) = core
        .router
        .plan()
        .reqs
        .iter()
        .next()
        .map(|(session, requests)| (session.clone(), requests[0].clone()))
        .unwrap();
    let handle = TransportRelayHandle {
        slot: 11,
        generation: 1,
    };
    accept_request(
        &mut core,
        &session,
        &request.sub_id,
        request.filter.hash(),
        handle,
    );
    let completed = core.white_box("attribution.attribute_eose_detailed", |s| {
        s.attribution
            .attribute_eose_detailed(
                &session,
                &wire_sub_id_string(&request.sub_id),
                Timestamp::from(1_000u64),
            )
            .unwrap()
    });
    core.white_box("retire_request_evidence", |s| {
        s.retire_request_evidence(completed.send_id())
    });
    assert_eq!(core.bench_ownership_census().attribution_shape_keys, 2);

    assert!(wire_ops(&core.handle(EngineMsg::Unsubscribe(a))).is_empty());
    assert_eq!(
        core.bench_ownership_census().attribution_shape_keys,
        2,
        "the still-live immutable request owns every claim it may replay"
    );

    assert_eq!(
        wire_ops(&core.handle(EngineMsg::Unsubscribe(b)))
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
fn departing_shape_remains_owned_through_atomic_eose_persistence() {
    let relay = RelayUrl::parse("wss://admission-attribution-completion.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let a = observation_id(&core.handle(EngineMsg::Subscribe(query(
        &relay,
        "alice",
        Freshness::Live,
    ))));
    let b =
        observation_id(&core.handle(EngineMsg::Subscribe(query(&relay, "bob", Freshness::Live))));
    flush(&mut core);
    let (session, request) = core
        .router
        .plan()
        .reqs
        .iter()
        .next()
        .map(|(session, requests)| (session.clone(), requests[0].clone()))
        .unwrap();
    assert_eq!(
        request.coverage_claims.len(),
        2,
        "the fixture requires one coalesced request with two exact coverage claims"
    );
    accept_request(
        &mut core,
        &session,
        &request.sub_id,
        request.filter.hash(),
        TransportRelayHandle {
            slot: 12,
            generation: 1,
        },
    );

    assert!(wire_ops(&core.handle(EngineMsg::Unsubscribe(a))).is_empty());
    assert_eq!(
        core.bench_ownership_census().attribution_shape_keys,
        2,
        "the outstanding send still owns the departed claim shape"
    );

    let completed = core.white_box("attribution.attribute_eose_detailed", |s| {
        s.attribution
            .attribute_eose_detailed(
                &session,
                &wire_sub_id_string(&request.sub_id),
                Timestamp::from(1_000u64),
            )
            .unwrap()
    });
    let send_id = completed.send_id();
    let mut effects = Vec::new();
    assert!(
        core.white_box("persist_attributed_completion", |s| s
            .persist_attributed_completion(completed, &relay, &mut effects)
            .is_some()),
        "completion must carry both shapes through the atomic store door"
    );
    for claim in &request.coverage_claims {
        assert!(
            core.store.get_coverage(*claim, &RelaySessionKey::unauthenticated(relay.clone())).unwrap().is_some(),
            "both coalesced claims commit even though one active owner departed"
        );
    }
    core.white_box("retire_request_evidence", |s| {
        s.retire_request_evidence(send_id)
    });

    assert_eq!(
        wire_ops(&core.handle(EngineMsg::Unsubscribe(b)))
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
fn closing_an_incumbent_rearms_an_already_pending_limited_atom_without_a_rebuild() {
    let first_relay = RelayUrl::parse("wss://admission-cap-first.example").unwrap();
    let second_relay = RelayUrl::parse("wss://admission-cap-second.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 1);
    let first = observation_id(&core.handle(EngineMsg::Subscribe(query(
        &first_relay,
        "alice",
        Freshness::Live,
    ))));
    assert_eq!(
        wire_ops(&flush(&mut core))
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        1
    );
    core.handle(EngineMsg::Subscribe(query(
        &second_relay,
        "bob",
        Freshness::Live,
    )));
    assert!(wire_ops(&flush(&mut core)).is_empty());
    assert_eq!(core.wire.pending_len(), 1);

    core.pending_atoms_rebuilt.set(0);
    core.evidence_candidates_examined.set(0);
    core.diagnostic_snapshots_built.set(0);
    let released = core.handle(EngineMsg::Unsubscribe(first));

    assert_eq!(
        wire_ops(&released)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert!(released
        .iter()
        .any(|effect| matches!(effect, Effect::ArmWireAdmission)));
    assert_eq!(core.pending_atoms_rebuilt.get(), 0);
    assert_eq!(core.evidence_candidates_examined.get(), 0);
    assert_eq!(core.diagnostic_snapshots_built.get(), 0);
    assert_eq!(core.wire.pending_len(), 1);
    assert_eq!(
        wire_ops(&flush(&mut core))
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        1
    );
    assert_eq!(core.wire.pending_len(), 0);
}

#[test]
fn ten_thousand_distinct_pending_cancellations_never_rebuild_surviving_demand() {
    let relay = RelayUrl::parse("wss://admission-pending-withdraw-10k.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    // This test drives `handle()` directly, once per item, specifically to
    // prove the production entrypoint never revisits O(n) state on a
    // cancellation -- it is an amortized-COST proof, not a mirror-structure
    // one. The turn-level check (#1606) already covers these same
    // Subscribe/Unsubscribe transitions via hundreds of other, bounded
    // tests; scanning full mirror structure at each of the 20,000 direct
    // calls below turns this O(n) claim into O(n^2) test-suite cost with no
    // added coverage (measured: unbounded past 9 minutes with the check on,
    // 4.85s with it off). See
    // `EngineCore::suppress_turn_level_consistency_for_named_exception`'s
    // doc for why only this test and its `scale_teardown` sibling may do
    // this.
    core.suppress_turn_level_consistency_for_named_exception();
    let mut observations = Vec::with_capacity(10_000);
    for index in 0..10_000 {
        observations.push(observation_id(&core.handle(EngineMsg::Subscribe(query(
            &relay,
            &format!("pending-{index:05}"),
            Freshness::Live,
        )))));
    }
    assert_eq!(core.wire.pending_len(), 10_000);

    core.projection_store_queries.set(0);
    core.router_compiles.set(0);
    core.withdrawal_handle_detaches.set(0);
    core.resolver_delta_ops_consumed.set(0);
    core.pending_atoms_rebuilt.set(0);
    core.evidence_candidates_examined.set(0);
    core.diagnostic_snapshots_built.set(0);
    core.white_box("router.reset_withdrawal_work", |s| {
        s.router.reset_withdrawal_work()
    });
    for observation in observations {
        let effects = core.handle(EngineMsg::Unsubscribe(observation));
        assert!(wire_ops(&effects).is_empty());
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitDiagnostics(_) | Effect::DiagnosticsChanged
        )));
    }

    let router = core.router.withdrawal_work();
    assert_eq!(core.withdrawal_handle_detaches.get(), 10_000);
    assert_eq!(core.resolver_delta_ops_consumed.get(), 10_000);
    assert_eq!(router.dropped_atoms, 10_000);
    assert_eq!(router.request_edges_touched, 0);
    assert_eq!(router.requests_closed, 0);
    assert_eq!(router.physical_coverage_edges_released, 0);
    assert_eq!(router.diagnostic_rebuilds, 0);
    assert_eq!(core.projection_store_queries.get(), 0);
    assert_eq!(core.router_compiles.get(), 0);
    assert_eq!(core.pending_atoms_rebuilt.get(), 0);
    assert_eq!(core.evidence_candidates_examined.get(), 0);
    assert_eq!(core.diagnostic_snapshots_built.get(), 0);
    assert_eq!(core.wire.pending_len(), 0);
    // One full structural check on the final state, in place of the 20,000
    // the turn-level call would otherwise have run.
    core.assert_owner_consistency("after ten-thousand cancellations");
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}
