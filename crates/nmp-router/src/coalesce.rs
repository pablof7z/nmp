//! Exact-canonical dedup + the widen-only `MergeRule` registry (M2 plan
//! §2.4, §4.1 step 4).
//!
//! The correctness contract is a single independently-checkable fact
//! (VISION §6 Q1(a)): `matches(try_merge(a,b)) ⊇ matches(a) ∪ matches(b)`
//! for all events. A rule not shown to widen is dropped (graceful
//! degradation): its filters ship as separate REQs. Exact-canonical dedup
//! alone is the trivially-correct floor and is not expressed as a rule.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nmp_grammar::{ConcreteFilter, DescriptorHash};
use nmp_store::CoverageKey;

use crate::component::{sole_difference, Component};
use crate::facts::PublicKey;
use crate::plan::DemandKey;
use crate::route::RouteProvenance;

/// One coalesce-in-progress entry: the filter plus the provenance/coverage
/// bookkeeping threaded alongside it through `coalesce_with`'s merges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EntryOwnership {
    pub(crate) coverage_claims: BTreeSet<CoverageKey>,
    pub(crate) owner_demands: BTreeSet<DemandKey>,
    pub(crate) coverage_assignments: BTreeSet<(DemandKey, PublicKey)>,
}

impl EntryOwnership {
    fn extend(&mut self, other: Self) {
        self.coverage_claims.extend(other.coverage_claims);
        self.owner_demands.extend(other.owner_demands);
        self.coverage_assignments.extend(other.coverage_assignments);
    }
}

pub(crate) type Entry = (ConcreteFilter, Vec<RouteProvenance>, EntryOwnership);

/// A widen-only, INTROSPECTABLE merge rule.
pub trait MergeRule {
    fn name(&self) -> &'static str;
    /// `Some(merged)` claims the widening contract for `(a, b)`. `None`
    /// means "I don't apply here". The property test (`merge_rule_widens`)
    /// is what VERIFIES the claim; a rule whose claim doesn't hold is
    /// excluded from [`RuleRegistry::default_widen_only`].
    fn try_merge(&self, a: &ConcreteFilter, b: &ConcreteFilter) -> Option<ConcreteFilter>;
}

/// Maximum event ids carried by one coalesced wire filter. Resolver fan-out
/// produces singleton projected-id atoms; the union packs those atoms up to
/// this bound and then leaves additional chunks as separate REQs.
pub const MAX_IDS_PER_FILTER: usize = 256;

/// Maximum values carried under ONE tag name by one coalesced wire filter.
///
/// Deliberately NOT [`MAX_IDS_PER_FILTER`]: that cap bounds 64-char hex event
/// ids, where 256 values is already ~16KB of filter. Indexed tag values —
/// `d` identifiers, hashtags, coordinates — are short, so 500 values
/// is a comparable frame size, and 500 is the number field experience reports
/// as unremarkable for a relay to accept. Measured against the operational
/// reality this whole module exists to respect: relays cap CONCURRENT
/// SUBSCRIPTIONS at roughly 20 while accepting arrays of ~500 values without
/// complaint (`crates/nmp-router/tests/tag_kill_measurement.rs`).
///
/// Like the id bound, this is operational rather than part of the widening
/// proof: an over-cap union is REFUSED, so the surplus ships as further REQs
/// and no demanded value is ever dropped.
pub const MAX_TAG_VALUES_PER_FILTER: usize = 500;

/// `StructuralUnion` — THE merge rule, derived from the filter's shape
/// rather than named after one field.
///
/// ```text
/// arrays  (kinds, authors, ids, and EACH tag name)  → union, when exactly ONE differs
/// scalars (since, until)                            → must be equal
/// limit                                             → refuse
/// caps                                              → refuse if the result exceeds
/// ```
///
/// It replaces the `AuthorUnion` / `KindUnion` / `IdUnion` trio, which was
/// three copies of one idea plus a missing fourth: nothing merged on tags, so
/// two filters differing only in a `#p` or `#d` value never combined at any
/// scale, and a 300-group catalog compiled to 300 subscriptions per host
/// against a real-world ceiling of ~20
/// (`docs/internals/subscriptions/identity-grouping-and-limits.md` §3.4).
/// Tags stop being a missing fourth rule and become instances of the general
/// case.
///
/// WIDENING. Every component but one is equal between the operands; the one
/// that differs has its constraint replaced by the UNION of both value sets,
/// which is a superset of each. So the merged predicate is weaker on that
/// axis and identical everywhere else:
/// `matches(merged) ⊇ matches(a) ∪ matches(b)`.
///
/// EXACTLY ONE COMPONENT, and this is a hard rule rather than a conservative
/// starting point. Unioning two at once over-widens into cartesian corners:
/// `{k:[1],a:[A]} + {k:[2],a:[B]} → {k:[1,2],a:[A,B]}` also fetches kind 2
/// from A and kind 1 from B, events neither side asked for, and the waste is
/// unbounded on sparse inputs. Ruled out by the owner, not merely deferred.
///
/// TAGS ARE ONE COMPONENT PER NAME. Tags are CONJUNCTIVE across names, so
/// `{#e:X}` and `{#p:Y}` differ in TWO components and are refused. Had they
/// been treated as one "tags" axis, the union would have demanded `#e:X` AND
/// `#p:Y` together — a filter matching NEITHER operand. That is a narrowing,
/// not a widening, and it is the single most dangerous mistake available on
/// this axis.
pub struct StructuralUnion;

