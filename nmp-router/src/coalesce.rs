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

use crate::route::RouteProvenance;

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
        && a.limit == b.limit
        && a.authors != b.authors
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
            && a.limit == b.limit
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
/// (`AuthorUnion`, `KindUnion`); `dropped_rules()` reports any rule that was
/// constructed but excluded (graceful-degradation visibility, M2 plan §6).
pub struct RuleRegistry {
    rules: Vec<Box<dyn MergeRule>>,
    dropped: Vec<&'static str>,
}

impl RuleRegistry {
    /// The default, PROVEN-widening registry.
    pub fn default_widen_only() -> Self {
        Self {
            rules: vec![Box::new(AuthorUnion), Box::new(KindUnion)],
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
        let entries = filters.into_iter().map(|f| (f, Vec::new())).collect();
        self.coalesce_with(entries)
            .into_iter()
            .map(|(f, _)| f)
            .collect()
    }

    /// Provenance-threading variant used by the router: identical merge
    /// decisions to [`Self::coalesce`] (implemented in terms of the exact
    /// same rule set, so the two can never diverge), but concatenates the
    /// provenance lists of every filter folded into a merge.
    pub(crate) fn coalesce_with(
        &self,
        entries: Vec<(ConcreteFilter, Vec<RouteProvenance>)>,
    ) -> Vec<(ConcreteFilter, Vec<RouteProvenance>)> {
        // 1. Exact-canonical dedup by hash (the trivially-correct floor).
        let mut by_hash: BTreeMap<DescriptorHash, (ConcreteFilter, Vec<RouteProvenance>)> =
            BTreeMap::new();
        for (f, prov) in entries {
            let h = f.hash();
            by_hash
                .entry(h)
                .and_modify(|(_, p)| p.extend(prov.clone()))
                .or_insert((f, prov));
        }
        let mut current: Vec<(ConcreteFilter, Vec<RouteProvenance>)> =
            by_hash.into_values().collect();

        // 2. Fixed-point pairwise merge across every registered rule.
        loop {
            let mut merged_once = false;
            'search: for i in 0..current.len() {
                for j in (i + 1)..current.len() {
                    for rule in &self.rules {
                        if let Some(merged) = rule.try_merge(&current[i].0, &current[j].0) {
                            let mut prov = current[i].1.clone();
                            prov.extend(current[j].1.clone());
                            let mut next = Vec::with_capacity(current.len() - 1);
                            for (k, entry) in current.into_iter().enumerate() {
                                if k != i && k != j {
                                    next.push(entry);
                                }
                            }
                            next.push((merged, prov));
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet as Set;

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
}
