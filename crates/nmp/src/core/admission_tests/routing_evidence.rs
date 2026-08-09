//! routing evidence admission proofs.

use super::*;

#[test]
fn disjoint_routing_evidence_owners_remain_exact_in_both_close_orders() {
    let relay = RelayUrl::parse("wss://evidence-owner.example").unwrap();
    let evidence_a = RoutingEvidence {
        relay: RelayUrl::parse("ws://127.0.0.1:7701").unwrap(),
        origin: nmp_grammar::RoutingEvidenceKind::Hint,
    };
    let evidence_b = RoutingEvidence {
        relay: RelayUrl::parse("ws://127.0.0.1:7702").unwrap(),
        origin: nmp_grammar::RoutingEvidenceKind::SourceProvenance,
    };

    for (first, survivor) in [
        (evidence_a.clone(), evidence_b.clone()),
        (evidence_b.clone(), evidence_a.clone()),
    ] {
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let with_evidence = |evidence: RoutingEvidence| {
            let mut atom = bounded_atom(&relay, "shared-selection");
            atom.routing_evidence.insert(evidence);
            atom
        };
        let first_atom = with_evidence(first.clone());
        let survivor_atom = with_evidence(survivor.clone());
        let key = nmp_router::DemandKey::for_atom(&first_atom);

        core.retain_wire_atom_owner(&first_atom);
        let admitted = flush(&mut core);
        assert_eq!(
            wire_ops(&admitted)
                .into_iter()
                .filter(|op| matches!(op, WireOp::Req(_, _)))
                .count(),
            1
        );
        let immutable_request = core.router.plan().reqs.values().next().unwrap()[0].clone();

        core.retain_wire_atom_owner(&survivor_atom);
        assert!(core.pending_wire_atoms.is_empty());
        assert_eq!(
            core.wire_owner_counts[&key].0.routing_evidence,
            BTreeSet::from([first.clone(), survivor.clone()])
        );
        assert_eq!(core.rejected_projected_evidence_by_demand[&key].len(), 2);
        assert_eq!(
            core.router.plan().reqs.values().next().unwrap()[0],
            immutable_request
        );

        core.routing_evidence_owner_keys_touched.set(0);
        assert!(core.release_wire_atom_owner(&first_atom).is_none());
        assert_eq!(core.routing_evidence_owner_keys_touched.get(), 1);
        assert!(core.pending_wire_atoms.is_empty());
        assert_eq!(
            core.wire_owner_counts[&key].0.routing_evidence,
            BTreeSet::from([survivor.clone()])
        );
        assert_eq!(
            core.rejected_projected_evidence_by_demand[&key]
                .iter()
                .map(|(_, evidence)| evidence)
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([survivor.clone()])
        );
        assert_eq!(
            core.router.plan().reqs.values().next().unwrap()[0],
            immutable_request
        );
        let full_live_union = core.wire_demand();
        assert_eq!(
            full_live_union.iter().next().unwrap().routing_evidence,
            BTreeSet::from([survivor])
        );

        core.routing_evidence_owner_keys_touched.set(0);
        let final_atom = core
            .release_wire_atom_owner(&survivor_atom)
            .expect("the last exact owner retires the logical demand");
        assert_eq!(core.routing_evidence_owner_keys_touched.get(), 1);
        let mut effects = Vec::new();
        core.withdraw_wire_demand(vec![final_atom], &mut effects);
        assert_eq!(
            wire_ops(&effects)
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
fn second_outbox_hint_opens_only_the_missing_relay_for_both_owner_close_orders() {
    let author = Keys::generate().public_key();
    let first_evidence = RoutingEvidence {
        relay: RelayUrl::parse("wss://core-first-partial.example").unwrap(),
        origin: nmp_grammar::RoutingEvidenceKind::Hint,
    };
    let second_evidence = RoutingEvidence {
        relay: RelayUrl::parse("wss://core-second-partial.example").unwrap(),
        origin: nmp_grammar::RoutingEvidenceKind::Hint,
    };

    for close_first_owner in [true, false] {
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let mut first = routeless_outbox_atom(author);
        first.routing_evidence.insert(first_evidence.clone());
        let mut second = routeless_outbox_atom(author);
        second.routing_evidence.insert(second_evidence.clone());

        core.retain_wire_atom_owner(&first);
        let initially_admitted = flush(&mut core);
        assert_eq!(
            wire_ops(&initially_admitted)
                .into_iter()
                .filter(|op| matches!(op, WireOp::Req(_, _)))
                .count(),
            1
        );
        assert_eq!(
            core.router.diagnostics().uncovered_authors[&author].reason,
            nmp_router::ShortfallReason::FewerCandidatesThanK
        );
        let first_session = RelaySessionKey::public(first_evidence.relay.clone());
        let incumbent = core.router.plan().reqs[&first_session][0].clone();
        assert!(core.pending_wire_atoms.is_empty());

        core.router_compiles.set(0);
        core.retain_wire_atom_owner(&first);
        assert!(core.pending_wire_atoms.is_empty());
        assert!(flush(&mut core).is_empty());
        assert_eq!(
            core.router_compiles.get(),
            0,
            "duplicate evidence is no cohort"
        );
        assert!(core.release_wire_atom_owner(&first).is_none());

        core.retain_wire_atom_owner(&second);
        assert_eq!(core.pending_wire_atoms.len(), 1);
        let healed = flush(&mut core);
        assert_eq!(
            wire_ops(&healed)
                .into_iter()
                .filter(|op| matches!(op, WireOp::Req(_, _)))
                .count(),
            1
        );
        assert!(wire_ops(&healed)
            .into_iter()
            .all(|op| !matches!(op, WireOp::Close(_))));
        assert_eq!(core.router.plan().reqs[&first_session], vec![incumbent]);
        assert!(!core
            .router
            .diagnostics()
            .uncovered_authors
            .contains_key(&author));

        let (departing, survivor) = if close_first_owner {
            (&first, &second)
        } else {
            (&second, &first)
        };
        assert!(core.release_wire_atom_owner(departing).is_none());
        let final_atom = core
            .release_wire_atom_owner(survivor)
            .expect("the final exact owner retires both immutable relay edges");
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
}

#[test]
fn preflush_hint_owner_churn_combines_pending_and_incumbent_assignment_truth() {
    let author = Keys::generate().public_key();
    let evidence_a = RoutingEvidence {
        relay: RelayUrl::parse("wss://core-interleave-a.example").unwrap(),
        origin: nmp_grammar::RoutingEvidenceKind::Hint,
    };
    let evidence_b = RoutingEvidence {
        relay: RelayUrl::parse("wss://core-interleave-b.example").unwrap(),
        origin: nmp_grammar::RoutingEvidenceKind::Hint,
    };

    for depart_a_before_flush in [true, false] {
        let mut core = EngineCore::new(MemoryStore::new(), 20);
        let mut a = routeless_outbox_atom(author);
        a.routing_evidence.insert(evidence_a.clone());
        let mut b = routeless_outbox_atom(author);
        b.routing_evidence.insert(evidence_b.clone());
        core.retain_wire_atom_owner(&a);
        flush(&mut core);
        let incumbent_session = RelaySessionKey::public(evidence_a.relay.clone());
        let incumbent = core.router.plan().reqs[&incumbent_session][0].clone();

        core.retain_wire_atom_owner(&b);
        let departing = if depart_a_before_flush { &a } else { &b };
        let survivor = if depart_a_before_flush { &b } else { &a };
        assert!(core.release_wire_atom_owner(departing).is_none());
        let admitted = flush(&mut core);
        assert_eq!(core.router.plan().reqs[&incumbent_session], vec![incumbent]);

        if depart_a_before_flush {
            assert_eq!(
                wire_ops(&admitted)
                    .into_iter()
                    .filter(|op| matches!(op, WireOp::Req(_, _)))
                    .count(),
                1,
                "the B-only pending union adds only its missing relay"
            );
            assert!(!core
                .router
                .diagnostics()
                .uncovered_authors
                .contains_key(&author));
        } else {
            assert!(wire_ops(&admitted).is_empty());
            assert_eq!(
                core.router.diagnostics().uncovered_authors[&author].reason,
                nmp_router::ShortfallReason::FewerCandidatesThanK
            );
        }

        let final_atom = core.release_wire_atom_owner(survivor).unwrap();
        let mut closed = Vec::new();
        core.withdraw_wire_demand(vec![final_atom], &mut closed);
        assert_eq!(
            wire_ops(&closed)
                .into_iter()
                .filter(|op| matches!(op, WireOp::Close(_)))
                .count(),
            if depart_a_before_flush { 2 } else { 1 }
        );
        assert_eq!(
            core.bench_ownership_census(),
            CoreOwnershipCensus::default()
        );
    }
}

#[test]
fn a_covered_owner_can_add_the_first_rejected_fact_without_rewriting_wire() {
    let relay = RelayUrl::parse("wss://evidence-owner-covered.example").unwrap();
    let mut core = EngineCore::new(MemoryStore::new(), 20);
    let mut accepted = bounded_atom(&relay, "shared-selection");
    accepted.routing_evidence.insert(RoutingEvidence {
        relay: RelayUrl::parse("wss://public-evidence.example").unwrap(),
        origin: nmp_grammar::RoutingEvidenceKind::Hint,
    });
    let mut rejected = bounded_atom(&relay, "shared-selection");
    let rejected_fact = RoutingEvidence {
        relay: RelayUrl::parse("ws://127.0.0.1:7799").unwrap(),
        origin: nmp_grammar::RoutingEvidenceKind::Hint,
    };
    rejected.routing_evidence.insert(rejected_fact.clone());
    let key = nmp_router::DemandKey::for_atom(&accepted);

    assert!(!core.retain_wire_atom_owner(&accepted));
    let admitted = flush(&mut core);
    assert_eq!(
        wire_ops(&admitted)
            .into_iter()
            .filter(|op| matches!(op, WireOp::Req(_, _)))
            .count(),
        1
    );
    assert!(!core
        .rejected_projected_evidence_by_demand
        .contains_key(&key));
    let immutable_request = core.router.plan().reqs.values().next().unwrap()[0].clone();

    assert!(core.retain_wire_atom_owner(&rejected));
    assert!(core.pending_wire_atoms.is_empty());
    assert_eq!(
        core.rejected_projected_evidence_by_demand[&key]
            .iter()
            .map(|(_, evidence)| evidence)
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([rejected_fact])
    );
    assert_eq!(
        core.router.plan().reqs.values().next().unwrap()[0],
        immutable_request
    );

    assert!(core.release_wire_atom_owner(&rejected).is_none());
    assert!(!core
        .rejected_projected_evidence_by_demand
        .contains_key(&key));
    let final_atom = core.release_wire_atom_owner(&accepted).unwrap();
    let mut effects = Vec::new();
    core.withdraw_wire_demand(vec![final_atom], &mut effects);
    assert_eq!(
        wire_ops(&effects)
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
