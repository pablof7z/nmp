//! scale withdrawal admission integration proofs.

use nmp_grammar::RelaySessionKey;
use super::*;

#[test]
fn ten_thousand_shared_keys_do_only_delta_edges_plus_one_physical_close() {
    let relay = RelayUrl::parse("wss://router-10k-withdraw.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let demand: BTreeSet<_> = (0..10_000)
        .map(|index| atom(&relay, &format!("owner-{index:05}")))
        .collect();
    let physical_requests = reqs(&router.admit(&demand, &facts, 20));
    assert!(
        physical_requests < 50,
        "the 10k atoms should remain coalesced"
    );
    assert_eq!(
        router
            .plan()
            .reqs
            .values()
            .flatten()
            .map(|request| request.owner_demands.len())
            .sum::<usize>(),
        10_000,
        "coalescing must retain one independently cancellable lifecycle edge per atom"
    );
    assert_eq!(
        router.ownership_census().requests_by_demand_edges,
        10_000,
        "every retained lifecycle edge must be indexed before withdrawal"
    );
    assert_eq!(
        router.ownership_census().physical_request_claim_keys,
        physical_requests,
        "every immutable physical request must retain its claim owner"
    );
    assert_eq!(
        router.ownership_census().physical_claim_edges,
        10_000,
        "all coalesced claim edges must survive local-owner pruning until CLOSE"
    );

    router.reset_withdrawal_work();
    let mut close_count = 0;
    for atom in demand {
        close_count += withdraw(&mut router, [atom], 20)
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count();
    }

    let work = router.withdrawal_work();
    assert_eq!(close_count, physical_requests);
    assert_eq!(work.dropped_atoms, 10_000);
    assert_eq!(work.request_edges_touched, 10_000);
    assert_eq!(work.requests_closed, physical_requests as u64);
    assert_eq!(work.physical_coverage_edges_released, 10_000);
    assert_eq!(work.diagnostic_rebuilds, physical_requests as u64);
    assert_eq!(work.diagnostic_requests_visited, 0);
}

#[test]
fn lifting_a_partial_source_limit_adds_only_the_missing_session() {
    let first_relay = RelayUrl::parse("wss://router-partial-first.example").unwrap();
    let second_relay = RelayUrl::parse("wss://router-partial-second.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let demand = BTreeSet::from([atom_on(
        BTreeSet::from([first_relay.clone(), second_relay.clone()]),
        "alice",
    )]);

    assert_eq!(reqs(&router.admit(&demand, &facts, 1)), 1);
    assert_eq!(router.plan().reqs.len(), 1);
    assert_eq!(router.plan().limited_demands.len(), 1);
    let incumbent_session = router.plan().reqs.keys().next().unwrap().clone();
    let incumbent = router.plan().reqs[&incumbent_session][0].clone();

    let completed = router.admit(&demand, &facts, 2);

    assert_eq!(reqs(&completed), 1);
    assert_eq!(router.plan().reqs.len(), 2);
    assert_eq!(router.plan().reqs[&incumbent_session], vec![incumbent]);
    assert!(router.plan().limited_demands.is_empty());
}

