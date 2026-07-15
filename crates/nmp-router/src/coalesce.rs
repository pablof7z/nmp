//! Exact-canonical dedup + the widen-only `MergeRule` registry (M2 plan
//! §2.4, §4.1 step 4).
//!
//! The correctness contract is a single independently-checkable fact
//! (VISION §6 Q1(a)): `matches(try_merge(a,b)) ⊇ matches(a) ∪ matches(b)`
//! for all events. A rule not shown to widen is dropped (graceful
//! degradation): its filters ship as separate REQs. Exact-canonical dedup
//! alone is the trivially-correct floor and is not expressed as a rule.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ConcreteFilter, DescriptorHash};
use nmp_store::CoverageKey;

use crate::route::RouteProvenance;

/// One coalesce-in-progress entry: the filter plus the provenance/coverage
/// bookkeeping threaded alongside it through `coalesce_with`'s merges.
pub(crate) type Entry = (ConcreteFilter, Vec<RouteProvenance>, BTreeSet<CoverageKey>);

/// A widen-only, INTROSPECTABLE merge rule.
pub trait MergeRule {
    fn name(&self) -> &'static str;
    /// `Some(merged)` claims the widening contract for `(a, b)`. `None`
    /// means "I don't apply here". The property test (`merge_rule_widens`)
    /// is what VERIFIES the claim; a rule whose claim doesn't hold is
    /// excluded from [`RuleRegistry::default_widen_only`].
    fn try_merge(&self, a: &ConcreteFilter, b: &ConcreteFilter) -> Option<ConcreteFilter>;
}

/// `AuthorUnion` — the load-bearing rule. Applies when `a` and `b` are
/// identical in every field except `authors`; merges into the union of both
/// author sets. Trivially widening: adding authors only matches MORE
/// events, never fewer.
pub struct AuthorUnion;

impl MergeRule for AuthorUnion {
    fn name(&self) -> &'static str {
        "AuthorUnion"
    }

    fn try_merge(&self, a: &ConcreteFilter, b: &ConcreteFilter) -> Option<ConcreteFilter> {
        if same_except_authors(a, b) {
            let mut authors = a.authors.clone().unwrap_or_default();
            authors.extend(b.authors.clone().unwrap_or_default());
            if authors.is_empty() {
                return None;
            }
            let mut merged = a.clone();
            merged.authors = Some(authors);
            Some(merged)
        } else {
            None
        }
    }
}

fn same_except_authors(a: &ConcreteFilter, b: &ConcreteFilter) -> bool {
    a.kinds == b.kinds
        && a.ids == b.ids
        && a.tags == b.tags
        && a.since == b.since
        && a.until == b.until
        && neither_limited(a, b)
        && a.authors != b.authors
}

/// Both `a` and `b` carry NO `limit` at all -- NOT merely `a.limit ==
/// b.limit`. A relay-side `limit` caps the RESULT COUNT, not a predicate:
/// two `limit:200` REQs for disjoint author sets each promise up to 200
/// rows (400 total), but a merged `{authors: a∪b, limit:200}` REQ still
/// only promises 200 -- the relay truncates the union, and the union
/// silently under-fetches relative to what the two original REQs would
/// have delivered. `matches(try_merge(a,b)) ⊇ matches(a) ∪ matches(b)`
/// only holds for a bounded-COUNT filter when neither side is bounded at
/// all; requiring equal (rather than absent) limits looked like a safety
/// guard but did not actually save the widening property.
fn neither_limited(a: &ConcreteFilter, b: &ConcreteFilter) -> bool {
    a.limit.is_none() && b.limit.is_none()
}

/// `KindUnion` — an optional, droppable rule. Applies when `a` and `b` are
/// identical in every field except `kinds` (and share the same `authors`
/// identity, so it never accidentally straddles two distinct outbox
/// routes). Trivially widening for the same reason as `AuthorUnion`: a
/// wider `kinds` set only matches more events.
pub struct KindUnion;

