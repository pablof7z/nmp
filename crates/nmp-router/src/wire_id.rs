//! Structural-signature matching: which previously-allocated wire
//! subscription token a newly-compiled filter CONTINUES (#899).
//!
//! Wire ids are allocated opaque tokens, not functions of the filter (see
//! [`crate::plan::SubId::allocate`]). Every byte-identical filter keeps its
//! token. Every byte-changed filter receives a fresh token; structural
//! matching identifies only the predecessor that Core must retire after the
//! fresh request is locally accepted (#774). It never authorizes an in-place
//! overwrite.
//!
//! THE MATCHING KEY is the per-component structural signature defined by
//! [`crate::component`] — `since | until | kinds | authors | ids | one
//! component PER TAG NAME | limit`. That module is shared with
//! [`crate::coalesce`] on purpose; see its doc for why one definition rather
//! than two.
//!
//! A new filter replaces the prior it differs from in EXACTLY ONE component.
//! One property of that rule is local to this module and load-bearing:
//! **zero-diff ranks first.** An unchanged filter must match ITSELF before
//! any one-diff candidate is considered, or a no-op recompile would churn the
//! wire and a subscription's identity would depend on which siblings happen
//! to share the compile.
//!
//! THE SIGNATURE IS COMPARED FIELD-BY-FIELD, not via per-component digests.
//! Both sides are full `ConcreteFilter`s already in memory and the partitions
//! are small, so equality on the fields themselves is exact, cheap, and — the
//! deciding reason — free of the framing ambiguity a hand-rolled per-set
//! digest would introduce (`{"aa","b"}` and `{"a","ab"}` fold identically
//! without length prefixing; see `nmp_grammar::concrete`'s `canonical_encoding`
//! doc for the same hazard at the whole-filter level).
//!
//! ASSIGNMENT, NOT LOOKUP. Each prior token may be assigned to at most ONE
//! new filter. Injectivity of the resulting plan comes from that constraint
//! plus unique minting — never from anything about the token's content.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use nmp_grammar::ConcreteFilter;

use crate::component::{sole_difference, Component};
use crate::plan::SubId;

/// The ordering key that picks ONE prior when several are a one-component
/// continuation of the same new filter: most shared values on the differing
/// component first, then a stable canonical tie-break so the choice never
/// depends on iteration order.
type TieBreak<'a> = (Reverse<usize>, u64, &'a ConcreteFilter, &'a SubId);

/// How strongly `a` and `b` are related ALONG the one component they differ
/// in — the content-grounded tiebreak between several one-diff candidates.
///
/// Returns `(overlap, distance)`, compared as "most overlap first, then least
/// distance". For a SET-valued component, overlap is the size of the shared
/// value set — genuine evidence that one filter is the other's continuation
/// rather than an unrelated selection. A SCALAR component (`since`, `until`,
/// `limit`) has no such evidence, so it falls back to nearest value; a scalar
/// that is absent on either side is maximally distant, since "gained a bound"
/// is not a small move.
fn affinity(component: &Component, a: &ConcreteFilter, b: &ConcreteFilter) -> (usize, u64) {
    fn overlap<T: Ord>(a: Option<&BTreeSet<T>>, b: Option<&BTreeSet<T>>) -> (usize, u64) {
        match (a, b) {
            (Some(a), Some(b)) => (a.intersection(b).count(), 0),
            _ => (0, 0),
        }
    }
    fn nearest(a: Option<u64>, b: Option<u64>) -> (usize, u64) {
        match (a, b) {
            (Some(a), Some(b)) => (0, a.abs_diff(b)),
            _ => (0, u64::MAX),
        }
    }
    match component {
        Component::Kinds => overlap(a.kinds.as_ref(), b.kinds.as_ref()),
        Component::Authors => overlap(a.authors.as_ref(), b.authors.as_ref()),
        Component::Ids => overlap(a.ids.as_ref(), b.ids.as_ref()),
        Component::Tag(name) => overlap(a.tags.get(name), b.tags.get(name)),
        Component::Since => nearest(a.since, b.since),
        Component::Until => nearest(a.until, b.until),
        Component::Limit => nearest(a.limit.map(|l| l as u64), b.limit.map(|l| l as u64)),
    }
}

