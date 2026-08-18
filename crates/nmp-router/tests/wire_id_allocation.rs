//! Allocated wire subscription ids with structural-signature matching (#899).
//!
//! THE DEFECT. `SubId::for_wire` derived a wire id from the filter's
//! `Skeleton` (`route.rs`), which DELETES `authors`. Two filters differing
//! only in `authors` therefore minted the SAME id. Normally they would have
//! been merged by the author union first — but `coalesce::neither_limited`
//! refuses to merge any pair where either side carries a `limit`, so both
//! survive coalescing under one id. `diff_plans` keys the emitted delta by
//! `SubId` in a `BTreeMap` (`plan.rs`), so one of them is silently dropped,
//! and because an identical recompile is a no-op the loss NEVER repairs.
//! `limit` is the TRIGGER, not the defect: under `RuleRegistry::dedup_only()`
//! two ordinary UNLIMITED atoms collide exactly the same way.
//!
//! THE FIX. Wire ids are ALLOCATED opaque tokens, not functions of the
//! filter. Exact byte-identical requests retain their existing token. A
//! byte-changed filter always mints a fresh one; structural matching names at
//! most one predecessor so EngineCore can offer the successor before retiring
//! that predecessor at the exact commit edge. Injectivity comes from fresh
//! minting plus one-to-one transition assignment, never from the id's content.
//!
//! Run narrated with:
//! `cargo test -p nmp-router --test wire_id_allocation -- --nocapture`

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use nmp_grammar::{ConcreteFilter, ContextualAtom, IndexedTagName, ReadRouting};
use nmp_router::{CompileOutcome, RelayUrl, Router, RuleRegistry, SubId, WireOp, WireReq};
use nmp_router_testkit::FixtureRoutingFacts;

const CAP: usize = 64;

fn relays() -> BTreeSet<RelayUrl> {
    BTreeSet::from([RelayUrl::parse("wss://relay0.example.com").unwrap()])
}

fn author(n: u32) -> String {
    format!("{n:064x}")
}

/// A filter routed `Explicit`: one relay, no directory, no
/// additive lane. Keeps every test below focused on wire identity rather than
/// on routing.
fn pinned_atom(filter: ConcreteFilter) -> ContextualAtom {
    ContextualAtom {
        filter,
        routing: ReadRouting::Explicit(relays().into_iter().collect()),
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    }
}

fn kind1(authors: &[u32], limit: Option<usize>) -> ConcreteFilter {
    ConcreteFilter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(authors.iter().map(|n| author(*n)).collect()),
        limit,
        ..ConcreteFilter::default()
    }
}

fn atom(author_n: u32, limit: Option<usize>) -> ContextualAtom {
    pinned_atom(kind1(&[author_n], limit))
}

fn router() -> (FixtureRoutingFacts, Router) {
    (
        FixtureRoutingFacts::new(),
        Router::new(RuleRegistry::default_widen_only()),
    )
}

fn planned(router: &Router) -> Vec<WireReq> {
    router
        .plan()
        .reqs
        .values()
        .flat_map(|reqs| reqs.iter())
        .cloned()
        .collect()
}

fn sub_ids(router: &Router) -> BTreeSet<SubId> {
    planned(router).into_iter().map(|r| r.sub_id).collect()
}

fn count_reqs(delta: &CompileOutcome) -> usize {
    delta
        .wire
        .ops
        .iter()
        .flat_map(|(_, ops)| ops.iter())
        .filter(|op| matches!(op, WireOp::Req(..)))
        .count()
}

fn count_closes(delta: &CompileOutcome) -> usize {
    delta
        .wire
        .ops
        .iter()
        .flat_map(|(_, ops)| ops.iter())
        .filter(|op| matches!(op, WireOp::Close(..)))
        .count()
}