impl MergeRule for KindUnion {
    fn name(&self) -> &'static str {
        "KindUnion"
    }

    fn try_merge(&self, a: &ConcreteFilter, b: &ConcreteFilter) -> Option<ConcreteFilter> {
        let same_rest = a.authors == b.authors
            && a.ids == b.ids
            && a.tags == b.tags
            && a.since == b.since
            && a.until == b.until
            && neither_limited(a, b)
            && a.kinds != b.kinds;
        if !same_rest {
            return None;
        }
        let mut kinds = a.kinds.clone().unwrap_or_default();
        kinds.extend(b.kinds.clone().unwrap_or_default());
        if kinds.is_empty() {
            return None;
        }
        let mut merged = a.clone();
        merged.kinds = Some(kinds);
        Some(merged)
    }
}

/// Maximum event ids carried by one coalesced wire filter. Resolver fan-out
/// produces singleton projected-id atoms; `IdUnion` packs those atoms up to
/// this bound and then leaves additional chunks as separate REQs.
pub const MAX_IDS_PER_FILTER: usize = 256;

/// `IdUnion` — identical-except-ids widening with an explicit output cap.
/// The cap is operational, not part of the widening proof: every successful
/// merge still contains the full union of both inputs.
pub struct IdUnion;

impl MergeRule for IdUnion {
    fn name(&self) -> &'static str {
        "IdUnion"
    }

    fn try_merge(&self, a: &ConcreteFilter, b: &ConcreteFilter) -> Option<ConcreteFilter> {
        let (Some(a_ids), Some(b_ids)) = (&a.ids, &b.ids) else {
            return None;
        };
        let same_rest = a.authors == b.authors
            && a.kinds == b.kinds
            && a.tags == b.tags
            && a.since == b.since
            && a.until == b.until
            && neither_limited(a, b)
            && a.ids != b.ids;
        if !same_rest {
            return None;
        }
        let mut ids = a_ids.clone();
        ids.extend(b_ids.iter().cloned());
        if ids.is_empty() || ids.len() > MAX_IDS_PER_FILTER {
            return None;
        }
        let mut merged = a.clone();
        merged.ids = Some(ids);
        Some(merged)
    }
}

/// A rule that is DELIBERATELY non-widening — construction-only, used by
/// `non_widening_rule_is_dropped_and_ships_separately` (test 13) to prove
/// the drop mechanism actually works. It "merges" `a`/`b` by discarding `b`
/// entirely, which drops `b`'s matches — a real widening-contract
/// violation. Not part of any default registry.
pub struct DiscardSecondOperand;

impl MergeRule for DiscardSecondOperand {
    fn name(&self) -> &'static str {
        "DiscardSecondOperand"
    }

    fn try_merge(&self, a: &ConcreteFilter, b: &ConcreteFilter) -> Option<ConcreteFilter> {
        // Deliberately unsound: "merges" any pair sharing the same `kinds`
        // by silently discarding `b`, regardless of every other field. If
        // `b` matched events `a` didn't, those matches are lost --
        // `matches(merged) ⊇ matches(a) ∪ matches(b)` fails whenever
        // `a != b`. Exists ONLY to prove the drop mechanism (test 13).
        if a.kinds == b.kinds && a != b {
            Some(a.clone())
        } else {
            None
        }
    }
}

/// The merge-rule registry. `default_widen_only()` contains only rules
/// whose widening claim has been independently property-tested green
/// (`AuthorUnion`, `KindUnion`, `IdUnion`); `dropped_rules()` reports any rule that was
/// constructed but excluded (graceful-degradation visibility, M2 plan §6).
pub struct RuleRegistry {
    rules: Vec<Box<dyn MergeRule>>,
    dropped: Vec<&'static str>,
}

impl RuleRegistry {
    /// The default, PROVEN-widening registry.
    pub fn default_widen_only() -> Self {
        Self {
            rules: vec![
                Box::new(AuthorUnion),
                Box::new(KindUnion),
                Box::new(IdUnion),
            ],
            dropped: Vec::new(),
        }
    }

    /// An empty registry — exact-canonical dedup only. Used as the
    /// "dedup-only floor" for the M2 kill measurement (test 16).
    pub fn dedup_only() -> Self {
        Self {
            rules: Vec::new(),
            dropped: Vec::new(),
        }
    }

    /// Register `rule`; if `verified_widening` is false, the rule is
    /// recorded as dropped (its name surfaces via `dropped_rules()`) and
    /// never actually applied — this is the drop mechanism test 13
    /// exercises directly, and it is how a builder wires in a candidate
    /// rule whose widening property test came back red without shipping an
    /// unproven merge.
    pub fn register(mut self, rule: Box<dyn MergeRule>, verified_widening: bool) -> Self {
        if verified_widening {
            self.rules.push(rule);
        } else {
            self.dropped.push(rule.name());
        }
        self
    }

