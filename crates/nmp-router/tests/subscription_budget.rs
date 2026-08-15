//! The per-relay concurrent-subscription budget (#931), measured against the
//! real router.
//!
//! Relays cap concurrent subscriptions — 20 at nos.lol and relay.primal.net,
//! 50 at nostr.wine and purplepag.es, 200 at relay.damus.io — while accepting
//! filter arrays of 500 values without complaint. Two of the eight relays
//! measured for this issue (relay.nostr.band, relay.snort.social) publish NO
//! NIP-11 document at all, and those relays must keep working exactly as they
//! did.
//!
//! The contract this file pins, in one sentence: an ADVERTISED budget is
//! enforced and every subscription it removes is visible as `limited`
//! coverage plus a per-session shortfall; an UNADVERTISED relay is
//! unbudgeted, because absence is not a number.
//!
//! Run narrated with:
//! `cargo test -p nmp-router --test subscription_budget -- --nocapture`

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    AccessContext, ConcreteFilter, ContextualAtom, IndexedTagName, RelaySessionKey, SourceAuthority,
};
use nmp_router::{
    AdvertisedRelayLimits, CompileBudget, DemandKey, RelayUrl, Router, RuleRegistry, WireOp,
};
use nmp_router_testkit::FixtureRoutingFacts;

/// Well clear of anything these fixtures plan: this file is about the
/// per-relay SUBSCRIPTION budget, never the whole-demand relay ceiling.
const RELAY_CAP: usize = 64;

fn hub() -> RelayUrl {
    RelayUrl::parse("wss://hub.example.com").unwrap()
}

fn session() -> RelaySessionKey {
    RelaySessionKey::public(hub())
}

fn relays() -> BTreeSet<RelayUrl> {
    BTreeSet::from([hub()])
}

/// A pinned atom carrying one `#d` value, with an optional `limit`.
///
/// A `limit` is what makes two otherwise-identical atoms UNMERGEABLE — it
/// caps the result count rather than the predicate, so unioning would
/// silently under-fetch (`coalesce.rs::neither_limited`). That is the honest
/// way to hold N distinct subscriptions open against one relay, which is
/// what a subscription budget is about.
fn atom(value: &str, limit: Option<usize>) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([39_000])),
            tags: BTreeMap::from([(
                IndexedTagName::new('d').unwrap(),
                BTreeSet::from([value.to_string()]),
            )]),
            limit,
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(relays()),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

/// `n` mutually unmergeable atoms — one live subscription each.
fn unmergeable(n: usize) -> BTreeSet<ContextualAtom> {
    (0..n)
        .map(|i| atom(&format!("group-{i}"), Some(10)))
        .collect()
}

/// `n` atoms differing in exactly one array component — the collapse's own
/// shape, one subscription for the lot.
fn collapsing(n: usize) -> BTreeSet<ContextualAtom> {
    (0..n).map(|i| atom(&format!("group-{i}"), None)).collect()
}

fn budget(max_subscriptions: Option<usize>) -> CompileBudget {
    CompileBudget::with_relay_cap(RELAY_CAP).advertising(
        hub(),
        AdvertisedRelayLimits {
            max_subscriptions,
            max_subid_length: None,
        },
    )
}

fn router() -> Router {
    Router::new(RuleRegistry::default_widen_only())
}

fn live_subs(router: &Router) -> usize {
    router.plan().reqs.values().map(|reqs| reqs.len()).sum()
}

/// A relay that publishes NO NIP-11 document is not budgeted at all.
///
/// This is the fail-open half of the ruling, and it is load-bearing:
/// relay.nostr.band and relay.snort.social publish nothing, and inventing a
/// number for them would drop demand they never refused. Absence is not a
/// number.
#[test]
fn an_unadvertised_relay_carries_every_subscription() {
    let dir = FixtureRoutingFacts::new();
    let mut r = router();
    let demand = unmergeable(30);

    r.compile(&demand, &dir, CompileBudget::with_relay_cap(RELAY_CAP));

    assert_eq!(
        live_subs(&r),
        30,
        "an unadvertised relay must not be capped"
    );
    assert!(
        r.plan().limited_demands.is_empty(),
        "nothing may be reported as limited when no budget was advertised"
    );
    assert!(r.plan().subscription_shortfalls.is_empty());
    let diag = r.diagnostics().per_session.get(&session()).unwrap();
    assert_eq!(diag.wire_sub_count, 30);
    assert_eq!(
        diag.subscription_budget, None,
        "diagnostics must say the budget is UNKNOWN, not fabricate one"
    );
    assert_eq!(diag.subscriptions_refused, 0);
}

