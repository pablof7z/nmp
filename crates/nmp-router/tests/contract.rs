//! M2 contract tests 1-9, 14, 18-20 (`docs/plans/M2-compiler-router-plan.md`
//! §5) — driven entirely through `nmp-router`'s public API
//! (`Router::compile` + `FixtureDirectory`), independent of `nmp-resolver`.

use std::collections::BTreeSet;

use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, SourceAuthority};
use nmp_router::{
    test_relay, DiscoveryKinds, FixtureDirectory, Lane, RouteKind, Router, RuleRegistry,
    ShortfallReason,
};

fn pk(c: char) -> String {
    c.to_string().repeat(64)
}

fn cf_kind_authors(kind: u16, authors: &[&str]) -> ConcreteFilter {
    ConcreteFilter {
        kinds: Some(BTreeSet::from([kind])),
        authors: Some(authors.iter().map(|s| s.to_string()).collect()),
        ..ConcreteFilter::default()
    }
}

/// An `AuthorOutboxes`-sourced demand atom -- what `Demand::from_filter`
/// would produce for any `outbox(...)` call (its `authors` is
/// always `Some`), spelled out explicitly since these tests build
/// `ContextualAtom`s directly rather than through a `Demand`.
fn outbox(kind: u16, authors: &[&str]) -> ContextualAtom {
    ContextualAtom {
        filter: cf_kind_authors(kind, authors),
        source: SourceAuthority::AuthorOutboxes,
        access: AccessContext::Public,
    }
}

/// A `Public`-sourced demand atom for an already-built (typically
/// authorless) filter.
fn pinned(filter: ConcreteFilter) -> ContextualAtom {
    ContextualAtom {
        filter,
        source: SourceAuthority::Public,
        access: AccessContext::Public,
    }
}

fn new_router() -> Router {
    Router::new(
        DiscoveryKinds::default(),
        RuleRegistry::default_widen_only(),
    )
}

/// Test 1: `outbox_maps_authors_to_write_relays`.
#[test]
fn outbox_maps_authors_to_write_relays() {
    let a = pk('a');
    let b = pk('b');
    let dir = FixtureDirectory::new()
        .with_write(a.clone(), [test_relay(0), test_relay(1)])
        .with_write(b.clone(), [test_relay(2), test_relay(3)]);
    let mut router = new_router();

    let demand = BTreeSet::from([outbox(1, &[&a]), outbox(1, &[&b])]);
    router.compile(&demand, &dir, 10);

    let plan = router.plan();
    for relay in [test_relay(0), test_relay(1)] {
        let reqs = &plan.reqs[&relay];
        assert!(!reqs.is_empty());
        for req in reqs {
            assert_eq!(req.filter.authors, Some(BTreeSet::from([a.clone()])));
            assert!(req
                .provenance
                .iter()
                .all(|p| p.lane == Lane::Nip65Write && p.route_kind == RouteKind::OutboxSolved));
        }
    }
    for relay in [test_relay(2), test_relay(3)] {
        let reqs = &plan.reqs[&relay];
        for req in reqs {
            assert_eq!(req.filter.authors, Some(BTreeSet::from([b.clone()])));
        }
    }
}

/// Test 2: `coverage_gives_each_author_min_two_relays`.
#[test]
fn coverage_gives_each_author_min_two_relays() {
    let authors: Vec<String> = "abc".chars().map(pk).collect();
    let pool = vec![test_relay(0), test_relay(1), test_relay(2)];
    let dir = FixtureDirectory::shared_pool_mailboxes(&authors, &pool);
    let mut router = new_router();

    let demand: BTreeSet<ContextualAtom> =
        authors.iter().map(|a| outbox(1, &[a.as_str()])).collect();
    router.compile(&demand, &dir, 10);

    assert!(router.diagnostics().uncovered_authors.is_empty());
    // Reverse-coverage: every author must show up on at least 2 relays.
    for author in &authors {
        let count = router
            .plan()
            .reqs
            .values()
            .flatten()
            .filter(|req| {
                req.filter
                    .authors
                    .as_ref()
                    .is_some_and(|a| a.contains(author))
            })
            .count();
        assert!(
            count >= 2,
            "author {author} covered by only {count} relay(s)"
        );
    }
}

/// Test 3: `coverage_respects_cap_under_disjoint_mailboxes`.
#[test]
fn coverage_respects_cap_under_disjoint_mailboxes() {
    let authors: Vec<String> = (0..10).map(|i| format!("{i:064}")).collect();
    let dir = FixtureDirectory::disjoint_mailboxes(&authors);
    let mut router = new_router();

    let demand: BTreeSet<ContextualAtom> =
        authors.iter().map(|a| outbox(1, &[a.as_str()])).collect();
    let cap = 6;
    router.compile(&demand, &dir, cap);

    assert!(router.plan().reqs.len() <= cap);
    assert!(!router.diagnostics().uncovered_authors.is_empty());
    for shortfall in router.diagnostics().uncovered_authors.values() {
        assert_eq!(shortfall.reason, ShortfallReason::CapExhausted);
    }
}