    pub fn dropped_rules(&self) -> &[&'static str] {
        &self.dropped
    }

    /// Exact-canonical dedup, then fixed-point pairwise merge across every
    /// registered rule.
    pub fn coalesce(&self, filters: BTreeSet<ConcreteFilter>) -> Vec<ConcreteFilter> {
        let entries = filters
            .into_iter()
            .map(|f| (f, Vec::new(), BTreeSet::new()))
            .collect();
        self.coalesce_with(entries)
            .into_iter()
            .map(|(f, _, _)| f)
            .collect()
    }

    /// Provenance/coverage-threading variant used by the router: identical
    /// merge decisions to [`Self::coalesce`] (implemented in terms of the
    /// exact same rule set, so the two can never diverge), but concatenates
    /// both the provenance list AND the `absorbed` coverage-key set of every
    /// filter folded into a merge.
    ///
    /// Deliberately PURE selection-only (#106, Fable D "locus fixed"): this
    /// engine never learns about `SourceAuthority`/`AccessContext` at all --
    /// equal-context-only coalescing is enforced one level up, by
    /// `Router::compile` partitioning its per-relay bag by `ContextKey`
    /// BEFORE calling this on each partition separately. Two atoms that
    /// happen to land in the same partition (same relay, same context)
    /// coalesce exactly as they always did; atoms in different partitions
    /// are never even offered to this function together, so its own
    /// widen-only proof (which reasons about `ConcreteFilter` pairs alone)
    /// and property tests stay untouched.
    ///
    /// `absorbed` threading is what discharges the coverage-attribution
    /// ruling's containment rule
    /// (`docs/consults/2026-07-11-fable-coverage-attribution.md` §2) at
    /// materialization time: because every rule here is proven widen-only
    /// (`matches(merged) ⊇ matches(a) ∪ matches(b)`), the union of two
    /// atoms' `absorbed` sets is still soundly contained in the merged
    /// filter's matches — the SAME real mechanism that already threads
    /// `provenance` through a merge.
    pub(crate) fn coalesce_with(&self, entries: Vec<Entry>) -> Vec<Entry> {
        // 1. Exact-canonical dedup by hash (the trivially-correct floor).
        let mut by_hash: BTreeMap<DescriptorHash, Entry> = BTreeMap::new();
        for (f, prov, absorbed) in entries {
            let h = f.hash();
            by_hash
                .entry(h)
                .and_modify(|(_, p, a)| {
                    p.extend(prov.clone());
                    a.extend(absorbed.clone());
                })
                .or_insert((f, prov, absorbed));
        }
        let mut current: Vec<Entry> = by_hash.into_values().collect();

        // 2. Fixed-point pairwise merge across every registered rule.
        self.merge_fixed_point(&mut current);
        current
    }

