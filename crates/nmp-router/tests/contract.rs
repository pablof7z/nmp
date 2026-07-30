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

fn assert_feasible_two_source_coverage(author_count: usize, cap: usize) {
    let authors: Vec<_> = (0..author_count).map(|_| author()).collect();
    let shared_relays = [test_relay(0), test_relay(1)];
    let facts = authors
        .iter()
        .fold(FixtureRoutingFacts::new(), |facts, author| {
            facts.with_author_routes(*author, shared_relays.clone(), [])
        });
    let demand: BTreeSet<_> = authors.iter().map(|author| outbox(1, &[*author])).collect();
    let mut router = router();

    router.compile(&demand, &facts, cap);

    assert!(
        router.plan().reqs.len() <= cap,
        "{author_count} authors must stay within the whole-demand cap of {cap}"
    );
    assert_eq!(
        router.plan().reqs.keys().cloned().collect::<BTreeSet<_>>(),
        shared_relays
            .iter()
            .cloned()
            .map(session)
            .collect::<BTreeSet<_>>(),
        "a feasible shared-source objective must contact exactly its two sources"
    );
    assert!(
        router.diagnostics().uncovered_authors.is_empty(),
        "feasible two-source coverage must not report author shortfall"
    );
    assert!(
        router.plan().limited.is_empty(),
        "a non-binding cap must not report demand as locally limited"
    );

    for author in authors {
        let author_hex = author.to_hex();
        let serving_relays = shared_relays
            .iter()
            .filter(|relay| {
                router.plan().reqs[&session((*relay).clone())]
                    .iter()
                    .any(|request| {
                        request
                            .filter
                            .authors
                            .as_ref()
                            .is_some_and(|authors| authors.contains(&author_hex))
                    })
            })
            .count();
        assert_eq!(
            serving_relays, 2,
            "author {author_hex} must be present on both planned relay sessions"
        );
    }
}

#[test]
fn feasible_two_source_author_coverage_stays_under_the_whole_demand_cap() {
    assert_feasible_two_source_coverage(5, 10);
    assert_feasible_two_source_coverage(50, 15);
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