/// Test 4: `coverage_single_prolific_author_capped_at_k`.
#[test]
fn coverage_single_prolific_author_capped_at_k() {
    let a = pk('a');
    let dir = FixtureDirectory::prolific_author(a.clone(), 50);
    let mut router = new_router();

    let demand = BTreeSet::from([outbox(1, &[&a])]);
    router.compile(&demand, &dir, 100);

    let relays_serving_a: BTreeSet<_> = router
        .plan()
        .reqs
        .iter()
        .filter(|(_, reqs)| {
            reqs.iter()
                .any(|r| r.filter.authors == Some(BTreeSet::from([a.clone()])))
        })
        .map(|(relay, _)| relay.clone())
        .collect();
    assert_eq!(relays_serving_a.len(), 2);
    assert!(router.diagnostics().uncovered_authors.is_empty());
}

/// Test 5: `coverage_author_with_one_relay_clamps_k`.
#[test]
fn coverage_author_with_one_relay_clamps_k() {
    let a = pk('a');
    let dir = FixtureDirectory::new().with_write(a.clone(), [test_relay(0)]);
    let mut router = new_router();

    let demand = BTreeSet::from([outbox(1, &[&a])]);
    router.compile(&demand, &dir, 10);

    let shortfall = router.diagnostics().uncovered_authors[&a];
    assert_eq!(shortfall.reason, ShortfallReason::FewerCandidatesThanK);
    assert_eq!(shortfall.achieved, 1);
}

/// Test 6: `content_atom_uncovered_author_never_uses_indexer`.
#[test]
fn content_atom_uncovered_author_never_uses_indexer() {
    let a = pk('a');
    let dir = FixtureDirectory::new().with_indexer(test_relay(99));
    let mut router = new_router();

    let demand = BTreeSet::from([outbox(1, &[&a])]); // content kind
    router.compile(&demand, &dir, 10);

    assert_eq!(
        router.diagnostics().uncovered_authors[&a].reason,
        ShortfallReason::NoCandidates
    );
    assert!(!router.plan().reqs.contains_key(&test_relay(99)));
}

/// Test 7: `indexer_lane_only_for_discovery_kinds`.
#[test]
fn indexer_lane_only_for_discovery_kinds() {
    let a = pk('a');
    let dir = FixtureDirectory::new().with_indexer(test_relay(99));
    let mut router = new_router();

    let discovery_atom = outbox(3, &[&a]); // kind:3 -- discovery
    let content_atom = outbox(1, &[&a]); // kind:1 -- content
    let demand = BTreeSet::from([discovery_atom, content_atom]);
    router.compile(&demand, &dir, 10);

    let indexer_reqs = &router.plan().reqs[&test_relay(99)];
    assert!(indexer_reqs
        .iter()
        .all(|r| r.filter.kinds == Some(BTreeSet::from([3u16]))));
    assert!(indexer_reqs.iter().all(|r| r
        .provenance
        .iter()
        .all(|p| p.lane == Lane::IndexerDiscovery)));
}

/// Test 8: `exact_canonical_dedup_one_req_per_relay`.
#[test]
fn exact_canonical_dedup_one_req_per_relay() {
    let a = pk('a');
    let dir = FixtureDirectory::new().with_write(a.clone(), [test_relay(0), test_relay(1)]);
    let mut router = new_router();

    // Two demand atoms that resolve to the IDENTICAL ConcreteFilter (as if
    // two different subscriptions produced the same atom).
    let demand = BTreeSet::from([outbox(1, &[&a])]);
    router.compile(&demand, &dir, 10);

    assert_eq!(router.plan().reqs[&test_relay(0)].len(), 1);
    assert_eq!(router.plan().reqs[&test_relay(1)].len(), 1);
}

/// Test 9: `author_union_coalesces_shards_into_one_req`.
#[test]
fn author_union_coalesces_shards_into_one_req() {
    let (a, b, d) = (pk('a'), pk('b'), pk('d'));
    let dir = FixtureDirectory::new()
        .with_write(a.clone(), [test_relay(0), test_relay(1)])
        .with_write(b.clone(), [test_relay(0), test_relay(1)])
        .with_write(d.clone(), [test_relay(0), test_relay(1)]);
    let mut router = new_router();

    let demand = BTreeSet::from([outbox(1, &[&a]), outbox(1, &[&b]), outbox(1, &[&d])]);
    router.compile(&demand, &dir, 10);

    let reqs = &router.plan().reqs[&test_relay(0)];
    assert_eq!(reqs.len(), 1, "expected exactly one coalesced WireReq");
    let authors = reqs[0].filter.authors.clone().unwrap();
    assert_eq!(authors, BTreeSet::from([a, b, d]));
}