    /// Advance `current` to the AuthorUnion/KindUnion/IdUnion fixed point,
    /// merging pairs in EXACTLY the order the original "nested loop, restart
    /// the whole O(n^2) scan from i=0 after every merge" implementation
    /// picked (#505): that loop always merges the FIRST pair `(i, j)`, in
    /// row-major order over the CURRENT array, that any registered rule
    /// accepts, then re-derives that first pair from scratch. Restarting
    /// from `i=0` is what made it O(n^3) (n-1 merges, each paying a fresh
    /// O(n^2) scan) -- but it is NOT simply replaceable by "only compare
    /// the freshly-merged entry against the rest and otherwise carry on",
    /// because a rule can unlock a match between an UNTOUCHED earlier
    /// entry and the freshly-merged one that neither original operand
    /// qualified for. Concretely: `AuthorUnion` merging `{authors:{a}}` and
    /// `{authors:{b}}` produces `{authors:{a,b}}`; a third entry
    /// `{kinds:{2}, authors:{a,b}}` cannot `KindUnion` with either input
    /// alone (their `authors` are `{a}`/`{b}`, not `{a,b}`) but CAN
    /// `KindUnion` with the merged entry. The original algorithm would
    /// find this via its next full restart; skipping straight to "only
    /// test the new entry against later entries" would miss it entirely.
    ///
    /// So every entry before the current merge point genuinely has to be
    /// re-offered against each newly merged entry -- this function does
    /// exactly that, and ONLY that (an O(n) check per merge, O(n) merges
    /// => O(n^2) total), instead of re-running the full O(n^2) scan on
    /// every merge (=> O(n^3) total).
    ///
    /// Invariant maintained throughout (`settled`): `current[0..settled]`
    /// is pairwise merge-free AND merge-free against every entry in
    /// `current[settled..]` -- exactly the invariant the original nested
    /// loop already had (by the time its outer `i` reaches `settled`,
    /// every `i' < settled` has been scanned against every `j' > i'`,
    /// including `i`'s own row, with no merge ever found). We only ever
    /// attempt a merge at `(settled, j)`, mirroring the original's `i`; a
    /// freshly merged entry is always appended at the tail (mirroring the
    /// original's rebuild), so `j` sweeping up to it is what naturally
    /// re-tests row `settled` against it, and
    /// `revalidate_prefix_against_tail` is what re-tests the settled
    /// PREFIX against it (the one comparison the natural `j` sweep can
    /// never reach, since rows `< settled` are never revisited by `j`).
    fn merge_fixed_point(&self, current: &mut Vec<Entry>) {
        let mut settled = 0usize;
        let mut j = settled + 1;
        while settled + 1 <= current.len() {
            if j >= current.len() {
                // Row `settled` fully scanned against everything currently
                // present, no merge found: it can never merge with anything
                // that already exists (unchanged entries stay unchanged),
                // and any FUTURE new entry is re-offered to it by
                // `revalidate_prefix_against_tail`. Move to the next row.
                settled += 1;
                j = settled + 1;
                continue;
            }
            if let Some(merged) = self.try_merge_pair(&current[settled], &current[j]) {
                Self::apply_merge(current, settled, j, merged);
                // The new tail entry has never been offered to the settled
                // prefix (only to whatever it gets compared against as `j`
                // sweeps row `settled` again below) -- do that first, since
                // in row-major order any prefix match outranks continuing
                // row `settled`.
                self.revalidate_prefix_against_tail(current, &mut settled);
                // Whatever now occupies position `settled` (post-removal
                // shift, post-revalidation) has never been tested against
                // the rest of its row -- restart the row's `j` sweep.
                j = settled + 1;
                continue;
            }
            j += 1;
        }
    }

    /// Try every registered rule, in registration order (matching the
    /// original's `for rule in &self.rules`), on `(a, b)`. The three
    /// default rules' domains are mutually exclusive on any given pair
    /// (each requires a DIFFERENT single field to be the one that
    /// differs, with every other field -- including whether the pair
    /// differs on that rule's field at all -- required equal), so at most
    /// one can ever match; the order is kept anyway for exact parity with
    /// the original loop.
    fn try_merge_pair(&self, a: &Entry, b: &Entry) -> Option<Entry> {
        for rule in &self.rules {
            if let Some(merged) = rule.try_merge(&a.0, &b.0) {
                let mut prov = a.1.clone();
                prov.extend(b.1.clone());
                let mut absorbed = a.2.clone();
                absorbed.extend(b.2.clone());
                return Some((merged, prov, absorbed));
            }
        }
        None
    }

    /// Remove `current[i]` and `current[j]` (`i < j`) and push `merged`
    /// onto the tail -- the same "remove both, append the merge result at
    /// the end" shape the original rebuild (`next.push(entry)` for
    /// `k != i && k != j`, then `next.push(merged)`) produced, so a
    /// freshly merged entry always lands in the same relative position
    /// (the very end) that the original algorithm would have put it in.
    fn apply_merge(current: &mut Vec<Entry>, i: usize, j: usize, merged: Entry) {
        debug_assert!(i < j);
        current.remove(j);
        current.remove(i);
        current.push(merged);
    }

