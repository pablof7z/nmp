//! diagnostics scale admission proofs.

use super::*;

#[test]
fn distinct_physical_closes_defer_diagnostic_coverage_projection() {
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let mut observations = Vec::new();
    let relay = RelayUrl::parse("wss://diagnostic-close.example").unwrap();
    for index in 0..8 {
        observations.push(observation_id(&core.handle(EngineMsg::Subscribe(
            bounded_query(&relay, &format!("owner-{index}")),
        ))));
    }
    assert_eq!(
        wire_ops(&flush(&mut core))
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        8
    );

    core.evidence_candidates_examined.set(0);
    core.diagnostic_snapshots_built.set(0);
    core.router.reset_withdrawal_work();
    let mut diagnostic_changes = 0;
    for observation in observations {
        let effects = core.handle(EngineMsg::Unsubscribe(observation));
        assert_eq!(
            wire_ops(&effects)
                .into_iter()
                .filter(|op| matches!(op, WireOp::Close(_)))
                .count(),
            1
        );
        diagnostic_changes += effects
            .iter()
            .filter(|effect| matches!(effect, Effect::DiagnosticsChanged))
            .count();
    }

    assert_eq!(core.evidence_candidates_examined.get(), 0);
    assert_eq!(core.diagnostic_snapshots_built.get(), 0);
    assert_eq!(core.router.withdrawal_work().diagnostic_requests_visited, 0);
    assert_eq!(diagnostic_changes, 8);
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn a_later_admission_cohort_never_visits_ten_thousand_incumbents() {
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let relay = RelayUrl::parse("wss://incremental-admission.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let incumbent_atoms: BTreeSet<_> = (0..10_000)
        .map(|index| bounded_atom(&relay, &format!("incumbent-{index:05}")))
        .collect();
    let budget = core.compile_budget();
    let initial = core
        .router
        .admit(&incumbent_atoms, &core.routing_facts, budget);
    assert_eq!(
        initial
            .wire
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        10_000
    );
    for atom in incumbent_atoms {
        core.attribution.observe_atom(&atom);
        core.wire_owner_counts
            .insert(nmp_router::DemandKey::for_atom(&atom), (atom, 1));
    }
    core.planned_read_sessions.insert(session.clone());
    core.planned_read_session_counts_by_relay
        .insert(relay.clone(), 1);
    let incumbents = core.router.plan().reqs[&session].clone();

    core.pending_atoms_rebuilt.set(0);
    core.pending_cohort_atoms_reconciled.set(0);
    core.attribution_atoms_rebuilt.set(0);
    core.evidence_candidates_examined.set(0);
    core.diagnostic_snapshots_built.set(0);
    core.router.reset_admission_work();
    let later = core.handle(EngineMsg::Subscribe(bounded_query(&relay, "later-owner")));
    assert!(later
        .iter()
        .any(|effect| matches!(effect, Effect::ArmWireAdmission)));
    let admitted = flush(&mut core);
    assert_eq!(
        wire_ops(&admitted)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        1
    );
    assert_eq!(&core.router.plan().reqs[&session][..10_000], incumbents);

    assert_eq!(core.pending_atoms_rebuilt.get(), 0);
    assert_eq!(core.pending_cohort_atoms_reconciled.get(), 1);
    assert_eq!(core.attribution_atoms_rebuilt.get(), 0);
    assert_eq!(core.evidence_candidates_examined.get(), 1);
    assert_eq!(core.diagnostic_snapshots_built.get(), 0);
    let router = core.router.admission_work();
    assert_eq!(router.cohort_compiles, 1);
    assert_eq!(router.incumbent_active_entries_visited, 0);
    assert_eq!(router.incumbent_plan_requests_visited, 0);
    assert_eq!(router.incumbent_limited_entries_visited, 0);
    assert_eq!(router.incumbent_refusal_entries_visited, 0);
    assert_eq!(router.active_entries_appended, 0);
    assert_eq!(router.request_edges_appended, 1);
}
