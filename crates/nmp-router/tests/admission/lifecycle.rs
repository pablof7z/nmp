//! lifecycle admission integration proofs.

use super::*;

#[test]
fn a_live_request_does_not_absorb_a_later_windowed_backfill() {
    let relay = RelayUrl::parse("wss://router-window.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let live = atom(&relay, "alice");
    let mut older = live.clone();
    older.filter.until = Some(99);
    older.filter.limit = Some(5);

    assert_eq!(
        reqs(&router.admit(&BTreeSet::from([live.clone()]), &facts, 20)),
        1
    );
    let backfill = router.admit(&BTreeSet::from([older.clone()]), &facts, 20);
    assert_eq!(reqs(&backfill), 1);
    assert!(backfill
        .wire
        .ops
        .iter()
        .flat_map(|(_, ops)| ops)
        .all(|op| !matches!(op, WireOp::Close(_))));
    assert_eq!(router.plan().reqs.values().flatten().count(), 2);

    let retired = withdraw(&mut router, [older], 20);
    assert_eq!(
        retired
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert_eq!(router.plan().reqs.values().flatten().count(), 1);
}

#[test]
fn withdrawal_keeps_a_shared_immutable_req_until_its_last_key_leaves() {
    let relay = RelayUrl::parse("wss://router-withdraw.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let alice = atom(&relay, "alice");
    let bob = atom(&relay, "bob");
    router.admit(&BTreeSet::from([alice.clone(), bob.clone()]), &facts, 20);

    router.reset_withdrawal_work();
    let keep_bob = withdraw(&mut router, [alice.clone()], 20);
    assert!(keep_bob.ops.is_empty());
    assert_eq!(router.plan().reqs.values().flatten().count(), 1);
    assert_eq!(router.withdrawal_work().dropped_atoms, 1);
    assert_eq!(router.withdrawal_work().request_edges_touched, 1);
    assert_eq!(router.withdrawal_work().requests_closed, 0);
    assert_eq!(router.withdrawal_work().diagnostic_rebuilds, 0);

    let reattached = router.admit(&BTreeSet::from([alice.clone()]), &facts, 20);
    assert!(
        reattached.wire.ops.is_empty(),
        "immutable physical coverage must be reusable without REQ"
    );

    let _ = withdraw(&mut router, [alice], 20);
    let close = withdraw(&mut router, [bob], 20);
    assert_eq!(
        close
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert!(router.plan().reqs.is_empty());
}

#[test]
fn withdrawing_the_final_routeless_outbox_owner_retracts_its_diagnostic() {
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let author = Keys::generate().public_key();
    let demand = routeless_outbox_atom(author);

    let admitted = router.admit(&BTreeSet::from([demand.clone()]), &facts, 20);
    assert!(admitted.wire.ops.is_empty());
    assert!(admitted.changed_coverage.is_empty());
    assert!(admitted.diagnostics_changed);
    assert!(router.diagnostics().uncovered_authors.contains_key(&author));

    router.reset_withdrawal_work();
    let withdrawn = router.withdraw([demand], 20);

    assert!(withdrawn.wire.ops.is_empty());
    assert!(withdrawn.changed_coverage.is_empty());
    assert!(withdrawn.diagnostics_changed);
    assert!(!router.diagnostics().uncovered_authors.contains_key(&author));
    assert_eq!(router.withdrawal_work().request_edges_touched, 0);
    assert_eq!(router.withdrawal_work().requests_closed, 0);
    assert_eq!(router.withdrawal_work().diagnostic_rebuilds, 1);
}

#[test]
fn partially_served_outbox_demand_owns_its_exact_shortfall_with_a_live_request() {
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let author = Keys::generate().public_key();
    let relay = RelayUrl::parse("wss://router-partial-shortfall.example").unwrap();
    let mut demand = routeless_outbox_atom(author);
    demand.routing_evidence.insert(RoutingEvidence {
        relay,
        origin: RoutingEvidenceKind::Hint,
    });

    let admitted = router.admit(&BTreeSet::from([demand.clone()]), &facts, 20);
    assert_eq!(reqs(&admitted), 1);
    assert_eq!(
        router.diagnostics().uncovered_authors[&author],
        Shortfall {
            requested_k: 2,
            achieved: 1,
            reason: ShortfallReason::FewerCandidatesThanK,
        }
    );
    assert_eq!(router.ownership_census().uncovered_demand_keys, 1);
    assert_eq!(router.ownership_census().uncovered_author_refs, 1);

    let withdrawn = router.withdraw([demand], 20);
    assert!(withdrawn.diagnostics_changed);
    assert_eq!(
        withdrawn
            .wire
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert_eq!(router.ownership_census(), Default::default());
}

#[test]
fn same_author_distinct_shortfalls_reveal_the_exact_survivor_in_both_orders() {
    let facts = FixtureRoutingFacts::new();
    let author = Keys::generate().public_key();
    let no_candidates = routeless_outbox_atom(author);
    let mut fewer_than_k = routeless_outbox_atom(author);
    fewer_than_k.filter.kinds = Some(BTreeSet::from([2u16]));
    fewer_than_k.routing_evidence.insert(RoutingEvidence {
        relay: RelayUrl::parse("wss://router-distinct-shortfall.example").unwrap(),
        origin: RoutingEvidenceKind::Hint,
    });

    for (departing, survivor) in [
        (no_candidates.clone(), fewer_than_k.clone()),
        (fewer_than_k.clone(), no_candidates.clone()),
    ] {
        let mut router = Router::new(RuleRegistry::default_widen_only());
        router.admit(
            &BTreeSet::from([departing.clone(), survivor.clone()]),
            &facts,
            20,
        );
        assert_eq!(router.ownership_census().uncovered_demand_keys, 2);
        assert_eq!(router.ownership_census().uncovered_author_refs, 2);

        let mut fresh = Router::new(RuleRegistry::default_widen_only());
        fresh.admit(&BTreeSet::from([survivor.clone()]), &facts, 20);
        let expected = fresh.diagnostics().uncovered_authors[&author];
        let before = router.diagnostics().uncovered_authors[&author];

        let withdrawn = router.withdraw([departing], 20);
        if before != expected {
            assert!(withdrawn.diagnostics_changed);
        }
        assert_eq!(router.diagnostics().uncovered_authors[&author], expected);
        assert_eq!(router.ownership_census().uncovered_demand_keys, 1);
        assert_eq!(router.ownership_census().uncovered_author_refs, 1);

        router.withdraw([survivor], 20);
        assert_eq!(router.ownership_census(), Default::default());
    }
}

#[test]
fn simultaneous_shortfall_reduction_is_semantic_not_demand_key_order() {
    let facts = FixtureRoutingFacts::new();
    let author = Keys::generate().public_key();
    let relay = RelayUrl::parse("wss://router-shortfall-order.example").unwrap();

    for (no_candidate_kind, fewer_kind) in [(1u16, 2u16), (2u16, 1u16)] {
        let mut no_candidates = routeless_outbox_atom(author);
        no_candidates.filter.kinds = Some(BTreeSet::from([no_candidate_kind]));
        let mut fewer = routeless_outbox_atom(author);
        fewer.filter.kinds = Some(BTreeSet::from([fewer_kind]));
        fewer.routing_evidence.insert(RoutingEvidence {
            relay: relay.clone(),
            origin: RoutingEvidenceKind::Hint,
        });
        let mut router = Router::new(RuleRegistry::default_widen_only());

        router.admit(&BTreeSet::from([no_candidates, fewer]), &facts, 20);

        assert_eq!(
            router.diagnostics().uncovered_authors[&author],
            Shortfall {
                requested_k: 2,
                achieved: 0,
                reason: ShortfallReason::NoCandidates,
            },
            "the greatest live deficit wins regardless of DemandKey ordering"
        );
    }
}