    /// After a merge produces a new tail entry, re-offer it against the
    /// SETTLED prefix (`current[0..*settled]`), lowest index first, exactly
    /// like a from-scratch row-major restart would re-discover it (the
    /// settled prefix was cleared against everything that existed before
    /// this merge, but never against this brand-new entry). A match here
    /// consumes a prefix member and produces yet another new tail entry,
    /// so this loops until the (shrinking) prefix is clear against the
    /// (ever-changing) tail -- each iteration is a real merge, so this
    /// terminates in at most `*settled` steps.
    fn revalidate_prefix_against_tail(&self, current: &mut Vec<Entry>, settled: &mut usize) {
        let mut k = 0;
        while k < *settled {
            let tail = current.len() - 1;
            if let Some(merged) = self.try_merge_pair(&current[k], &current[tail]) {
                Self::apply_merge(current, k, tail, merged);
                *settled -= 1;
                k = 0;
            } else {
                k += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet as Set;

    /// Reference oracle for the O(n^2) `merge_fixed_point` above: an exact
    /// copy of the ORIGINAL (pre-#505) `coalesce_with` fixed-point loop,
    /// which restarts the full O(n^2) all-pairs scan from `i=0` after every
    /// successful merge (O(n^3) total). Kept ONLY as a differential-testing
    /// oracle -- never call this outside `#[cfg(test)]`.
    fn naive_coalesce_with(registry: &RuleRegistry, entries: Vec<Entry>) -> Vec<Entry> {
        let mut by_hash: BTreeMap<DescriptorHash, Entry> = BTreeMap::new();
        for (f, prov, absorbed) in entries {
            let h = f.hash();
            by_hash
                .entry(h)
                .and_modify(|(_, p, a)| {
                    p.extend(prov.clone());
                    a.extend(absorbed.clone());
                })
                .or_insert((f, prov, absorbed));
        }
        let mut current: Vec<Entry> = by_hash.into_values().collect();

        loop {
            let mut merged_once = false;
            'search: for i in 0..current.len() {
                for j in (i + 1)..current.len() {
                    for rule in &registry.rules {
                        if let Some(merged) = rule.try_merge(&current[i].0, &current[j].0) {
                            let mut prov = current[i].1.clone();
                            prov.extend(current[j].1.clone());
                            let mut absorbed = current[i].2.clone();
                            absorbed.extend(current[j].2.clone());
                            let mut next = Vec::with_capacity(current.len() - 1);
                            for (k, entry) in current.into_iter().enumerate() {
                                if k != i && k != j {
                                    next.push(entry);
                                }
                            }
                            next.push((merged, prov, absorbed));
                            current = next;
                            merged_once = true;
                            break 'search;
                        }
                    }
                }
            }
            if !merged_once {
                break;
            }
        }
        current
    }

    fn entries_of(filters: Vec<ConcreteFilter>) -> Vec<Entry> {
        filters
            .into_iter()
            .map(|f| (f, Vec::new(), Set::new()))
            .collect()
    }

    /// The falsifier the #505 fix has to survive: a rule can unlock a match
    /// between an UNTOUCHED earlier entry and a freshly merged one that
    /// neither original operand qualified for. `AuthorUnion(a, b)` produces
    /// `authors: {a, b}` -- a set that exists nowhere in the input until
    /// that merge happens. `c` carries exactly that author set already (but
    /// a different `kinds`), so it cannot `KindUnion` with `a` or `b` alone
    /// (their `authors` are `{a}`/`{b}`, not `{a,b}`), only with their
    /// merge. An "only compare the new entry against later entries"
    /// shortcut would miss this; `merge_fixed_point`'s prefix revalidation
    /// must not.
    #[test]
    fn incremental_merge_matches_naive_restart_on_cross_rule_unlock() {
        let a = cf(&[1], &["a"]);
        let b = cf(&[1], &["b"]);
        let c = ConcreteFilter {
            kinds: Some(Set::from([2u16])),
            authors: Some(Set::from(["a".to_string(), "b".to_string()])),
            ..ConcreteFilter::default()
        };
        let entries = entries_of(vec![a, b, c]);

        let registry = RuleRegistry::default_widen_only();
        let naive = naive_coalesce_with(&registry, entries.clone());
        let fast = registry.coalesce_with(entries);

        assert_eq!(fast, naive);
        // Sanity: the cross-rule unlock actually fires -- everything
        // collapses into ONE filter (kinds {1,2}, authors {a,b}).
        assert_eq!(fast.len(), 1);
        assert_eq!(
            fast[0].0.authors,
            Some(Set::from(["a".to_string(), "b".to_string()]))
        );
        assert_eq!(fast[0].0.kinds, Some(Set::from([1u16, 2u16])));
    }

    /// A bigger fixture exercising all three rules together, including an
    /// `IdUnion` shard large enough that the `MAX_IDS_PER_FILTER` cap forces
    /// it to split into multiple wire filters -- the one place merge ORDER
    /// can change which ids land in which final filter (bin-packing). The
    /// O(n^2) incremental merge must reproduce the O(n^3) naive restart's
    /// bucketing byte-for-byte, not just its aggregate shape.
    #[test]
    fn incremental_merge_matches_naive_restart_on_large_fixture() {
        let mut filters: Vec<ConcreteFilter> = Vec::new();

        // 4 disjoint AuthorUnion shards (10 authors each, distinct `kinds`).
        for shard in 0..4u16 {
            for author in 0..10 {
                filters.push(ConcreteFilter {
                    kinds: Some(Set::from([100 + shard])),
                    authors: Some(Set::from([format!("author-{shard}-{author}")])),
                    ..ConcreteFilter::default()
                });
            }
        }

        // A KindUnion shard: one author, 6 distinct singleton kinds.
        for kind in 0..6u16 {
            filters.push(ConcreteFilter {
                kinds: Some(Set::from([200 + kind])),
                authors: Some(Set::from(["kind-shard-author".to_string()])),
                ..ConcreteFilter::default()
            });
        }

        // An IdUnion shard big enough to force the cap to split it.
        for i in 0..(MAX_IDS_PER_FILTER * 2 + 17) {
            filters.push(ConcreteFilter {
                kinds: Some(Set::from([1u16])),
                ids: Some(Set::from([format!("{i:064x}")])),
                ..ConcreteFilter::default()
            });
        }

        let entries = entries_of(filters);

        let registry = RuleRegistry::default_widen_only();
        let naive = naive_coalesce_with(&registry, entries.clone());
        let fast = registry.coalesce_with(entries);

        assert_eq!(
            fast, naive,
            "the O(n^2) incremental merge must produce a byte-identical \
             coalesced set (including IdUnion's cap-driven bucketing) to \
             the original O(n^3) restart-from-scratch algorithm"
        );
    }

    fn cf(kinds: &[u16], authors: &[&str]) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(kinds.iter().copied().collect()),
            authors: if authors.is_empty() {
                None
            } else {
                Some(authors.iter().map(|s| s.to_string()).collect())
            },
            ..ConcreteFilter::default()
        }
    }

