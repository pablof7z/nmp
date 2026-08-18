use std::collections::BTreeSet;

use nmp_grammar::{ConcreteFilter, ContextualAtom, ReadRouting, RelaySessionKey};
use nmp_router::{Router, RuleRegistry};
use nmp_router_testkit::{test_relay, FixtureRoutingFacts};
use nostr::Keys;

#[test]
fn authorless_unauthenticated_a_b_are_three_exact_session_plans() {
    let relay = test_relay(0);
    let source = ReadRouting::Explicit(vec![relay.clone()]);
    let filter = ConcreteFilter {
        kinds: Some(BTreeSet::from([1])),
        ..ConcreteFilter::default()
    };
    let a = Keys::generate().public_key();
    let b = Keys::generate().public_key();
    let accesses = [
        None,
        Some(a),
        Some(b),
    ];
    let demand = accesses
        .into_iter()
        .map(|authenticate_as| ContextualAtom {
            filter: filter.clone(),
            routing: source.clone(),
            authenticate_as,
            routing_evidence: BTreeSet::new(),
        })
        .collect();
    let mut router = Router::new(RuleRegistry::default_widen_only());

    router.compile(&demand, &FixtureRoutingFacts::new(), 10);

    let expected = BTreeSet::from([
        RelaySessionKey::unauthenticated(relay.clone()),
        RelaySessionKey::new(relay.clone(), Some(a)),
        RelaySessionKey::new(relay, Some(b)),
    ]);
    assert_eq!(
        router.plan().reqs.keys().cloned().collect::<BTreeSet<_>>(),
        expected
    );
    let sub_ids = router
        .plan()
        .reqs
        .values()
        .flatten()
        .map(|req| req.sub_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(sub_ids.len(), 3);
    let coverage = router
        .plan()
        .reqs
        .values()
        .flatten()
        .flat_map(|req| req.coverage_claims.iter().cloned())
        .collect::<BTreeSet<_>>();
    assert_eq!(coverage.len(), 3);

    router.compile(&demand, &FixtureRoutingFacts::new(), 2);
    assert_eq!(router.plan().reqs.len(), 2);
    assert_eq!(router.plan().refused_sessions.len(), 1);
    assert_eq!(router.plan().limited_demands.len(), 1);
}

#[test]
fn same_session_different_source_partitions_are_extended_not_overwritten() {
    let relay = test_relay(1);
    let filter = ConcreteFilter {
        kinds: Some(BTreeSet::from([7])),
        ..ConcreteFilter::default()
    };
    let demand = BTreeSet::from([
        ContextualAtom {
            filter: filter.clone(),
            routing: ReadRouting::Auto,
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        },
        ContextualAtom {
            filter,
            routing: ReadRouting::Explicit(vec![relay.clone()]),
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        },
    ]);
    let mut router = Router::new(RuleRegistry::default_widen_only());

    router.compile(
        &demand,
        &FixtureRoutingFacts::new().with_operator_app([relay.clone()]),
        10,
    );

    let reqs = &router.plan().reqs[&RelaySessionKey::unauthenticated(relay)];
    assert_eq!(reqs.len(), 2);
    assert_ne!(reqs[0].sub_id, reqs[1].sub_id);
}
