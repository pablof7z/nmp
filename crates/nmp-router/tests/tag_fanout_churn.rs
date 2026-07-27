//! The cost comparison behind mosaico #693: what a growing `#d` value set
//! actually costs on the wire, measured three ways.
//!
//! NMP fans a derived tag binding out into one atom per value
//! (`nmp_resolver::graph::Graph::compute_atoms` takes the cartesian product
//! of every bound field's resolved elements) and that is CORRECT — narrow
//! atoms are the ratified identity for coverage, evidence and routing. The
//! fix belongs on the wire, not on the atoms. This file measures that it now
//! is there: per-value atoms and a pre-batched atom compile to the same
//! plan.
//!
//! Run narrated with:
//! `cargo test -p nmp-router --test tag_fanout_churn -- --nocapture`

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, IndexedTagName, SourceAuthority};
use nmp_router::{DiscoveryKinds, FixtureDirectory, RelayUrl, Router, RuleRegistry, WireOp};

const OUTER_KINDS: [u16; 3] = [39_000, 39_001, 39_002];
const CAP: usize = 64;

fn relays() -> BTreeSet<RelayUrl> {
    BTreeSet::from([RelayUrl::parse("wss://relay0.example.com").unwrap()])
}

/// One pinned atom over `groups` — a singleton set reproduces the resolver's
/// per-value fan-out; a multi-value set is the batched shape the merge rule
/// produces from it.
fn atom(groups: &[String]) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(OUTER_KINDS.iter().copied().collect()),
            tags: BTreeMap::from([(
                IndexedTagName::new('d').unwrap(),
                groups.iter().cloned().collect::<BTreeSet<String>>(),
            )]),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(relays()),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn group(n: usize) -> String {
    format!("group-{n}")
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
struct Ops {
    reqs: usize,
    closes: usize,
}

impl Ops {
    fn total(&self) -> usize {
        self.reqs + self.closes
    }
}

fn compile_step(
    router: &mut Router,
    dir: &FixtureDirectory,
    demand: &BTreeSet<ContextualAtom>,
) -> Ops {
    let delta = router.compile(demand, dir, CAP);
    let mut ops = Ops::default();
    for (_, relay_ops) in &delta.ops {
        for op in relay_ops {
            match op {
                WireOp::Req(..) => ops.reqs += 1,
                WireOp::Close(_) => ops.closes += 1,
            }
        }
    }
    ops
}

fn live_reqs(router: &Router) -> usize {
    router.plan().reqs.values().map(|reqs| reqs.len()).sum()
}

/// Grow a `#d` set from 1 to `n` values, one at a time, under `shape`.
/// Returns (per-step ops, cumulative ops, live subs at the end).
fn grow(n: usize, batched: bool) -> (Vec<Ops>, Ops, usize) {
    let dir = FixtureDirectory::new();
    let mut router = Router::new(
        DiscoveryKinds::default(),
        RuleRegistry::default_widen_only(),
    );
    let mut per_step = Vec::new();
    let mut total = Ops::default();

    for step in 1..=n {
        let groups: Vec<String> = (1..=step).map(group).collect();
        let demand: BTreeSet<ContextualAtom> = if batched {
            BTreeSet::from([atom(&groups)])
        } else {
            groups
                .iter()
                .map(|g| atom(std::slice::from_ref(g)))
                .collect()
        };
        let ops = compile_step(&mut router, &dir, &demand);
        total.reqs += ops.reqs;
        total.closes += ops.closes;
        per_step.push(ops);
    }
    (per_step, total, live_reqs(&router))
}

/// H — resolver fan-out vs a pre-batched filter, both compiled by the real
/// router, over identical incremental growth.
///
/// FULLY INVERTED, in two stages, and the two stages are the two halves of
/// the design (`docs/internals/subscriptions/identity-grouping-and-limits.md`
/// §7.3).
///
/// This file originally measured a gap that no longer exists. It asserted
/// that fan-out held N live subscriptions while batching held 1 but paid a
/// Close plus a Req per growth step, because `SubId::for_wire` erased
/// `authors` and nothing else, so a widened `#d` filter minted a new id every
/// time it grew. #899's allocated tokens removed the churn half (a grown
/// value set is a one-component difference that overwrites the same token in
/// place). `StructuralUnion` removes the remaining half: the router now
/// coalesces the fan-out into the batched shape ITSELF, so the two columns
/// are no longer two shapes at all -- they are the same plan reached from two
/// different demand encodings.
///
/// The assertion that carries the weight is therefore the EQUALITY of the two
/// columns. A regression in either half separates them again: lose the merge
/// and fan-out's live count climbs back to N; lose in-place continuation and
/// the batched column's Closes reappear.
#[test]
fn resolver_fan_out_and_a_pre_batched_filter_compile_to_the_same_plan() {
    const N: usize = 8;
    let (fan_steps, fan_total, fan_live) = grow(N, false);
    let (batch_steps, batch_total, batch_live) = grow(N, true);

    println!("\n=== H. growing a #d set from 1 to {N} values, compiled by the real router ===");
    println!(
        "{:<8} {:>18} {:>20}",
        "step", "fan-out (atoms)", "batched (one atom)"
    );
    for i in 0..N {
        println!(
            "{:<8} {:>18} {:>20}",
            i + 1,
            format!("{}req {}close", fan_steps[i].reqs, fan_steps[i].closes),
            format!("{}req {}close", batch_steps[i].reqs, batch_steps[i].closes),
        );
    }
    println!(
        "{:<8} {:>18} {:>20}",
        "TOTAL",
        format!("{} msgs", fan_total.total()),
        format!("{} msgs", batch_total.total()),
    );
    println!("live subs at end   fan-out: {fan_live}   batched: {batch_live}");

    // INVERTED by `StructuralUnion`. This asserted `fan_live == N` -- one
    // live subscription per resolved value, which at catalog scale is the
    // 300-against-a-ceiling-of-20 defect. The coalescer now folds the N
    // singleton atoms into ONE filter carrying N values, because they differ
    // in exactly one array component (`#d`'s value set).
    assert_eq!(
        fan_live, 1,
        "the resolver's per-value atoms must coalesce onto ONE wire sub"
    );
    assert_eq!(batch_live, 1, "a pre-batched filter holds exactly one live sub");

    // INVERTED when allocated ids landed (#899). Under derived ids a widened
    // filter's SubId moved on every growth step, so batching bought one live
    // sub at the price of a Close plus a Req per value -- measured then as 15
    // wire messages against fan-out's 8. Allocation decides continuity by
    // structural signature instead, so a grown value set is a one-component
    // difference that overwrites the SAME token in place.
    assert_eq!(
        fan_total.closes, 0,
        "growth must never close a sub -- a widening value set is a \
         one-component difference that replaces in place"
    );
    assert_eq!(
        batch_total.closes, 0,
        "a widened filter must grow in place: allocated ids do not move \
         when a value set grows"
    );

    // THE assertion: the two encodings are indistinguishable on the wire.
    assert_eq!(
        fan_steps, batch_steps,
        "step for step, per-value atoms and a pre-batched atom must produce \
         the SAME wire ops -- the router does the batching, so how the demand \
         was encoded stops being visible to the relay"
    );
    assert_eq!(
        fan_total.total(),
        N,
        "one in-place REQ per growth step, no closes -- the cheapest shape \
         available, and the one the author axis already had"
    );
}

/// The control: the SAME growth in the `authors` slot. This is the behaviour
/// the tag axis had to reach, and it is now reached by the same mechanism
/// rather than by a parallel one -- `StructuralUnion` treats `authors` and a
/// tag name as two instances of one case. The two tests must agree exactly;
/// if they ever diverge, one axis has grown a special case.
#[test]
fn the_authors_slot_already_achieves_one_stable_sub_with_no_churn() {
    const N: usize = 8;
    let dir = FixtureDirectory::new();
    let mut router = Router::new(
        DiscoveryKinds::default(),
        RuleRegistry::default_widen_only(),
    );
    let mut total = Ops::default();

    for step in 1..=N {
        let demand: BTreeSet<ContextualAtom> = (1..=step)
            .map(|n| ContextualAtom {
                filter: ConcreteFilter {
                    kinds: Some(BTreeSet::from([1u16])),
                    authors: Some(BTreeSet::from([format!("{n:064x}")])),
                    ..ConcreteFilter::default()
                },
                source: SourceAuthority::Pinned(relays()),
                access: AccessContext::Public,
                routing_evidence: BTreeSet::new(),
            })
            .collect();
        let ops = compile_step(&mut router, &dir, &demand);
        total.reqs += ops.reqs;
        total.closes += ops.closes;
    }

    println!(
        "\n=== control: {N} values in the AUTHORS slot ===\n\
         live subs: {}   cumulative: {}req {}close",
        live_reqs(&router),
        total.reqs,
        total.closes
    );

    assert_eq!(
        live_reqs(&router),
        1,
        "the union collapses every author atom onto one wire sub"
    );
    assert_eq!(
        total.closes, 0,
        "a one-component difference replaces in place, so growth never closes"
    );
    assert_eq!(
        total.reqs, N,
        "one in-place REQ per growth step — the cheapest of the three shapes"
    );
}

// ---- the injectivity falsifier ------------------------------------------

/// A `SubId` must be unique within one (relay session, source) partition.
/// `diff_plans` keys the emitted delta by `SubId` (`plan.rs`), so two
/// `WireReq`s sharing one id cannot both reach the wire — one is silently
/// dropped, and the next compile is a no-op, so it never repairs.
///
/// INVERTED when allocated ids landed (#899). This test previously asserted
/// the DEFECT: `Skeleton::of` erased `authors`, which was only safe while the
/// author union was TOTAL over the partition, and `neither_limited` makes it
/// partial — so two atoms identical except `authors`, both carrying a `limit`,
/// refused to merge and then collided on the erased skeleton, with one REQ
/// silently never reaching the relay.
///
/// Allocation removes the bet entirely: injectivity comes from the assignment
/// (each prior token used at most once, fresh tokens unique by minting), so
/// two unmergeable filters simply get two tokens.
#[test]
fn limited_identical_except_authors_atoms_each_reach_the_wire() {
    let dir = FixtureDirectory::new();
    let mut router = Router::new(
        DiscoveryKinds::default(),
        RuleRegistry::default_widen_only(),
    );

    let limited = |author: &str| ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(BTreeSet::from([author.to_string()])),
            limit: Some(10),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(relays()),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let a = format!("{:064x}", 0xaa);
    let b = format!("{:064x}", 0xbb);
    let demand = BTreeSet::from([limited(&a), limited(&b)]);

    let delta = router.compile(&demand, &dir, CAP);

    let planned: Vec<_> = router
        .plan()
        .reqs
        .values()
        .flat_map(|reqs| reqs.iter())
        .cloned()
        .collect();
    let planned_ids: BTreeSet<_> = planned.iter().map(|req| req.sub_id.clone()).collect();
    let emitted: usize = delta
        .ops
        .iter()
        .map(|(_, ops)| {
            ops.iter()
                .filter(|op| matches!(op, WireOp::Req(..)))
                .count()
        })
        .sum();

    println!("\n=== injectivity check: two LIMITED identical-except-authors atoms ===");
    println!("demand atoms:            2");
    println!("WireReqs in the plan:    {}", planned.len());
    println!("distinct SubIds:         {}", planned_ids.len());
    println!("REQs actually emitted:   {emitted}");
    for req in &planned {
        println!(
            "  planned: authors={:?} limit={:?}",
            req.filter.authors, req.filter.limit
        );
    }

    // Recompiling identical demand must not repair the loss.
    let second = router.compile(&demand, &dir, CAP);
    let repaired: usize = second.ops.iter().map(|(_, ops)| ops.len()).sum();
    println!("ops on identical recompile: {repaired}");

    assert_eq!(planned.len(), 2, "both atoms are planned separately");
    assert_eq!(
        planned_ids.len(),
        2,
        "each unmergeable filter must carry its OWN token — this asserted 1 \
         before #899, which is exactly how demand went missing"
    );
    assert_eq!(
        emitted, 2,
        "BOTH REQs must reach the wire; before #899 only one did and the other \
         author's demand was silently lost"
    );
    assert_eq!(
        repaired, 0,
        "an identical recompile stays a no-op — nothing to repair, because \
         nothing was dropped"
    );
}

/// The control that proves the collision is caused by `limit` blocking the
/// merge, not by the two-author shape itself: drop the limit and the union
/// merges them into one REQ carrying both authors — no loss.
#[test]
fn unlimited_identical_except_authors_atoms_merge_instead_of_colliding() {
    let dir = FixtureDirectory::new();
    let mut router = Router::new(
        DiscoveryKinds::default(),
        RuleRegistry::default_widen_only(),
    );

    let unlimited = |author: &str| ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(BTreeSet::from([author.to_string()])),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(relays()),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let a = format!("{:064x}", 0xaa);
    let b = format!("{:064x}", 0xbb);
    router.compile(&BTreeSet::from([unlimited(&a), unlimited(&b)]), &dir, CAP);

    let planned: Vec<_> = router
        .plan()
        .reqs
        .values()
        .flat_map(|reqs| reqs.iter())
        .cloned()
        .collect();
    assert_eq!(planned.len(), 1, "the union merges the unlimited pair");
    assert_eq!(
        planned[0].filter.authors.as_ref().map(|a| a.len()),
        Some(2),
        "the merged filter carries BOTH authors — nothing is lost"
    );
}