    fn cf_since(kinds: &[u16], since: u64) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(kinds.iter().copied().collect()),
            since: Some(since),
            ..ConcreteFilter::default()
        }
    }

    #[test]
    fn author_union_merges_identical_except_authors() {
        let a = cf(&[1], &["aa"]);
        let b = cf(&[1], &["bb"]);
        let merged = AuthorUnion.try_merge(&a, &b).expect("should merge");
        assert_eq!(
            merged.authors,
            Some(Set::from(["aa".to_string(), "bb".to_string()]))
        );
    }

    #[test]
    fn author_union_refuses_when_other_fields_differ() {
        let a = cf(&[1], &["aa"]);
        let b = cf(&[2], &["bb"]);
        assert!(AuthorUnion.try_merge(&a, &b).is_none());
    }

    /// The load-bearing regression test for this fix: two SAME-limit
    /// filters for disjoint author sets must NOT be merged. Before this
    /// fix, `same_except_authors` accepted `a.limit == b.limit` as a
    /// "safety guard" and merged them anyway into one filter that still
    /// carries the same limit -- a relay serving `{authors:{aa,bb},
    /// limit:200}` truncates at 200 total rows, silently under-fetching
    /// relative to the two original `limit:200` REQs (up to 400 rows
    /// between them). Excluding ANY limited filter from the union rules
    /// entirely is what actually preserves
    /// `matches(try_merge(a,b)) ⊇ matches(a) ∪ matches(b)`.
    #[test]
    fn author_union_refuses_to_merge_same_limit_filters() {
        let mut a = cf(&[1], &["aa"]);
        a.limit = Some(200);
        let mut b = cf(&[1], &["bb"]);
        b.limit = Some(200);
        assert!(
            AuthorUnion.try_merge(&a, &b).is_none(),
            "a limited filter must never be merged, even with an identical limit"
        );
    }

    /// Same falsifier, `KindUnion`'s domain.
    #[test]
    fn kind_union_refuses_to_merge_same_limit_filters() {
        let mut a = cf(&[1], &["aa"]);
        a.limit = Some(50);
        let mut b = cf(&[2], &["aa"]);
        b.limit = Some(50);
        assert!(
            KindUnion.try_merge(&a, &b).is_none(),
            "a limited filter must never be merged, even with an identical limit"
        );
    }

    /// End-to-end through the registry: two limited, otherwise-mergeable
    /// filters ship as TWO separate REQs (each keeping its own `limit`),
    /// never coalesced into one truncating REQ.
    #[test]
    fn coalesce_never_merges_limited_filters_even_with_matching_limits() {
        let mut a = cf(&[1], &["aa"]);
        a.limit = Some(10);
        let mut b = cf(&[1], &["bb"]);
        b.limit = Some(10);
        let filters = Set::from([a, b]);
        let out = RuleRegistry::default_widen_only().coalesce(filters);
        assert_eq!(
            out.len(),
            2,
            "limited filters must ship as separate REQs, never merged"
        );
        assert!(out.iter().all(|f| f.limit == Some(10)));
    }

    #[test]
    fn kind_union_merges_identical_except_kinds() {
        let a = cf(&[1], &["aa"]);
        let b = cf(&[2], &["aa"]);
        let merged = KindUnion.try_merge(&a, &b).expect("should merge");
        assert_eq!(merged.kinds, Some(Set::from([1u16, 2u16])));
    }

    #[test]
    fn coalesce_dedups_then_author_unions_shards() {
        let filters = Set::from([cf(&[1], &["aa"]), cf(&[1], &["bb"]), cf(&[1], &["dd"])]);
        let out = RuleRegistry::default_widen_only().coalesce(filters);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].authors,
            Some(Set::from([
                "aa".to_string(),
                "bb".to_string(),
                "dd".to_string()
            ]))
        );
    }

    #[test]
    fn coalesce_exact_duplicate_yields_one_req() {
        let filters = Set::from([cf(&[1], &["aa"]), cf(&[1], &["aa"])]);
        assert_eq!(filters.len(), 1, "BTreeSet already dedups identical values");
        let out = RuleRegistry::default_widen_only().coalesce(filters);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn dedup_only_registry_never_merges() {
        let filters = Set::from([cf(&[1], &["aa"]), cf(&[1], &["bb"])]);
        let out = RuleRegistry::dedup_only().coalesce(filters);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn register_with_verified_false_drops_the_rule_without_applying_it() {
        let registry = RuleRegistry::default_widen_only().register(
            Box::new(DiscardSecondOperand),
            false, // the widen property test failed for this rule
        );
        assert_eq!(registry.dropped_rules(), &["DiscardSecondOperand"]);

        // Two filters sharing `kinds` but differing in `since` -- outside
        // AuthorUnion/KindUnion's domain (both require every other field
        // equal), but squarely inside DiscardSecondOperand's (unsound)
        // applicability predicate. With the rule dropped, both ship as
        // separate REQs -- neither is silently discarded.
        let filters = Set::from([cf_since(&[1], 100), cf_since(&[1], 200)]);
        let out = registry.coalesce(filters);
        assert_eq!(out.len(), 2, "dropped rule must not fire");
    }

    #[test]
    fn id_union_chunks_projected_singletons_at_the_wire_bound() {
        let filters: Set<ConcreteFilter> = (0..(MAX_IDS_PER_FILTER * 2 + 17))
            .map(|i| ConcreteFilter {
                kinds: Some(Set::from([1])),
                ids: Some(Set::from([format!("{i:064x}")])),
                ..ConcreteFilter::default()
            })
            .collect();
        let expected: Set<String> = filters
            .iter()
            .flat_map(|filter| filter.ids.clone().unwrap_or_default())
            .collect();

        let out = RuleRegistry::default_widen_only().coalesce(filters);

        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|filter| {
            filter
                .ids
                .as_ref()
                .is_some_and(|ids| ids.len() <= MAX_IDS_PER_FILTER)
        }));
        assert_eq!(
            out.iter()
                .flat_map(|filter| filter.ids.clone().unwrap_or_default())
                .collect::<Set<_>>(),
            expected
        );
    }
}
