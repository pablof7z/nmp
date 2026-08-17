//! diagnostics scale admission proofs.

use super::*;
use nmp_store::testing;

/// #763 falsifier: diagnostics keeps an unreadable coverage entry coupled to
/// the exact store-degradation fact instead of rendering a healthy `None`.
#[test]
fn a_diagnostics_snapshot_built_over_corrupt_coverage_says_so() {
    let relay = RelayUrl::parse("wss://diagnostic-coverage.example").unwrap();
    let atom = bounded_atom(&relay, "corrupt-owner");
    let key = coverage_key(&atom);
    let directory = tempfile::tempdir().expect("diagnostic coverage corruption directory");
    let path = directory.path().join("diagnostic-coverage.redb");
    {
        let mut store = RedbStore::open(&path).expect("create persistent Redb fixture");
        store
            .record_coverage(&[(
                atom.clone(),
                relay.clone(),
                CoverageInterval::new(Timestamp::from(10u64), Timestamp::from(20u64)),
            )])
            .expect("seed exact coverage row");
    }
    testing::corrupt_coverage(&path, key, &relay).expect("corrupt exact coverage row");

    let store = RedbStore::open(&path).expect("reopen diagnostic coverage fixture");
    let mut core = EngineCore::new(store, 20);
    let budget = core.compile_budget();
    let admitted = core.white_box("router.admit", |s| {
        s.router
            .admit(&BTreeSet::from([atom]), &s.routing_facts, budget)
    });
    assert_eq!(
        admitted
            .wire
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        1,
        "the private plan installation must create exactly one coverage entry"
    );
    core.store.reset_coverage_reads();

    let snapshot = core.diagnostics_snapshot();

    assert_eq!(
        core.store.coverage_reads(),
        1,
        "diagnostics_snapshot must own the first and only coverage dereference"
    );
    assert!(
        snapshot
            .store_degraded
            .as_deref()
            .is_some_and(|message| message.contains("decode coverage row")),
        "the exact decode failure must accompany the unreadable entry: {snapshot:?}"
    );
    let relay_snapshot = snapshot
        .relays
        .iter()
        .find(|candidate| candidate.relay == relay)
        .expect("the installed plan projects its relay");
    assert_eq!(relay_snapshot.coverage.len(), 1);
    assert_eq!(relay_snapshot.coverage[0].coverage, None);
}

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
    core.white_box("router.reset_withdrawal_work", |s| {
        s.router.reset_withdrawal_work()
    });
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
    // `core.wire.retain(&atom)` below exercises owner-count bookkeeping
    // directly, with no handle ever indexed for these 10,000 atoms -- a
    // state real production cannot reach (`attach_wire_handle` always
    // indexes the handle first). See
    // `EngineCore::suppress_turn_level_consistency_for_named_exception`'s
    // doc.
    core.suppress_turn_level_consistency_for_named_exception();
    let relay = RelayUrl::parse("wss://incremental-admission.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let incumbent_atoms: BTreeSet<_> = (0..10_000)
        .map(|index| bounded_atom(&relay, &format!("incumbent-{index:05}")))
        .collect();
    let budget = core.compile_budget();
    let initial = core.white_box("router.admit", |s| {
        s.router.admit(&incumbent_atoms, &s.routing_facts, budget)
    });
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
        core.white_box("attribution.observe_atom", |s| {
            s.attribution.observe_atom(&atom)
        });
        core.white_box("wire.retain", |s| s.wire.retain(&atom));
    }
    core.white_box("planned_read_sessions.insert", |s| {
        s.planned_read_sessions.insert(session.clone())
    });
    core.white_box("planned_read_session_counts_by_relay.insert", |s| {
        s.planned_read_session_counts_by_relay
            .insert(relay.clone(), 1)
    });
    let incumbents = core.router.plan().reqs[&session].clone();

    core.pending_atoms_rebuilt.set(0);
    core.pending_cohort_atoms_reconciled.set(0);
    core.attribution_atoms_rebuilt.set(0);
    core.evidence_candidates_examined.set(0);
    core.diagnostic_snapshots_built.set(0);
    core.white_box("router.reset_admission_work", |s| {
        s.router.reset_admission_work()
    });
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
