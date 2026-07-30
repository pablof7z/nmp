//! Protocol-neutral router contract.

use std::collections::BTreeSet;

use nmp_grammar::{
    AccessContext, ConcreteFilter, ContextualAtom, RelaySessionKey, SourceAuthority,
};
use nmp_router::{
    test_relay, FixtureRoutingFacts, Lane, PublicKey, RouteKind, Router, RuleRegistry,
    ShortfallReason,
};
use nostr::Keys;

fn author() -> PublicKey {
    Keys::generate().public_key()
}

fn outbox(kind: u16, authors: &[PublicKey]) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            authors: Some(authors.iter().map(PublicKey::to_hex).collect()),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::AuthorOutboxes,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn exact(filter: ConcreteFilter, relays: BTreeSet<nmp_router::RelayUrl>) -> ContextualAtom {
    ContextualAtom {
        filter,
        source: SourceAuthority::Pinned(relays),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn router() -> Router {
    Router::new(RuleRegistry::default_widen_only())
}

fn session(relay: nmp_router::RelayUrl) -> RelaySessionKey {
    RelaySessionKey::public(relay)
}

#[test]
fn outbound_facts_form_coverage_routes() {
    let first = author();
    let second = author();
    let facts = FixtureRoutingFacts::new()
        .with_author_routes(first, [test_relay(0), test_relay(1)], [])
        .with_author_routes(second, [test_relay(2), test_relay(3)], []);
    let mut router = router();

    router.compile(
        &BTreeSet::from([outbox(1, &[first]), outbox(1, &[second])]),
        &facts,
        10,
    );

    for relay in [test_relay(0), test_relay(1)] {
        let reqs = &router.plan().reqs[&session(relay)];
        assert!(reqs.iter().all(|request| {
            request.filter.authors == Some(BTreeSet::from([first.to_hex()]))
                && request.provenance.iter().all(|route| {
                    route.lane == Lane::AuthorOutbound && route.route_kind == RouteKind::Coverage
                })
        }));
    }
}

#[test]
fn coverage_respects_whole_demand_cap() {
    let authors: Vec<_> = (0..10).map(|_| author()).collect();
    let facts = FixtureRoutingFacts::disjoint_mailboxes(&authors);
    let demand = authors.iter().map(|author| outbox(1, &[*author])).collect();
    let mut router = router();

    router.compile(&demand, &facts, 6);

    assert!(router.plan().reqs.len() <= 6);
    assert!(router
        .diagnostics()
        .uncovered_authors
        .values()
        .all(|shortfall| shortfall.reason == ShortfallReason::CapExhausted));
}

#[test]
fn present_empty_absent_and_unknown_are_all_routeless_but_not_inferred() {
    let present = author();
    let absent = author();
    let unknown = author();
    let facts = FixtureRoutingFacts::new()
        .with_author_routes(present, [], [])
        .with_author_absent(absent);
    let mut router = router();
    router.compile(
        &BTreeSet::from([outbox(1, &[present, absent, unknown])]),
        &facts,
        10,
    );

    for key in [present, absent, unknown] {
        assert_eq!(
            router.diagnostics().uncovered_authors[&key].reason,
            ShortfallReason::NoCandidates
        );
    }
}

#[test]
fn operator_app_is_supplemental_and_does_not_count_as_coverage() {
    let key = author();
    let app = test_relay(9);
    let facts = FixtureRoutingFacts::new()
        .with_author_routes(key, [test_relay(0)], [])
        .with_operator_app([app.clone()]);
    let mut router = router();
    router.compile(&BTreeSet::from([outbox(1, &[key])]), &facts, 10);

    assert!(router.plan().reqs.contains_key(&session(app.clone())));
    assert!(router.plan().reqs[&session(app)]
        .iter()
        .flat_map(|request| &request.provenance)
        .all(|route| {
            route.lane == Lane::OperatorApp && route.route_kind == RouteKind::Supplemental
        }));
    assert_eq!(
        router.diagnostics().uncovered_authors[&key].reason,
        ShortfallReason::FewerCandidatesThanK
    );
}

#[test]
fn exact_authority_bypasses_every_fact_and_operator_route() {
    let key = author();
    let exact_relay = test_relay(7);
    let facts = FixtureRoutingFacts::new()
        .with_author_routes(key, [test_relay(0)], [])
        .with_operator_app([test_relay(1)])
        .with_operator_fallback([test_relay(2)]);
    let filter = outbox(1, &[key]).filter;
    let mut router = router();
    router.compile(
        &BTreeSet::from([exact(filter, BTreeSet::from([exact_relay.clone()]))]),
        &facts,
        10,
    );

    assert_eq!(
        router.plan().reqs.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([session(exact_relay.clone())])
    );
    assert!(router.plan().reqs[&session(exact_relay)]
        .iter()
        .flat_map(|request| &request.provenance)
        .all(|route| route.lane == Lane::Exact && route.route_kind == RouteKind::Exact));
}

#[test]
fn public_authorless_atom_has_no_hidden_directory_destination() {
    let app = test_relay(5);
    let facts = FixtureRoutingFacts::new().with_operator_app([app.clone()]);
    let atom = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([39_000])),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Public,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let mut router = router();
    router.compile(&BTreeSet::from([atom]), &facts, 10);
    assert_eq!(
        router.plan().reqs.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([session(app)])
    );
}