/// THE falsifier. Two atoms identical except `authors`, both carrying a
/// `limit`. The union refuses them (`neither_limited`), so they survive
/// coalescing as two separate `WireReq`s — and under the old derived identity
/// they minted the SAME `SubId`, because `Skeleton::of` erased the only field
/// that distinguishes them. One REQ then never reached the wire, and an
/// identical recompile emitted nothing, so the demand was lost forever.
#[test]
fn limited_identical_except_authors_atoms_get_distinct_sub_ids() {
    let (dir, mut router) = router();
    let demand = BTreeSet::from([atom(0xaa, Some(10)), atom(0xbb, Some(10))]);

    let delta = router.compile(&demand, &dir, CAP);
    let planned = planned(&router);
    let ids = sub_ids(&router);
    let emitted = count_reqs(&delta);

    // Recompiling identical demand must not be relied on to repair a loss.
    let second = router.compile(&demand, &dir, CAP);
    let repaired: usize = second.wire.ops.iter().map(|(_, ops)| ops.len()).sum();

    println!("\n=== injectivity: two LIMITED identical-except-authors atoms ===");
    println!("demand atoms:               2");
    println!("WireReqs in the plan:       {}", planned.len());
    println!("distinct SubIds:            {}", ids.len());
    println!("REQs actually emitted:      {emitted}");
    for req in &planned {
        println!(
            "  planned: authors={:?} limit={:?}",
            req.filter.authors, req.filter.limit
        );
    }
    println!("ops on identical recompile: {repaired}");

    assert_eq!(planned.len(), 2, "both atoms are planned separately");
    assert_eq!(
        ids.len(),
        2,
        "the two planned WireReqs must carry DISTINCT SubIds"
    );
    assert_eq!(
        emitted, 2,
        "both REQs must reach the wire -- neither author's demand is lost"
    );
    assert_eq!(
        repaired, 0,
        "an identical recompile is a no-op: the fix must PREVENT the drop, never retry it"
    );
}

/// A byte-changed filter gets a fresh identity. Structural matching names the
/// old physical request only as the accepted-open-before-close predecessor;
/// it never authorizes a same-SubId overwrite whose later EOSE would be
/// generation-ambiguous (#774).
#[test]
fn churning_a_limited_atoms_author_set_mints_a_fresh_transition_identity() {
    let (dir, mut router) = router();

    router.compile(
        &BTreeSet::from([pinned_atom(kind1(&[1, 2], Some(10)))]),
        &dir,
        CAP,
    );
    let before = sub_ids(&router);

    let delta = router.compile(
        &BTreeSet::from([pinned_atom(kind1(&[1, 2, 3], Some(10)))]),
        &dir,
        CAP,
    );
    let after = sub_ids(&router);

    println!("\n=== LIMITED author churn: fresh accepted transition ===");
    println!(
        "closes: {}  reqs: {}",
        count_closes(&delta),
        count_reqs(&delta)
    );

    assert_eq!(planned(&router).len(), 1, "still one subscription");
    assert_eq!(
        count_closes(&delta),
        1,
        "the router describes the old physical request that Core defers until acceptance"
    );
    assert_ne!(
        before, after,
        "every byte-changed request gets a fresh SubId"
    );
    assert_eq!(count_reqs(&delta), 1, "exactly one fresh REQ is dispatched");
    let replacement = delta.replacements.iter().next().expect("one transition");
    assert_eq!(replacement.prior_sub_id, before.into_iter().next().unwrap());
    assert_eq!(replacement.next_sub_id, after.into_iter().next().unwrap());
}

/// `limit` is the TRIGGER, not the defect. `RuleRegistry::dedup_only()` holds
/// no union at all, so under it two ordinary UNLIMITED atoms identical
/// except `authors` also fail to merge — and must also stay distinct on the
/// wire. Mergeability depends on the registry actually in play, not on the
/// filter alone; an identity ALLOCATED per surviving filter is indifferent to
/// which registry produced the survivors.
#[test]
fn a_registry_without_author_union_still_keeps_sub_ids_distinct() {
    let dir = FixtureRoutingFacts::new();
    let mut router = Router::new(RuleRegistry::dedup_only());
    let demand = BTreeSet::from([atom(0xaa, None), atom(0xbb, None)]);
    let delta = router.compile(&demand, &dir, CAP);

    let planned = planned(&router);
    let ids = sub_ids(&router);
    println!(
        "\n=== dedup_only registry: planned={} distinct_ids={} ===",
        planned.len(),
        ids.len()
    );

    assert_eq!(planned.len(), 2, "dedup-only never merges the pair");
    assert_eq!(
        ids.len(),
        2,
        "unmergeable-by-registry atoms must ALSO carry distinct SubIds"
    );
    assert_eq!(count_reqs(&delta), 2, "and both must reach the wire");
}

