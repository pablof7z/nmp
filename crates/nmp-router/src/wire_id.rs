//! Structural-signature matching: which previously-allocated wire
//! subscription token a newly-compiled filter CONTINUES (#899).
//!
//! Wire ids are allocated opaque tokens, not functions of the filter (see
//! [`crate::plan::SubId::allocate`]). Every compile therefore has to decide,
//! for each filter the coalescer produced, whether it is the continuation of
//! a filter the previous plan already held — in which case it inherits that
//! filter's token and the relay sees ONE overwriting REQ — or something new,
//! in which case a fresh token is minted.
//!
//! THE MATCHING KEY is a per-component structural signature:
//!
//! ```text
//! since | until | kinds | authors | ids | one component PER TAG NAME | limit
//! ```
//!
//! A new filter continues the prior it differs from in EXACTLY ONE component.
//! Two properties of that rule are load-bearing:
//!
//! - **Zero-diff ranks first.** An unchanged filter must match ITSELF before
//!   any one-diff candidate is considered, or a no-op recompile would churn
//!   the wire and a subscription's identity would depend on which siblings
//!   happen to share the compile.
//! - **Tag values are one component PER TAG NAME, never conflated.** Tags are
//!   conjunctive across names (`nmp_grammar::ConcreteFilter::tags`), so one
//!   lumped tag component would make `{#e:X,#p:Y}` and `{#e:X',#p:Y'}` look
//!   like a single-component difference. That is unsound: they are two
//!   unrelated selections, and treating them as a continuation would carry a
//!   subscription across a filter that shares nothing with it.
//!
//! `ids` is a component in its own right: `ConcreteFilter` has FOUR array
//! axes (`kinds`, `authors`, `ids`, `tags`), not three.
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

use nmp_grammar::{ConcreteFilter, DescriptorHash, IndexedTagName};

use crate::plan::SubId;

/// One component of a filter's structural signature. `Tag` is per tag NAME.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Component {
    Since,
    Until,
    Kinds,
    Authors,
    Ids,
    Limit,
    Tag(IndexedTagName),
}

