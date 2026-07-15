use std::collections::BTreeSet;

use nmp_grammar::{
    AccessContext, ConcreteFilter, ContextualAtom, RelaySessionKey, SourceAuthority,
};
use nmp_router::{test_relay, DiscoveryKinds, FixtureDirectory, Router, RuleRegistry};
use nostr::Keys;

#[test]
fn authorless_public_a_b_are_three_exact_session_plans() {
    let relay = test_relay(0);
    let source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    let filter = ConcreteFilter {
        kinds: Some(BTreeSet::from([1])),
        ..ConcreteFilter::default()
    };
    let a = Keys::generate().public_key();
    let b = Keys::generate().public_key();
    let accesses = [
        AccessContext::Public,
        AccessContext::Nip42(a),
        AccessContext::Nip42(b),
    ];
    let demand = accesses
        .into_iter()
        .map(|access| ContextualAtom {
            filter: filter.clone(),
            source: source.clone(),
            access,
            routing_evidence: BTreeSet::new(),
        })
        .collect();
    let mut router = Router::new(
        DiscoveryKinds::default(),
        RuleRegistry::default_widen_only(),
    );

    router.compile(&demand, &FixtureDirectory::new(), 10);

    let expected = BTreeSet::from([
        RelaySessionKey::public(relay.clone()),
        RelaySessionKey::new(relay.clone(), AccessContext::Nip42(a)),
        RelaySessionKey::new(relay, AccessContext::Nip42(b)),
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
        .flat_map(|req| req.absorbed.iter().copied())
        .collect::<BTreeSet<_>>();
    assert_eq!(coverage.len(), 3);

    router.compile(&demand, &FixtureDirectory::new(), 2);
    assert_eq!(router.plan().reqs.len(), 2);
    assert_eq!(router.plan().refused_sessions.len(), 1);
    assert_eq!(router.plan().limited.len(), 1);
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
            source: SourceAuthority::Public,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        },
        ContextualAtom {
            filter,
            source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        },
    ]);
    let mut router = Router::new(
        DiscoveryKinds::default(),
        RuleRegistry::default_widen_only(),
    );

    router.compile(
        &demand,
        &FixtureDirectory::new().with_app([relay.clone()]),
        10,
    );

    let reqs = &router.plan().reqs[&RelaySessionKey::public(relay)];
    assert_eq!(reqs.len(), 2);
    assert_ne!(reqs[0].sub_id, reqs[1].sub_id);
}
