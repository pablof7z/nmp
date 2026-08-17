//! M2 contract test 12: the differential oracle
//! (M2 plan §4.4). Wires the REAL resolver
//! (`nmp_resolver_testkit`) into the router: generated demand (a real
//! "my follows" subscription, fanned out by M1 into per-author atoms) is
//! compiled two ways over the SAME injected relay facts --
//!
//! - Path A (dedup-only floor): `RuleRegistry::dedup_only()` -- one WireReq
//!   per (author, relay) pair, no merging.
//! - Path B (coalesced): `RuleRegistry::default_widen_only()` -- StructuralUnion
//!   folds shards sharing a relay into one widened WireReq.
//!
//! Both paths route through the IDENTICAL coverage solve (registry choice
//! only affects the downstream coalesce step), so any delivery difference
//! can only come from coalescing + local re-filter. Assert IDENTICAL
//! per-consumer-atom delivered row sets.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{Binding, ConcreteFilter, Demand, Derived, Filter, IdentityField, Selector};
use nmp_resolver_testkit::{kind1, kind3, Harness};
use nostr::filter::MatchEventOptions;
use nostr::{Event, EventId, Keys};

use nmp_router::{RelayUrl, Router, RuleRegistry};
use nmp_router_testkit::{test_relay, FixtureRoutingFacts};

fn my_follows_filter() -> Filter {
    Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Demand::author_outboxes(Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            })
            .expect("the selection binds `authors`"),
            project: Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    }
}

fn matches(cf: &ConcreteFilter, e: &Event) -> bool {
    cf.to_nostr().match_event(e, MatchEventOptions::new())
}

/// Deliver `wire_events` (what a relay returned for the WIRE filter that
/// carried `atom`) filtered down to exactly `atom`'s own matches -- the
/// mandatory local re-filter.
fn local_deliver(wire_events: &[Event], atom: &ConcreteFilter) -> BTreeSet<EventId> {
    wire_events
        .iter()
        .filter(|e| matches(atom, e))
        .map(|e| e.id)
        .collect()
}

