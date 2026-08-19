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

use nmp_grammar::ConcreteFilter;
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
                kinds.extend(b_kinds.iter().cloned());
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
    /// The default, PROVEN-widening registry. ONE rule, spanning all four
    /// array axes — see [`StructuralUnion`] for why it is not three rules
    /// plus a missing fourth.
    pub fn default_widen_only() -> Self {
        Self {
            rules: vec![Arc::new(StructuralUnion)],
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
        // 1. Exact dedup by filter VALUE (the trivially-correct floor).
        let mut by_filter: BTreeMap<ConcreteFilter, Entry> = BTreeMap::new();
        for (f, prov, ownership) in entries {
            by_filter
                .entry(f.clone())
                .and_modify(|(_, p, retained)| {
                    p.extend(prov.clone());
                    retained.extend(ownership.clone());
                })
                .or_insert((f, prov, ownership));
        }
        let mut current: Vec<Entry> = by_filter.into_values().collect();

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
        let mut first_index: BTreeMap<ConcreteFilter, usize> = BTreeMap::new();
        let mut out: Vec<Entry> = Vec::with_capacity(entries.len());
        for (filter, provenance, ownership) in std::mem::take(entries) {
            match first_index.get(&filter) {
                Some(&first) => {
                    out[first].1.extend(provenance);
                    out[first].2.extend(ownership);
                }
                None => {
                    first_index.insert(filter.clone(), out.len());
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