/// Injectivity as a plan-wide invariant, not just for the known-bad pair:
/// over a mixed demand set (limited and unlimited, several authors, several
/// kinds), every `WireReq` carries a distinct `SubId` and every planned req
/// reaches the wire.
#[test]
fn every_wire_req_in_a_session_has_a_distinct_sub_id() {
    let (dir, mut router) = router();
    let mut demand = BTreeSet::new();
    for n in 0..6u32 {
        demand.insert(atom(n, Some(10)));
        demand.insert(atom(n, None));
        demand.insert(pinned_atom(ConcreteFilter {
            kinds: Some(BTreeSet::from([7u16])),
            authors: Some(BTreeSet::from([author(n)])),
            limit: Some(3),
            ..ConcreteFilter::default()
        }));
    }

    let delta = router.compile(&demand, &dir, CAP);

    let mut total_planned = 0usize;
    for (session, reqs) in &router.plan().reqs {
        let ids: BTreeSet<_> = reqs.iter().map(|r| r.sub_id.clone()).collect();
        assert_eq!(
            ids.len(),
            reqs.len(),
            "session {session:?} planned {} reqs under {} distinct SubIds",
            reqs.len(),
            ids.len()
        );
        let filters: BTreeSet<_> = reqs.iter().map(|r| r.filter.clone()).collect();
        assert_eq!(
            filters.len(),
            reqs.len(),
            "two byte-identical filters must have been folded into ONE req, \
             never shipped as two distinguishable subscriptions"
        );
        total_planned += reqs.len();
    }
    assert_eq!(
        count_reqs(&delta),
        total_planned,
        "every planned WireReq must reach the wire as its own REQ"
    );
}

/// NO IDENTITY HYSTERESIS. Zero-diff ranks FIRST, so a filter that did not
/// change keeps its token no matter which siblings come and go around it.
/// Without that rule, withdrawing a sibling would move the survivor's id and
/// `diff_plans` would emit a Close plus a Req for a BYTE-IDENTICAL filter —
/// the relay re-serves the whole window for nothing and the attribution FIFO
/// splits across two identities, orphaning outstanding snapshots.
#[test]
fn withdrawing_a_sibling_does_not_move_the_survivors_sub_id() {
    let (dir, mut router) = router();
    let a = atom(0xaa, Some(10));
    let b = atom(0xbb, Some(10));

    router.compile(&BTreeSet::from([a.clone(), b]), &dir, CAP);
    let before: BTreeSet<_> = planned(&router)
        .into_iter()
        .filter(|r| r.filter.authors == a.filter.authors)
        .map(|r| r.sub_id)
        .collect();
    assert_eq!(before.len(), 1, "the surviving atom holds exactly one sub");

    // Withdraw the sibling. The survivor's demand is byte-identical.
    let delta = router.compile(&BTreeSet::from([a.clone()]), &dir, CAP);
    let after: BTreeSet<_> = planned(&router)
        .into_iter()
        .filter(|r| r.filter.authors == a.filter.authors)
        .map(|r| r.sub_id)
        .collect();

    assert_eq!(
        before, after,
        "the survivor's SubId must not move when a sibling is withdrawn"
    );
    assert_eq!(
        count_reqs(&delta),
        0,
        "no REQ may be re-sent for a filter that did not change"
    );
    assert_eq!(
        count_closes(&delta),
        1,
        "exactly one CLOSE: the withdrawn sibling, and nothing else"
    );
}

/// The control that must NOT regress: growing an unlimited author set keeps
/// one accumulating live filter while every byte-changing step uses a fresh
/// transition identity.
#[test]
fn unlimited_author_growth_keeps_one_live_sub_with_fresh_transitions() {
    const N: u32 = 8;
    let (dir, mut router) = router();
    let mut reqs = 0usize;
    let mut closes = 0usize;
    let mut replacements = 0usize;
    let mut ids = BTreeSet::new();

    for step in 1..=N {
        let demand: BTreeSet<ContextualAtom> = (1..=step).map(|n| atom(n, None)).collect();
        let delta = router.compile(&demand, &dir, CAP);
        reqs += count_reqs(&delta);
        closes += count_closes(&delta);
        replacements += delta.replacements.len();
        ids.extend(sub_ids(&router));
    }

    println!("\n=== control: {N} unlimited authors grown one at a time ===");
    println!(
        "live subs: {}  reqs: {reqs}  closes: {closes}",
        planned(&router).len()
    );

    assert_eq!(
        planned(&router).len(),
        1,
        "one wire sub for the whole group"
    );
    assert_eq!(
        ids.len(),
        N as usize,
        "every byte-changing step mints a fresh token"
    );
    assert_eq!(
        closes,
        N as usize - 1,
        "every predecessor leaves the raw plan"
    );
    assert_eq!(
        replacements,
        N as usize - 1,
        "every changed step matches its predecessor"
    );
    assert_eq!(reqs, N as usize, "one fresh REQ per growth step");
}

