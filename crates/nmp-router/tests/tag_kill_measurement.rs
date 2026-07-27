//! The tag-axis twin of `kill_measurement.rs`, and THE ACCEPTANCE TEST for
//! the structural merge rule.
//!
//! `kill_measurement.rs` asks whether the AUTHOR axis stays inside real relay
//! admission limits once coalescing runs. Its verdict is no-kill: the union
//! brings every relay back under the cap. This file asks the same question of
//! the TAG axis.
//!
//! The thresholds encode the operational reality: relays accept large value
//! arrays but cap CONCURRENT SUBSCRIPTIONS. Field evidence puts the
//! subscription cap at roughly 20 (sometimes up to 200), while a filter
//! carrying 500 values is entirely unremarkable.
//!
//! THIS FILE WAS WRITTEN INVERTED, and its own comment said so. Before
//! `nmp_router::StructuralUnion` landed, nothing in the registry merged on
//! tags: `AuthorUnion`/`KindUnion` both required `a.tags == b.tags` and
//! `IdUnion` required both sides to carry `ids`, so this measurement asserted
//! the kill FIRES (300 subscriptions per host against a ceiling of 20) and
//! asserted `dedup_only()` EQUAL to `default_widen_only()`, because the
//! registry was a measured no-op on this axis. Inverting those two
//! assertions -- to `!kill_fired` and to a strict improvement, matching
//! `kill_measurement.rs` -- is the acceptance criterion, and it is what this
//! file now asserts.
//!
//! Run narrated with:
//! `cargo test -p nmp-router --test tag_kill_measurement -- --nocapture`

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, IndexedTagName, SourceAuthority};
use nmp_router::{
    test_relay, DiscoveryKinds, FixtureDirectory, RelayUrl, Router, RuleRegistry,
    MAX_TAG_VALUES_PER_FILTER,
};

/// A realistic mid-size mosaico channel catalog.
const NUM_GROUPS: usize = 300;

/// Group state is pinned to the NIP-29 host set — mosaico pins every
/// observation to its configured relays rather than routing by outbox.
const NUM_HOSTS: usize = 2;

/// Relay admission thresholds, carried over verbatim from
/// `kill_measurement.rs`, which took them from #123's deleted `RelayLimits`.
const MAX_SUBS_PER_RELAY: usize = 20;

const GROUP_STATE_KINDS: [u16; 3] = [39_000, 39_001, 39_002];

fn hosts() -> BTreeSet<RelayUrl> {
    (0..NUM_HOSTS).map(test_relay).collect()
}

fn group_d(i: usize) -> String {
    format!("group-{i:04}")
}

/// One pinned group-state atom per group — exactly what the resolver's
/// cartesian fan-out produces for a derived (or literal) `#d` binding.
fn falsifier_demand() -> BTreeSet<ContextualAtom> {
    (0..NUM_GROUPS)
        .map(|i| ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(GROUP_STATE_KINDS.iter().copied().collect()),
                tags: BTreeMap::from([(
                    IndexedTagName::new('d').unwrap(),
                    BTreeSet::from([group_d(i)]),
                )]),
                ..ConcreteFilter::default()
            },
            source: SourceAuthority::Pinned(hosts()),
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        })
        .collect()
}

struct Measurement {
    per_relay_sub_count: Vec<(RelayUrl, usize)>,
    max_filter_tag_values: usize,
}

fn measure(router: &Router) -> Measurement {
    let mut per_relay_sub_count = Vec::new();
    let mut max_filter_tag_values = 0usize;
    for (session, reqs) in &router.plan().reqs {
        per_relay_sub_count.push((session.relay.clone(), reqs.len()));
        for req in reqs {
            let values = req
                .filter
                .tags
                .get(&IndexedTagName::new('d').unwrap())
                .map(|v| v.len())
                .unwrap_or(0);
            max_filter_tag_values = max_filter_tag_values.max(values);
        }
    }
    Measurement {
        per_relay_sub_count,
        max_filter_tag_values,
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
        "  max_filter_tag_values={} (limit {MAX_TAG_VALUES_PER_FILTER})",
        m.max_filter_tag_values
    );
}

