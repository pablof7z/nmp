//! The cost comparison behind mosaico #693: what a growing `#d` value set
//! actually costs on the wire, measured three ways.
//!
//! Today NMP fans a derived tag binding out into one atom per value
//! (`nmp_resolver::graph::Graph::compute_atoms` takes the cartesian product
//! of every bound field's resolved elements). This file measures what that
//! costs against the two alternatives, at the layer where a fix would live.
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

/// One pinned atom over `groups` — a singleton set reproduces today's
/// per-value fan-out; a multi-value set is the batched shape a
/// `TagValueUnion` merge rule would produce.
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

#[derive(Default, Debug, Clone, Copy)]
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

/// H — today's fan-out vs a naive batched filter, both compiled by the real
/// router, over identical incremental growth.
///
/// The result that matters: batching is NOT free. Because `Skeleton::of`
/// erases `authors` and nothing else, a widened `#d` filter mints a NEW
/// `SubId` on every growth step, so each added value costs a Close plus a
/// Req instead of a single Req. Steady-state subs drop from N to 1;
/// cumulative wire messages roughly DOUBLE.
///
/// That is why a `TagValueUnion` merge rule alone would not reproduce
/// `AuthorUnion`'s behavior — the rule needs the matching skeleton erasure
/// for the unioned tag, or it trades one cost for another.
#[test]
fn batching_a_growing_tag_set_trades_sub_count_for_wire_churn() {
    const N: usize = 8;
    let (fan_steps, fan_total, fan_live) = grow(N, false);
    let (batch_steps, batch_total, batch_live) = grow(N, true);

    println!("\n=== H. growing a #d set from 1 to {N} values, compiled by the real router ===");
    println!(
        "{:<8} {:>18} {:>20}",
        "step", "fan-out (today)", "batched (widened)"
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

    assert_eq!(fan_live, N, "fan-out holds one live sub per value");
    assert_eq!(batch_live, 1, "a widened filter holds exactly one live sub");
    assert_eq!(
        fan_total.closes, 0,
        "fan-out never closes a sub as the set grows"
    );
    assert!(
        batch_total.closes > 0,
        "a widened filter's SubId moves on every growth step, forcing Close+Req"
    );
    assert!(
        batch_total.total() > fan_total.total(),
        "batching costs MORE cumulative wire messages than fan-out: \
         batched {} vs fan-out {}",
        batch_total.total(),
        fan_total.total()
    );
}

/// The control: the SAME growth in the `authors` slot. `AuthorUnion` widens
/// the atoms into one filter AND `Skeleton::of` erases `authors`, so the
/// sub-id is stable — one live sub, one in-place REQ per step, zero closes.
/// This is the behavior a tag union should aim at, proven to exist already.
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
        "AuthorUnion collapses every author atom onto one wire sub"
    );
    assert_eq!(
        total.closes, 0,
        "the erased-authors skeleton keeps the sub-id stable, so growth never closes"
    );
    assert_eq!(
        total.reqs, N,
        "one in-place REQ per growth step — the cheapest of the three shapes"
    );
}