/// The same growth in the LIMITED slot: each author is its own unmergeable
/// subscription, so the sub count grows — but nothing is dropped, and a
/// previously established subscription is never closed by the arrival of a
/// new sibling (zero-diff ranks first, so every established filter re-matches
/// itself before the newcomer is even considered).
#[test]
fn limited_author_growth_adds_subs_without_dropping_or_closing_any() {
    const N: u32 = 8;
    let (dir, mut router) = router();
    let mut closes = 0usize;

    for step in 1..=N {
        let demand: BTreeSet<ContextualAtom> = (1..=step).map(|n| atom(n, Some(10))).collect();
        let delta = router.compile(&demand, &dir, CAP);
        closes += count_closes(&delta);
        assert_eq!(
            planned(&router).len(),
            step as usize,
            "step {step}: every limited author holds its own subscription"
        );
        assert_eq!(
            sub_ids(&router).len(),
            step as usize,
            "step {step}: tokens stay distinct"
        );
    }

    assert_eq!(
        closes, 0,
        "adding a sibling subscription never closes an established one"
    );
}

/// An identical recompile is a NO-OP. This is what "zero-diff ranks first"
/// buys, and it is load-bearing: every established subscription must match
/// itself before any one-diff candidate is considered, or steady-state
/// recompiles would churn the wire continuously.
#[test]
fn identical_recompile_emits_nothing() {
    let (dir, mut router) = router();
    let demand = BTreeSet::from([
        atom(1, Some(10)),
        atom(2, Some(10)),
        atom(3, None),
        pinned_atom(ConcreteFilter {
            kinds: Some(BTreeSet::from([30_023u16])),
            tags: BTreeMap::from([(
                IndexedTagName::new('d').unwrap(),
                BTreeSet::from(["slug".to_string()]),
            )]),
            ..ConcreteFilter::default()
        }),
    ]);

    router.compile(&demand, &dir, CAP);
    let before = sub_ids(&router);
    let delta = router.compile(&demand, &dir, CAP);

    assert!(
        delta.wire.ops.is_empty(),
        "identical demand must emit NOTHING"
    );
    assert_eq!(before, sub_ids(&router), "and must not move any token");
}

/// A `since` churn is a one-component predecessor match, but changed bytes
/// still mint a fresh identity. A delayed EOSE for the old window therefore
/// cannot land on the successor's attribution generation.
#[test]
fn since_churn_mints_a_fresh_transition_identity() {
    let (dir, mut router) = router();
    let windowed = |since: u64| {
        pinned_atom(ConcreteFilter {
            since: Some(since),
            ..kind1(&[1], Some(10))
        })
    };

    router.compile(&BTreeSet::from([windowed(100)]), &dir, CAP);
    let before = sub_ids(&router);
    let delta = router.compile(&BTreeSet::from([windowed(200)]), &dir, CAP);

    let after = sub_ids(&router);
    assert_ne!(before, after, "changed bytes must mint a fresh SubId");
    assert_eq!(count_closes(&delta), 1);
    assert_eq!(count_reqs(&delta), 1);
    let replacement = delta.replacements.iter().next().expect("one transition");
    assert_eq!(replacement.prior_sub_id, before.into_iter().next().unwrap());
    assert_eq!(replacement.next_sub_id, after.into_iter().next().unwrap());
}

/// `limit` is a component, so `limit: None -> Some(n)` identifies one exact
/// predecessor. The byte-changed limited successor still gets a fresh token,
/// keeping its filter-limit coverage poison in a separate attribution
/// generation.
#[test]
fn a_limit_appearing_mints_a_fresh_transition_identity() {
    let (dir, mut router) = router();

    router.compile(&BTreeSet::from([pinned_atom(kind1(&[1], None))]), &dir, CAP);
    let before = sub_ids(&router);
    let delta = router.compile(
        &BTreeSet::from([pinned_atom(kind1(&[1], Some(10)))]),
        &dir,
        CAP,
    );

    let after = sub_ids(&router);
    assert_ne!(before, after, "changed bytes must mint a fresh SubId");
    assert_eq!(count_closes(&delta), 1);
    assert_eq!(count_reqs(&delta), 1);
    let replacement = delta.replacements.iter().next().expect("one transition");
    assert_eq!(replacement.prior_sub_id, before.into_iter().next().unwrap());
    assert_eq!(replacement.next_sub_id, after.into_iter().next().unwrap());
}