/// THE TAG KILL, inverted.
///
/// One structural rule now rescues the tag axis exactly as it rescues the
/// author axis, because both are instances of the same case: exactly one
/// array component differs, so union it. A realistic catalog compiles to a
/// handful of subscriptions per host carrying large value arrays -- the shape
/// relays are actually provisioned for -- instead of one subscription per
/// value.
///
/// The kill fires only if the coalesced plan STILL leaves a host over
/// `MAX_SUBS_PER_RELAY`, or leaves a filter over
/// `MAX_TAG_VALUES_PER_FILTER`. Reported honestly, not hidden.
#[test]
fn tag_axis_stays_within_relay_subscription_limits_once_coalesced() {
    let dir = FixtureDirectory::new();
    let demand = falsifier_demand();
    let discovery = DiscoveryKinds::default();
    let cap = NUM_HOSTS;

    // ---- Tier 1: dedup-only floor (registry EMPTY) ----------------------
    let mut router_dedup_only = Router::new(discovery.clone(), RuleRegistry::dedup_only());
    router_dedup_only.compile(&demand, &dir, cap);
    let m_dedup = measure(&router_dedup_only);
    print_measurement("dedup-only floor", &m_dedup);

    // ---- Tier 2: the full proven-widening registry -----------------------
    let mut router_with_union = Router::new(discovery, RuleRegistry::default_widen_only());
    router_with_union.compile(&demand, &dir, cap);
    let m_union = measure(&router_with_union);
    print_measurement("with default_widen_only()", &m_union);

    let over_sub_limit: Vec<_> = m_union
        .per_relay_sub_count
        .iter()
        .filter(|(_, c)| *c > MAX_SUBS_PER_RELAY)
        .collect();
    let over_value_limit = m_union.max_filter_tag_values > MAX_TAG_VALUES_PER_FILTER;
    let kill_fired = !over_sub_limit.is_empty() || over_value_limit;

    println!("KILL VERDICT: fired={kill_fired}");
    println!(
        "  {} group(s) over {} host(s) → {} subscription(s) per host, limit {}",
        NUM_GROUPS,
        NUM_HOSTS,
        m_union
            .per_relay_sub_count
            .iter()
            .map(|(_, c)| *c)
            .max()
            .unwrap_or(0),
        MAX_SUBS_PER_RELAY
    );
    println!(
        "  a single coalesced filter carries all {NUM_GROUPS} values, inside \
         the {MAX_TAG_VALUES_PER_FILTER}-value bound"
    );
    println!(
        "  array headroom now in use: the widest filter carries {} value(s) out of {}",
        m_union.max_filter_tag_values, MAX_TAG_VALUES_PER_FILTER
    );

    // ---- The DEFECT, pinned at the floor so the tier-2 number means -----
    // something. Dedup alone cannot touch this axis: every atom is a distinct
    // filter, so 300 groups are 300 subscriptions carrying one value each.
    assert_eq!(
        m_dedup.max_filter_tag_values, 1,
        "the dedup-only floor must still carry exactly one #d value per filter -- \
         if this changes, the tier-2 comparison below is measuring something else"
    );
    assert!(
        m_dedup
            .per_relay_sub_count
            .iter()
            .all(|(_, c)| *c > MAX_SUBS_PER_RELAY),
        "the dedup-only floor must still blow the subscription ceiling on every host"
    );

    // ---- Strict improvement: the registry must actually reduce the axis --
    // This assertion is the INVERSION of the one this file shipped with. It
    // asserted `total_union == total_dedup` -- the registry measured as a
    // no-op on tags -- which is precisely the defect the structural rule
    // removes.
    let total_dedup: usize = m_dedup.per_relay_sub_count.iter().map(|(_, c)| *c).sum();
    let total_union: usize = m_union.per_relay_sub_count.iter().map(|(_, c)| *c).sum();
    println!("total wire_sub_count: dedup-only={total_dedup}, with registry={total_union}");
    assert!(
        total_union < total_dedup,
        "the registry must strictly reduce total wire subscription count on the tag axis"
    );

    // ---- The value bound: chunked, never truncated ----------------------
    assert!(
        m_union.max_filter_tag_values <= MAX_TAG_VALUES_PER_FILTER,
        "a coalesced filter carries {} #d values, over the {MAX_TAG_VALUES_PER_FILTER} bound",
        m_union.max_filter_tag_values
    );

    // ---- Nothing was lost on the way ------------------------------------
    let covered: BTreeSet<String> = router_with_union
        .plan()
        .reqs
        .values()
        .flatten()
        .flat_map(|req| {
            req.filter
                .tags
                .get(&IndexedTagName::new('d').unwrap())
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let wanted: BTreeSet<String> = (0..NUM_GROUPS).map(group_d).collect();
    assert_eq!(
        covered, wanted,
        "every demanded #d value must reach some subscription -- coalescing may \
         over-fetch, never under-fetch"
    );

    // ---- The pre-committed assertion: report the kill, do not hide it ---
    assert!(
        !kill_fired,
        "TAG KILL FIRED: coalescing leaves a host over max_subs_per_relay or a filter \
         over max_filter_tag_values on this falsifier demand (see the printed \
         measurement above)"
    );
}

/// What the fix has to achieve, stated as arithmetic rather than prose: at
/// `MAX_TAG_VALUES_PER_FILTER`, a catalog this size needs a handful of
/// subscriptions per host, not one per group.
#[test]
fn a_bounded_tag_union_would_fit_the_catalog_in_a_single_subscription_per_host() {
    let chunks = NUM_GROUPS.div_ceil(MAX_TAG_VALUES_PER_FILTER);
    println!(
        "\n{NUM_GROUPS} groups at {MAX_TAG_VALUES_PER_FILTER} values/filter → {chunks} \
         subscription(s) per host (limit {MAX_SUBS_PER_RELAY})"
    );
    // The catalog would have to grow past 6,000 groups before chunking alone
    // pushed a host back over its subscription limit.
    let groups_at_limit = MAX_TAG_VALUES_PER_FILTER * MAX_SUBS_PER_RELAY;
    println!("headroom before the limit returns: {groups_at_limit} groups per host");

    assert!(chunks <= MAX_SUBS_PER_RELAY);
    assert_eq!(chunks, 1);
    assert!(groups_at_limit > NUM_GROUPS * 10);
}
