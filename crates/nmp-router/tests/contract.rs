//! Protocol-neutral router contract.

use std::collections::BTreeSet;

use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, ReadRouting, RelaySessionKey};
use nmp_router::{Lane, PublicKey, RouteKind, Router, RuleRegistry, ShortfallReason};
use nmp_router_testkit::{test_relay, FixtureRoutingFacts};
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
        routing: ReadRouting::Auto,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn exact(filter: ConcreteFilter, relays: BTreeSet<nmp_router::RelayUrl>) -> ContextualAtom {
    ContextualAtom {
        filter,
        routing: ReadRouting::Explicit(relays.into_iter().collect()),
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
fn feasible_two_source_author_coverage_stays_under_the_whole_demand_cap() {
    for (author_count, cap) in [(5, 10), (50, 15)] {
        let authors: Vec<_> = (0..author_count).map(|_| author()).collect();
        let relays = [test_relay(0), test_relay(1)];
        let facts = FixtureRoutingFacts::shared_pool_mailboxes(&authors, &relays);
        let mut router = router();

        router.compile(&BTreeSet::from([outbox(1, &authors)]), &facts, cap);

        assert!(
            router.plan().reqs.len() <= cap,
            "{} authors planned {} relay sessions above cap {cap}",
            authors.len(),
            router.plan().reqs.len()
        );
        for relay in relays.iter().cloned() {
            let planned_authors: BTreeSet<_> = router.plan().reqs[&session(relay)]
                .iter()
                .filter_map(|request| request.filter.authors.as_ref())
                .flatten()
                .cloned()
                .collect();
            assert_eq!(
                planned_authors,
                authors.iter().map(PublicKey::to_hex).collect(),
                "every author needs the second feasible coverage source"
            );
        }
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
        routing: ReadRouting::Auto,
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

/// An authorless demand sharing a skeleton with an author-bearing one keeps
/// its own reach.
///
/// This is the falsifier for the bag merge. `Auto` collapsed two routing
/// values into one, so `bag.entry(routing)` now puts what used to be the
/// outbox lane and the supplemental lane in ONE partition. That partition is
/// the coalescing unit, so the merge is observable: entries that previously
/// could not touch each other now can.
///
/// The dangerous case is not the merge itself, it is the author union. Both
/// atoms below hash to the same author-erased `Skeleton`, so they land in one
/// group; the group's author union is `{alice}`; and routing the additive
/// lanes under `skeleton.with_authors({alice})` would silently narrow "kind:1
/// from anyone" into "kind:1 from alice". The app asked for everyone and
/// would receive one author, with nothing anywhere reporting a loss.
///
/// Break it by replacing `lane_filter`'s `group.unbounded` branch in
/// `Router::compile` with `skeleton.with_authors(authors.clone())`, or by
/// passing `false` for `unbounded` into the lane `auto_ownership` call: the
/// authorless filter stops reaching the wire and its coverage claim
/// disappears with it.
#[test]
fn an_authorless_demand_is_not_narrowed_by_an_author_bearing_sibling() {
    let alice = author();
    let app = test_relay(9);
    let facts = FixtureRoutingFacts::new()
        .with_author_routes(alice, [test_relay(0)], [])
        .with_operator_app([app.clone()]);
    let mut router = router();

    // Same kind, so the SAME author-erased skeleton, so one `Auto` group.
    let authorless = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Auto,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    router.compile(
        &BTreeSet::from([outbox(1, &[alice]), authorless.clone()]),
        &facts,
        10,
    );

    // The app lane carries the whole group. Its filter must still be
    // author-unbound: that is the only shape covering the authorless demand,
    // and it supersets the author-bearing one rather than losing it.
    let app_reqs = &router.plan().reqs[&session(app)];
    assert!(
        app_reqs
            .iter()
            .any(|request| request.filter.authors.is_none()),
        "the operator lane must carry the author-unbound skeleton, not the \
         group's author union: {app_reqs:#?}"
    );

    // And the authorless demand still OWNS what it asked for -- a merge that
    // routed the right filter but dropped the owner would leave this demand
    // with no request to close, which is the refcount half of the same bug.
    let owner = nmp_router::DemandKey::for_atom(&authorless);
    assert!(
        app_reqs
            .iter()
            .any(|request| request.owner_demands.contains(&owner)),
        "the authorless demand must own the request its selection reaches the \
         wire through: {app_reqs:#?}"
    );

    // The outbox lane is untouched by the merge: alice's own relay still
    // carries her author-bearing coverage route.
    let outbox_reqs = &router.plan().reqs[&session(test_relay(0))];
    assert!(
        outbox_reqs.iter().any(|request| {
            request
                .provenance
                .iter()
                .any(|route| route.lane == Lane::AuthorOutbound)
        }),
        "merging the partitions must not cost the outbox lane its route: {outbox_reqs:#?}"
    );
}

/// The outbox-author refcount balances across the collapse.
///
/// `active_outbox_authors` is incremented at two sites and decremented at
/// two more, with a fifth that rebuilds it from scratch. Before the collapse
/// each site asked its own version of "does this atom solve outboxes?";
/// five copies of one predicate is exactly the shape that lets counting and
/// decrementing drift
/// apart, and a demand whose count never reaches zero is a demand whose
/// request never closes.
///
/// They now all call ONE function, `route::outbox_authors`, so they cannot
/// disagree by construction. This pins the consequence: after admitting and
/// then withdrawing a mixed cohort, the census returns to zero.
///
/// The `Explicit` atom is the interesting member. It names a DIFFERENT author
/// from the `Auto` one but consults no outbox, so it must contribute nothing
/// in either direction. A different author is load-bearing here: with the
/// same one, counting `Explicit` too would leave the census length unchanged
/// and this test would pass against a broken predicate. Counting it on the
/// way in only would leak; on the way out only would underflow, which
/// `checked_sub` turns into a panic rather than a silent zero.
#[test]
fn the_outbox_author_refcount_returns_to_zero_across_auto_and_explicit() {
    let alice = author();
    let bob = author();
    let facts = FixtureRoutingFacts::new()
        .with_author_routes(alice, [test_relay(0)], [])
        .with_author_routes(bob, [test_relay(1)], []);
    let mut router = router();

    let auto = outbox(1, &[alice]);
    let explicit = exact(
        ConcreteFilter {
            kinds: Some(BTreeSet::from([2u16])),
            authors: Some(BTreeSet::from([bob.to_hex()])),
            ..ConcreteFilter::default()
        },
        BTreeSet::from([test_relay(5)]),
    );

    router.compile(
        &BTreeSet::from([auto.clone(), explicit.clone()]),
        &facts,
        10,
    );
    router.activate(auto.clone());
    router.activate(explicit.clone());
    assert_eq!(
        router.ownership_census().active_outbox_authors,
        1,
        "only the Auto atom chases an outbox: bob is named by an Explicit          demand and must never be counted as an outbox author"
    );

    router.withdraw([auto, explicit], 10);
    assert_eq!(
        router.ownership_census().active_outbox_authors,
        0,
        "every counted author must be released; a leftover count is a request \
         that can never close"
    );
}

/// An author-bearing `Auto` group never reaches a hint relay OUTSIDE the
/// coverage solve.
///
/// Hints already reach such a group: `add_projected_candidates` enters them
/// as per-author candidates, where they compete for the k=2 slots and earn
/// coverage like any other relay. Routing them a second time, directly,
/// would give a hinted relay a REQ outside the solve and outside coverage —
/// and because `routing_evidence` is unioned across the group, one member's
/// `nevent` hint would drag every sibling's filter along with it.
///
/// The discriminator is exact rather than positional: a hint relay CHOSEN BY
/// THE SOLVE carries `RouteKind::Coverage`, while the direct lane mints
/// `RouteKind::Supplemental`. So this asserts on the pair, not on whether the
/// relay appears at all — the solver is free to pick the hint relay on merit,
/// and that is not what this forbids.
///
/// Break it by making the `if group.unbounded` guard around
/// `provenance_for_projected` in `Router::compile` unconditional.
#[test]
fn an_author_bearing_group_never_reaches_a_hint_relay_outside_the_solve() {
    let alice = author();
    let hint = test_relay(7);
    let facts = FixtureRoutingFacts::new()
        .with_author_routes(alice, [test_relay(0), test_relay(1)], [])
        .with_operator_app([test_relay(8)]);
    let mut router = router();

    let mut hinted = outbox(1, &[alice]);
    hinted.routing_evidence = BTreeSet::from([nmp_grammar::RoutingEvidence {
        relay: hint.clone(),
        origin: nmp_grammar::RoutingEvidenceKind::Hint,
    }]);

    router.compile(&BTreeSet::from([hinted]), &facts, 10);

    let smuggled: Vec<_> = router
        .plan()
        .reqs
        .iter()
        .flat_map(|(session, reqs)| reqs.iter().map(move |request| (session, request)))
        .flat_map(|(session, request)| {
            request
                .provenance
                .iter()
                .map(move |route| (session.relay.clone(), route))
        })
        .filter(|(_, route)| {
            route.lane == Lane::Hint && route.route_kind == RouteKind::Supplemental
        })
        .collect();

    assert!(
        smuggled.is_empty(),
        "an author-bearing group's hints belong to the solve; a Supplemental \
         hint route is the direct lane leaking into a group that never had \
         it: {smuggled:#?}"
    );
}

/// The other half of the same rule: an UNBOUND group's hints DO get routed
/// directly, because they have nowhere else to go.
///
/// An unbound selection resolves no authors, so `add_projected_candidates`
/// has no author to key its candidates on and the hint would simply vanish.
/// This is the entire case the direct lane exists for, and narrowing it to
/// `unbounded` must not narrow it to nothing.
#[test]
fn an_unbound_group_routes_its_hints_directly() {
    let hint = test_relay(7);
    let facts = FixtureRoutingFacts::new().with_operator_app([test_relay(8)]);
    let mut router = router();

    let unbound = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Auto,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::from([nmp_grammar::RoutingEvidence {
            relay: hint.clone(),
            origin: nmp_grammar::RoutingEvidenceKind::Hint,
        }]),
    };

    router.compile(&BTreeSet::from([unbound]), &facts, 10);

    let reqs = router
        .plan()
        .reqs
        .get(&session(hint))
        .expect("an unbound group's hint relay must be asked -- nothing else would ask it");
    assert!(reqs.iter().any(|request| request
        .provenance
        .iter()
        .any(|route| route.lane == Lane::Hint)));
}

/// The same non-narrowing rule, but through `admit` rather than `compile`.
///
/// `compile` sees the whole demand set at once, so the group is assembled
/// complete and `unbounded` is known before any lane runs. `admit` does not:
/// it compiles one cohort against an EMPTY incumbent namespace and appends
/// the result to a plan that already has requests. An authorless atom
/// arriving after its author-bearing sibling therefore forms its group with
/// no knowledge of the sibling, and vice versa.
///
/// Both arrival orders are asserted, because they are different code paths
/// and only one of them was covered by the compile-time guard.
#[test]
fn an_authorless_atom_keeps_its_reach_whichever_order_admission_sees_it() {
    let alice = author();
    let app = test_relay(9);

    let authorless = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Auto,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let bearing = outbox(1, &[alice]);

    // Three shapes. The two sequential ones are structurally immune -- `admit`
    // compiles each cohort against an empty incumbent namespace, so a lone
    // authorless atom forms its own group and never meets the sibling's
    // author set. The THIRD is the one that can actually bite: both atoms in
    // ONE cohort are grouped exactly as `compile` would group them.
    let cohorts: [(Vec<ContextualAtom>, &str); 3] = [
        (
            vec![bearing.clone(), authorless.clone()],
            "author-bearing cohort, then authorless",
        ),
        (
            vec![authorless.clone(), bearing.clone()],
            "authorless cohort, then author-bearing",
        ),
        (vec![], "both in one cohort"),
    ];
    for (sequence, order) in cohorts {
        let facts = FixtureRoutingFacts::new()
            .with_author_routes(alice, [test_relay(0)], [])
            .with_operator_app([app.clone()]);
        let mut router = router();

        if sequence.is_empty() {
            router.admit(
                &BTreeSet::from([bearing.clone(), authorless.clone()]),
                &facts,
                10,
            );
        } else {
            for atom in sequence {
                router.admit(&BTreeSet::from([atom]), &facts, 10);
            }
        }

        let owner = nmp_router::DemandKey::for_atom(&authorless);
        let serving: Vec<_> = router
            .plan()
            .reqs
            .values()
            .flatten()
            .filter(|request| request.owner_demands.contains(&owner))
            .collect();

        assert!(
            !serving.is_empty(),
            "{order}: the authorless demand must own some request"
        );
        assert!(
            serving
                .iter()
                .any(|request| request.filter.authors.is_none()),
            "{order}: the authorless demand must be served by an author-unbound \
             filter -- being merged into a sibling's author set is the silent \
             under-fetch: {serving:#?}"
        );
    }
}