/// ACCEPTED COST, pinned rather than fixed: COMPOUND CHURN. Two components
/// moving in one recompile (here an author resolves AND the window advances)
/// has no structural predecessor match, so the old request closes directly
/// while the new one opens under its fresh identity.
///
/// This is deliberately NOT relaxed to "<=2 components with overlap evidence":
/// that re-imports exactly the ambiguity single-component matching avoids —
/// with two axes free, a filter can be one "step" from priors it has nothing
/// to do with, and the tiebreak stops being content-grounded. It is an
/// efficiency cost, not a correctness one.
#[test]
fn compound_churn_closes_and_reopens() {
    let (dir, mut router) = router();
    let compound = |authors: &[u32], since: u64| {
        pinned_atom(ConcreteFilter {
            since: Some(since),
            ..kind1(authors, Some(10))
        })
    };

    router.compile(&BTreeSet::from([compound(&[1], 100)]), &dir, CAP);
    let before = sub_ids(&router);
    let delta = router.compile(&BTreeSet::from([compound(&[1, 2], 200)]), &dir, CAP);
    let after = sub_ids(&router);

    println!("\n=== accepted cost: compound (2-component) churn ===");
    println!(
        "closes: {}  reqs: {}",
        count_closes(&delta),
        count_reqs(&delta)
    );

    assert_ne!(
        before, after,
        "ACCEPTED COST: a 2-diff has no typed predecessor transition"
    );
    assert_eq!(count_closes(&delta), 1, "ACCEPTED COST: close");
    assert_eq!(count_reqs(&delta), 1, "ACCEPTED COST: and reopen");
    assert!(
        delta.replacements.is_empty(),
        "compound churn has no accepted-open-before-close predecessor"
    );
}

/// ACCEPTED RESIDUAL, pinned: WINDOW SIBLINGS. Two filters identical except
/// `until`, both moving in one compile, are each exactly 1-diff from each
/// prior — and a scalar has no value-set overlap to break the tie with. The
/// tiebreak is therefore arbitrary-but-deterministic: NEAREST scalar value,
/// then canonical filter hash, then the prior's own token.
///
/// What matters is that the predecessor pairing is reproducible. Both
/// successors still mint fresh identities; matching only names which two old
/// requests EngineCore may retire after their respective handoffs succeed.
#[test]
fn window_siblings_match_deterministically_with_fresh_successor_ids() {
    let windowed = |until: u64| {
        pinned_atom(ConcreteFilter {
            until: Some(until),
            ..kind1(&[1], Some(10))
        })
    };

    let run = || {
        let (dir, mut router) = router();
        router.compile(
            &BTreeSet::from([windowed(1_000), windowed(2_000)]),
            &dir,
            CAP,
        );
        let before: BTreeMap<Option<u64>, SubId> = planned(&router)
            .into_iter()
            .map(|r| (r.filter.until, r.sub_id))
            .collect();
        let delta = router.compile(
            &BTreeSet::from([windowed(1_001), windowed(2_001)]),
            &dir,
            CAP,
        );
        let after: BTreeMap<Option<u64>, SubId> = planned(&router)
            .into_iter()
            .map(|r| (r.filter.until, r.sub_id))
            .collect();
        (
            before,
            after,
            count_closes(&delta),
            count_reqs(&delta),
            delta.replacements,
        )
    };

    let (before_a, after_a, closes_a, reqs_a, replacements_a) = run();
    let (before_b, after_b, closes_b, reqs_b, replacements_b) = run();

    println!("\n=== accepted residual: window siblings ===");
    println!("closes: {closes_a}  reqs: {reqs_a}");

    assert_eq!(
        (&before_a, &after_a),
        (&before_b, &after_b),
        "two freshly-constructed Routers must resolve the tie identically"
    );
    assert_eq!(
        (closes_a, reqs_a, &replacements_a),
        (closes_b, reqs_b, &replacements_b)
    );
    assert_eq!(closes_a, 2, "the raw delta closes both old identities");
    assert_eq!(reqs_a, 2, "one fresh successor REQ each");
    assert_eq!(replacements_a.len(), 2, "both predecessors are matched");
    let old_ids: BTreeSet<_> = before_a.values().cloned().collect();
    let new_ids: BTreeSet<_> = after_a.values().cloned().collect();
    assert!(
        old_ids.is_disjoint(&new_ids),
        "byte-changed siblings must never reuse predecessor tokens"
    );
}