#[test]
fn withdrawing_one_exact_refused_owner_updates_only_its_session_shortfall() {
    let relay = RelayUrl::parse("wss://router-refused-withdraw.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let budget = subscription_budget(&relay, 2);
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let demand: BTreeSet<_> = (0..5)
        .map(|index| incompatible_atom(&relay, &format!("owner-{index}")))
        .collect();

    let admitted = router.admit(&demand, &facts, budget.clone());
    assert_eq!(reqs(&admitted), 2);
    assert_eq!(router.plan().limited_demands.len(), 3);
    assert_eq!(router.plan().limited_demands.len(), 3);
    assert_eq!(
        router.plan().subscription_shortfalls[&session],
        nmp_router::BudgetShortfall {
            budget: 2,
            planned: 5,
            refused: 3,
        }
    );
    assert_eq!(
        router.diagnostics().per_session[&session].subscriptions_refused,
        3
    );
    let refused = demand
        .iter()
        .find(|atom| {
            router
                .plan()
                .limited_demands
                .contains(&DemandKey::for_atom(atom))
        })
        .expect("the relay budget must refuse three exact owners")
        .clone();

    router.reset_withdrawal_work();
    let withdrawn = router.withdraw([refused], budget);

    assert!(withdrawn.wire.ops.is_empty());
    assert_eq!(router.plan().limited_demands.len(), 2);
    assert_eq!(router.plan().limited_demands.len(), 2);
    assert_eq!(
        router.plan().subscription_shortfalls[&session],
        nmp_router::BudgetShortfall {
            budget: 2,
            planned: 4,
            refused: 2,
        }
    );
    assert_eq!(
        router.diagnostics().per_session[&session].subscriptions_refused,
        2
    );
    assert_eq!(router.withdrawal_work().diagnostic_requests_visited, 0);
}

#[test]
fn zero_budget_refusal_lives_until_its_final_exact_owner_withdraws() {
    let relay = RelayUrl::parse("wss://router-zero-budget-withdraw.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let budget = subscription_budget(&relay, 0);
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let demand: Vec<_> = (0..3)
        .map(|index| incompatible_atom(&relay, &format!("owner-{index}")))
        .collect();

    let admitted = router.admit(&demand.iter().cloned().collect(), &facts, budget.clone());
    assert_eq!(reqs(&admitted), 0);
    assert!(router.plan().refused_sessions.contains(&session));
    assert_eq!(
        router.plan().subscription_shortfalls[&session],
        nmp_router::BudgetShortfall {
            budget: 0,
            planned: 3,
            refused: 3,
        }
    );

    for (index, atom) in demand.into_iter().enumerate() {
        router.reset_withdrawal_work();
        let withdrawn = router.withdraw([atom], budget.clone());
        assert!(withdrawn.wire.ops.is_empty());
        assert_eq!(router.withdrawal_work().diagnostic_requests_visited, 0);
        if index < 2 {
            assert!(router.plan().refused_sessions.contains(&session));
            assert_eq!(
                router.plan().subscription_shortfalls[&session],
                nmp_router::BudgetShortfall {
                    budget: 0,
                    planned: 2 - index,
                    refused: 2 - index,
                }
            );
            assert_eq!(
                router.diagnostics().sessions_refused_by_subscription_budget,
                1
            );
        } else {
            assert!(!router.plan().refused_sessions.contains(&session));
            assert!(!router.plan().subscription_shortfalls.contains_key(&session));
            assert_eq!(
                router.diagnostics().sessions_refused_by_subscription_budget,
                0
            );
        }
    }
}

#[test]
fn later_cohort_never_rebuilds_or_visits_ten_thousand_incumbent_active_entries() {
    let relay = RelayUrl::parse("wss://router-10k-admission.example").unwrap();
    let refused_relay = RelayUrl::parse("wss://router-10k-refused.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let mut incumbents: BTreeSet<_> = (0..10_000)
        .map(|index| atom(&relay, &format!("incumbent-{index:05}")))
        .collect();
    incumbents.insert(atom(&refused_relay, "retained-refusal"));
    router.admit(&incumbents, &facts, 1);
    let before: BTreeMap<_, _> = router
        .plan()
        .reqs
        .values()
        .flatten()
        .map(|request| (request.sub_id.clone(), request.clone()))
        .collect();
    let limited_before = router.plan().limited_demands.clone();
    let limited_demands_before = router.plan().limited_demands.clone();
    let refused_sessions_before = router.plan().refused_sessions.clone();
    let shortfalls_before = router.plan().subscription_shortfalls.clone();
    let later = atom(&relay, "later-compatible-uncovered");

    router.reset_admission_work();
    let admitted = router.admit(&BTreeSet::from([later]), &facts, 1);

    assert_eq!(reqs(&admitted), 1);
    let preserved: BTreeMap<_, _> = router
        .plan()
        .reqs
        .values()
        .flatten()
        .filter(|request| before.contains_key(&request.sub_id))
        .map(|request| (request.sub_id.clone(), request.clone()))
        .collect();
    assert_eq!(preserved, before);
    assert_eq!(router.plan().limited_demands, limited_before);
    assert_eq!(router.plan().limited_demands, limited_demands_before);
    assert_eq!(router.plan().refused_sessions, refused_sessions_before);
    assert_eq!(router.plan().subscription_shortfalls, shortfalls_before);
    assert_eq!(router.admission_work().cohort_compiles, 1);
    assert_eq!(router.admission_work().incumbent_active_entries_visited, 0);
    assert_eq!(router.admission_work().incumbent_plan_requests_visited, 0);
    assert_eq!(router.admission_work().incumbent_limited_entries_visited, 0);
    assert_eq!(router.admission_work().incumbent_refusal_entries_visited, 0);
}

#[test]
fn ten_thousand_same_session_requests_withdraw_by_exact_position() {
    let relay = RelayUrl::parse("wss://router-10k-exact-removal.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::dedup_only());
    let demand: BTreeSet<_> = (0..10_000)
        .map(|index| incompatible_atom(&relay, &format!("owner-{index:05}")))
        .collect();
    let admitted = router.admit(&demand, &facts, 20);
    assert_eq!(reqs(&admitted), 10_000);
    assert_eq!(router.ownership_census().request_coverage_keys, 10_000);
    assert_eq!(router.ownership_census().request_position_keys, 10_000);

    router.reset_withdrawal_work();
    for atom in demand {
        let withdrawn = router.withdraw([atom], 20);
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
    }

    let work = router.withdrawal_work();
    assert_eq!(work.dropped_atoms, 10_000);
    assert_eq!(work.request_edges_touched, 10_000);
    assert_eq!(work.plan_request_entries_visited, 10_000);
    assert_eq!(work.requests_closed, 10_000);
    assert_eq!(work.physical_coverage_edges_released, 10_000);
    assert_eq!(work.diagnostic_requests_visited, 0);
    assert_eq!(router.ownership_census(), Default::default());
}