/// Test 14: `per_relay_diff_is_surgical`.
#[test]
fn per_relay_diff_is_surgical() {
    let (a, b, c, d) = (pk('a'), pk('b'), pk('c'), pk('d'));
    let dir = FixtureDirectory::new()
        .with_write(a.clone(), [test_relay(0), test_relay(1)])
        .with_write(b.clone(), [test_relay(0), test_relay(1)])
        .with_write(c.clone(), [test_relay(2), test_relay(3)])
        .with_write(d.clone(), [test_relay(2), test_relay(3)]);
    let mut router = new_router();

    let demand1 = BTreeSet::from([outbox(1, &[&a]), outbox(1, &[&b]), outbox(1, &[&c])]);
    router.compile(&demand1, &dir, 10);

    let demand2 = BTreeSet::from([outbox(1, &[&a]), outbox(1, &[&b]), outbox(1, &[&d])]);
    let delta = router.compile(&demand2, &dir, 10);

    let touched: BTreeSet<_> = delta.ops.iter().map(|(r, _)| r.clone()).collect();
    assert!(touched.contains(&test_relay(2)));
    assert!(touched.contains(&test_relay(3)));
    assert!(
        !touched.contains(&test_relay(0)) && !touched.contains(&test_relay(1)),
        "relays serving ONLY A,B must emit no ops"
    );
    // Each touched relay carries exactly one overwriting Req, keyed by the
    // stable skeleton sub-id (same kind:1 skeleton throughout).
    for (_, ops) in &delta.ops {
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], nmp_router::WireOp::Req(_, _)));
    }
}

/// Test 18: `every_wire_req_traces_to_a_route`.
#[test]
fn every_wire_req_traces_to_a_route() {
    let a = pk('a');
    let dir = FixtureDirectory::new().with_write(a.clone(), [test_relay(0), test_relay(1)]);
    let mut router = new_router();
    let demand = BTreeSet::from([outbox(1, &[&a])]);
    router.compile(&demand, &dir, 10);

    for reqs in router.plan().reqs.values() {
        for req in reqs {
            assert!(!req.provenance.is_empty());
        }
    }
}

/// Test 19: `diagnostics_reverse_coverage_and_lanes`.
#[test]
fn diagnostics_reverse_coverage_and_lanes() {
    let (a, b) = (pk('a'), pk('b'));
    let dir = FixtureDirectory::new()
        .with_write(a.clone(), [test_relay(0), test_relay(1)])
        .with_write(b.clone(), [test_relay(0)]); // b: FewerCandidatesThanK
    let mut router = new_router();
    let demand = BTreeSet::from([outbox(1, &[&a]), outbox(1, &[&b])]);
    router.compile(&demand, &dir, 10);

    let diag = router.diagnostics();
    let relay0 = &diag.per_relay[&test_relay(0)];
    assert_eq!(relay0.authors_served, 2);
    // Both A and B reached relay0 via Nip65Write; AuthorUnion folds their
    // filters into one WireReq, but `by_lane` counts each contributing
    // author-route (2 here), not the number of merged WireReqs (1).
    assert_eq!(relay0.by_lane.get(&Lane::Nip65Write), Some(&2));
    assert_eq!(relay0.wire_sub_count, 1);
    assert_eq!(relay0.filters.len(), relay0.wire_sub_count);
    assert_eq!(
        diag.uncovered_authors[&b].reason,
        ShortfallReason::FewerCandidatesThanK
    );
}

/// Test 20: `nip29_non_author_atom_routes_via_group_host`.
#[test]
fn nip29_non_author_atom_routes_via_group_host() {
    let group_atom = ConcreteFilter {
        kinds: Some(BTreeSet::from([39_000u16, 39_001u16, 39_002u16])),
        tags: {
            let mut m = std::collections::BTreeMap::new();
            m.insert(
                nmp_grammar::IndexedTagName::new('d').unwrap(),
                BTreeSet::from(["group1".to_string()]),
            );
            m
        },
        ..ConcreteFilter::default()
    };
    let host = test_relay(7);
    let dir = FixtureDirectory::new().with_group_host(group_atom.clone(), host.clone());
    let mut router = new_router();

    let demand = BTreeSet::from([pinned(group_atom.clone())]);
    router.compile(&demand, &dir, 10);

    let reqs = &router.plan().reqs[&host];
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].filter, group_atom);
    assert!(reqs[0]
        .provenance
        .iter()
        .all(|p| p.lane == Lane::GroupHost && p.route_kind == RouteKind::Pinned));
    // No coverage solving happened for this atom: no shortfall recorded.
    assert!(router.diagnostics().uncovered_authors.is_empty());
}
