//! scale teardown admission proofs.

use super::*;

#[test]
fn probed_nip77_plan_closes_touch_only_their_exact_children() {
    const PLANS: u16 = 64;
    let relay = RelayUrl::parse("wss://nip77-exact-close.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    core.prober
        .states
        .insert(relay.clone(), crate::negentropy::ProbeState::Supported);
    let mut observations = Vec::with_capacity(PLANS as usize);
    for index in 0..PLANS {
        observations.push(observation_id(&core.handle(EngineMsg::Subscribe(
            unbounded_incompatible_query(&relay, index),
        ))));
    }

    let admitted = flush(&mut core);
    assert_eq!(
        wire_ops(&admitted)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        PLANS as usize
    );
    assert_eq!(core.nip77.handoffs.len(), PLANS as usize);
    assert_eq!(core.nip77.handoffs.plan_keys(), PLANS as usize);

    core.nip77_plan_children_touched.set(0);
    core.router.reset_withdrawal_work();
    for observation in observations {
        let effects = core.handle(EngineMsg::Unsubscribe(observation));
        assert!(!wire_ops(&effects).is_empty());
    }

    let work = core.router.withdrawal_work();
    assert_eq!(work.dropped_atoms, PLANS as u64);
    assert_eq!(work.request_edges_touched, PLANS as u64);
    assert_eq!(work.plan_request_entries_visited, PLANS as u64);
    assert_eq!(work.requests_closed, PLANS as u64);
    assert_eq!(core.nip77_plan_children_touched.get(), PLANS as u64);
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn a_large_open_and_close_burst_never_reprojects_sibling_rows() {
    let relay = RelayUrl::parse("wss://admission-scale.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
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
    core.router.reset_withdrawal_work();
    let mut diagnostics = 0;
    for (index, observation) in observations.into_iter().enumerate() {
        diagnostics += core
            .handle(EngineMsg::Unsubscribe(observation))
            .iter()
            .filter(|effect| matches!(effect, Effect::DiagnosticsChanged))
            .count();
        if index == 205 {
            let census = core.bench_ownership_census();
            assert_eq!(census.plan_execution_claims, 1);
            assert_eq!(census.plan_execution_owner_demands, 1);
            assert_eq!(census.attribution_live_shape_keys, 207);
            assert_eq!(census.attribution_live_shape_refs, 207);
            assert_eq!(census.attribution_inflight_shape_keys, 207);
            assert_eq!(census.attribution_inflight_shape_refs, 207);
            assert_eq!(census.router_request_owner_contribution_keys, 1);
            assert_eq!(census.router_request_claim_owner_count_keys, 1);
            assert_eq!(census.router_request_provenance_owner_count_keys, 1);
            assert_eq!(census.router_request_demand_coverage_owner_count_keys, 1);
            assert_eq!(census.router_physical_request_claim_keys, 1);
            assert_eq!(census.router_physical_claim_keys, 207);
            assert_eq!(census.router_physical_claim_edges, 207);
            assert_eq!(census.router_physical_request_contribution_keys, 207);
            assert_eq!(census.router_physical_demand_keys, 207);
            assert_eq!(census.router_physical_demand_edges, 207);
        }
    }
    assert_eq!(core.projection_store_queries.get(), 0);
    let work = core.router.withdrawal_work();
    assert_eq!(work.dropped_atoms, 207);
    assert_eq!(work.request_edges_touched, 207);
    assert_eq!(work.metadata_owner_entries_touched, 206);
    assert_eq!(work.metadata_claim_entries_touched, 206);
    assert_eq!(work.metadata_assignment_entries_touched, 0);
    assert_eq!(work.metadata_provenance_entries_touched, 0);
    assert_eq!(work.requests_closed, 1);
    assert_eq!(work.physical_coverage_edges_released, 207);
    assert_eq!(work.diagnostic_rebuilds, 1);
    assert_eq!(
        diagnostics, 1,
        "only the final owner changes the immutable relay plan"
    );
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn ten_thousand_shared_bounded_owners_withdraw_in_owner_plus_one_close_work() {
    let relay = RelayUrl::parse("wss://admission-shared-10k.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let mut observations = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        observations.push(observation_id(
            &core.handle(EngineMsg::Subscribe(bounded_query(&relay, "same-owner"))),
        ));
    }
    assert_eq!(
        observations.iter().copied().collect::<BTreeSet<_>>().len(),
        10_000
    );
    let admitted = flush(&mut core);
    assert_eq!(
        wire_ops(&admitted)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        1
    );

    core.projection_store_queries.set(0);
    core.router_compiles.set(0);
    core.withdrawal_handle_detaches.set(0);
    core.resolver_delta_ops_consumed.set(0);
    core.pending_atoms_rebuilt.set(0);
    core.router.reset_withdrawal_work();
    for observation in observations.iter().take(9_999) {
        let effects = core.handle(EngineMsg::Unsubscribe(*observation));
        assert!(wire_ops(&effects).is_empty());
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitDiagnostics(_) | Effect::DiagnosticsChanged
        )));
    }

    let non_final = core.router.withdrawal_work();
    assert_eq!(core.withdrawal_handle_detaches.get(), 9_999);
    assert_eq!(core.resolver_delta_ops_consumed.get(), 0);
    assert_eq!(core.pending_atoms_rebuilt.get(), 0);
    assert_eq!(non_final.dropped_atoms, 0);
    assert_eq!(non_final.request_edges_touched, 0);
    assert_eq!(non_final.requests_closed, 0);
    assert_eq!(non_final.diagnostic_rebuilds, 0);
    assert_eq!(core.projection_store_queries.get(), 0);
    assert_eq!(core.router_compiles.get(), 0);

    let final_effects = core.handle(EngineMsg::Unsubscribe(observations[9_999]));
    assert_eq!(
        wire_ops(&final_effects)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert_eq!(
        final_effects
            .iter()
            .filter(|effect| matches!(effect, Effect::DiagnosticsChanged))
            .count(),
        1
    );
    let final_work = core.router.withdrawal_work();
    assert_eq!(core.withdrawal_handle_detaches.get(), 10_000);
    assert_eq!(core.resolver_delta_ops_consumed.get(), 1);
    assert_eq!(core.pending_atoms_rebuilt.get(), 0);
    assert_eq!(final_work.dropped_atoms, 1);
    assert_eq!(final_work.request_edges_touched, 1);
    assert_eq!(final_work.requests_closed, 1);
    assert_eq!(final_work.physical_coverage_edges_released, 1);
    assert_eq!(final_work.diagnostic_rebuilds, 1);
    assert_eq!(core.projection_store_queries.get(), 0);
    assert_eq!(core.router_compiles.get(), 0);
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn withdrawing_the_final_routeless_observation_emits_its_diagnostic_retraction() {
    let author = Keys::generate().public_key();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let opened = core.handle(EngineMsg::Subscribe(routeless_outbox_query(author)));
    let observation = observation_id(&opened);
    let admitted = flush(&mut core);
    assert!(wire_ops(&admitted).is_empty());
    assert_eq!(core.diagnostics_snapshot().uncovered_author_count, 1);

    let withdrawn = core.handle(EngineMsg::Unsubscribe(observation));

    assert!(wire_ops(&withdrawn).is_empty());
    assert!(withdrawn
        .iter()
        .any(|effect| matches!(effect, Effect::DiagnosticsChanged)));
    assert_eq!(core.diagnostics_snapshot().uncovered_author_count, 0);
}

#[test]
fn later_exact_owner_routing_evidence_retracts_the_uncovered_diagnostic_on_admission() {
    let author = Keys::generate().public_key();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let routeless = routeless_outbox_atom(author);
    assert!(!core.retain_wire_atom_owner(&routeless));
    let first = flush(&mut core);
    assert!(wire_ops(&first).is_empty());
    assert_eq!(core.diagnostics_snapshot().uncovered_author_count, 1);
    assert_eq!(core.router.ownership_census().uncovered_demand_keys, 1);

    let mut routed = routeless.clone();
    routed.routing_evidence.extend([
        RoutingEvidence {
            relay: RelayUrl::parse("wss://core-projected-route-a.example").unwrap(),
            origin: nmp_grammar::RoutingEvidenceKind::Hint,
        },
        RoutingEvidence {
            relay: RelayUrl::parse("wss://core-projected-route-b.example").unwrap(),
            origin: nmp_grammar::RoutingEvidenceKind::Hint,
        },
    ]);
    assert!(!core.retain_wire_atom_owner(&routed));
    let admitted = flush(&mut core);
    assert_eq!(
        wire_ops(&admitted)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        2
    );
    assert!(admitted
        .iter()
        .any(|effect| matches!(effect, Effect::DiagnosticsChanged)));
    assert!(
        !core
            .router
            .diagnostics()
            .uncovered_authors
            .contains_key(&author),
        "router diagnostic ownership must retract before core projects it: {:?}",
        core.router.ownership_census()
    );
    assert_eq!(core.diagnostics_snapshot().uncovered_author_count, 0);

    assert!(core.release_wire_atom_owner(&routed).is_none());
    let final_atom = core
        .release_wire_atom_owner(&routeless)
        .expect("the final exact owner retires physical work");
    let mut closed = Vec::new();
    core.withdraw_wire_demand(vec![final_atom], &mut closed);
    assert_eq!(
        wire_ops(&closed)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        2
    );
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn history_open_waits_for_the_same_flush_without_refreshing_an_ordinary_sibling() {
    let relay = RelayUrl::parse("wss://admission-history.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
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
