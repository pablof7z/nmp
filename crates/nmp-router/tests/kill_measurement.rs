//! M2 contract test 16 -- THE M2 KILL (`docs/plans/M2-compiler-router-plan.md`
//! §6). Builds a realistic falsifier demand (~300 follows over a realistic,
//! overlapping write-relay distribution: a handful of "big" relays most
//! authors share, plus a wider spread of smaller relays), compiles it
//! DEDUP-ONLY (registry empty) and measures per-relay `wire_sub_count` +
//! the max author-count of any single filter against this test's own
//! admission thresholds (`MAX_SUBS_PER_RELAY` / `MAX_FILTER_AUTHORS`), PRINTS
//! the numbers, then recompiles WITH the union rule and re-measures.
//!
//! The kill is pre-committed: with coalescing fully disabled (dedup-only
//! floor), M1's per-author atoms should indeed blow relay sub-count limits
//! on the popular relays (expected, not itself a failure). The kill FIRES
//! only if even the author union -- the trivially-provable widening case
//! (test 10) -- fails to bring every relay back within those thresholds. If it
//! fires, that is reported honestly, not hidden.

use std::collections::BTreeSet;

use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, SourceAuthority};
use nmp_router::{test_relay, FixtureRoutingFacts, PublicKey, RelayUrl, Router, RuleRegistry};
use nostr::{Keys, SecretKey};

const NUM_AUTHORS: usize = 300;
const POOL_SIZE: usize = 15;
const NUM_BIG_RELAYS: usize = 3;

/// The relay-admission thresholds this measurement asserts the compiled plan
/// stays within. Previously read off `RelayLimits` (deleted in #123 as a
/// never-enforced router contract); the kill measurement is the ONLY thing
/// that ever consumed them, so the values now live here as the test's own
/// v1-evidence expectations (relays accept large author arrays but cap
/// concurrent subscriptions).
const MAX_SUBS_PER_RELAY: usize = 20;
const MAX_FILTER_AUTHORS: usize = 1_000;

fn author(i: usize) -> PublicKey {
    let mut bytes = [0u8; 32];
    bytes[0] = 1;
    bytes[24..].copy_from_slice(&(i as u64 + 1).to_be_bytes());
    Keys::new(SecretKey::from_slice(&bytes).unwrap()).public_key()
}

/// A small, deterministic (no external RNG dependency) "realistic"
/// write-relay distribution: every author's FIRST write relay is one of
/// `NUM_BIG_RELAYS` popular relays (heavy overlap -- most users cluster on
/// a handful of relays in practice); their SECOND is spread evenly across
/// the remaining smaller relays (`step=7` is coprime with
/// `POOL_SIZE - NUM_BIG_RELAYS = 12`, so it cycles through every small
/// relay index over 300 authors rather than degenerating to a few).
fn realistic_directory() -> FixtureRoutingFacts {
    let mut dir = FixtureRoutingFacts::new();
    let small_pool = POOL_SIZE - NUM_BIG_RELAYS;
    for i in 0..NUM_AUTHORS {
        let big = i % NUM_BIG_RELAYS;
        let small = NUM_BIG_RELAYS + (i * 7) % small_pool;
        dir = dir.with_author_routes(author(i), [test_relay(big), test_relay(small)], []);
    }
    dir
}

fn falsifier_demand() -> BTreeSet<ContextualAtom> {
    (0..NUM_AUTHORS)
        .map(|i| ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1u16])),
                authors: Some(BTreeSet::from([author(i).to_hex()])),
                ..ConcreteFilter::default()
            },
            source: SourceAuthority::AuthorOutboxes,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        })
        .collect()
}

struct Measurement {
    per_relay_sub_count: Vec<(RelayUrl, usize)>,
    max_filter_authors: usize,
}

fn measure(router: &Router) -> Measurement {
    let per_relay_sub_count: Vec<(RelayUrl, usize)> = router
        .plan()
        .reqs
        .iter()
        .map(|(session, reqs)| (session.relay.clone(), reqs.len()))
        .collect();
    let max_filter_authors = router
        .plan()
        .reqs
        .values()
        .flatten()
        .map(|req| req.filter.authors.as_ref().map(|a| a.len()).unwrap_or(0))
        .max()
        .unwrap_or(0);
    Measurement {
        per_relay_sub_count,
        max_filter_authors,
    }
}