/// An advertised budget binds, and every subscription it removes is
/// reported — as exact limited demands (which `plan_is_fresh_for` refuses
/// to call fresh) and as a per-session shortfall. Silent truncation is the
/// one outcome this must never be.
#[test]
fn an_advertised_budget_refuses_the_excess_and_says_so() {
    let dir = FixtureRoutingFacts::new();
    let mut r = router();
    let demand = unmergeable(5);

    r.compile(&demand, &dir, budget(Some(2)));

    assert_eq!(live_subs(&r), 2, "the advertised budget of 2 must bind");

    let refused_keys: Vec<_> = demand
        .iter()
        .map(DemandKey::for_atom)
        .filter(|key| r.plan().limited_demands.contains(key))
        .collect();
    assert_eq!(
        refused_keys.len(),
        3,
        "every atom whose subscription was refused must be reported as limited"
    );

    let shortfall = r
        .plan()
        .subscription_shortfalls
        .get(&session())
        .copied()
        .expect("a bound budget must record a per-session shortfall");
    assert_eq!(shortfall.budget, 2);
    assert_eq!(shortfall.planned, 5);
    assert_eq!(shortfall.refused, 3);

    let diag = r.diagnostics().per_session.get(&session()).unwrap();
    assert_eq!(diag.wire_sub_count, 2);
    assert_eq!(diag.subscription_budget, Some(2));
    assert_eq!(diag.subscriptions_refused, 3);
}

/// Nothing refused by the budget may reach the wire, and everything that
/// survived must — the plan and the delta agree.
#[test]
fn only_the_surviving_subscriptions_reach_the_wire() {
    let dir = FixtureRoutingFacts::new();
    let mut r = router();

    let delta = r.compile(&unmergeable(5), &dir, budget(Some(2)));

    let reqs: Vec<_> = delta
        .wire
        .ops
        .iter()
        .flat_map(|(_, ops)| ops)
        .filter(|op| matches!(op, WireOp::Req(..)))
        .collect();
    assert_eq!(reqs.len(), 2, "exactly the budget reaches the socket");
}

/// A bound budget must not oscillate. Incumbents — subscriptions the
/// previous plan already carried — outrank newcomers, so a relay at its
/// budget keeps serving what it is already serving instead of swapping one
/// refused demand for another every recompile.
#[test]
fn a_bound_budget_does_not_churn_what_it_already_serves() {
    let dir = FixtureRoutingFacts::new();
    let mut r = router();
    let first = unmergeable(3);
    r.compile(&first, &dir, budget(Some(2)));
    let served: BTreeSet<_> = r
        .plan()
        .reqs
        .values()
        .flatten()
        .map(|req| req.filter.clone())
        .collect();

    // More demand arrives at an already-saturated relay.
    let mut grown = first.clone();
    grown.extend(unmergeable(6).into_iter().skip(3));
    let delta = r.compile(&grown, &dir, budget(Some(2)));

    assert_eq!(live_subs(&r), 2);
    let still_served: BTreeSet<_> = r
        .plan()
        .reqs
        .values()
        .flatten()
        .map(|req| req.filter.clone())
        .collect();
    assert_eq!(
        served, still_served,
        "an incumbent subscription must not be evicted for a newcomer"
    );
    assert!(
        delta.wire.ops.is_empty(),
        "a saturated relay whose served set is unchanged must emit no wire ops, \
         got {:?}",
        delta.wire.ops
    );
}

/// Recompiling identical demand under an identical budget is a no-op, the
/// same as it is with no budget at all.
#[test]
fn a_bound_budget_is_idempotent_across_recompiles() {
    let dir = FixtureRoutingFacts::new();
    let mut r = router();
    let demand = unmergeable(5);
    r.compile(&demand, &dir, budget(Some(2)));
    let delta = r.compile(&demand, &dir, budget(Some(2)));
    assert!(delta.wire.ops.is_empty(), "got {:?}", delta.wire.ops);
}

/// The sequencing claim from the issue, made a test: AFTER the collapse
/// (#930), a realistic catalog is one subscription per host, so a budget of
/// 20 is a guard rail rather than a guillotine. Before it, this same demand
/// was 300 subscriptions and a budget would have dropped 280 of them.
#[test]
fn a_collapsed_catalog_of_three_hundred_stays_inside_a_budget_of_twenty() {
    let dir = FixtureRoutingFacts::new();
    let mut r = router();

    r.compile(&collapsing(300), &dir, budget(Some(20)));

    assert_eq!(
        live_subs(&r),
        1,
        "300 values collapse to one subscription carrying all of them"
    );
    assert!(r.plan().limited_demands.is_empty());
    assert!(r.plan().subscription_shortfalls.is_empty());
}

/// A relay advertising 20 and a relay advertising 200 plan IDENTICALLY for
/// realistic demand. The budget is a bound, not a shaping input: it may only
/// ever remove, never rearrange.
#[test]
fn twenty_and_two_hundred_plan_the_same_realistic_catalog() {
    let dir = FixtureRoutingFacts::new();
    let demand = collapsing(300);

    let mut small = router();
    small.compile(&demand, &dir, budget(Some(20)));
    let mut large = router();
    large.compile(&demand, &dir, budget(Some(200)));

    let filters = |r: &Router| -> Vec<ConcreteFilter> {
        r.plan()
            .reqs
            .values()
            .flatten()
            .map(|req| req.filter.clone())
            .collect()
    };
    assert_eq!(filters(&small), filters(&large));
    assert!(small.plan().subscription_shortfalls.is_empty());
    assert!(large.plan().subscription_shortfalls.is_empty());
}

