//! routing evidence admission proofs.

use super::*;

#[test]
fn disjoint_routing_evidence_owners_remain_exact_in_both_close_orders() {
    let relay = RelayUrl::parse("wss://evidence-owner.example").unwrap();
    let evidence_a = RoutingEvidence {
        relay: RelayUrl::parse("wss://evidence-a.example").unwrap(),
        origin: nmp_grammar::RoutingEvidenceKind::Hint,
    };
    let evidence_b = RoutingEvidence {
        relay: RelayUrl::parse("wss://evidence-b.example").unwrap(),
        origin: nmp_grammar::RoutingEvidenceKind::SourceProvenance,
    };

    for (first, survivor) in [
        (evidence_a.clone(), evidence_b.clone()),
        (evidence_b.clone(), evidence_a.clone()),
    ] {
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
        // `retain_wire_atom_owner`/`release_wire_atom_owner` exercise the
        // owner-count/routing-evidence algebra directly, with no handle ever
        // indexed for these atoms -- a state real production cannot reach
        // (`attach_wire_handle` always indexes the handle first). See
        // `EngineCore::suppress_turn_level_consistency_for_named_exception`'s
        // doc.
        core.suppress_turn_level_consistency_for_named_exception();
        let with_evidence = |evidence: RoutingEvidence| {
            let mut atom = bounded_atom(&relay, "shared-selection");
            atom.routing_evidence.insert(evidence);
            atom
        };
        let first_atom = with_evidence(first.clone());
        let survivor_atom = with_evidence(survivor.clone());
        let key = nmp_router::DemandKey::for_atom(&first_atom);

        core.white_box("retain_wire_atom_owner", |s| {
            s.retain_wire_atom_owner(&first_atom)
        });
        let opened = flush(&mut core);
        assert_eq!(
            wire_ops(&opened)
                .into_iter()
                .filter(|op| matches!(op, WireOp::Req(_, _)))
                .count(),
            1
        );
        let immutable_request = core.router.plan().reqs.values().next().unwrap()[0].clone();

        core.white_box("retain_wire_atom_owner", |s| {
            s.retain_wire_atom_owner(&survivor_atom)
        });
        assert_eq!(core.wire.pending_len(), 0);
        assert_eq!(
            core.wire.effective_atom(&key).unwrap().routing_evidence,
            BTreeSet::from([first, survivor.clone()])
        );
        assert_eq!(
            core.router.plan().reqs.values().next().unwrap()[0],
            immutable_request
        );

        assert!(core.white_box("release_wire_atom_owner", |s| s
            .release_wire_atom_owner(&first_atom)
            .is_none()));
        assert_eq!(core.wire.pending_len(), 0);
        assert_eq!(
            core.wire.effective_atom(&key).unwrap().routing_evidence,
            BTreeSet::from([survivor.clone()])
        );
        assert_eq!(
            core.router.plan().reqs.values().next().unwrap()[0],
            immutable_request
        );
        assert_eq!(
            core.wire_demand().iter().next().unwrap().routing_evidence,
            BTreeSet::from([survivor])
        );

        let final_atom = core.white_box("release_wire_atom_owner", |s| {
            s.release_wire_atom_owner(&survivor_atom)
                .expect("the final exact owner retires the shared selection")
        });
        let mut closed = Vec::new();
        core.white_box("withdraw_wire_demand", |s| {
            s.withdraw_wire_demand(vec![final_atom], &mut closed)
        });
        assert_eq!(
            wire_ops(&closed)
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
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
        // Same handle-less fixture as above; see
        // `EngineCore::suppress_turn_level_consistency_for_named_exception`'s
        // doc.
        core.suppress_turn_level_consistency_for_named_exception();
        let mut first = routeless_outbox_atom(author);
        first.routing_evidence.insert(first_evidence.clone());
        let mut second = routeless_outbox_atom(author);
        second.routing_evidence.insert(second_evidence.clone());

        core.white_box("retain_wire_atom_owner", |s| {
            s.retain_wire_atom_owner(&first)
        });
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
        assert_eq!(core.wire.pending_len(), 0);

        core.router_compiles.set(0);
        core.white_box("retain_wire_atom_owner", |s| {
            s.retain_wire_atom_owner(&first)
        });
        assert_eq!(core.wire.pending_len(), 0);
        assert!(flush(&mut core).is_empty());
        assert_eq!(
            core.router_compiles.get(),
            0,
            "duplicate evidence is no cohort"
        );
        assert!(core.white_box("release_wire_atom_owner", |s| s
            .release_wire_atom_owner(&first)
            .is_none()));

        core.white_box("retain_wire_atom_owner", |s| {
            s.retain_wire_atom_owner(&second)
        });
        assert_eq!(core.wire.pending_len(), 1);
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
        assert!(core.white_box("release_wire_atom_owner", |s| s
            .release_wire_atom_owner(departing)
            .is_none()));
        let final_atom = core.white_box("release_wire_atom_owner", |s| {
            s.release_wire_atom_owner(survivor)
                .expect("the final exact owner retires both immutable relay edges")
        });
        let mut closed = Vec::new();
        core.white_box("withdraw_wire_demand", |s| {
            s.withdraw_wire_demand(vec![final_atom], &mut closed)
        });
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
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
        // Same handle-less fixture as above; see
        // `EngineCore::suppress_turn_level_consistency_for_named_exception`'s
        // doc.
        core.suppress_turn_level_consistency_for_named_exception();
        let mut a = routeless_outbox_atom(author);
        a.routing_evidence.insert(evidence_a.clone());
        let mut b = routeless_outbox_atom(author);
        b.routing_evidence.insert(evidence_b.clone());
        core.white_box("retain_wire_atom_owner", |s| s.retain_wire_atom_owner(&a));
        flush(&mut core);
        let incumbent_session = RelaySessionKey::public(evidence_a.relay.clone());
        let incumbent = core.router.plan().reqs[&incumbent_session][0].clone();

        core.white_box("retain_wire_atom_owner", |s| s.retain_wire_atom_owner(&b));
        let departing = if depart_a_before_flush { &a } else { &b };
        let survivor = if depart_a_before_flush { &b } else { &a };
        assert!(core.white_box("release_wire_atom_owner", |s| s
            .release_wire_atom_owner(departing)
            .is_none()));
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

        let final_atom = core.white_box("release_wire_atom_owner", |s| {
            s.release_wire_atom_owner(survivor).unwrap()
        });
        let mut closed = Vec::new();
        core.white_box("withdraw_wire_demand", |s| {
            s.withdraw_wire_demand(vec![final_atom], &mut closed)
        });
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
