//! M2 contract test 16 -- THE M2 KILL (`docs/plans/M2-compiler-router-plan.md`
//! §6). Builds a realistic falsifier demand (~300 follows over a realistic,
//! overlapping write-relay distribution: a handful of "big" relays most
//! authors share, plus a wider spread of smaller relays), compiles it
//! DEDUP-ONLY (registry empty) and measures per-relay `wire_sub_count` +
//! the max author-count of any single filter against `RelayLimits`, PRINTS
//! the numbers, then recompiles WITH `AuthorUnion` and re-measures.
//!
//! The kill is pre-committed: with coalescing fully disabled (dedup-only
//! floor), M1's per-author atoms should indeed blow relay sub-count limits
//! on the popular relays (expected, not itself a failure). The kill FIRES
//! only if even `AuthorUnion` -- the one trivially-provable widening rule
//! (test 10) -- fails to bring every relay back within `RelayLimits`. If it
//! fires, that is reported honestly, not hidden.

use std::collections::BTreeSet;

use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, SourceAuthority};
use nmp_router::{
    test_relay, DiscoveryKinds, FixtureDirectory, PubkeyHex, RelayLimits, RelayUrl, Router,
    RuleRegistry,
};

const NUM_AUTHORS: usize = 300;
const POOL_SIZE: usize = 15;
const NUM_BIG_RELAYS: usize = 3;

fn author_hex(i: usize) -> PubkeyHex {
    format!("{i:064}")
}

/// A small, deterministic (no external RNG dependency) "realistic"
/// write-relay distribution: every author's FIRST write relay is one of
/// `NUM_BIG_RELAYS` popular relays (heavy overlap -- most users cluster on
/// a handful of relays in practice); their SECOND is spread evenly across
/// the remaining smaller relays (`step=7` is coprime with
/// `POOL_SIZE - NUM_BIG_RELAYS = 12`, so it cycles through every small
/// relay index over 300 authors rather than degenerating to a few).
fn realistic_directory() -> FixtureDirectory {
    let mut dir = FixtureDirectory::new();
    let small_pool = POOL_SIZE - NUM_BIG_RELAYS;
    for i in 0..NUM_AUTHORS {
        let big = i % NUM_BIG_RELAYS;
        let small = NUM_BIG_RELAYS + (i * 7) % small_pool;
        dir = dir.with_write(author_hex(i), [test_relay(big), test_relay(small)]);
    }
    dir
}

fn falsifier_demand() -> BTreeSet<ContextualAtom> {
    (0..NUM_AUTHORS)
        .map(|i| ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1u16])),
                authors: Some(BTreeSet::from([author_hex(i)])),
                ..ConcreteFilter::default()
            },
            source: SourceAuthority::AuthorOutboxes,
            access: AccessContext::Public,
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
        .map(|(relay, reqs)| (relay.clone(), reqs.len()))
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

fn print_measurement(label: &str, m: &Measurement, limits: &RelayLimits) {
    println!("--- {label} ---");
    let mut sorted = m.per_relay_sub_count.clone();
    sorted.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (relay, count) in &sorted {
        println!(
            "  {relay}: wire_sub_count={count} (limit {})",
            limits.max_subs_per_relay
        );
    }
    println!(
        "  max_filter_authors={} (limit {})",
        m.max_filter_authors, limits.max_filter_authors
    );
}

#[test]
fn kill_measurement_dedup_only_within_relay_limits() {
    let dir = realistic_directory();
    let demand = falsifier_demand();
    let limits = RelayLimits::default();
    let discovery = DiscoveryKinds::default();
    let cap = POOL_SIZE;

    // ---- Tier 1: dedup-only floor (registry EMPTY) ----------------------
    let mut router_dedup_only = Router::new(limits, discovery.clone(), RuleRegistry::dedup_only());
    router_dedup_only.compile(&demand, &dir, cap);
    let m_dedup = measure(&router_dedup_only);
    print_measurement("dedup-only floor", &m_dedup, &limits);

    let dedup_over_sub_limit = m_dedup
        .per_relay_sub_count
        .iter()
        .any(|(_, c)| *c > limits.max_subs_per_relay);
    println!(
        "dedup-only exceeds max_subs_per_relay on >=1 relay: {dedup_over_sub_limit} (expected: true -- \
         M1 emits per-author atoms, so a relay serving many authors gets one sub per author)"
    );

    // ---- Tier 2: with AuthorUnion ----------------------------------------
    let mut router_with_union = Router::new(limits, discovery, RuleRegistry::default_widen_only());
    router_with_union.compile(&demand, &dir, cap);
    let m_union = measure(&router_with_union);
    print_measurement("with AuthorUnion", &m_union, &limits);

    // ---- The kill verdict, printed honestly ------------------------------
    let union_over_sub_limit: Vec<_> = m_union
        .per_relay_sub_count
        .iter()
        .filter(|(_, c)| *c > limits.max_subs_per_relay)
        .collect();
    let union_over_filter_limit = m_union.max_filter_authors > limits.max_filter_authors;
    let kill_fired = !union_over_sub_limit.is_empty() || union_over_filter_limit;
    println!("KILL VERDICT: fired={kill_fired}");
    if kill_fired {
        println!(
            "  relays still over max_subs_per_relay after AuthorUnion: {:?}",
            union_over_sub_limit
        );
        println!(
            "  max_filter_authors after AuthorUnion: {} (limit {})",
            m_union.max_filter_authors, limits.max_filter_authors
        );
    }

    // ---- Strict-improvement sanity: AuthorUnion must actually help -------
    let total_dedup: usize = m_dedup.per_relay_sub_count.iter().map(|(_, c)| *c).sum();
    let total_union: usize = m_union.per_relay_sub_count.iter().map(|(_, c)| *c).sum();
    println!("total wire_sub_count: dedup-only={total_dedup}, with AuthorUnion={total_union}");
    assert!(
        total_union < total_dedup,
        "AuthorUnion must strictly reduce total wire subscription count"
    );

    // ---- The pre-committed assertion: report the kill, do not hide it ---
    assert!(
        !kill_fired,
        "M2 KILL FIRED: even AuthorUnion coalescing leaves a relay over max_subs_per_relay or a \
         filter over max_filter_authors on this falsifier demand -- per-relay compilation needs \
         redesign (see printed measurement above)"
    );
}