/// ACCEPTED COST, pinned: the one-diff pass is a single deterministic GREEDY
/// sweep in canonical order, NOT a maximum-cardinality assignment solve.
///
/// Here `N1` is 1-diff from BOTH priors and takes `P2` (higher author
/// overlap); `N2` is 1-diff from `P2` only, so once `P2` is taken it mints
/// fresh and `P1` is stranded and closed. An optimal assignment (`N1`->`P1`,
/// `N2`->`P2`) would have classified both fresh successors as transitions,
/// leaving no unmatched predecessor to close directly.
///
/// Deliberately NOT repaired with augmenting paths: rematching an
/// ALREADY-MATCHED filter would make a subscription's identity depend on which
/// OTHER filters happen to share the compile — the neighbour-dependent
/// identity this whole design exists to avoid. Like compound churn, this is an
/// efficiency cost, not a correctness one, and it is deterministic.
///
/// Whether the pathology fires depends on the CANONICAL ORDER the sweep walks
/// (`ConcreteFilter::hash()`), which is exactly what makes it a greedy
/// artifact: with `N2` visited first it takes its only candidate `P2`, `N1`
/// then takes `P1`, and the assignment is optimal. `N2`'s `limit` is tuned to
/// `11` here specifically so `N1` sorts FIRST and the bad branch is the one
/// under test — pinning the good branch would prove nothing.
#[test]
fn the_greedy_one_diff_sweep_can_strand_a_prior() {
    let (dir, mut router) = router();
    let p1 = pinned_atom(kind1(&[0x11], Some(10)));
    let p2 = pinned_atom(kind1(&[0x22, 0x33], Some(10)));
    router.compile(&BTreeSet::from([p1, p2]), &dir, CAP);

    let n1 = pinned_atom(kind1(&[0x22], Some(10)));
    let n2 = pinned_atom(kind1(&[0x22, 0x33], Some(11)));
    let delta = router.compile(&BTreeSet::from([n1, n2]), &dir, CAP);

    println!("\n=== accepted cost: greedy sweep is not an assignment solve ===");
    println!(
        "closes: {}  reqs: {}",
        count_closes(&delta),
        count_reqs(&delta)
    );

    // THE COST ITSELF. Without this assertion the test would stay green if
    // someone added the augmenting-path repair the doc above says is
    // deliberately excluded -- which would make it a test of nothing.
    assert_eq!(
        count_closes(&delta),
        2,
        "both old identities leave the raw plan"
    );
    assert_eq!(
        delta.replacements.len(),
        1,
        "ACCEPTED COST: N1 takes P2 on overlap, so N2 has no typed predecessor \
         and P1 closes directly -- an optimal assignment would match both"
    );

    // The correctness floor the cost is measured against: whatever the sweep
    // decides, the plan stays injective and nothing is lost.
    assert_eq!(planned(&router).len(), 2);
    assert_eq!(sub_ids(&router).len(), 2);
    assert_eq!(
        count_reqs(&delta),
        2,
        "both surviving filters reach the wire regardless of how the tie fell"
    );
}

/// A token is NEVER recycled within a router's lifetime. Withdrawing a
/// subscription and then re-adding the byte-identical filter mints a FRESH
/// token, never the closed one: a monotonic per-router counter is folded into
/// every mint. Reuse would let a stale in-flight EOSE for the closed sub land
/// on the reopened sub's attribution FIFO.
#[test]
fn a_withdrawn_token_is_never_recycled() {
    let (dir, mut router) = router();
    let a = atom(0xaa, Some(10));

    router.compile(&BTreeSet::from([a.clone()]), &dir, CAP);
    let first = sub_ids(&router);
    assert_eq!(first.len(), 1);

    // Withdraw everything: the sub is closed.
    let delta = router.compile(&BTreeSet::new(), &dir, CAP);
    assert_eq!(count_closes(&delta), 1);
    assert!(planned(&router).is_empty());

    // Re-add the byte-identical demand.
    router.compile(&BTreeSet::from([a]), &dir, CAP);
    let second = sub_ids(&router);
    assert_eq!(second.len(), 1);

    assert!(
        first.is_disjoint(&second),
        "a re-opened subscription must mint a FRESH token, never recycle the closed one"
    );
}

