//! coverage behavior admission integration proofs.

use super::*;

#[test]
fn one_pending_cohort_coalesces_but_a_later_cohort_never_rewrites_it() {
    let relay = RelayUrl::parse("wss://router-admission.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());

    let first = router.admit(
        &BTreeSet::from([atom(&relay, "alice"), atom(&relay, "bob")]),
        &facts,
        20,
    );
    assert_eq!(reqs(&first), 1);
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let incumbent = router.plan().reqs[&session][0].clone();

    let later = router.admit(&BTreeSet::from([atom(&relay, "carol")]), &facts, 20);
    assert_eq!(reqs(&later), 1);
    assert!(later
        .wire
        .ops
        .iter()
        .flat_map(|(_, ops)| ops)
        .all(|op| !matches!(op, WireOp::Close(_))));
    assert!(router.plan().reqs[&session]
        .iter()
        .any(|request| request == &incumbent));
    assert_eq!(router.plan().reqs[&session].len(), 2);
}

#[test]
fn compatible_later_filter_executes_when_the_running_filter_does_not_cover_it() {
    let relay = RelayUrl::parse("wss://router-compatible-not-covered.example").unwrap();
    let author_a = Keys::generate().public_key();
    let author_b = Keys::generate().public_key();
    let first = pinned_author_kind_atom(&relay, [0], [author_a]);
    let later = pinned_author_kind_atom(&relay, [0], [author_b]);
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let first_outcome = router.admit(
        &BTreeSet::from([first.clone()]),
        &FixtureRoutingFacts::new(),
        20,
    );
    assert_eq!(reqs(&first_outcome), 1);
    let incumbent = router
        .plan()
        .reqs
        .values()
        .flatten()
        .next()
        .unwrap()
        .clone();

    let later_outcome = router.admit(
        &BTreeSet::from([later.clone()]),
        &FixtureRoutingFacts::new(),
        20,
    );
    assert_eq!(reqs(&later_outcome), 1);
    assert_eq!(router.plan().reqs.values().map(Vec::len).sum::<usize>(), 2);
    assert!(router
        .plan()
        .reqs
        .values()
        .flatten()
        .any(|request| request == &incumbent));
    assert!(router
        .plan()
        .reqs
        .values()
        .flatten()
        .any(|request| request.filter == later.filter));

    router.withdraw([first], 20);
    router.withdraw([later], 20);
    assert_eq!(router.ownership_census(), Default::default());
}

#[test]
fn partial_running_coverage_never_underfetches_the_uncovered_author_residual() {
    let relay = RelayUrl::parse("wss://router-partial-filter-coverage.example").unwrap();
    let author_a = Keys::generate().public_key();
    let author_b = Keys::generate().public_key();
    let author_c = Keys::generate().public_key();
    let first = pinned_author_kind_atom(&relay, [0, 1], [author_a, author_b]);
    let later = pinned_author_kind_atom(&relay, [1], [author_a, author_b, author_c]);
    let mut router = Router::new(RuleRegistry::default_widen_only());
    router.admit(
        &BTreeSet::from([first.clone()]),
        &FixtureRoutingFacts::new(),
        20,
    );
    let incumbent = router
        .plan()
        .reqs
        .values()
        .flatten()
        .next()
        .unwrap()
        .clone();

    let later_outcome = router.admit(
        &BTreeSet::from([later.clone()]),
        &FixtureRoutingFacts::new(),
        20,
    );
    assert_eq!(reqs(&later_outcome), 1);
    assert!(router
        .plan()
        .reqs
        .values()
        .flatten()
        .any(|request| request == &incumbent));
    assert!(
        router
            .plan()
            .reqs
            .values()
            .flatten()
            .any(|request| request.filter == later.filter),
        "until residual subtraction is proven safe, executing the full later filter prevents underfetch"
    );

    router.withdraw([first], 20);
    router.withdraw([later], 20);
    assert_eq!(router.ownership_census(), Default::default());
}

#[test]
#[ignore = "known violation #1341: representable incumbent residual is not yet subtracted"]
fn representable_running_filter_residual_is_executed_and_owned_as_one_lifecycle() {
    let relay = RelayUrl::parse("wss://router-representable-filter-residual.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let author_a = Keys::generate().public_key();
    let author_b = Keys::generate().public_key();
    let author_c = Keys::generate().public_key();
    let first = pinned_author_kind_atom(&relay, [0, 1], [author_a, author_b]);
    let later = pinned_author_kind_atom(&relay, [1], [author_a, author_b, author_c]);
    let residual = pinned_author_kind_atom(&relay, [1], [author_c]);
    let mut router = Router::new(RuleRegistry::default_widen_only());
    router.admit(
        &BTreeSet::from([first.clone()]),
        &FixtureRoutingFacts::new(),
        20,
    );
    let incumbent = router.plan().reqs[&session][0].clone();

    let admitted = router.admit(
        &BTreeSet::from([later.clone()]),
        &FixtureRoutingFacts::new(),
        20,
    );
    let req_filters: Vec<_> = admitted
        .wire
        .ops
        .iter()
        .flat_map(|(_, ops)| ops)
        .filter_map(|op| match op {
            WireOp::Req(_, filter) => Some(filter.clone()),
            WireOp::Close(_) => None,
        })
        .collect();
    assert_eq!(req_filters, vec![residual.filter]);
    assert!(router.plan().reqs[&session].contains(&incumbent));

    let first_withdrawn = router.withdraw([first], 20);
    assert!(first_withdrawn.wire.ops.is_empty());
    assert_eq!(router.plan().reqs[&session].len(), 2);

    let later_withdrawn = router.withdraw([later], 20);
    assert_eq!(
        later_withdrawn
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
fn exact_running_coverage_makes_repeated_admission_a_noop() {
    let relay = RelayUrl::parse("wss://router-covered.example").unwrap();
    let facts = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::default_widen_only());
    let demand = BTreeSet::from([atom(&relay, "alice")]);
    assert_eq!(reqs(&router.admit(&demand, &facts, 20)), 1);

    let duplicate = router.admit(&demand, &facts, 20);

    assert!(duplicate.wire.ops.is_empty());
    assert!(duplicate.changed_coverage.is_empty());
    assert!(!duplicate.diagnostics_changed);
    assert_eq!(router.plan().reqs.len(), 1);
    assert_eq!(router.plan().reqs.values().flatten().count(), 1);
}

#[test]
fn multi_author_outbox_withdrawal_closes_its_exact_logical_owner() {
    let first = Keys::generate().public_key();
    let second = Keys::generate().public_key();
    let relay = RelayUrl::parse("wss://router-multi-author-owner.example").unwrap();
    let facts = FixtureRoutingFacts::new()
        .with_outbound_routes(first, [relay.clone()])
        .with_outbound_routes(second, [relay]);
    let demand = projected_outbox_atom(BTreeSet::from([first, second]), []);
    let demand_key = DemandKey::for_atom(&demand);
    let mut router = Router::new(RuleRegistry::default_widen_only());

    let admitted = router.admit(&BTreeSet::from([demand.clone()]), &facts, 20);
    assert_eq!(reqs(&admitted), 1);
    let request = router.plan().reqs.values().flatten().next().unwrap();
    assert_eq!(request.owner_demands, BTreeSet::from([demand_key]));
    assert_eq!(
        request.coverage_claims.len(),
        2,
        "coverage remains per author"
    );

    let withdrawn = router.withdraw([demand], 20);
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
    assert_eq!(router.ownership_census(), Default::default());
}