impl MergeRule for StructuralUnion {
    fn name(&self) -> &'static str {
        "StructuralUnion"
    }

    fn try_merge(&self, a: &ConcreteFilter, b: &ConcreteFilter) -> Option<ConcreteFilter> {
        // FIRST, and not expressible as a component comparison: a `limit` on
        // EITHER side refuses, even when both carry the SAME limit. Equal
        // limits produce no `Component::Limit` difference, so the match below
        // would never see them -- see `neither_limited`'s doc for why merging
        // them under-fetches.
        if !neither_limited(a, b) {
            return None;
        }

        // `None` means zero components differ (exact duplicates --
        // `coalesce_with`'s hash dedup owns those, and a rule that "merged"
        // them would spin the fixed point) or that two or more do.
        let component = sole_difference(a, b)?;

        let mut merged = a.clone();
        match &component {
            // Scalars must be EQUAL. `since`/`until` are BOUNDS, not value
            // sets: there is no union of two windows that is not either a
            // narrowing or a widening far past both operands, and a filter is
            // co-pinned across its fields (`nmp_grammar::ConcreteFilter`), so
            // widening the window silently changes what the co-pinned value
            // set means.
            Component::Since | Component::Until => return None,
            // UNREACHABLE, and kept as defence in depth: `neither_limited`
            // above already refused any filter carrying a limit, so no pair
            // reaching here can differ in `limit`. Do not delete the check
            // above on the strength of this arm -- two EQUAL `Some(200)`
            // limits are not a differing component and would sail past it.
            Component::Limit => return None,
            Component::Kinds => {
                let (a_kinds, b_kinds) = both_constrain(&a.kinds, &b.kinds)?;
                let mut kinds = a_kinds.clone();
                kinds.extend(b_kinds.iter().copied());
                merged.kinds = Some(kinds);
            }
            Component::Authors => {
                let (a_authors, b_authors) = both_constrain(&a.authors, &b.authors)?;
                let mut authors = a_authors.clone();
                authors.extend(b_authors.iter().cloned());
                merged.authors = Some(authors);
            }
            Component::Ids => {
                let (a_ids, b_ids) = both_constrain(&a.ids, &b.ids)?;
                let mut ids = a_ids.clone();
                ids.extend(b_ids.iter().cloned());
                if ids.len() > MAX_IDS_PER_FILTER {
                    return None;
                }
                merged.ids = Some(ids);
            }
            Component::Tag(name) => {
                // THE POLARITY INVERTS HERE, and getting it backwards
                // reintroduces #900 on a new axis. On `authors`/`kinds`/`ids`
                // the unconstrained shapes are `None` and `Some(∅)`. On tags
                // the unconstrained shape is an ABSENT NAME (a filter that
                // does not mention `#d` matches every event, tagged or not),
                // while a PRESENT name with an empty value set matches
                // NOTHING -- `nostr`'s `match_event` evaluates `any()` over an
                // empty set, which is false for tagged and untagged events
                // alike.
                //
                // So `?` on the lookups is the whole admission test, and it
                // refuses the dangerous end: folding an absent name into a
                // present one would take a filter matching EVERYTHING on that
                // axis and constrain it to a value list. `{#t:∅}` is the
                // harmless end and is deliberately allowed -- it matches
                // nothing, so `matches(a ∪ b) ⊇ ∅ ∪ matches(b)` trivially.
                let a_values = a.tags.get(name)?;
                let b_values = b.tags.get(name)?;
                let mut values = a_values.clone();
                values.extend(b_values.iter().cloned());
                if values.len() > MAX_TAG_VALUES_PER_FILTER {
                    return None;
                }
                merged.tags.insert(*name, values);
            }
        }
        Some(merged)
    }
}

/// The union rule's admission test (#900): a set-valued component may
/// only be UNIONED when BOTH operands actually constrain it — `Some` and
/// non-empty.
///
/// `None` on `authors`/`kinds`/`ids` is not "the empty set", it is NO
/// CONSTRAINT ON THIS AXIS: the filter matches every author / every kind /
/// every id. `unwrap_or_default()` silently converts that into `∅`, so the
/// union of an unconstrained operand with a constrained one came out equal
/// to the constrained one — a filter matching strictly FEWER events than
/// its own first input, and a direct violation of
/// `matches(try_merge(a,b)) ⊇ matches(a) ∪ matches(b)`, this module's only
/// correctness contract. `Some(∅)` is refused for the same reason and not
/// as belt-and-braces: `nostr`'s `match_event` treats an empty
/// authors/kinds/ids set as unconstrained too (measured in
/// `tests/coalescing.rs`), so folding it into a constrained sibling narrows
/// identically.
///
/// Refusing costs nothing. An operand that constrains nothing on the axis is
/// ALREADY a superset of any sibling sharing its skeleton, so the merge
/// bought no wire reduction that was correct to take; the pair simply ships
/// as two REQs, which is this module's documented graceful-degradation
/// behaviour.
fn both_constrain<'a, T: Ord>(
    a: &'a Option<BTreeSet<T>>,
    b: &'a Option<BTreeSet<T>>,
) -> Option<(&'a BTreeSet<T>, &'a BTreeSet<T>)> {
    match (a, b) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => Some((a, b)),
        _ => None,
    }
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

/// `Containment` — the merge rule for the case `StructuralUnion` cannot see:
/// one operand's match set is already a SUBSET of the other's, so the
/// container alone serves both and nothing needs widening at all.
///
/// `StructuralUnion` asks a question about SHAPE — "does exactly one array
/// component differ?" — and uses it as a proxy for a question about MATCH
/// SETS. The proxy is sound where it fires but silent where a subset
/// relationship spans two or more components. Measured case, from
/// `docs/internals/subscriptions/identity-grouping-and-limits.md` §3.2:
/// `{kinds:[0,1], authors:[a,b]}` and `{kinds:[1], authors:[a]}` in one
/// cohort shipped BOTH REQs, one wholly contained in the other, because two
/// components differ. The relay was asked twice for work the first REQ
/// already covers.
///
/// Containment against ALREADY-SENT work was never the gap — `admission.rs`
/// consults per-axis containment against a running incumbent and emits no
/// REQ at all. This rule closes the asymmetry: the same question, asked
/// between two candidates in one cohort.
///
/// WIDENING, in its strongest form. Where `StructuralUnion` returns a
/// filter strictly weaker than both operands, this returns one of the
/// operands unchanged, having established `matches(b) ⊆ matches(a)`. So
/// `matches(merged) = matches(a) ⊇ matches(a) ∪ matches(b)` — the contract
/// holds with EQUALITY, and no event is fetched that the container was not
/// already going to fetch. The cartesian-corner objection that pins
/// `StructuralUnion` to exactly one component does not apply here, because
/// no union is formed: there are no corners to over-widen into.
///
/// The polarity that makes tags dangerous in `StructuralUnion` inverts
/// again here, in the opposite direction, and is worth stating outright.
/// Tags are CONJUNCTIVE across names: a filter that does not mention `#d`
/// matches every event, tagged or not. So for `b` to be contained in `a`,
/// every name `a` constrains must ALSO be constrained by `b`, with a subset
/// of `a`'s values. `b` carrying EXTRA names is fine — each extra name
/// narrows `b` further, which only strengthens the containment. Getting
/// this backwards would treat a filter matching everything on an axis as
/// contained by one matching a single value.
pub struct Containment;

