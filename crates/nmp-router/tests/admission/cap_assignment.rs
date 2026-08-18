//! cap assignment admission integration proofs.

use nmp_grammar::RelaySessionKey;
use super::*;

#[test]
fn supplemental_lanes_never_count_as_author_coverage_in_full_or_incremental_plans() {
    let author = Keys::generate().public_key();
    let demand = routeless_outbox_atom(author);
    let expected = Shortfall {
        requested_k: 2,
        achieved: 0,
        reason: ShortfallReason::NoCandidates,
    };
    let fixtures = [
        FixtureRoutingFacts::new()
            .with_operator_app([RelayUrl::parse("wss://router-app-one.example").unwrap()]),
        FixtureRoutingFacts::new().with_operator_app([
            RelayUrl::parse("wss://router-app-two-a.example").unwrap(),
            RelayUrl::parse("wss://router-app-two-b.example").unwrap(),
        ]),
        FixtureRoutingFacts::new()
            .with_operator_fallback([RelayUrl::parse("wss://router-fallback.example").unwrap()]),
    ];

    for facts in fixtures {
        let mut full = Router::new(RuleRegistry::default_widen_only());
        full.compile(&BTreeSet::from([demand.clone()]), &facts, 20);
        assert_eq!(full.diagnostics().uncovered_authors[&author], expected);
        assert!(full
            .plan()
            .reqs
            .values()
            .flatten()
            .all(|request| request.coverage_assignments.is_empty()));

        let mut incremental = Router::new(RuleRegistry::default_widen_only());
        incremental.admit(&BTreeSet::from([demand.clone()]), &facts, 20);
        assert_eq!(incremental.diagnostics(), full.diagnostics());
        incremental.withdraw([demand.clone()], 20);
        assert_eq!(incremental.ownership_census(), Default::default());
    }
}

#[test]
fn coverage_assignments_outrank_wider_supplemental_claims_under_the_relay_cap() {
    let covered = Keys::generate().public_key();
    let missing = Keys::generate().public_key();
    let first = RelayUrl::parse("wss://router-rank-coverage-a.example").unwrap();
    let second = RelayUrl::parse("wss://router-rank-coverage-b.example").unwrap();
    let app = RelayUrl::parse("wss://router-rank-supplemental.example").unwrap();
    let facts = FixtureRoutingFacts::new()
        .with_outbound_routes(covered, [first, second])
        .with_operator_app([app]);
    let demand = projected_outbox_atom(BTreeSet::from([covered, missing]), []);

    let mut full = Router::new(RuleRegistry::default_widen_only());
    full.compile(&BTreeSet::from([demand.clone()]), &facts, 1);
    let selected = full.plan().reqs.values().flatten().next().unwrap();
    assert!(!selected.coverage_assignments.is_empty());
    assert_eq!(full.diagnostics().uncovered_authors[&covered].achieved, 1);
    assert_eq!(full.diagnostics().uncovered_authors[&missing].achieved, 0);

    let mut incremental = Router::new(RuleRegistry::default_widen_only());
    incremental.admit(&BTreeSet::from([demand.clone()]), &facts, 1);
    assert_eq!(incremental.diagnostics(), full.diagnostics());
    assert!(!incremental
        .plan()
        .reqs
        .values()
        .flatten()
        .next()
        .unwrap()
        .coverage_assignments
        .is_empty());
    incremental.withdraw([demand], 1);
    assert_eq!(incremental.ownership_census(), Default::default());
}