/// Assign a wire token to every filter in `next`, returning them in `next`'s
/// own order.
///
/// `priors` is the previous plan's `(filter, token)` pairs for THIS matching
/// partition — one relay session and one `ReadRouting`. Anything in
/// `priors` left unassigned simply does not appear in the new plan, so
/// `crate::plan::diff_plans` emits its `Close` with no extra bookkeeping.
///
/// The sweep is deterministic: new filters are considered in canonical hash
/// order, and every candidate comparison terminates in the prior's own token.
///
/// ACCEPTED COST — the one-diff sweep is GREEDY, not a maximum-cardinality
/// assignment solve. A new filter can take a prior that a later new filter
/// needed more, stranding a prior that then closes where an optimal
/// assignment would have paid nothing. This is deliberately not repaired with
/// augmenting paths: re-matching an ALREADY-MATCHED filter would make a
/// subscription's identity depend on which OTHER filters happen to share the
/// compile — the neighbour-dependent identity this design exists to avoid.
/// Like compound churn it costs efficiency, never correctness, and the plan
/// stays injective either way (`the_greedy_one_diff_sweep_can_strand_a_prior`).
pub(crate) fn assign(
    priors: &[(ConcreteFilter, SubId)],
    next: &[ConcreteFilter],
    mut mint: impl FnMut() -> SubId,
) -> Vec<Assignment> {
    let mut taken = vec![false; priors.len()];
    let mut out: Vec<Option<Assignment>> = vec![None; next.len()];

    // Canonical order, independent of how the coalescer happened to emit its
    // survivors, so both the matching decisions and the order in which fresh
    // tokens are minted are reproducible.
    let mut order: Vec<usize> = (0..next.len()).collect();
    order.sort_by(|&i, &j| next[i].cmp(&next[j]).then(i.cmp(&j)));

    // Phase 1 — ZERO-DIFF, and it ranks first unconditionally: a filter that
    // did not change must keep its token, whatever else moved around it.
    for &i in &order {
        let mut best: Option<usize> = None;
        for (p, (prior_filter, prior_sub)) in priors.iter().enumerate() {
            if taken[p] || prior_filter != &next[i] {
                continue;
            }
            if best.is_none_or(|b| prior_sub < &priors[b].1) {
                best = Some(p);
            }
        }
        if let Some(p) = best {
            taken[p] = true;
            out[i] = Some(Assignment {
                sub_id: priors[p].1.clone(),
                predecessor: None,
            });
        }
    }

    // Phase 2 — ONE-DIFF continuation, best affinity on the differing
    // component, then canonical filter hash, then the prior's own token.
    for &i in &order {
        if out[i].is_some() {
            continue;
        }
        let mut best: Option<(usize, TieBreak<'_>)> = None;
        for (p, (prior_filter, prior_sub)) in priors.iter().enumerate() {
            if taken[p] {
                continue;
            }
            let Some(component) = sole_difference(prior_filter, &next[i]) else {
                continue;
            };
            let (overlap, distance) = affinity(&component, prior_filter, &next[i]);
            let key = (Reverse(overlap), distance, prior_filter, prior_sub);
            if best.as_ref().is_none_or(|(_, best_key)| &key < best_key) {
                best = Some((p, key));
            }
        }
        if let Some((p, _)) = best {
            taken[p] = true;
            out[i] = Some(Assignment {
                sub_id: mint(),
                predecessor: Some(priors[p].1.clone()),
            });
        }
    }

    // Phase 3 — everything still unmatched is genuinely new. Minting in
    // canonical order (not `next`'s order) keeps the counter's assignment
    // reproducible too.
    for &i in &order {
        if out[i].is_none() {
            out[i] = Some(Assignment {
                sub_id: mint(),
                predecessor: None,
            });
        }
    }

    out.into_iter()
        .map(|assignment| assignment.expect("every filter is matched or minted by phase 3"))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Assignment {
    pub(crate) sub_id: SubId,
    pub(crate) predecessor: Option<SubId>,
}