fn print_measurement(label: &str, m: &Measurement) {
    println!("--- {label} ---");
    let mut sorted = m.per_relay_sub_count.clone();
    sorted.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (relay, count) in &sorted {
        println!("  {relay}: wire_sub_count={count} (limit {MAX_SUBS_PER_RELAY})");
    }
    println!(
        "  max_filter_authors={} (limit {MAX_FILTER_AUTHORS})",
        m.max_filter_authors
    );
}

#[test]
fn kill_measurement_dedup_only_within_relay_limits() {
    let dir = realistic_directory();
    let demand = falsifier_demand();
    let cap = POOL_SIZE;

    // ---- Tier 1: dedup-only floor (registry EMPTY) ----------------------
    let mut router_dedup_only = Router::new(RuleRegistry::dedup_only());
    router_dedup_only.compile(&demand, &dir, cap);
    let m_dedup = measure(&router_dedup_only);
    print_measurement("dedup-only floor", &m_dedup);

    let dedup_over_sub_limit = m_dedup
        .per_relay_sub_count
        .iter()
        .any(|(_, c)| *c > MAX_SUBS_PER_RELAY);
    println!(
        "dedup-only exceeds max_subs_per_relay on >=1 relay: {dedup_over_sub_limit} (expected: true -- \
         M1 emits per-author atoms, so a relay serving many authors gets one sub per author)"
    );

    // ---- Tier 2: with the union rule -------------------------------------
    let mut router_with_union = Router::new(RuleRegistry::default_widen_only());
    router_with_union.compile(&demand, &dir, cap);
    let m_union = measure(&router_with_union);
    print_measurement("with StructuralUnion", &m_union);

    // ---- The kill verdict, printed honestly ------------------------------
    let union_over_sub_limit: Vec<_> = m_union
        .per_relay_sub_count
        .iter()
        .filter(|(_, c)| *c > MAX_SUBS_PER_RELAY)
        .collect();
    let union_over_filter_limit = m_union.max_filter_authors > MAX_FILTER_AUTHORS;
    let kill_fired = !union_over_sub_limit.is_empty() || union_over_filter_limit;
    println!("KILL VERDICT: fired={kill_fired}");
    if kill_fired {
        println!(
            "  relays still over max_subs_per_relay after coalescing: {:?}",
            union_over_sub_limit
        );
        println!(
            "  max_filter_authors after coalescing: {} (limit {})",
            m_union.max_filter_authors, MAX_FILTER_AUTHORS
        );
    }

    // ---- Where the author join now happens -------------------------------
    //
    // This used to assert `total_union < total_dedup`: that the coalescer
    // strictly beat the dedup-only floor. That premise is gone, deliberately
    // (#937). `Router::compile` now emits ONE bag entry per (relay, skeleton)
    // carrying every author that relay was solved for, rather than one per
    // (author, relay) route, so the author join happens in ROUTING -- before
    // either registry runs -- and the dedup-only "floor" is no longer a
    // per-author fan-out.
    //
    // That is this measurement's own subject matter, so read the numbers
    // rather than the old inequality: the floor being within limits is the
    // #937 fix showing up on 300 authors. A bounded feed could not reach the
    // union rule at all (`neither_limited` refuses any filter carrying a
    // `limit`), so before this change a paginated 300-author feed shipped one
    // REQ per author no matter what the registry said.
    let total_dedup: usize = m_dedup.per_relay_sub_count.iter().map(|(_, c)| *c).sum();
    let total_union: usize = m_union.per_relay_sub_count.iter().map(|(_, c)| *c).sum();
    println!("total wire_sub_count: dedup-only={total_dedup}, coalesced={total_union}");
    assert!(
        total_union <= total_dedup,
        "coalescing must never INCREASE the wire subscription count"
    );

    // The property that actually needs pinning now, and it is stronger than
    // the old one because it holds for BOTH registries: the plan carries
    // strictly fewer subscriptions than there are authors, on a demand that
    // fans out to one atom per author.
    assert!(
        total_dedup < NUM_AUTHORS,
        "the author axis must be joined during routing: {total_dedup} subscription(s) for \
         {NUM_AUTHORS} authors even with coalescing disabled"
    );

    // ---- The pre-committed assertion: report the kill, do not hide it ---
    assert!(
        !kill_fired,
        "M2 KILL FIRED: even the author union leaves a relay over max_subs_per_relay or a \
         filter over max_filter_authors on this falsifier demand -- per-relay compilation needs \
         redesign (see printed measurement above)"
    );
}