/// `true` when `matches(inner) ⊆ matches(outer)` for every event.
///
/// Set axes (`kinds`/`authors`/`ids`): `None` is UNCONSTRAINED, so an
/// unconstrained `outer` contains any `inner`, while an unconstrained
/// `inner` is contained only by an equally unconstrained `outer`.
///
/// Bounds (`since`/`until`): `None` is the open end. `outer` must admit a
/// window at least as wide as `inner`'s on both sides.
fn contains(outer: &ConcreteFilter, inner: &ConcreteFilter) -> bool {
    fn set_contains<T: Ord>(outer: &Option<BTreeSet<T>>, inner: &Option<BTreeSet<T>>) -> bool {
        match (outer, inner) {
            // Outer constrains nothing on this axis: it admits everything.
            (None, _) => true,
            // Outer constrains, inner does not: inner admits values outer
            // rejects.
            (Some(_), None) => false,
            (Some(o), Some(i)) => i.is_subset(o),
        }
    }

    if !set_contains(&outer.kinds, &inner.kinds)
        || !set_contains(&outer.authors, &inner.authors)
        || !set_contains(&outer.ids, &inner.ids)
    {
        return false;
    }

    // CONJUNCTIVE: every name `outer` constrains must be constrained by
    // `inner` too, with a subset of the values. Extra names on `inner` only
    // narrow it further and are fine.
    for (name, outer_values) in &outer.tags {
        match inner.tags.get(name) {
            Some(inner_values) if inner_values.is_subset(outer_values) => {}
            _ => return false,
        }
    }

    // `since` is an inclusive LOWER bound: a smaller value admits more.
    match (outer.since, inner.since) {
        (None, _) => {}
        (Some(_), None) => return false,
        (Some(o), Some(i)) if o <= i => {}
        _ => return false,
    }
    // `until` is an inclusive UPPER bound: a larger value admits more.
    match (outer.until, inner.until) {
        (None, _) => {}
        (Some(_), None) => return false,
        (Some(o), Some(i)) if o >= i => {}
        _ => return false,
    }

    true
}