#[test]
fn differential_oracle_identical_delivery() {
    // ---- Arrange: real resolver-generated demand ------------------------
    let me = Keys::generate();
    let follows: Vec<Keys> = (0..4).map(|_| Keys::generate()).collect();

    let mut h = Harness::new();
    h.set_active(Some(me.public_key()));
    let (_handle, _open_delta) = h.subscribe(
        Demand::author_outboxes(my_follows_filter()).expect("the selection binds `authors`"),
    );
    let follow_pks: Vec<_> = follows.iter().map(|k| k.public_key()).collect();
    h.deliver(vec![kind3(&me, &follow_pks, 100)]);

    let demand = h.demand();
    let demand_ctx = h.demand_with_context();
    // One per-author atom per follow (the kind:1 fan-out) PLUS the inner
    // kind:3 atom itself (the follow-list subscription that makes the
    // fan-out reactive) -- `me` has no write relays in `dir` below, so that
    // atom simply never routes anywhere and contributes nothing to either
    // path's delivery (still exercised as a no-op consistency check).
    assert_eq!(demand.len(), follows.len() + 1);

    // ---- Arrange: injected relay facts + a per-relay event universe ----
    // Overlapping relay pool -- forces multiple authors to share a relay,
    // which is exactly what needs coalescing.
    let pool = vec![test_relay(0), test_relay(1), test_relay(2)];
    let dir = FixtureRoutingFacts::shared_pool_mailboxes(&follow_pks, &pool);

    let mut relay_store: BTreeMap<RelayUrl, Vec<Event>> = BTreeMap::new();
    for relay in &pool {
        let mut events = Vec::new();
        // Each follow contributes a matching kind:1 note...
        for follow in &follows {
            events.push(kind1(follow, "hello", 200));
        }
        // ...plus noise: an unrelated author's kind:1 note (must never be
        // delivered to any consumer) and a non-matching kind from a follow.
        let stranger = Keys::generate();
        events.push(kind1(&stranger, "noise", 201));
        relay_store.insert(relay.clone(), events);
    }

    let cap = 10;

    // ---- Act: compile both paths over the identical demand/facts -------
    let mut router_a = Router::new(RuleRegistry::dedup_only());
    router_a.compile(&demand_ctx, &dir, cap);

    let mut router_b = Router::new(RuleRegistry::default_widen_only());
    router_b.compile(&demand_ctx, &dir, cap);

    // ---- Path A: one WireReq per (author, relay), no merge --------------
    let mut delivered_a: BTreeMap<ConcreteFilter, BTreeSet<EventId>> = demand
        .iter()
        .map(|a| (a.clone(), BTreeSet::new()))
        .collect();
    for (relay, reqs) in &router_a.plan().reqs {
        let store = &relay_store[&relay.relay];
        for req in reqs {
            for prov in &req.provenance {
                for author in &prov.covers_authors {
                    if let Some(atom) = demand
                        .iter()
                        .find(|a| a.authors.as_ref() == Some(&BTreeSet::from([author.to_hex()])))
                    {
                        let wire_events: Vec<Event> = store
                            .iter()
                            .filter(|e| matches(&req.filter, e))
                            .cloned()
                            .collect();
                        delivered_a
                            .get_mut(atom)
                            .unwrap()
                            .extend(local_deliver(&wire_events, atom));
                    }
                }
            }
        }
    }

    // ---- Path B: coalesced, widened wire filters + mandatory re-filter -
    let mut delivered_b: BTreeMap<ConcreteFilter, BTreeSet<EventId>> = demand
        .iter()
        .map(|a| (a.clone(), BTreeSet::new()))
        .collect();
    for (relay, reqs) in &router_b.plan().reqs {
        let store = &relay_store[&relay.relay];
        for req in reqs {
            let wire_events: Vec<Event> = store
                .iter()
                .filter(|e| matches(&req.filter, e))
                .cloned()
                .collect();
            for prov in &req.provenance {
                for author in &prov.covers_authors {
                    if let Some(atom) = demand
                        .iter()
                        .find(|a| a.authors.as_ref() == Some(&BTreeSet::from([author.to_hex()])))
                    {
                        delivered_b
                            .get_mut(atom)
                            .unwrap()
                            .extend(local_deliver(&wire_events, atom));
                    }
                }
            }
        }
    }

    // ---- Assert: IDENTICAL per-consumer delivered row sets --------------
    assert_eq!(delivered_a, delivered_b);

    // Sanity: the oracle actually exercised something (non-trivial
    // delivery), and noise events were never delivered to anyone.
    let all_delivered: BTreeSet<EventId> = delivered_a.values().flatten().cloned().collect();
    assert!(!all_delivered.is_empty());
    for events in relay_store.values() {
        let noise_id = events.last().unwrap().id;
        assert!(
            !all_delivered.contains(&noise_id),
            "noise must never be delivered"
        );
    }

    // Sanity: the author axis was actually JOINED -- otherwise this oracle
    // would be comparing two identical fan-outs and proving nothing.
    //
    // This used to assert `total_reqs_b < total_reqs_a`, i.e. that path B's
    // coalescer beat path A's dedup-only floor. That premise no longer holds
    // and the change is deliberate (#937): `Router::compile` now emits ONE bag
    // entry per (relay, skeleton) carrying every author that relay was solved
    // for, instead of one per (author, relay) route. The join happens in
    // ROUTING, before either registry runs, so `dedup_only` is no longer a
    // per-author floor on this axis and both paths land on the same count.
    //
    // Asserting `<=` instead would have kept the test green while proving
    // nothing at all, so assert the property that actually matters now: the
    // plan carries strictly fewer requests than there are (author, relay)
    // routes. Provenance is retained per route, so that count is still exact.
    //
    // What this oracle still proves is unchanged and is the reason it exists:
    // `delivered_a == delivered_b` above -- joining authors, by whichever
    // mechanism, does not alter any consumer's delivered rows.
    for (label, router) in [("A", &router_a), ("B", &router_b)] {
        let total_reqs: usize = router.plan().reqs.values().map(|v| v.len()).sum();
        let total_routes: usize = router
            .plan()
            .reqs
            .values()
            .flat_map(|reqs| reqs.iter())
            .map(|req| req.provenance.len())
            .sum();
        assert!(
            total_reqs < total_routes,
            "path {label} must join the author axis: {total_reqs} request(s) for \
             {total_routes} (author, relay) route(s)"
        );
    }
}