/// A relay advertising zero concurrent subscriptions cannot be planned at
/// all. That is a whole-session refusal, so it joins `refused_sessions` —
/// the same evidence the whole-demand relay ceiling uses — and the session
/// is absent from the plan by construction.
#[test]
fn a_budget_of_zero_refuses_the_whole_session() {
    let dir = FixtureRoutingFacts::new();
    let mut r = router();
    let demand = unmergeable(3);

    r.compile(&demand, &dir, budget(Some(0)));

    assert_eq!(live_subs(&r), 0);
    assert!(!r.plan().reqs.contains_key(&session()));
    assert!(r.plan().refused_sessions.contains(&session()));
    for atom in &demand {
        assert!(
            r.plan()
                .limited_demands
                .contains(&DemandKey::for_atom(atom)),
            "a session refused outright limits every atom it would have served"
        );
    }
    assert_eq!(r.diagnostics().sessions_refused_by_subscription_budget, 1);
}

/// `max_subid_length` is a DIAGNOSTIC, never an input to identity. NMP's
/// wire ids are fixed 64-hex-char strings (exactly NIP-01's cap), so a relay
/// advertising less would reject every REQ we send it — and until now
/// nothing would have noticed.
#[test]
fn a_subscription_id_length_below_ours_is_reported() {
    let dir = FixtureRoutingFacts::new();
    let mut r = router();
    let limits = |max_subid_length| {
        CompileBudget::with_relay_cap(RELAY_CAP).advertising(
            hub(),
            AdvertisedRelayLimits {
                max_subscriptions: None,
                max_subid_length,
            },
        )
    };

    r.compile(&collapsing(2), &dir, limits(Some(32)));
    let diag = r.diagnostics().per_session.get(&session()).unwrap();
    assert_eq!(diag.subid_length_limit, Some(32));
    assert!(
        diag.subid_length_rejects_our_ids,
        "32 < 64 means every REQ we send is rejected"
    );

    // nostr.wine advertises 71, and 64 is NIP-01's own cap: both fine.
    r.compile(&collapsing(2), &dir, limits(Some(71)));
    let diag = r.diagnostics().per_session.get(&session()).unwrap();
    assert!(!diag.subid_length_rejects_our_ids);
    r.compile(&collapsing(2), &dir, limits(Some(64)));
    let diag = r.diagnostics().per_session.get(&session()).unwrap();
    assert!(!diag.subid_length_rejects_our_ids);

    // And an unadvertised relay claims nothing.
    r.compile(&collapsing(2), &dir, limits(None));
    let diag = r.diagnostics().per_session.get(&session()).unwrap();
    assert_eq!(diag.subid_length_limit, None);
    assert!(!diag.subid_length_rejects_our_ids);
}

/// The advertised subscription-id length must never move a wire id. Ids are
/// allocated tokens; a NIP-11 document refreshes, and a mutable derivation
/// input is identity instability (`identity-grouping-and-limits.md` §6).
#[test]
fn advertised_limits_never_move_an_established_wire_id() {
    let dir = FixtureRoutingFacts::new();
    let mut r = router();
    let demand = collapsing(3);

    r.compile(&demand, &dir, budget(Some(20)));
    let before: Vec<_> = r
        .plan()
        .reqs
        .values()
        .flatten()
        .map(|req| req.sub_id.clone())
        .collect();

    // The relay re-publishes its document with different numbers.
    let delta = r.compile(
        &demand,
        &dir,
        CompileBudget::with_relay_cap(RELAY_CAP).advertising(
            hub(),
            AdvertisedRelayLimits {
                max_subscriptions: Some(200),
                max_subid_length: Some(71),
            },
        ),
    );
    let after: Vec<_> = r
        .plan()
        .reqs
        .values()
        .flatten()
        .map(|req| req.sub_id.clone())
        .collect();

    assert_eq!(
        before, after,
        "a refreshed document must not rename anything"
    );
    assert!(delta.wire.ops.is_empty(), "nor cost a single wire op");
}

/// A relaxed budget re-admits what it refused, in place: the atoms that were
/// `limited` stop being limited and reach the wire.
#[test]
fn relaxing_the_budget_re_admits_the_refused_demand() {
    let dir = FixtureRoutingFacts::new();
    let mut r = router();
    let demand = unmergeable(5);

    r.compile(&demand, &dir, budget(Some(2)));
    assert_eq!(live_subs(&r), 2);

    r.compile(&demand, &dir, budget(Some(20)));
    assert_eq!(live_subs(&r), 5);
    assert!(r.plan().limited_demands.is_empty());
    assert!(r.plan().subscription_shortfalls.is_empty());
}

/// A `usize` still names the whole-demand relay ceiling on its own — the
/// budget carrier is additive, and every caller that only has a relay cap
/// keeps saying exactly that.
#[test]
fn a_bare_relay_cap_is_still_a_whole_budget() {
    let dir = FixtureRoutingFacts::new();
    let mut r = router();
    r.compile(&unmergeable(4), &dir, RELAY_CAP);
    assert_eq!(live_subs(&r), 4);
    assert!(r.plan().subscription_shortfalls.is_empty());
}