impl MergeRule for Containment {
    fn name(&self) -> &'static str {
        "Containment"
    }

    fn try_merge(&self, a: &ConcreteFilter, b: &ConcreteFilter) -> Option<ConcreteFilter> {
        // Same reasoning as `StructuralUnion`, and it must be checked FIRST
        // for the same reason: a `limit` caps DELIVERED ROWS, not the match
        // set. `{kinds:[1], limit:10}` is contained by `{kinds:[0,1]}` as a
        // predicate, but serving both from the container delivers the
        // container's ten newest rows, which need not include ten of kind 1.
        // Containment of predicates is not containment of results.
        if !neither_limited(a, b) {
            return None;
        }

        // Exact duplicates are `coalesce_with`'s hash dedup to own; a rule
        // that "merged" them would spin the fixed point.
        if a == b {
            return None;
        }

        if contains(a, b) {
            Some(a.clone())
        } else if contains(b, a) {
            Some(b.clone())
        } else {
            None
        }
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
/// ([`StructuralUnion`]); `dropped_rules()` reports any rule that was
/// constructed but excluded (graceful-degradation visibility, M2 plan §6).
pub struct RuleRegistry {
    rules: Vec<Arc<dyn MergeRule>>,
    dropped: Vec<&'static str>,
    pair_attempts: Cell<u64>,
}

impl RuleRegistry {
    /// The default, PROVEN-widening registry. Two rules, asking two
    /// different questions:
    ///
    /// - [`Containment`] first — is one operand's match set already a
    ///   SUBSET of the other's? Then the container alone serves both and
    ///   nothing widens. Tried first because it is the cheaper outcome:
    ///   one operand survives unchanged rather than a new union filter.
    /// - [`StructuralUnion`] second — spanning all four array axes; see its
    ///   docs for why it is not three rules plus a missing fourth.
    ///
    /// `StructuralUnion` alone was shape-only, and shape is a proxy for the
    /// match-set question that goes silent whenever a subset relationship
    /// spans more than one component (#1907).
    pub fn default_widen_only() -> Self {
        Self {
            rules: vec![Arc::new(Containment), Arc::new(StructuralUnion)],
            dropped: Vec::new(),
            pair_attempts: Cell::new(0),
        }
    }

    /// An empty registry — exact-canonical dedup only. Used as the
    /// "dedup-only floor" for the M2 kill measurement (test 16).
    pub fn dedup_only() -> Self {
        Self {
            rules: Vec::new(),
            dropped: Vec::new(),
            pair_attempts: Cell::new(0),
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
            self.rules.push(Arc::from(rule));
        } else {
            self.dropped.push(rule.name());
        }
        self
    }

    pub fn dropped_rules(&self) -> &[&'static str] {
        &self.dropped
    }

    pub(crate) fn fork(&self) -> Self {
        Self {
            rules: self.rules.clone(),
            dropped: self.dropped.clone(),
            pair_attempts: Cell::new(0),
        }
    }

    pub(crate) fn pair_attempts(&self) -> u64 {
        self.pair_attempts.get()
    }

    /// Exact-canonical dedup, then fixed-point pairwise merge across every
    /// registered rule.
    pub fn coalesce(&self, filters: BTreeSet<ConcreteFilter>) -> Vec<ConcreteFilter> {
        let entries = filters
            .into_iter()
            .map(|f| (f, Vec::new(), EntryOwnership::default()))
            .collect();
        self.coalesce_with(entries)
            .into_iter()
            .map(|(f, _, _)| f)
            .collect()
    }

    /// Provenance/coverage-threading variant used by the router: identical
    /// merge decisions to [`Self::coalesce`] (implemented in terms of the
    /// exact same rule set, so the two can never diverge), but concatenates
    /// both the provenance list AND the `coverage_claims` coverage-key set of every
    /// filter folded into a merge.
    ///
    /// Deliberately PURE selection-only (#106, Fable D "locus fixed"): this
    /// engine never learns about `ReadRouting` or authenticated identity at all --
    /// equal-context-only coalescing is enforced one level up, by
    /// `Router::compile` partitioning its per-relay bag by `ContextKey`
    /// BEFORE calling this on each partition separately. Two atoms that
    /// happen to land in the same partition (same relay, same context)
    /// coalesce exactly as they always did; atoms in different partitions
    /// are never even offered to this function together, so its own
    /// widen-only proof (which reasons about `ConcreteFilter` pairs alone)
    /// and property tests stay untouched.
    ///
    /// `coverage_claims` threading is what discharges the coverage-attribution
    /// ruling's containment rule
    /// (`docs/design/query-demand-and-evidence.md`) at
    /// materialization time: because every rule here is proven widen-only
    /// (`matches(merged) ⊇ matches(a) ∪ matches(b)`), the union of two
    /// atoms' `coverage_claims` sets is still soundly contained in the merged
    /// filter's matches — the SAME real mechanism that already threads
    /// `provenance` through a merge.
    pub(crate) fn coalesce_with(&self, entries: Vec<Entry>) -> Vec<Entry> {
        // 1. Exact-canonical dedup by hash (the trivially-correct floor).
        let mut by_hash: BTreeMap<DescriptorHash, Entry> = BTreeMap::new();
        for (f, prov, ownership) in entries {
            let h = f.hash();
            by_hash
                .entry(h)
                .and_modify(|(_, p, retained)| {
                    p.extend(prov.clone());
                    retained.extend(ownership.clone());
                })
                .or_insert((f, prov, ownership));
        }
        let mut current: Vec<Entry> = by_hash.into_values().collect();

        // 2. Fixed-point pairwise merge across every registered rule.
        self.merge_fixed_point(&mut current);

        // 3. Exact-canonical dedup AGAIN, over the SURVIVORS. Step 1 alone is
        //    not enough: a merge can RE-CREATE a filter the pool already
        //    holds, and no rule can then remove the duplicate, because
        //    `StructuralUnion` requires EXACTLY ONE component to differ and
        //    byte-identical entries differ in NONE -- they match no rule and
        //    both survive the fixed point. (That refusal is deliberate, not
        //    an oversight: a rule that "merged" a filter with its own twin
        //    would never reach a fixed point.)
        //    Reachable today: the per-author outbox entries `{authors:{a}}`
        //    and `{authors:{b}}` alongside the additive app lane's full-set
        //    entry `{authors:{a,b}}` on one relay; merging the first pair
        //    reproduces the third.
        //
        //    This is a PREREQUISITE for allocated wire tokens (#899), not a
        //    cosmetic tidy. Two byte-identical filters cannot be told apart by
        //    ANY identity scheme, so the only correct outcome is one req.
        //    Under the old derived identity they collided onto one id and
        //    `diff_plans` quietly kept one. Under allocation they would each
        //    get their OWN token and become two permanently-live duplicate
        //    REQs -- the relay double-delivering every matching event forever,
        //    with `coverage_claims` split across two entries so neither is fully
        //    credited. Strictly worse than the bug it replaced.
        //
        //    Order-preserving: each duplicate folds into the FIRST entry
        //    carrying its filter, so the fixed point's own output order (which
        //    the differential oracle pins byte-for-byte) is untouched and the
        //    only change is that duplicates are gone.
        Self::dedup_survivors(&mut current);
        current
    }

    /// Fold byte-identical survivors into their first occurrence, unioning
    /// `provenance` and `coverage_claims`. See `coalesce_with` step 3 for why this
    /// exists and why it must preserve order.
    fn dedup_survivors(entries: &mut Vec<Entry>) {
        let mut first_index: BTreeMap<DescriptorHash, usize> = BTreeMap::new();
        let mut out: Vec<Entry> = Vec::with_capacity(entries.len());
        for (filter, provenance, ownership) in std::mem::take(entries) {
            match first_index.get(&filter.hash()) {
                Some(&first) => {
                    out[first].1.extend(provenance);
                    out[first].2.extend(ownership);
                }
                None => {
                    first_index.insert(filter.hash(), out.len());
                    out.push((filter, provenance, ownership));
                }
            }
        }
        *entries = out;
    }

    /// Advance `current` to the [`StructuralUnion`] fixed point, merging
    /// pairs in EXACTLY the order the original "nested loop, restart
    /// the whole O(n^2) scan from i=0 after every merge" implementation
    /// picked (#505): that loop always merges the FIRST pair `(i, j)`, in
    /// row-major order over the CURRENT array, that any registered rule
    /// accepts, then re-derives that first pair from scratch. Restarting
    /// from `i=0` is what made it O(n^3) (n-1 merges, each paying a fresh
    /// O(n^2) scan) -- but it is NOT simply replaceable by "only compare
    /// the freshly-merged entry against the rest and otherwise carry on",
    /// because a merge can UNLOCK a match between an UNTOUCHED earlier
    /// entry and the freshly-merged one that neither original operand
    /// qualified for. Concretely: merging `{authors:{a}}` and
    /// `{authors:{b}}` on the authors component produces `{authors:{a,b}}`;
    /// a third entry `{kinds:{2}, authors:{a,b}}` is a TWO-component move
    /// from either input alone (their `authors` are `{a}`/`{b}`, not
    /// `{a,b}`, so both `kinds` AND `authors` differ) but a ONE-component
    /// move from the merged entry. The original algorithm would
    /// find this via its next full restart; skipping straight to "only
    /// test the new entry against later entries" would miss it entirely.
    ///
    /// TERMINATION is unchanged by the collapse to one rule: every merge
    /// removes two entries and appends one, so `current.len()` strictly
    /// decreases, and no rule accepts a zero-diff pair (which is what a
    /// self-merge would be).
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
        while settled < current.len() {
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
    /// original's `for rule in &self.rules`), on `(a, b)`, and take the
    /// FIRST that accepts. `default_widen_only()` now holds exactly one
    /// rule, so the iteration is a formality there -- but the registry is
    /// open (`register`), and first-match-wins is the behaviour a caller
    /// adding a candidate rule alongside the default gets.
    fn try_merge_pair(&self, a: &Entry, b: &Entry) -> Option<Entry> {
        self.pair_attempts
            .set(self.pair_attempts.get().saturating_add(1));
        for rule in &self.rules {
            if let Some(merged) = rule.try_merge(&a.0, &b.0) {
                let mut prov = a.1.clone();
                prov.extend(b.1.clone());
                let mut ownership = a.2.clone();
                ownership.extend(b.2.clone());
                return Some((merged, prov, ownership));
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

    use nmp_grammar::IndexedTagName;

    /// Reference oracle for the O(n^2) `merge_fixed_point` above: an exact
    /// copy of the ORIGINAL (pre-#505) `coalesce_with` fixed-point loop,
    /// which restarts the full O(n^2) all-pairs scan from `i=0` after every
    /// successful merge (O(n^3) total). Kept ONLY as a differential-testing
    /// oracle -- never call this outside `#[cfg(test)]`.
    fn naive_coalesce_with(registry: &RuleRegistry, entries: Vec<Entry>) -> Vec<Entry> {
        let mut by_hash: BTreeMap<DescriptorHash, Entry> = BTreeMap::new();
        for (f, prov, ownership) in entries {
            let h = f.hash();
            by_hash
                .entry(h)
                .and_modify(|(_, p, retained)| {
                    p.extend(prov.clone());
                    retained.extend(ownership.clone());
                })
                .or_insert((f, prov, ownership));
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
                            let mut ownership = current[i].2.clone();
                            ownership.extend(current[j].2.clone());
                            let mut next = Vec::with_capacity(current.len() - 1);
                            for (k, entry) in current.into_iter().enumerate() {
                                if k != i && k != j {
                                    next.push(entry);
                                }
                            }
                            next.push((merged, prov, ownership));
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
            .map(|f| (f, Vec::new(), EntryOwnership::default()))
            .collect()
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

    fn name(c: char) -> IndexedTagName {
        IndexedTagName::new(c).expect("test tag names are ASCII letters")
    }

    /// `{kinds:[1], #<tag>: values}` — the shape the resolver's cartesian
    /// fan-out produces for a bound tag field (one value per atom).
    fn cf_tag(tag: char, values: &[&str]) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(Set::from([1u16])),
            tags: BTreeMap::from([(
                name(tag),
                values
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Set<String>>(),
            )]),
            ..ConcreteFilter::default()
        }
    }

    fn tag_values(f: &ConcreteFilter, tag: char) -> Set<String> {
        f.tags.get(&name(tag)).cloned().unwrap_or_default()
    }

    // ---- the fixed point, against its naive oracle ----------------------

    /// The falsifier the #505 fix has to survive: a merge can unlock a match
    /// between an UNTOUCHED earlier entry and a freshly merged one that
    /// neither original operand qualified for. Merging `a` and `b` on the
    /// authors component produces `authors: {a, b}` -- a set that exists
    /// nowhere in the input until that merge happens. `c` carries exactly
    /// that author set already but a different `kinds`, so it is a
    /// TWO-component move from `a` or `b` alone (both `kinds` and `authors`
    /// differ) and a ONE-component move from their merge. An "only compare
    /// the new entry against later entries" shortcut would miss this;
    /// `merge_fixed_point`'s prefix revalidation must not.
    #[test]
    fn incremental_merge_matches_naive_restart_on_cross_axis_unlock() {
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
        // Sanity: the cross-axis unlock actually fires -- everything
        // collapses into ONE filter (kinds {1,2}, authors {a,b}).
        assert_eq!(fast.len(), 1);
        assert_eq!(
            fast[0].0.authors,
            Some(Set::from(["a".to_string(), "b".to_string()]))
        );
        assert_eq!(fast[0].0.kinds, Some(Set::from([1u16, 2u16])));
    }

    /// A bigger fixture exercising ALL FOUR array axes together, including
    /// two shards large enough that a cap forces them to split into multiple
    /// wire filters -- the one place merge ORDER can change which values land
    /// in which final filter (bin-packing). The O(n^2) incremental merge must
    /// reproduce the O(n^3) naive restart's bucketing byte-for-byte, not just
    /// its aggregate shape.
    #[test]
    fn incremental_merge_matches_naive_restart_on_large_fixture() {
        let mut filters: Vec<ConcreteFilter> = Vec::new();

        // 4 disjoint author shards (10 authors each, distinct `kinds`).
        for shard in 0..4u16 {
            for author in 0..10 {
                filters.push(ConcreteFilter {
                    kinds: Some(Set::from([100 + shard])),
                    authors: Some(Set::from([format!("author-{shard}-{author}")])),
                    ..ConcreteFilter::default()
                });
            }
        }

        // A kinds shard: one author, 6 distinct singleton kinds.
        for kind in 0..6u16 {
            filters.push(ConcreteFilter {
                kinds: Some(Set::from([200 + kind])),
                authors: Some(Set::from(["kind-shard-author".to_string()])),
                ..ConcreteFilter::default()
            });
        }

        // An ids shard big enough to force `MAX_IDS_PER_FILTER` to split it.
        for i in 0..(MAX_IDS_PER_FILTER * 2 + 17) {
            filters.push(ConcreteFilter {
                kinds: Some(Set::from([1u16])),
                ids: Some(Set::from([format!("{i:064x}")])),
                ..ConcreteFilter::default()
            });
        }

        // A tag shard big enough to force `MAX_TAG_VALUES_PER_FILTER` to
        // split it, plus a SECOND tag name that must never fold into the
        // first.
        for i in 0..(MAX_TAG_VALUES_PER_FILTER * 2 + 11) {
            filters.push(cf_tag('d', &[&format!("group-{i:05}")]));
        }
        for i in 0..7 {
            filters.push(cf_tag('e', &[&format!("thread-{i:05}")]));
        }

        let entries = entries_of(filters);

        let registry = RuleRegistry::default_widen_only();
        let naive = naive_coalesce_with(&registry, entries.clone());
        let fast = registry.coalesce_with(entries);

        assert_eq!(
            fast, naive,
            "the O(n^2) incremental merge must produce a byte-identical \
             coalesced set (including cap-driven bucketing on ids AND tags) \
             to the original O(n^3) restart-from-scratch algorithm"
        );
    }

    // ---- the rule: what it merges ----------------------------------------

    #[test]
    fn merges_identical_except_authors() {
        let a = cf(&[1], &["aa"]);
        let b = cf(&[1], &["bb"]);
        let merged = StructuralUnion.try_merge(&a, &b).expect("should merge");
        assert_eq!(
            merged.authors,
            Some(Set::from(["aa".to_string(), "bb".to_string()]))
        );
    }

    #[test]
    fn merges_identical_except_kinds() {
        let a = cf(&[1], &["aa"]);
        let b = cf(&[2], &["aa"]);
        let merged = StructuralUnion.try_merge(&a, &b).expect("should merge");
        assert_eq!(merged.kinds, Some(Set::from([1u16, 2u16])));
    }

    #[test]
    fn merges_identical_except_ids() {
        let a = ConcreteFilter {
            ids: Some(Set::from([format!("{:064x}", 1)])),
            ..ConcreteFilter::default()
        };
        let b = ConcreteFilter {
            ids: Some(Set::from([format!("{:064x}", 2)])),
            ..ConcreteFilter::default()
        };
        let merged = StructuralUnion.try_merge(&a, &b).expect("should merge");
        assert_eq!(merged.ids.map(|ids| ids.len()), Some(2));
    }

    /// THE NEW AXIS. Two filters differing only in the value set under one
    /// tag NAME merge into the union of those values -- the case no rule
    /// covered before, and the whole reason a 300-group catalog compiled to
    /// 300 subscriptions.
    #[test]
    fn merges_identical_except_one_tag_names_values() {
        let a = cf_tag('d', &["group-1"]);
        let b = cf_tag('d', &["group-2"]);
        let merged = StructuralUnion.try_merge(&a, &b).expect("should merge");
        assert_eq!(
            tag_values(&merged, 'd'),
            Set::from(["group-1".to_string(), "group-2".to_string()])
        );
        assert_eq!(
            merged.kinds,
            Some(Set::from([1u16])),
            "the untouched components come through unchanged"
        );
    }

    /// A filter carrying TWO tag names merges on the one that differs, and
    /// leaves the shared name exactly as it was.
    #[test]
    fn merges_one_tag_name_while_another_stays_pinned() {
        let mut a = cf_tag('d', &["group-1"]);
        a.tags.insert(name('e'), Set::from(["thread".to_string()]));
        let mut b = cf_tag('d', &["group-2"]);
        b.tags.insert(name('e'), Set::from(["thread".to_string()]));

        let merged = StructuralUnion.try_merge(&a, &b).expect("should merge");
        assert_eq!(
            tag_values(&merged, 'd'),
            Set::from(["group-1".to_string(), "group-2".to_string()])
        );
        assert_eq!(
            tag_values(&merged, 'e'),
            Set::from(["thread".to_string()]),
            "the co-pinned tag name is untouched -- merging it too would be a \
             two-component move"
        );
    }

    // ---- the rule: what it REFUSES ---------------------------------------

    /// The single most dangerous mistake available on this axis. Tags are
    /// CONJUNCTIVE across names, so `{#e:X}` unioned with `{#p:Y}` would
    /// demand BOTH -- a filter matching NEITHER operand. `differing` reports
    /// two components (a name present on one side only is itself a
    /// difference), so the rule refuses.
    #[test]
    fn refuses_two_different_tag_names() {
        let a = cf_tag('e', &["x"]);
        let b = cf_tag('p', &["y"]);
        assert!(StructuralUnion.try_merge(&a, &b).is_none());
        assert!(StructuralUnion.try_merge(&b, &a).is_none());
    }

    /// THE TAG POLARITY, and the inverse of the `authors` case below: an
    /// ABSENT tag name is UNCONSTRAINED (the filter matches every event on
    /// that axis), so folding it into a present name NARROWS.
    #[test]
    fn refuses_an_absent_tag_name_against_a_present_one() {
        let unconstrained = cf(&[1], &[]);
        let bearing = cf_tag('d', &["group-1"]);
        assert!(
            unconstrained.tags.is_empty(),
            "fixture sanity: no tag constraint at all"
        );
        assert!(
            StructuralUnion
                .try_merge(&unconstrained, &bearing)
                .is_none(),
            "a filter with no #d constraint matches EVERY #d -- folding it \
             into {{group-1}} narrows"
        );
        assert!(
            StructuralUnion
                .try_merge(&bearing, &unconstrained)
                .is_none(),
            "refusal must not depend on operand order"
        );
    }

    /// The harmless end of the same axis, and the reason "refuse `None` vs
    /// `Some`" cannot be transplanted onto tags unexamined: a PRESENT tag
    /// name with an EMPTY value set matches NOTHING (`match_event` evaluates
    /// `any()` over an empty set), so unioning it in is a widening and is
    /// allowed.
    #[test]
    fn merges_an_empty_tag_value_set_because_it_matches_nothing() {
        let empty = cf_tag('d', &[]);
        let bearing = cf_tag('d', &["group-1"]);
        let merged = StructuralUnion
            .try_merge(&empty, &bearing)
            .expect("{{#d:∅}} matches nothing, so unioning it in only widens");
        assert_eq!(tag_values(&merged, 'd'), Set::from(["group-1".to_string()]));
    }

    /// #900's exact reported case: an authorless (`authors: None`) atom and
    /// an author-bearing sibling with the same skeleton. Before that fix,
    /// `unwrap_or_default()` read `None` as `∅` and produced
    /// `authors: {aa}` — a filter matching strictly FEWER events than its
    /// own first operand, which matched every author alive. Reachable on
    /// master: a `Public`-sourced pinned atom carries no authors, and #106
    /// explicitly permits an author-bearing atom to declare `Public` too, so
    /// both land in one relay's `ReadRouting::Auto` partition.
    #[test]
    fn refuses_an_unconstrained_authors_operand() {
        let unconstrained = cf(&[1], &[]);
        let bearing = cf(&[1], &["aa"]);
        assert_eq!(unconstrained.authors, None, "fixture sanity");
        assert!(StructuralUnion
            .try_merge(&unconstrained, &bearing)
            .is_none());
        assert!(StructuralUnion
            .try_merge(&bearing, &unconstrained)
            .is_none());
    }

    /// `Some(∅)` is refused for the same reason: `nostr`'s matcher treats an
    /// empty author set as unconstrained, not as "matches nothing". This is
    /// the polarity that INVERTS on tags.
    #[test]
    fn refuses_an_empty_authors_operand() {
        let mut empty = cf(&[1], &["aa"]);
        empty.authors = Some(Set::new());
        let bearing = cf(&[1], &["aa"]);
        assert!(StructuralUnion.try_merge(&empty, &bearing).is_none());
        assert!(StructuralUnion.try_merge(&bearing, &empty).is_none());
    }

    /// Same defect, the `kinds` axis. `kinds: None` means EVERY kind and is
    /// reachable through the FFI filter boundary, which carries `kinds` as an
    /// option and propagates it verbatim.
    #[test]
    fn refuses_an_unconstrained_or_empty_kinds_operand() {
        let bearing = cf(&[1], &["aa"]);
        let mut unconstrained = bearing.clone();
        unconstrained.kinds = None;
        assert!(StructuralUnion
            .try_merge(&unconstrained, &bearing)
            .is_none());
        assert!(StructuralUnion
            .try_merge(&bearing, &unconstrained)
            .is_none());

        let mut empty = bearing.clone();
        empty.kinds = Some(Set::new());
        assert!(StructuralUnion.try_merge(&empty, &bearing).is_none());
        assert!(StructuralUnion.try_merge(&bearing, &empty).is_none());
    }

    /// And the `ids` axis.
    #[test]
    fn refuses_an_unconstrained_or_empty_ids_operand() {
        let bearing = ConcreteFilter {
            ids: Some(Set::from([format!("{:064x}", 1)])),
            ..ConcreteFilter::default()
        };
        let mut empty = bearing.clone();
        empty.ids = Some(Set::new());
        assert!(StructuralUnion.try_merge(&empty, &bearing).is_none());
        assert!(StructuralUnion.try_merge(&bearing, &empty).is_none());

        let mut unconstrained = bearing.clone();
        unconstrained.ids = None;
        assert!(StructuralUnion
            .try_merge(&unconstrained, &bearing)
            .is_none());
    }

    /// TWO components at once is the cartesian-corner refusal, and it is a
    /// hard rule rather than a conservative default: the union
    /// `{k:[1,2], a:[A,B]}` also fetches kind 2 from A and kind 1 from B --
    /// events neither operand asked for, with unbounded waste on sparse
    /// inputs.
    #[test]
    fn refuses_when_two_components_differ() {
        let a = cf(&[1], &["aa"]);
        let b = cf(&[2], &["bb"]);
        assert!(StructuralUnion.try_merge(&a, &b).is_none());
    }

    /// A differing SCALAR is a refusal, not a union: `since`/`until` are
    /// bounds, and no combination of two windows both widens and stays near
    /// either operand.
    #[test]
    fn refuses_when_a_scalar_differs() {
        let a = cf_since(&[1], 100);
        let b = cf_since(&[1], 200);
        assert!(StructuralUnion.try_merge(&a, &b).is_none());

        let mut a = cf(&[1], &["aa"]);
        a.until = Some(100);
        let mut b = cf(&[1], &["aa"]);
        b.until = Some(200);
        assert!(StructuralUnion.try_merge(&a, &b).is_none());

        // ... and a differing scalar does not become mergeable just because
        // an array axis differs too: that is a two-component move.
        let mut a = cf(&[1], &["aa"]);
        a.since = Some(100);
        let mut b = cf(&[1], &["bb"]);
        b.since = Some(200);
        assert!(StructuralUnion.try_merge(&a, &b).is_none());
    }

    /// Byte-identical filters differ in NOTHING, and the rule refuses them.
    /// `coalesce_with`'s hash dedup owns that case; a rule that accepted it
    /// would never reach a fixed point.
    #[test]
    fn refuses_a_zero_diff_pair() {
        let a = cf(&[1], &["aa"]);
        assert!(StructuralUnion.try_merge(&a, &a.clone()).is_none());
    }

    /// The load-bearing regression test for the limit rule: two SAME-limit
    /// filters for disjoint author sets must NOT be merged. An equal limit
    /// produces no `Component::Limit` difference, so the upfront
    /// `neither_limited` check is the ONLY thing standing between this pair
    /// and a merged `{authors:{aa,bb}, limit:200}` -- which a relay truncates
    /// at 200 total rows, silently under-fetching relative to the two
    /// original `limit:200` REQs (up to 400 rows between them).
    #[test]
    fn refuses_to_merge_same_limit_filters() {
        let mut a = cf(&[1], &["aa"]);
        a.limit = Some(200);
        let mut b = cf(&[1], &["bb"]);
        b.limit = Some(200);
        assert!(
            StructuralUnion.try_merge(&a, &b).is_none(),
            "a limited filter must never be merged, even with an identical limit"
        );

        // The same on the tag axis -- the limit guard is axis-independent.
        let mut a = cf_tag('d', &["group-1"]);
        a.limit = Some(10);
        let mut b = cf_tag('d', &["group-2"]);
        b.limit = Some(10);
        assert!(StructuralUnion.try_merge(&a, &b).is_none());
    }

    /// One side limited, the other not: the pair differs in `limit` AND in
    /// its array axis, so it is a two-component move even before
    /// `neither_limited` refuses it. Both guards agree.
    #[test]
    fn refuses_to_merge_when_only_one_side_is_limited() {
        let mut a = cf(&[1], &["aa"]);
        a.limit = Some(200);
        let b = cf(&[1], &["bb"]);
        assert!(StructuralUnion.try_merge(&a, &b).is_none());
        assert!(StructuralUnion.try_merge(&b, &a).is_none());
    }

    // ---- through the registry --------------------------------------------

    /// THE measured gap, verbatim from
    /// `docs/internals/subscriptions/identity-grouping-and-limits.md` §3.2:
    /// `{kinds:[0,1], authors:[a,b]}` and `{kinds:[1], authors:[a]}` in one
    /// cohort produced TWO REQs, "one wholly contained in the other",
    /// because the pair differs in two components and `StructuralUnion`
    /// requires exactly one.
    ///
    /// Nothing about that pair needs widening. The second filter's match set
    /// is already a subset of the first's, so the first alone serves both.
    /// The relay was being asked twice for work one REQ already covered, and
    /// per-relay REQ budget is the scarce resource this whole module exists
    /// to respect.
    #[test]
    fn a_wholly_contained_candidate_ships_no_req_of_its_own() {
        let wide = cf(&[0, 1], &["aa", "bb"]);
        let narrow = cf(&[1], &["aa"]);
        // The premise: two components differ, so StructuralUnion is silent.
        assert!(
            StructuralUnion.try_merge(&wide, &narrow).is_none(),
            "premise: this pair is outside StructuralUnion's one-component domain"
        );

        let out = RuleRegistry::default_widen_only().coalesce(Set::from([wide.clone(), narrow]));
        assert_eq!(out.len(), 1, "one REQ, not two");
        assert_eq!(out[0], wide, "the survivor is the container, unchanged");
    }

    /// Containment must not fire on a pair that merely OVERLAPS. Each of
    /// these admits events the other rejects (`kind:0` by `aa` only matches
    /// the first; `kind:2` by `aa` only the second), so collapsing either
    /// way would drop demanded events -- the #900 failure mode on a new
    /// axis.
    #[test]
    fn overlapping_but_uncontained_filters_do_not_collapse() {
        let a = cf(&[0, 1], &["aa"]);
        let b = cf(&[1, 2], &["aa"]);
        assert!(Containment.try_merge(&a, &b).is_none());
        assert!(Containment.try_merge(&b, &a).is_none());
    }

    /// Tags are CONJUNCTIVE across names, so containment on them runs the
    /// opposite way from the value sets. A filter constraining `#e` is
    /// contained by one constraining nothing on `#e`; a filter constraining
    /// BOTH `#e` and `#p` is contained by one constraining only `#e`,
    /// because each extra name narrows further.
    ///
    /// The inverse must never hold: `{#e:X}` does not contain `{}` on that
    /// axis, and treating it as if it did would collapse a
    /// matches-everything filter into a single-value one.
    #[test]
    fn tag_containment_runs_the_conjunctive_way() {
        let untagged = cf(&[1], &["aa"]);
        let mut e_only = untagged.clone();
        e_only.tags.insert(name('e'), Set::from(["X".to_string()]));
        let mut e_and_p = e_only.clone();
        e_and_p.tags.insert(name('p'), Set::from(["Y".to_string()]));

        // Fewer names = wider, so the untagged filter absorbs both.
        assert_eq!(
            Containment.try_merge(&untagged, &e_only),
            Some(untagged.clone())
        );
        assert_eq!(
            Containment.try_merge(&untagged, &e_and_p),
            Some(untagged.clone())
        );
        // And `#e` alone absorbs `#e` AND `#p`.
        assert_eq!(
            Containment.try_merge(&e_only, &e_and_p),
            Some(e_only.clone())
        );
        // A narrower tag value set never absorbs a wider one.
        let mut e_wide = untagged.clone();
        e_wide.tags.insert(name('e'), Set::from(["X".to_string(), "Z".to_string()]));
        assert_eq!(Containment.try_merge(&e_wide, &e_only), Some(e_wide));
    }

    /// A `limit` caps DELIVERED ROWS, not the match set, so predicate
    /// containment does not imply result containment: serving
    /// `{kinds:[1], limit:10}` from `{kinds:[0,1]}` delivers the
    /// container's ten newest rows, which need not include ten of kind 1.
    /// Refused on either side, exactly as `StructuralUnion` refuses it.
    #[test]
    fn containment_refuses_when_either_side_is_limited() {
        let wide = cf(&[0, 1], &["aa"]);
        let mut narrow = cf(&[1], &["aa"]);
        narrow.limit = Some(10);
        assert!(Containment.try_merge(&wide, &narrow).is_none());
        assert!(Containment.try_merge(&narrow, &wide).is_none());
    }

    /// End to end on the #900 pair: it now COLLAPSES to one REQ, and the
    /// survivor is the UNCONSTRAINED filter.
    ///
    /// #900 was a NARROWING: `AuthorUnion` read `authors: None` as the empty
    /// set and merged `{kinds:[1], authors:None}` with `{kinds:[1],
    /// authors:[aa]}` down to `{authors:[aa]}`, silently dropping every
    /// other author from the wire. The fix at the time was to refuse the
    /// pair, so it shipped as two REQs.
    ///
    /// Refusing was never the only sound answer, just the only one
    /// `StructuralUnion` could express. `Containment` sees that the
    /// authorless filter's match set already contains the other's and keeps
    /// THE WIDE ONE — one REQ, and every event both operands asked for.
    ///
    /// This is a strictly stronger #900 guard than the two-REQ assertion it
    /// replaces: that one proved no narrowing occurred by proving no merge
    /// occurred at all. This proves the merge happens AND lands on the wide
    /// side. A regression to #900's behaviour would surface here as
    /// `authors == Some({"aa"})`.
    #[test]
    fn the_unconstrained_authors_filter_absorbs_the_narrower_one() {
        let filters = Set::from([cf(&[1], &[]), cf(&[1], &["aa"])]);
        let out = RuleRegistry::default_widen_only().coalesce(filters);
        assert_eq!(out.len(), 1, "the narrower filter is wholly contained");
        assert!(
            out[0].authors.is_none(),
            "the survivor must be the UNCONSTRAINED filter, never the \
             narrowed one -- that direction is #900: got {:?}",
            out[0].authors
        );
        assert_eq!(out[0].kinds, Some(Set::from([1u16])));
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
    fn coalesce_dedups_then_unions_author_shards() {
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

    /// The catalog shape, through the registry: 300 singleton `#d` atoms --
    /// exactly what the resolver's cartesian fan-out produces for a derived
    /// tag binding -- become ONE filter carrying all 300 values.
    #[test]
    fn coalesce_folds_a_catalog_of_tag_values_into_one_filter() {
        let filters: Set<ConcreteFilter> = (0..300)
            .map(|i| cf_tag('d', &[&format!("group-{i:04}")]))
            .collect();
        let out = RuleRegistry::default_widen_only().coalesce(filters);
        assert_eq!(out.len(), 1, "300 groups are ONE subscription, not 300");
        assert_eq!(tag_values(&out[0], 'd').len(), 300);
    }

    /// Two tag NAMES stay two filters however many values each carries --
    /// the conjunctive refusal, end to end.
    #[test]
    fn coalesce_keeps_two_tag_names_apart() {
        let mut filters: Set<ConcreteFilter> = (0..10)
            .map(|i| cf_tag('d', &[&format!("group-{i}")]))
            .collect();
        filters.extend((0..10).map(|i| cf_tag('e', &[&format!("thread-{i}")])));

        let out = RuleRegistry::default_widen_only().coalesce(filters);
        assert_eq!(out.len(), 2, "one filter per tag NAME");
        for filter in &out {
            assert_eq!(
                filter.tags.len(),
                1,
                "no filter may demand two tag names at once: {filter:?}"
            );
        }
        assert_eq!(
            out.iter()
                .map(|f| tag_values(f, 'd').len() + tag_values(f, 'e').len())
                .sum::<usize>(),
            20,
            "every value survives somewhere"
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

        // Two filters sharing `kinds`, one bounded BELOW and one bounded
        // ABOVE. Outside StructuralUnion's domain (differing scalars are a
        // refusal, and these differ in two components), and outside
        // Containment's (neither window contains the other: each admits
        // events the other rejects). Squarely inside DiscardSecondOperand's
        // (unsound) applicability predicate, which asks only that `kinds`
        // match. With the rule dropped, both ship as separate REQs --
        // neither is silently discarded.
        //
        // Deliberately NOT two `since` bounds: `since:100` CONTAINS
        // `since:200`, so Containment would legitimately collapse them and
        // this test would pass for a reason that has nothing to do with the
        // drop mechanism it exists to prove.
        let mut lower = cf(&[1], &["aa"]);
        lower.since = Some(100);
        let mut upper = cf(&[1], &["aa"]);
        upper.until = Some(100);
        let filters = Set::from([lower, upper]);
        let out = registry.coalesce(filters);
        assert_eq!(out.len(), 2, "dropped rule must not fire");
    }

    // ---- the caps: chunk, never truncate ---------------------------------

    /// The cap SHARDS rather than drops: every input id reaches some output
    /// filter, and no output filter exceeds the bound.
    #[test]
    fn ids_chunk_at_the_wire_bound_without_losing_a_value() {
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
            expected,
            "the cap must chunk, never truncate"
        );
        assert!(chunk_count_is_provable(
            out.len(),
            expected.len(),
            MAX_IDS_PER_FILTER
        ));
    }

    /// The same on the tag axis, at `MAX_TAG_VALUES_PER_FILTER`.
    #[test]
    fn tag_values_chunk_at_the_wire_bound_without_losing_a_value() {
        let total = MAX_TAG_VALUES_PER_FILTER * 2 + 200;
        let filters: Set<ConcreteFilter> = (0..total)
            .map(|i| cf_tag('d', &[&format!("group-{i:05}")]))
            .collect();

        let out = RuleRegistry::default_widen_only().coalesce(filters);

        let covered: Set<String> = out.iter().flat_map(|f| tag_values(f, 'd')).collect();
        assert_eq!(covered.len(), total, "the cap must chunk, never truncate");
        for filter in &out {
            assert!(
                tag_values(filter, 'd').len() <= MAX_TAG_VALUES_PER_FILTER,
                "a coalesced filter carries {} #d values, over the bound",
                tag_values(filter, 'd').len()
            );
        }
        println!(
            "{total} #d values at a {MAX_TAG_VALUES_PER_FILTER} cap → {} filter(s), sizes {:?}",
            out.len(),
            out.iter()
                .map(|f| tag_values(f, 'd').len())
                .collect::<Vec<_>>()
        );
        assert!(chunk_count_is_provable(
            out.len(),
            total,
            MAX_TAG_VALUES_PER_FILTER
        ));
    }

    /// The provable window on how many chunks a cap-split leaves, and the
    /// reason no test here asserts `⌈n/cap⌉` exactly.
    ///
    /// FLOOR: `⌈n/cap⌉` chunks are needed to carry `n` values at `cap` each.
    ///
    /// CEILING: a TERMINAL state of `merge_fixed_point` has no mergeable
    /// pair left, and the only thing that can refuse two same-axis chunks is
    /// the cap -- so for every pair `|c_i| + |c_j| > cap`, which means at
    /// most ONE chunk holds `cap/2` or fewer values. With `k` chunks over `n`
    /// values that gives `(k-1) * (cap/2 + 1) <= n`.
    ///
    /// The actual number lands inside that window and is an artifact of the
    /// greedy merge ORDER, not of the arithmetic: mutually-mergeable
    /// singletons pair up in a doubling cascade (1→2→4→...), so chunks stall
    /// at the largest power of two that still fits under the cap. At
    /// `MAX_IDS_PER_FILTER = 256` that lands exactly on the cap; at
    /// `MAX_TAG_VALUES_PER_FILTER = 500` it stalls at 256 and leaves real
    /// headroom unused. That is a bin-packing inefficiency, never a
    /// correctness problem -- every value still ships, and the resulting
    /// count is orders of magnitude inside the ~20-subscription relay
    /// ceiling this module exists to respect. Asserting the window rather
    /// than the number keeps these tests honest about which part is proven.
    fn chunk_count_is_provable(chunks: usize, values: usize, cap: usize) -> bool {
        let floor = values.div_ceil(cap);
        let ceiling = values / (cap / 2 + 1) + 1;
        assert!(
            chunks >= floor && chunks <= ceiling,
            "{chunks} chunks for {values} values at cap {cap} is outside the \
             provable window {floor}..={ceiling}"
        );
        true
    }
}
