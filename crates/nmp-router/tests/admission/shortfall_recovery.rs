//! shortfall recovery admission integration proofs.

use super::*;

#[test]
fn second_projected_hint_adds_only_the_missing_session_and_heals_shortfall() {
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let author = Keys::generate().public_key();
    let first_relay = RelayUrl::parse("wss://router-first-projected.example").unwrap();
    let second_relay = RelayUrl::parse("wss://router-second-projected.example").unwrap();
    let mut effective = routeless_outbox_atom(author);
    effective.routing_evidence.insert(RoutingEvidence {
        relay: first_relay.clone(),
        origin: RoutingEvidenceKind::Hint,
    });

    assert_eq!(
        reqs(&router.admit(&BTreeSet::from([effective.clone()]), &facts, 20)),
        1
    );
    let first_session = RelaySessionKey::public(first_relay);
    let incumbent = router.plan().reqs[&first_session][0].clone();
    assert_eq!(
        router.diagnostics().uncovered_authors[&author].reason,
        ShortfallReason::FewerCandidatesThanK
    );

    effective.routing_evidence.insert(RoutingEvidence {
        relay: second_relay.clone(),
        origin: RoutingEvidenceKind::Hint,
    });
    router.activate(effective.clone());
    let healed = router.admit(&BTreeSet::from([effective.clone()]), &facts, 20);
    assert_eq!(reqs(&healed), 1);
    assert_eq!(router.plan().reqs[&first_session], vec![incumbent]);
    assert_eq!(
        router.plan().reqs[&RelaySessionKey::public(second_relay)].len(),
        1
    );
    assert!(!router.diagnostics().uncovered_authors.contains_key(&author));

    let closed = router.withdraw([effective], 20);
    assert_eq!(
        closed
            .wire
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        2
    );
    assert_eq!(router.ownership_census(), Default::default());
}

#[test]
fn later_routing_evidence_updates_only_its_exact_uncovered_demand_owner() {
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let author = Keys::generate().public_key();
    let other_kind = {
        let mut atom = routeless_outbox_atom(author);
        atom.filter.kinds = Some(BTreeSet::from([2u16]));
        atom
    };
    let mut newly_routed = routeless_outbox_atom(author);
    let relay = RelayUrl::parse("wss://router-projected-route.example").unwrap();

    router.admit(
        &BTreeSet::from([newly_routed.clone(), other_kind.clone()]),
        &facts,
        20,
    );
    assert_eq!(router.ownership_census().uncovered_demand_keys, 2);
    assert_eq!(router.ownership_census().uncovered_author_keys, 1);
    assert_eq!(router.ownership_census().uncovered_author_refs, 2);
    assert!(router.diagnostics().uncovered_authors.contains_key(&author));

    newly_routed.routing_evidence.insert(RoutingEvidence {
        relay,
        origin: RoutingEvidenceKind::Hint,
    });
    router.activate(newly_routed.clone());
    let admitted = router.admit(&BTreeSet::from([newly_routed.clone()]), &facts, 20);
    assert_eq!(reqs(&admitted), 1);
    assert!(admitted.diagnostics_changed);
    assert_eq!(router.ownership_census().uncovered_demand_keys, 2);
    assert_eq!(router.ownership_census().uncovered_author_refs, 2);
    assert!(
        router.diagnostics().uncovered_authors.contains_key(&author),
        "the author's independent routeless DemandKey still owns the diagnostic"
    );

    let first = router.withdraw([other_kind], 20);
    assert!(first.diagnostics_changed);
    assert_eq!(
        router.diagnostics().uncovered_authors[&author],
        Shortfall {
            requested_k: 2,
            achieved: 1,
            reason: ShortfallReason::FewerCandidatesThanK,
        }
    );
    assert_eq!(router.ownership_census().uncovered_demand_keys, 1);
    let final_close = router.withdraw([newly_routed], 20);
    assert_eq!(
        final_close
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
fn routed_and_routeless_same_author_remain_exact_in_the_other_withdrawal_order() {
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let author = Keys::generate().public_key();
    let mut routed = routeless_outbox_atom(author);
    routed.routing_evidence.insert(RoutingEvidence {
        relay: RelayUrl::parse("wss://router-projected-route-order.example").unwrap(),
        origin: RoutingEvidenceKind::Hint,
    });
    let mut routeless = routeless_outbox_atom(author);
    routeless.filter.kinds = Some(BTreeSet::from([2u16]));

    router.admit(
        &BTreeSet::from([routed.clone(), routeless.clone()]),
        &facts,
        20,
    );
    assert!(router.diagnostics().uncovered_authors.contains_key(&author));
    let routed_first = router.withdraw([routed], 20);
    assert_eq!(
        routed_first
            .wire
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert!(
        router.diagnostics().uncovered_authors.contains_key(&author),
        "withdrawing a served sibling cannot retract the routeless owner's diagnostic"
    );
    let routeless_last = router.withdraw([routeless], 20);
    assert!(routeless_last.diagnostics_changed);
    assert!(!router.diagnostics().uncovered_authors.contains_key(&author));
    assert_eq!(router.ownership_census(), Default::default());
}

#[test]
fn a_refused_pending_atom_is_admitted_after_an_incumbent_releases_the_relay_cap() {
    let first_relay = RelayUrl::parse("wss://router-cap-first.example").unwrap();
    let second_relay = RelayUrl::parse("wss://router-cap-second.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let first = atom(&first_relay, "alice");
    let second = atom(&second_relay, "bob");

    assert_eq!(
        reqs(&router.admit(&BTreeSet::from([first.clone()]), &facts, 1)),
        1
    );
    assert_eq!(
        reqs(&router.admit(&BTreeSet::from([second.clone()]), &facts, 1)),
        0
    );
    assert_eq!(router.plan().limited_demands.len(), 1);

    let close = withdraw(&mut router, [first], 1);
    assert_eq!(
        close
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    let admitted = router.admit(&BTreeSet::from([second]), &facts, 1);
    assert_eq!(reqs(&admitted), 1);
    assert_eq!(admitted.changed_coverage.len(), 1);
    assert!(router.plan().limited_demands.is_empty());
    assert!(router.plan().refused_sessions.is_empty());
}