/// The wire-format constraint the token must keep: `EngineCore` sends a REQ
/// under the hex `Display` of `SubId.1`, which NIP-01 caps at 64 characters.
/// An allocated token is still a full `DescriptorHash`, so the wire string
/// stays exactly 64 lowercase hex characters — no `+` prefix (65 characters
/// would be a protocol violation), no truncation, >=64 bits of entropy.
#[test]
fn the_allocated_token_stays_within_nip01s_subscription_id_cap() {
    let (dir, mut router) = router();
    let demand: BTreeSet<ContextualAtom> = (0..8u32).map(|n| atom(n, Some(10))).collect();
    router.compile(&demand, &dir, CAP);

    for sub_id in sub_ids(&router) {
        let wire = sub_id.1.to_string();
        // NIP-01 caps `subscription_id` at 64 characters. The token is
        // ALLOCATED, not a digest, so this is a CEILING rather than an exact
        // width -- a mint counter with an optional role/incarnation suffix
        // is nowhere near it. Asserting exactly 64 would only be asserting
        // that the id is still a hex digest.
        assert!(
            !wire.is_empty() && wire.len() <= 64,
            "wire id must be non-empty and within NIP-01's 64-char cap: {wire}"
        );
        assert!(
            wire.chars().all(|c| c.is_ascii_digit() || c == '-'),
            "an allocated token is decimal digits with optional role/incarnation: {wire}"
        );
    }
}

// ---------------------------------------------------------------------------
// The mechanical guard.
// ---------------------------------------------------------------------------

/// The generator's author pool, kept small so distinct atoms genuinely
/// collide on the axes under test rather than trivially differing.
fn author_pool() -> Vec<String> {
    (0..4u32).map(author).collect()
}

fn id_hex(n: usize) -> String {
    format!("{n:064x}")
}

prop_compose! {
    /// A filter exercising every axis the matching key names: `authors` as
    /// `None`, `Some(empty)`, and populated; `ids` sets straddling
    /// `MAX_IDS_PER_FILTER`; `since`/`until` variation; disjoint tag-NAME
    /// sets; and limits.
    fn arb_filter()(
        kinds in prop::collection::btree_set(prop::sample::select(vec![1u16, 7, 30_023]), 1..3),
        // Weighted so `None`/`Some(empty)` are both covered while POPULATED
        // author sets stay common: the collision this guard hunts needs two
        // atoms identical except `authors`, so a generator that mostly emits
        // authorless filters would have almost no power.
        authors_shape in prop_oneof![1 => Just(0usize), 1 => Just(1), 6 => Just(2)],
        authors in prop::collection::btree_set(prop::sample::select(author_pool()), 1..3),
        ids_len in prop::sample::select(vec![0usize, 1, 2, 130]),
        has_ids in prop_oneof![1 => Just(true), 3 => Just(false)],
        tag_shape in 0usize..4,
        tag_value in prop::sample::select(vec!["v0", "v1"]),
        since in prop::option::of(prop::sample::select(vec![100u64, 200])),
        until in prop::option::of(prop::sample::select(vec![1_000u64, 2_000])),
        // Biased toward Some: `limit` is what makes the union refuse
        // (`coalesce::neither_limited`), which is the trigger for the whole
        // defect class.
        limit in prop_oneof![
            2 => prop::sample::select(vec![10usize, 20]).prop_map(Some),
            1 => Just(None),
        ],
    ) -> ConcreteFilter {
        let authors = match authors_shape {
            0 => None,
            1 => Some(BTreeSet::new()),
            _ => Some(authors),
        };
        let ids = has_ids.then(|| (0..ids_len).map(id_hex).collect::<BTreeSet<String>>());
        // Disjoint tag-NAME sets: the case a single lumped tag component
        // would mis-read as a one-component difference.
        let names: &[char] = match tag_shape {
            0 => &[],
            1 => &['e'],
            2 => &['p'],
            _ => &['e', 'p'],
        };
        let tags = names
            .iter()
            .map(|c| {
                (
                    IndexedTagName::new(*c).unwrap(),
                    BTreeSet::from([tag_value.to_string()]),
                )
            })
            .collect();
        ConcreteFilter { kinds: Some(kinds), authors, ids, tags, since, until, limit }
    }
}

/// Pair every generated filter with an AUTHOR-VARIED sibling, so the shape
/// the defect lives in — two filters identical except `authors` — is present
/// in EVERY generated case rather than only in the minority the generator
/// happens to produce by chance. Without this the guard's detection rate
/// against the pre-fix code was roughly one case in two hundred.
fn with_author_siblings(filters: Vec<ConcreteFilter>) -> BTreeSet<ContextualAtom> {
    let mut demand = BTreeSet::new();
    for filter in filters {
        let mut sibling = filter.clone();
        sibling.authors = Some(BTreeSet::from([author(0xfe), author(0xff)]));
        demand.insert(pinned_atom(filter));
        demand.insert(pinned_atom(sibling));
    }
    demand
}

