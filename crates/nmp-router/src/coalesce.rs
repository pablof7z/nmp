//! Exact-canonical dedup + the widen-only `MergeRule` registry (M2 plan
//! §2.4, §4.1 step 4).
//!
//! The correctness contract is a single independently-checkable fact
//! (VISION §6 Q1(a)): `matches(try_merge(a,b)) ⊇ matches(a) ∪ matches(b)`
//! for all events. A rule not shown to widen is dropped (graceful
//! degradation): its filters ship as separate REQs. Exact-canonical dedup
//! alone is the trivially-correct floor and is not expressed as a rule.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{fold_context, AccessContext, ConcreteFilter, SourceAuthority};
use nmp_store::CoverageKey;

use crate::route::RouteProvenance;

/// One entry in a relay's not-yet-coalesced bag (#106-widened): a
/// materialized atom's filter, its ORIGIN context, a single-lane
/// provenance, and absorbed coverage keys. `source`/`access` are the
/// equal-context-only coalescing gate (Fable D): two entries never merge
/// -- not even at the exact-canonical dedup floor -- unless their FULL
/// context matches, closing the exact alias atlas flagged (two different-
/// authority atoms sharing a filter must never collapse into one wire REQ
/// / one attribution FIFO).
#[derive(Clone)]
pub(crate) struct CoalesceEntry {
    pub(crate) filter: ConcreteFilter,
    pub(crate) source: SourceAuthority,
    pub(crate) access: AccessContext,
    pub(crate) provenance: Vec<RouteProvenance>,
    pub(crate) absorbed: BTreeSet<CoverageKey>,
}

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
    /// registered rule. Context-free convenience for this module's own
    /// property tests: every entry gets a uniform fixed context
    /// (`AuthorOutboxes`/`Public`), so equal-context-only gating never
    /// fires here and every existing filter-only test keeps its exact
    /// prior behavior.
    pub fn coalesce(&self, filters: BTreeSet<ConcreteFilter>) -> Vec<ConcreteFilter> {
        let entries = filters
            .into_iter()
            .map(|filter| CoalesceEntry {
                filter,
                source: SourceAuthority::AuthorOutboxes,
                access: AccessContext::Public,
                provenance: Vec::new(),
                absorbed: BTreeSet::new(),
            })
            .collect();
        self.coalesce_with(entries)
            .into_iter()
            .map(|entry| entry.filter)
            .collect()
    }

    /// Provenance/coverage-threading variant used by the router: identical
    /// merge decisions to [`Self::coalesce`] (implemented in terms of the
    /// exact same rule set, so the two can never diverge), but concatenates
    /// both the provenance list AND the `absorbed` coverage-key set of every
    /// filter folded into a merge.
    ///
    /// `absorbed` threading is what discharges the coverage-attribution
    /// ruling's containment rule
    /// (`docs/consults/2026-07-11-fable-coverage-attribution.md` §2) at
    /// materialization time: because every rule here is proven widen-only
    /// (`matches(merged) ⊇ matches(a) ∪ matches(b)`), the union of two
    /// atoms' `absorbed` sets is still soundly contained in the merged
    /// filter's matches — the SAME real mechanism that already threads
    /// `provenance` through a merge.
    pub(crate) fn coalesce_with(&self, entries: Vec<CoalesceEntry>) -> Vec<CoalesceEntry> {
        // 1. Exact-canonical dedup, keyed on (filter, source, access) via
        // `fold_context` -- context-aware from the FIRST step, so two
        // entries sharing an identical `ConcreteFilter` but different
        // `SourceAuthority`/`AccessContext` never collapse together even at
        // this trivially-correct floor (#106 anti-alias; Fable D).
        let mut by_hash: BTreeMap<_, CoalesceEntry> = BTreeMap::new();
        for entry in entries {
            let h = fold_context(entry.filter.hash(), entry.source, entry.access);
            by_hash
                .entry(h)
                .and_modify(|existing| {
                    existing.provenance.extend(entry.provenance.clone());
                    existing.absorbed.extend(entry.absorbed.clone());
                })
                .or_insert(entry);
        }
        let mut current: Vec<CoalesceEntry> = by_hash.into_values().collect();

        // 2. Fixed-point pairwise merge across every registered rule --
        // EQUAL-CONTEXT-ONLY (Fable D): two entries are never even offered
        // to a `MergeRule` unless their full context already matches, so
        // every rule's own widen-only proof (which reasons about
        // `ConcreteFilter` pairs alone) stays sound without needing to know
        // about context itself.
        loop {
            let mut merged_once = false;
            'search: for i in 0..current.len() {
                for j in (i + 1)..current.len() {
                    if current[i].source != current[j].source
                        || current[i].access != current[j].access
                    {
                        continue;
                    }
                    for rule in &self.rules {
                        if let Some(merged) = rule.try_merge(&current[i].filter, &current[j].filter)
                        {
                            let mut provenance = current[i].provenance.clone();
                            provenance.extend(current[j].provenance.clone());
                            let mut absorbed = current[i].absorbed.clone();
                            absorbed.extend(current[j].absorbed.clone());
                            let source = current[i].source;
                            let access = current[i].access;
                            let mut next = Vec::with_capacity(current.len() - 1);
                            for (k, entry) in current.into_iter().enumerate() {
                                if k != i && k != j {
                                    next.push(entry);
                                }
                            }
                            next.push(CoalesceEntry {
                                filter: merged,
                                source,
                                access,
                                provenance,
                                absorbed,
                            });
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

    /// #106/Fable D's headline falsifier: two entries that are IDENTICAL as
    /// `ConcreteFilter`s but carry DIFFERENT `SourceAuthority` must ship as
    /// TWO separate wire entries, never merged -- not even at the exact-
    /// canonical dedup floor (the cheapest possible merge). Coalescing is
    /// equal-context-only; this is the exact alias atlas flagged (two
    /// different-authority atoms for the same filter must never share one
    /// wire REQ / one attribution FIFO).
    #[test]
    fn coalesce_with_never_merges_identical_filters_under_different_source_authority() {
        let filter = cf(&[1], &["aa"]);
        let entries = vec![
            CoalesceEntry {
                filter: filter.clone(),
                source: SourceAuthority::AuthorOutboxes,
                access: AccessContext::Public,
                provenance: Vec::new(),
                absorbed: BTreeSet::new(),
            },
            CoalesceEntry {
                filter,
                source: SourceAuthority::Public,
                access: AccessContext::Public,
                provenance: Vec::new(),
                absorbed: BTreeSet::new(),
            },
        ];
        let out = RuleRegistry::default_widen_only().coalesce_with(entries);
        assert_eq!(
            out.len(),
            2,
            "identical filters under different SourceAuthority must never coalesce"
        );
    }

    /// Same filter, same context: the existing exact-dedup floor still
    /// applies unchanged (equal-context-only doesn't mean never-coalesce).
    #[test]
    fn coalesce_with_still_dedups_identical_filter_and_context() {
        let filter = cf(&[1], &["aa"]);
        let entries = vec![
            CoalesceEntry {
                filter: filter.clone(),
                source: SourceAuthority::AuthorOutboxes,
                access: AccessContext::Public,
                provenance: Vec::new(),
                absorbed: BTreeSet::new(),
            },
            CoalesceEntry {
                filter,
                source: SourceAuthority::AuthorOutboxes,
                access: AccessContext::Public,
                provenance: Vec::new(),
                absorbed: BTreeSet::new(),
            },
        ];
        let out = RuleRegistry::default_widen_only().coalesce_with(entries);
        assert_eq!(out.len(), 1, "same filter, same context: still dedups");
    }
}
