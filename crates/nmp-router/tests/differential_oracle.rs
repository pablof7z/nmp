//! M2 contract test 12: the differential oracle
//! (`docs/plans/M2-compiler-router-plan.md` §4.4). Wires the REAL resolver
//! (`nmp_resolver::testkit`) into the router: generated demand (a real
//! "my follows" subscription, fanned out by M1 into per-author atoms) is
//! compiled two ways over the SAME injected relay facts --
//!
//! - Path A (dedup-only floor): `RuleRegistry::dedup_only()` -- one WireReq
//!   per (author, relay) pair, no merging.
//! - Path B (coalesced): `RuleRegistry::default_widen_only()` -- AuthorUnion
//!   folds shards sharing a relay into one widened WireReq.
//!
//! Both paths route through the IDENTICAL coverage solve (registry choice
//! only affects the downstream coalesce step), so any delivery difference
//! can only come from coalescing + local re-filter. Assert IDENTICAL
//! per-consumer-atom delivered row sets.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{Binding, ConcreteFilter, Demand, Derived, Filter, IdentityField, Selector};
use nmp_resolver::testkit::{kind1, kind3, Harness};
use nmp_resolver::LiveQuery;
use nostr::filter::MatchEventOptions;
use nostr::{Event, EventId, Keys};

use nmp_router::{test_relay, DiscoveryKinds, FixtureDirectory, RelayUrl, Router, RuleRegistry};

fn my_follows_filter() -> Filter {
    Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            }),
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
    let follows_hex: Vec<String> = follows.iter().map(|k| k.public_key().to_hex()).collect();

    let mut h = Harness::new();
    h.set_active(Some(me.public_key()));
    let (_handle, _open_delta) = h.subscribe(LiveQuery::from_filter(my_follows_filter()));
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
    let dir = FixtureDirectory::shared_pool_mailboxes(&follows_hex, &pool);

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

    let discovery = DiscoveryKinds::default();
    let cap = 10;

    // ---- Act: compile both paths over the identical demand/facts -------
    let mut router_a = Router::new(discovery.clone(), RuleRegistry::dedup_only());
    router_a.compile(&demand_ctx, &dir, cap);

    let mut router_b = Router::new(discovery, RuleRegistry::default_widen_only());
    router_b.compile(&demand_ctx, &dir, cap);

    // ---- Path A: one WireReq per (author, relay), no merge --------------
    let mut delivered_a: BTreeMap<ConcreteFilter, BTreeSet<EventId>> = demand
        .iter()
        .map(|a| (a.clone(), BTreeSet::new()))
        .collect();
    for (relay, reqs) in &router_a.plan().reqs {
        let store = &relay_store[relay];
        for req in reqs {
            for prov in &req.provenance {
                for author in &prov.covers_authors {
                    if let Some(atom) = demand
                        .iter()
                        .find(|a| a.authors.as_ref() == Some(&BTreeSet::from([author.clone()])))
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
        let store = &relay_store[relay];
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
                        .find(|a| a.authors.as_ref() == Some(&BTreeSet::from([author.clone()])))
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

    // Sanity: path B actually coalesced (fewer WireReqs than path A) --
    // otherwise this oracle wouldn't be exercising coalescing at all.
    let total_reqs_a: usize = router_a.plan().reqs.values().map(|v| v.len()).sum();
    let total_reqs_b: usize = router_b.plan().reqs.values().map(|v| v.len()).sum();
    assert!(
        total_reqs_b < total_reqs_a,
        "path B must coalesce vs path A's floor"
    );
}