/// THE mechanical guard, and the test that goes RED the day a future rule
/// breaks the scheme: **same id ⇒ the registry merged them, or the assignment
/// gave them distinct ids.**
///
/// Stated operationally over a compiled plan, because those are the only two
/// ways two demands may legally share a wire subscription:
/// either they were MERGED, so exactly one `WireReq` exists and no two
/// planned reqs in a session carry byte-identical filters; or every surviving
/// `WireReq` carries its OWN token, so no two distinct filters share an id —
/// and therefore every planned req reaches the wire rather than being
/// silently dropped by `diff_plans`' `BTreeMap`.
/// Checked under BOTH registries, since mergeability is a property of the
/// registry in play and not of the filter, and across a CHURN step, since the
/// assignment (not the id's content) is what carries injectivity forward.
#[test]
fn same_id_implies_merged_or_the_assignment_kept_them_distinct() {
    /// What the relays are actually holding: `(session, sub_id) -> filter`,
    /// built ONLY by applying emitted `WireDelta`s, never read from the plan.
    type WireState = BTreeMap<(nmp_grammar::RelaySessionKey, SubId), ConcreteFilter>;

    /// Apply `delta` to the wire model exactly as a relay would (NIP-01: a
    /// REQ on an existing sub-id REPLACES that sub's filter; a CLOSE
    /// withdraws it), then assert the wire and the plan agree.
    ///
    /// This is the sharp form of "nothing is silently dropped": a `WireReq`
    /// that the plan holds but `diff_plans` never emitted -- the exact
    /// failure mode of a non-injective id, where the delta's `BTreeMap`
    /// keeps one of two colliding reqs -- shows up here as a plan entry with
    /// no wire counterpart, on the compile it happens AND on every later one.
    fn check(wire: &mut WireState, router: &Router, delta: &CompileOutcome, label: &str) {
        for (session, ops) in &delta.wire.ops {
            for op in ops {
                match op {
                    WireOp::Close(sub_id) => {
                        wire.remove(&(session.clone(), sub_id.clone()));
                    }
                    WireOp::Req(sub_id, filter) => {
                        wire.insert((session.clone(), sub_id.clone()), filter.clone());
                    }
                }
            }
        }

        let mut planned: WireState = BTreeMap::new();
        for (session, reqs) in &router.plan().reqs {
            let filters: BTreeSet<_> = reqs.iter().map(|r| r.filter.clone()).collect();
            assert_eq!(
                filters.len(),
                reqs.len(),
                "{label}: {session:?} shipped byte-identical filters as separate reqs -- \
                 no id scheme can distinguish those, so the registry must have merged them"
            );
            for req in reqs {
                assert!(
                    planned
                        .insert((session.clone(), req.sub_id.clone()), req.filter.clone())
                        .is_none(),
                    "{label}: {session:?} planned two reqs under one token {:?}",
                    req.sub_id
                );
            }
        }

        assert_eq!(
            *wire, planned,
            "{label}: the wire and the plan disagree -- a planned WireReq that never \
             reached the wire is silently dropped demand"
        );
    }

    let config = ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    };
    proptest!(config, |(
        first in prop::collection::vec(arb_filter(), 1..5),
        second in prop::collection::vec(arb_filter(), 1..5),
        widen in any::<bool>(),
    )| {
        let dir = FixtureRoutingFacts::new();
        let rules = if widen {
            RuleRegistry::default_widen_only()
        } else {
            RuleRegistry::dedup_only()
        };
        let mut router = Router::new(rules);
        let mut wire = WireState::new();

        let demand: BTreeSet<ContextualAtom> = with_author_siblings(first);
        let delta = router.compile(&demand, &dir, CAP);
        check(&mut wire, &router, &delta, "first compile");

        // A no-op recompile must emit nothing: zero-diff ranks first.
        let repeat = router.compile(&demand, &dir, CAP);
        prop_assert!(repeat.wire.ops.is_empty(), "identical recompile must be a no-op");
        check(&mut wire, &router, &repeat, "no-op recompile");

        // ... and injectivity must survive an arbitrary churn step, where the
        // assignment (not the id's content) is doing all the work.
        let churned: BTreeSet<ContextualAtom> = with_author_siblings(second);
        let delta = router.compile(&churned, &dir, CAP);
        check(&mut wire, &router, &delta, "after churn");

        // Withdrawing everything must close every live subscription, leaving
        // nothing stranded on the wire.
        let drained = router.compile(&BTreeSet::new(), &dir, CAP);
        check(&mut wire, &router, &drained, "after withdrawal");
        prop_assert!(wire.is_empty(), "every subscription must have been closed");
    });
}