/// Every signature component in which `a` and `b` disagree.
///
/// A field that is `None` in one filter and `Some` in the other DOES differ —
/// including `authors: None` vs `authors: Some(∅)`, which are distinct
/// selections (absent dimension vs a dimension constrained to nothing), and a
/// tag name present in one filter and absent from the other.
pub(crate) fn differing(a: &ConcreteFilter, b: &ConcreteFilter) -> Vec<Component> {
    let mut out = Vec::new();
    if a.since != b.since {
        out.push(Component::Since);
    }
    if a.until != b.until {
        out.push(Component::Until);
    }
    if a.kinds != b.kinds {
        out.push(Component::Kinds);
    }
    if a.authors != b.authors {
        out.push(Component::Authors);
    }
    if a.ids != b.ids {
        out.push(Component::Ids);
    }
    if a.limit != b.limit {
        out.push(Component::Limit);
    }
    // The UNION of both filters' tag names: a name only one side carries is
    // itself a differing component.
    let names: BTreeSet<&IndexedTagName> = a.tags.keys().chain(b.tags.keys()).collect();
    for name in names {
        if a.tags.get(name) != b.tags.get(name) {
            out.push(Component::Tag(name.clone()));
        }
    }
    out
}

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
/// partition — one relay session and one `SourceAuthority`. Anything in
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
) -> Vec<SubId> {
    let mut taken = vec![false; priors.len()];
    let mut out: Vec<Option<SubId>> = vec![None; next.len()];

    // Canonical order, independent of how the coalescer happened to emit its
    // survivors, so both the matching decisions and the order in which fresh
    // tokens are minted are reproducible.
    let mut order: Vec<usize> = (0..next.len()).collect();
    let hashes: Vec<DescriptorHash> = next.iter().map(|f| f.hash()).collect();
    order.sort_by(|&i, &j| hashes[i].cmp(&hashes[j]).then(i.cmp(&j)));

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
            out[i] = Some(priors[p].1.clone());
        }
    }

    // Phase 2 — ONE-DIFF continuation, best affinity on the differing
    // component, then canonical filter hash, then the prior's own token.
    for &i in &order {
        if out[i].is_some() {
            continue;
        }
        let mut best: Option<(usize, (Reverse<usize>, u64, DescriptorHash, &SubId))> = None;
        for (p, (prior_filter, prior_sub)) in priors.iter().enumerate() {
            if taken[p] {
                continue;
            }
            let diff = differing(prior_filter, &next[i]);
            let [component] = diff.as_slice() else {
                continue;
            };
            let (overlap, distance) = affinity(component, prior_filter, &next[i]);
            let key = (Reverse(overlap), distance, prior_filter.hash(), prior_sub);
            if best.as_ref().is_none_or(|(_, best_key)| &key < best_key) {
                best = Some((p, key));
            }
        }
        if let Some((p, _)) = best {
            taken[p] = true;
            out[i] = Some(priors[p].1.clone());
        }
    }

    // Phase 3 — everything still unmatched is genuinely new. Minting in
    // canonical order (not `next`'s order) keeps the counter's assignment
    // reproducible too.
    for &i in &order {
        if out[i].is_none() {
            out[i] = Some(mint());
        }
    }

    out.into_iter()
        .map(|sub| sub.expect("every filter is matched or minted by phase 3"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use nmp_grammar::{AccessContext, SourceAuthority};

    fn cf() -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(BTreeSet::from(["aa".to_string()])),
            ..ConcreteFilter::default()
        }
    }

    fn tag(c: char, values: &[&str]) -> BTreeMap<IndexedTagName, BTreeSet<String>> {
        BTreeMap::from([(
            IndexedTagName::new(c).unwrap(),
            values.iter().map(|s| s.to_string()).collect(),
        )])
    }

    fn token(n: u64) -> SubId {
        SubId::allocate(
            crate::facts::test_relay(0),
            &SourceAuthority::Public,
            AccessContext::Public,
            ConcreteFilter::default().hash(),
            n,
        )
    }

    #[test]
    fn identical_filters_differ_in_nothing() {
        assert!(differing(&cf(), &cf()).is_empty());
    }

    #[test]
    fn each_axis_is_its_own_component() {
        let mut b = cf();
        b.authors = Some(BTreeSet::from(["bb".to_string()]));
        assert_eq!(differing(&cf(), &b), vec![Component::Authors]);

        let mut b = cf();
        b.ids = Some(BTreeSet::from(["cc".to_string()]));
        assert_eq!(
            differing(&cf(), &b),
            vec![Component::Ids],
            "ids is a component in its own right -- ConcreteFilter has FOUR array axes"
        );

        let mut b = cf();
        b.limit = Some(10);
        assert_eq!(differing(&cf(), &b), vec![Component::Limit]);
    }

    /// `authors: None` and `authors: Some(∅)` are DIFFERENT selections: an
    /// absent dimension versus a dimension constrained to nothing.
    #[test]
    fn absent_and_empty_author_sets_differ() {
        let mut none = cf();
        none.authors = None;
        let mut empty = cf();
        empty.authors = Some(BTreeSet::new());
        assert_eq!(differing(&none, &empty), vec![Component::Authors]);
    }

    /// The unsoundness one lumped tag component would introduce: tags are
    /// CONJUNCTIVE across names, so moving `#e` AND `#p` together is a
    /// two-component move, not a one-component continuation.
    #[test]
    fn tag_values_are_one_component_per_tag_name() {
        let mut a = cf();
        a.tags = tag('e', &["x"]);
        a.tags
            .extend(tag('p', &["y"]).into_iter().map(|(k, v)| (k, v)));
        let mut b = cf();
        b.tags = tag('e', &["x2"]);
        b.tags
            .extend(tag('p', &["y2"]).into_iter().map(|(k, v)| (k, v)));

        assert_eq!(
            differing(&a, &b).len(),
            2,
            "{{#e:X,#p:Y}} vs {{#e:X',#p:Y'}} must be TWO components, never one"
        );
    }

    #[test]
    fn a_tag_name_present_on_only_one_side_is_a_difference() {
        let mut b = cf();
        b.tags = tag('e', &["x"]);
        assert_eq!(
            differing(&cf(), &b),
            vec![Component::Tag(IndexedTagName::new('e').unwrap())]
        );
    }

    /// Zero-diff ranks first even when a one-diff candidate would otherwise
    /// have been preferred: the unchanged filter takes its own token.
    #[test]
    fn zero_diff_wins_over_a_one_diff_candidate() {
        let unchanged = cf();
        let mut other = cf();
        other.authors = Some(BTreeSet::from(["bb".to_string()]));

        let priors = vec![(other, token(0)), (unchanged.clone(), token(1))];
        let assigned = assign(&priors, &[unchanged], || panic!("must not mint"));
        assert_eq!(assigned, vec![token(1)]);
    }

    /// Each prior token is assigned to at MOST one new filter -- injectivity
    /// comes from the assignment, not from the token's content.
    #[test]
    fn a_prior_token_is_never_assigned_twice() {
        let mut a = cf();
        a.authors = Some(BTreeSet::from(["a1".to_string()]));
        let mut b = cf();
        b.authors = Some(BTreeSet::from(["b1".to_string()]));

        let priors = vec![(cf(), token(0))];
        let mut next_token = 100;
        let assigned = assign(&priors, &[a, b], || {
            next_token += 1;
            token(next_token)
        });
        assert_eq!(
            assigned.iter().collect::<BTreeSet<_>>().len(),
            2,
            "one prior cannot serve two new filters"
        );
        assert!(
            assigned.contains(&token(0)),
            "one of them continues the prior"
        );
    }

    /// The set-valued tiebreak is content-grounded: the candidate sharing
    /// more values wins, regardless of hash order.
    #[test]
    fn one_diff_ties_break_on_value_set_overlap() {
        let mut shares_two = cf();
        shares_two.authors = Some(BTreeSet::from(["a".to_string(), "b".to_string()]));
        let mut shares_none = cf();
        shares_none.authors = Some(BTreeSet::from(["z".to_string()]));
        let mut new = cf();
        new.authors = Some(BTreeSet::from([
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
        ]));

        let priors = vec![(shares_none, token(0)), (shares_two, token(1))];
        let assigned = assign(&priors, &[new], || panic!("a one-diff candidate exists"));
        assert_eq!(assigned, vec![token(1)], "the overlapping prior wins");
    }

    /// A scalar has no overlap metric, so the tiebreak is nearest value.
    #[test]
    fn scalar_ties_break_on_nearest_value() {
        let windowed = |until: u64| ConcreteFilter {
            until: Some(until),
            ..cf()
        };
        let priors = vec![(windowed(10), token(0)), (windowed(1_000), token(1))];
        let assigned = assign(&priors, &[windowed(1_001)], || panic!("candidates exist"));
        assert_eq!(assigned, vec![token(1)], "1001 is nearest 1000, not 10");
    }

    /// A two-component move is not a continuation -- it mints.
    #[test]
    fn a_two_component_move_mints_a_fresh_token() {
        let mut moved = cf();
        moved.authors = Some(BTreeSet::from(["bb".to_string()]));
        moved.since = Some(500);

        let priors = vec![(cf(), token(0))];
        let assigned = assign(&priors, &[moved], || token(99));
        assert_eq!(assigned, vec![token(99)]);
    }
}