#[test]
fn identical_incumbent_filter_attaches_new_exact_metadata_without_wire_or_budget_slot() {
    let relay = RelayUrl::parse("wss://router-identical-incumbent.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let wide = pinned_kind_atom(&relay, [1, 2]);
    let one = pinned_kind_atom(&relay, [1]);
    let two = pinned_kind_atom(&relay, [2]);
    let budget = subscription_budget(&relay, 1);

    let mut full = Router::new(RuleRegistry::default_widen_only());
    full.compile(
        &BTreeSet::from([wide.clone(), one.clone(), two.clone()]),
        &FixtureRoutingFacts::new(),
        budget.clone(),
    );
    let expected = full.plan().reqs[&session][0].clone();

    for order in [
        [wide.clone(), one.clone(), two.clone()],
        [one.clone(), wide.clone(), two.clone()],
        [two.clone(), one.clone(), wide.clone()],
    ] {
        let mut router = Router::new(RuleRegistry::default_widen_only());
        router.admit(
            &BTreeSet::from([wide.clone()]),
            &FixtureRoutingFacts::new(),
            budget.clone(),
        );
        let attached = router.admit(
            &BTreeSet::from([one.clone(), two.clone()]),
            &FixtureRoutingFacts::new(),
            budget.clone(),
        );
        assert!(attached.wire.ops.is_empty());
        assert_eq!(attached.request_metadata_updates.len(), 1);
        assert!(router.plan().limited_demands.is_empty());
        assert!(router.plan().subscription_shortfalls.is_empty());
        assert_eq!(router.plan().reqs[&session], vec![expected.clone()]);

        for (index, atom) in order.into_iter().enumerate() {
            let closed = router.withdraw([atom], budget.clone());
            let closes = closed
                .wire
                .ops
                .iter()
                .flat_map(|(_, ops)| ops)
                .filter(|op| matches!(op, WireOp::Close(_)))
                .count();
            assert_eq!(closes, usize::from(index == 2));
        }
        assert_eq!(router.ownership_census(), Default::default());
    }
}

#[test]
fn global_cap_shortfalls_are_owned_by_exact_windowed_demands() {
    let author = Keys::generate().public_key();
    let relays: Vec<_> = (0..4)
        .map(|index| RelayUrl::parse(&format!("wss://router-window-cap-{index}.example")).unwrap())
        .collect();
    let mut first = projected_outbox_atom(
        BTreeSet::from([author]),
        [relays[0].clone(), relays[1].clone()],
    );
    first.filter.until = Some(1);
    let mut second = projected_outbox_atom(
        BTreeSet::from([author]),
        [relays[2].clone(), relays[3].clone()],
    );
    second.filter.until = Some(2);
    let first_key = DemandKey::for_atom(&first);
    let second_key = DemandKey::for_atom(&second);
    let mut router = Router::new(RuleRegistry::default_widen_only());

    router.compile(
        &BTreeSet::from([first.clone(), second.clone()]),
        &FixtureRoutingFacts::new(),
        2,
    );

    assert!(router.demand_shortfalls(first_key).is_none());
    assert_eq!(
        router.demand_shortfalls(second_key).unwrap()[&author],
        Shortfall {
            requested_k: 2,
            achieved: 0,
            reason: ShortfallReason::CapExhausted,
        }
    );
    router.withdraw([first, second], 2);
    assert_eq!(router.ownership_census(), Default::default());
}

#[test]
fn incremental_relay_cap_shortfall_matches_full_compile_and_survives_second_hint() {
    let author = Keys::generate().public_key();
    let first_relay = RelayUrl::parse("wss://router-cap-first.example").unwrap();
    let second_relay = RelayUrl::parse("wss://router-cap-second.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let first = projected_outbox_atom(BTreeSet::from([author]), [first_relay]);
    let both = projected_outbox_atom(
        BTreeSet::from([author]),
        [
            first.routing_evidence.iter().next().unwrap().relay.clone(),
            second_relay,
        ],
    );
    let expected = Shortfall {
        requested_k: 2,
        achieved: 1,
        reason: ShortfallReason::CapExhausted,
    };

    let mut full = Router::new(RuleRegistry::default_widen_only());
    full.compile(&BTreeSet::from([both.clone()]), &facts, 1);
    assert_eq!(full.diagnostics().uncovered_authors[&author], expected);

    let mut fresh_incremental = Router::new(RuleRegistry::default_widen_only());
    fresh_incremental.admit(&BTreeSet::from([both.clone()]), &facts, 1);
    assert_eq!(fresh_incremental.diagnostics(), full.diagnostics());

    let mut staged = Router::new(RuleRegistry::default_widen_only());
    staged.admit(&BTreeSet::from([first]), &facts, 1);
    staged.activate(both.clone());
    staged.admit(&BTreeSet::from([both.clone()]), &facts, 1);
    assert_eq!(staged.diagnostics().uncovered_authors[&author], expected);

    staged.withdraw([both], 1);
    assert_eq!(staged.ownership_census(), Default::default());
}

#[test]
fn exact_request_removal_keeps_plan_and_diagnostics_position_aligned_without_resorting() {
    let relay = RelayUrl::parse("wss://router-canonical-removal.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let demands = [
        incompatible_atom(&relay, "first"),
        incompatible_atom(&relay, "second"),
        incompatible_atom(&relay, "third"),
    ];
    let session = RelaySessionKey::unauthenticated(relay);
    let mut router = Router::new(RuleRegistry::default_widen_only());
    router.admit(&demands.iter().cloned().collect(), &facts, 20);
    let initial_ids: Vec<_> = router.plan().reqs[&session]
        .iter()
        .map(|request| request.sub_id.clone())
        .collect();
    assert!(initial_ids.is_sorted());
    let first_key = router.plan().reqs[&session][0]
        .owner_demands
        .iter()
        .next()
        .copied()
        .unwrap();
    let departing = demands
        .iter()
        .find(|atom| DemandKey::for_atom(atom) == first_key)
        .unwrap()
        .clone();

    router.withdraw([departing.clone()], 20);
    let remaining_ids: Vec<_> = router.plan().reqs[&session]
        .iter()
        .map(|request| request.sub_id.clone())
        .collect();
    assert_eq!(
        remaining_ids,
        vec![initial_ids[2].clone(), initial_ids[1].clone()]
    );
    assert_eq!(
        router.diagnostics().per_session[&session].filters,
        router.plan().reqs[&session]
            .iter()
            .map(|request| request.filter.clone())
            .collect::<Vec<_>>()
    );

    router.withdraw(demands.into_iter().filter(|atom| atom != &departing), 20);
    assert_eq!(router.ownership_census(), Default::default());
}
