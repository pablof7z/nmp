//! The tag-axis twin of `kill_measurement.rs`.
//!
//! `kill_measurement.rs` asks whether the AUTHOR axis stays inside real relay
//! admission limits once `AuthorUnion` coalesces it. Its verdict is no-kill:
//! the union brings every relay back under the cap. This file asks the same
//! question of the TAG axis, where no union rule exists.
//!
//! The thresholds are the ones that file already carries, and they encode
//! the operational reality: relays accept large value arrays but cap
//! CONCURRENT SUBSCRIPTIONS. Field evidence puts the subscription cap at
//! roughly 20 (sometimes up to 200), while a filter carrying 500 values is
//! entirely unremarkable.
//!
//! The kill fires. That is the point of the file: it converts "fan-out does
//! not work" from an assertion into a measured number, and it is the
//! acceptance test a fix has to move.
//!
//! Run narrated with:
//! `cargo test -p nmp-router --test tag_kill_measurement -- --nocapture`

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, IndexedTagName, SourceAuthority};
use nmp_router::{test_relay, DiscoveryKinds, FixtureDirectory, RelayUrl, Router, RuleRegistry};

/// A realistic mid-size mosaico channel catalog.
const NUM_GROUPS: usize = 300;

/// Group state is pinned to the NIP-29 host set — mosaico pins every
/// observation to its configured relays rather than routing by outbox.
const NUM_HOSTS: usize = 2;

/// Relay admission thresholds, carried over verbatim from
/// `kill_measurement.rs`, which took them from #123's deleted `RelayLimits`.
const MAX_SUBS_PER_RELAY: usize = 20;

/// The value-array bound for a coalesced tag filter. Deliberately NOT 256
/// (`coalesce::MAX_IDS_PER_FILTER`): that cap bounds 64-char hex event ids,
/// where 256 values is already ~16KB of filter. NIP-29 `d` identifiers are
/// short, so 500 values is a comparable frame size, and 500 is the number
/// field experience reports as unremarkable for a relay to accept.
const MAX_TAG_VALUES_PER_FILTER: usize = 500;

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

/// THE TAG KILL.
///
/// `AuthorUnion` rescues the author axis. Nothing rescues the tag axis, so a
/// realistic catalog puts every host far over its concurrent-subscription
/// limit while every filter carries a single value — the exact inverse of
/// what relays are provisioned for.
///
/// This test asserts the kill FIRES, because it does. When a tag union rule
/// and its wire-identity counterpart land, this assertion must be INVERTED
/// (to `!kill_fired`, matching `kill_measurement.rs`) rather than deleted —
/// that inversion is the acceptance criterion for the fix.
#[test]
fn tag_axis_exceeds_relay_subscription_limits_and_no_rule_rescues_it() {
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
    let kill_fired = !over_sub_limit.is_empty();

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
        "  a single coalesced filter could carry all {NUM_GROUPS} values and stay well inside \
         the {MAX_TAG_VALUES_PER_FILTER}-value bound"
    );
    println!(
        "  headroom wasted: every filter carries {} value(s) out of {}",
        m_union.max_filter_tag_values, MAX_TAG_VALUES_PER_FILTER
    );

    // The registry does not touch the tag axis at all: tier 1 and tier 2 are
    // identical. That is the defect, stated as an equality.
    let total_dedup: usize = m_dedup.per_relay_sub_count.iter().map(|(_, c)| *c).sum();
    let total_union: usize = m_union.per_relay_sub_count.iter().map(|(_, c)| *c).sum();
    println!("total wire_sub_count: dedup-only={total_dedup}, with registry={total_union}");
    assert_eq!(
        total_union, total_dedup,
        "no rule in default_widen_only() reduces the tag axis — coalescing is a no-op here"
    );

    assert_eq!(
        m_union.max_filter_tag_values, 1,
        "every filter carries exactly one #d value; the array headroom is entirely unused"
    );

    assert!(
        kill_fired,
        "EXPECTED TO FIRE TODAY. If this now passes, a tag union has landed — invert this \
         assertion to `!kill_fired` and assert max_filter_tag_values <= \
         MAX_TAG_VALUES_PER_FILTER instead."
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
