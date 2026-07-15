//! [`Router`] — the entry point (M2 plan §2.7, §4.1). Full-recompile-then-
//! diff, not delta-threading: `compile` recomputes the whole per-relay plan
//! from the engine's CURRENT demand set each call, diffs it against the
//! previous plan, stores the new plan + diagnostics, and returns the
//! surgical wire delta. This also discharges M1 nit #2 by construction: a
//! withdrawn atom simply vanishes from `demand`, so the next `compile`
//! emits its `Close` (see `dropped_handle_close_reaches_wire`, test 15).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    AccessContext, ConcreteFilter, ContextualAtom, RoutingEvidence, SourceAuthority,
};
use nmp_store::{coverage_key, CoverageKey};

use crate::coalesce::RuleRegistry;
use crate::diag::{self, Diagnostics};
use crate::facts::{DiscoveryKinds, PubkeyHex, RelayDirectory, RelayUrl};
use crate::plan::{diff_plans, RelayPlan, SubId, WireDelta, WireReq};
use crate::route::{self, AtomClass, RouteKind, RouteProvenance, Skeleton};
use crate::solver::{self, CoverageInput, Shortfall, ShortfallReason};

/// The equal-context-only coalescing gate (Fable D, "locus fixed"): two
/// atoms only ever share wire work if their FULL context matches. Bagged
/// and coalesced entirely inside `Router::compile` -- `coalesce.rs` itself
/// stays PURE selection-only and never learns this type exists.
type ContextKey = (SourceAuthority, AccessContext);

/// One relay's not-yet-coalesced bag entry, PER context partition: a
/// materialized (filter, single-lane provenance, absorbed coverage-key)
/// triple -- selection-only, exactly what `coalesce.rs::coalesce_with`
/// (unchanged, context-free) has always taken.
type BagEntry = (ConcreteFilter, Vec<RouteProvenance>, BTreeSet<CoverageKey>);

/// Apply the ONE whole-demand relay ceiling after every routing lane has
/// materialized. The previous implementation handed the full `cap` to each
/// outbox skeleton independently and then added indexer/app/fallback/pinned
/// relays outside those solves, so the assembled plan could exceed `cap` by
/// an arbitrary factor.
///
/// Selection is deterministic and coverage-biased: the relay carrying the
/// most typed route facts wins, with the canonical relay URL as the stable
/// tie-break. Refused relays are removed from the only bag that can become a
/// [`RelayPlan`], and every absorbed atom they would have served is retained
/// as explicit local-limit evidence. This is intentionally conservative: if
/// a cap removes an additive or redundant planned source, the demand still
/// reports that local limit instead of pretending the smaller plan was the
/// complete requested acquisition.
fn apply_global_relay_cap(
    bag: &mut BTreeMap<RelayUrl, BTreeMap<ContextKey, Vec<BagEntry>>>,
    cap: usize,
    uncovered_authors: &mut BTreeMap<PubkeyHex, Shortfall>,
) -> (BTreeSet<CoverageKey>, BTreeSet<RelayUrl>) {
    if bag.len() <= cap {
        return (BTreeSet::new(), BTreeSet::new());
    }

    let mut ranked: Vec<(RelayUrl, usize)> = bag
        .iter()
        .map(|(relay, by_context)| {
            let route_facts = by_context
                .values()
                .flatten()
                .map(|(_, provenance, absorbed)| provenance.len().max(absorbed.len()).max(1))
                .sum();
            (relay.clone(), route_facts)
        })
        .collect();
    ranked.sort_by(|(a_url, a_score), (b_url, b_score)| {
        b_score.cmp(a_score).then_with(|| a_url.cmp(b_url))
    });

    let selected: BTreeSet<RelayUrl> = ranked
        .iter()
        .take(cap)
        .map(|(relay, _)| relay.clone())
        .collect();
    let refused: BTreeSet<RelayUrl> = bag
        .keys()
        .filter(|relay| !selected.contains(*relay))
        .cloned()
        .collect();

    let mut limited = BTreeSet::new();
    let mut cap_limited_authors = BTreeSet::new();
    for relay in &refused {
        if let Some(by_context) = bag.get(relay) {
            for (_, provenance, absorbed) in by_context.values().flatten() {
                limited.extend(absorbed.iter().copied());
                for route in provenance {
                    if route.route_kind == RouteKind::OutboxSolved {
                        cap_limited_authors.extend(route.covers_authors.iter().cloned());
                    }
                }
            }
        }
    }

    // Preserve the router diagnostic's historical per-author floor while
    // moving cap enforcement to the assembled plan. Intrinsic no-candidate /
    // fewer-than-k evidence from the uncapped solve remains more specific
    // and is never overwritten by this cap-derived fact.
    for author in cap_limited_authors {
        if uncovered_authors.contains_key(&author) {
            continue;
        }
        let achieved: BTreeSet<RelayUrl> = selected
            .iter()
            .filter(|relay| {
                bag.get(*relay).is_some_and(|by_context| {
                    by_context.values().flatten().any(|(_, provenance, _)| {
                        provenance.iter().any(|route| {
                            route.route_kind == RouteKind::OutboxSolved
                                && route.covers_authors.contains(&author)
                        })
                    })
                })
            })
            .cloned()
            .collect();
        uncovered_authors.insert(
            author,
            Shortfall {
                requested_k: 2,
                achieved: achieved.len(),
                reason: ShortfallReason::CapExhausted,
            },
        );
    }

    bag.retain(|relay, _| selected.contains(relay));
    (limited, refused)
}

/// Push `(filter, provenance, coverage_key(atom))` into `bag[relay][ctx]`
/// for every `(relay, provenance)` pair in `routes` — the shared
/// materialization step `compile` uses for every lane. A no-op when
/// `routes` is empty (no configured relays for that lane, or the lane's
/// gate didn't fire).
fn push_routes(
    bag: &mut BTreeMap<RelayUrl, BTreeMap<ContextKey, Vec<BagEntry>>>,
    filter: &ConcreteFilter,
    source: &SourceAuthority,
    access: AccessContext,
    routes: Vec<(RelayUrl, RouteProvenance)>,
) {
    if routes.is_empty() {
        return;
    }
    let key = coverage_key(&ContextualAtom {
        filter: filter.clone(),
        source: source.clone(),
        access,
        routing_evidence: BTreeSet::new(),
    });
    for (relay, prov) in routes {
        bag.entry(relay)
            .or_default()
            .entry((source.clone(), access))
            .or_default()
            .push((filter.clone(), vec![prov], BTreeSet::from([key])));
    }
}

pub struct Router {
    discovery: DiscoveryKinds,
    rules: RuleRegistry,
    prev_plan: RelayPlan,
    last_diag: Diagnostics,
}

impl Router {
    pub fn new(discovery: DiscoveryKinds, rules: RuleRegistry) -> Self {
        Self {
            discovery,
            rules,
            prev_plan: RelayPlan::default(),
            last_diag: Diagnostics::default(),
        }
    }

    /// THE entry point. Recompile the whole per-relay plan from `demand`,
    /// diff vs the previous plan, store the new plan + diagnostics, return
    /// the surgical wire delta.
    pub fn compile(
        &mut self,
        demand: &BTreeSet<ContextualAtom>,
        dir: &dyn RelayDirectory,
        cap: usize,
    ) -> WireDelta {
        // Step 1: group demand by (Skeleton, AccessContext) (outbox) /
        // classify pinned -- classification is now by DECLARED
        // `SourceAuthority` (#106), never by filter shape alone. Grouping by
        // `AccessContext` alongside the skeleton keeps the seam ready for a
        // future non-`Public` access variant (#8's NIP-42 AUTH) without
        // needing a second widening later; every atom reaching this branch
        // shares `source: AuthorOutboxes` by construction (that's the
        // `classify` arm that produced it), so it isn't tracked per-group.
        let mut outbox_groups: BTreeMap<
            (Skeleton, AccessContext),
            BTreeMap<PubkeyHex, BTreeSet<RoutingEvidence>>,
        > = BTreeMap::new();
        let mut pinned_atoms: BTreeMap<ConcreteFilter, BTreeSet<RoutingEvidence>> = BTreeMap::new();
        // #107: query-declared `SourceAuthority::Pinned(relays)` atoms — kept
        // in their OWN collection, never merged into `pinned_atoms` (the
        // directory-fact `Public`-sourced kind), since these must skip every
        // additive lane (indexer/app/fallback) below, not just the solve.
        let mut explicit_pinned_atoms: Vec<(ConcreteFilter, AccessContext, BTreeSet<RelayUrl>)> =
            Vec::new();
        for atom in demand {
            match route::classify(&atom.filter, &atom.source) {
                AtomClass::Outbox { skeleton, authors } => {
                    let group = outbox_groups.entry((skeleton, atom.access)).or_default();
                    for author in authors {
                        group
                            .entry(author)
                            .or_default()
                            .extend(atom.routing_evidence.iter().cloned());
                    }
                }
                AtomClass::Pinned => {
                    pinned_atoms
                        .entry(atom.filter.clone())
                        .or_default()
                        .extend(atom.routing_evidence.iter().cloned());
                }
                AtomClass::ExplicitPinned(relays) => {
                    explicit_pinned_atoms.push((atom.filter.clone(), atom.access, relays));
                }
            }
        }

        // Step 2 + 3: route (coverage-solve outbox groups / pinned lookup),
        // apply the additive indexer/app/fallback lanes OUTSIDE the solve
        // (Unit B, `routing-and-ownership.md` §2.1/§2.2 — never counted
        // toward `k`), and materialize each relay's bag of (filter,
        // context, single-lane provenance, absorbed) entries. `absorbed` is
        // the coverage-attribution ruling's per-atom `CoverageKey` (§2):
        // each entry here is exactly one pre-coalesce demand atom (one
        // author, for outbox; the full/shortfall author set, for an
        // additive lane; the pinned atom itself, for pinned), so it
        // contributes exactly one key, later unioned by `coalesce_with`
        // alongside provenance as same-skeleton, SAME-CONTEXT atoms merge
        // (Fable D: equal-context-only).
        let mut bag: BTreeMap<RelayUrl, BTreeMap<ContextKey, Vec<BagEntry>>> = BTreeMap::new();
        let mut uncovered_authors: BTreeMap<PubkeyHex, Shortfall> = BTreeMap::new();

        for ((skeleton, access), evidence_by_author) in &outbox_groups {
            let access = *access;
            let source = SourceAuthority::AuthorOutboxes;
            let authors: BTreeSet<PubkeyHex> = evidence_by_author.keys().cloned().collect();
            let candidates = route::build_candidates(&authors, dir);
            let mut candidates = candidates;
            route::add_projected_candidates(&mut candidates, evidence_by_author);
            let coverage = solver::solve(&CoverageInput {
                candidates: candidates.clone(),
                k: 2,
                // Per-skeleton limiting is the defect #20 removes. Build
                // each skeleton's honest k-cover first; the ONE assembled-
                // plan ceiling below accounts for every skeleton and every
                // additive/pinned lane together.
                //
                // #505 asked whether threading the real (or a "generous
                // multiple" of the) whole-demand `cap` in here instead of
                // `usize::MAX` would bound `solver::solve`'s greedy loop
                // without changing the assembled plan. It would not, and is
                // deliberately NOT done:
                //   1. `solve`'s iteration count is already bounded by
                //      `sum(per-author ceilings) <= k * authors_in_group`
                //      (`k` is 2 here) regardless of `cap` -- every
                //      iteration's selected relay must satisfy at least one
                //      outstanding (author, slot) need, or the loop exits
                //      via the "no candidate relay helps" branch. So any
                //      `cap` at or above `2 * authors_in_group` is a no-op
                //      (no perf change), and the O(authors^2) cost the
                //      issue flags is the per-iteration O(authors *
                //      candidates) rescan, not iteration count.
                //   2. Any `cap` BELOW that natural bound stops the solve
                //      before every author reaches `k`, for exactly the
                //      relay-diverse (low-overlap) follow sets that make
                //      this slow in the first place -- reintroducing the
                //      truncation defect #20 removed, since a later skeleton
                //      or additive lane might have had global-cap headroom
                //      this skeleton never got to use, changing both the
                //      shortfall diagnostics and the wire plan.
                // A real fix would make the per-iteration scores rescan
                // incremental instead of touching `cap`; out of scope here.
                cap: usize::MAX,
            });
            uncovered_authors.extend(coverage.shortfall.clone());

            for (relay, prov) in route::provenance_for_outbox(&coverage, &candidates) {
                let filter = skeleton.with_authors(prov.covers_authors.clone());
                let key = coverage_key(&ContextualAtom {
                    filter: filter.clone(),
                    source: source.clone(),
                    access,
                    routing_evidence: evidence_by_author
                        .get(prov.covers_authors.first().expect("one-author route"))
                        .cloned()
                        .unwrap_or_default(),
                });
                bag.entry(relay)
                    .or_default()
                    .entry((source.clone(), access))
                    .or_default()
                    .push((filter, vec![prov], BTreeSet::from([key])));
            }

            // Additive indexer + app lanes: both route the group's FULL
            // author set, so they share the same (filter, key).
            let mut additive = route::indexer_lane_routes(dir, &self.discovery, skeleton, &authors);
            additive.extend(route::app_lane_routes(dir, &authors));
            push_routes(
                &mut bag,
                &skeleton.with_authors(authors.clone()),
                &source,
                access,
                additive,
            );

            // Additive fallback lane: routes exactly the shortfall authors,
            // iff no appRelay is configured. `Coverage.shortfall` above has
            // already recorded the shortfall regardless of whether this
            // lane fires — fallback is a lane, not coverage.
            let shortfall_authors: BTreeSet<PubkeyHex> =
                coverage.shortfall.keys().cloned().collect();
            let fallback = route::fallback_lane_routes(dir, &shortfall_authors);
            push_routes(
                &mut bag,
                &skeleton.with_authors(shortfall_authors),
                &source,
                access,
                fallback,
            );
        }

        for (atom, routing_evidence) in &pinned_atoms {
            // #106's closed vocabulary has only one directory-fact non-outbox
            // source (`Public`) and one `AccessContext` (`Public`), so a
            // fixed context here is exact today, not a placeholder — #107's
            // `SourceAuthority::Pinned(relays)` is query-declared, not a
            // directory fact, and is routed entirely separately below
            // (`explicit_pinned_atoms`), never through this loop.
            let source = SourceAuthority::Public;
            let access = AccessContext::Public;
            let key = coverage_key(&ContextualAtom {
                filter: atom.clone(),
                source: source.clone(),
                access,
                routing_evidence: routing_evidence.clone(),
            });
            let mut routes = route::provenance_for_pinned(atom, dir);
            routes.extend(route::provenance_for_projected(routing_evidence));
            for (relay, prov) in routes {
                bag.entry(relay)
                    .or_default()
                    .entry((source.clone(), access))
                    .or_default()
                    .push((atom.clone(), vec![prov], BTreeSet::from([key])));
            }

            // App lane routes every atom, including authorless/pinned ones
            // (closes #7 — the authorless-routing-lane gap).
            let app = route::app_lane_routes(dir, &BTreeSet::new());
            push_routes(&mut bag, atom, &source, access, app);
        }

        // #107: explicit, query-declared pinned wire authority — route
        // DIRECTLY to the Demand's own relay set. NO additive lane
        // (indexer/app/fallback) is ever applied here: that's the #107
        // Contract's core guarantee ("Pinned author filters never contact
        // directory, author-outbox, app, fallback, or indexer relays").
        for (filter, access, relays) in &explicit_pinned_atoms {
            let source = SourceAuthority::Pinned(relays.clone());
            let key = coverage_key(&ContextualAtom {
                filter: filter.clone(),
                source: source.clone(),
                access: *access,
                routing_evidence: BTreeSet::new(),
            });
            for (relay, prov) in route::provenance_for_explicit_pinned(relays) {
                bag.entry(relay)
                    .or_default()
                    .entry((source.clone(), *access))
                    .or_default()
                    .push((filter.clone(), vec![prov], BTreeSet::from([key])));
            }
        }

        // Step 4: enforce the ONE whole-demand ceiling over the fully
        // materialized bag. Nothing removed here can reach coalescing, the
        // plan, or the wire; its contextual coverage keys remain as exact
        // local-limit evidence.
        let (limited, refused_relays) =
            apply_global_relay_cap(&mut bag, cap, &mut uncovered_authors);

        // Step 5 + 6: per relay, PER CONTEXT PARTITION, dedup + widen-only
        // coalesce (`coalesce.rs` stays pure selection-only, Fable D "locus
        // fixed" -- partitioning by `ContextKey` here is what makes
        // coalescing equal-context-only, never a change to the rule
        // engine itself), then assign stable sub-ids (context-folded,
        // `SubId::for_wire` — atlas's 3rd proof floor / Fable D's wire
        // consequence).
        let mut reqs: BTreeMap<RelayUrl, Vec<WireReq>> = BTreeMap::new();
        for (relay, by_context) in bag {
            let mut relay_reqs: Vec<WireReq> = Vec::new();
            for ((source, access), entries) in by_context {
                let merged = self.rules.coalesce_with(entries);
                relay_reqs.extend(merged.into_iter().map(|(filter, provenance, absorbed)| {
                    let sub_id = SubId::for_wire(relay.clone(), &filter, &source, access);
                    WireReq {
                        sub_id,
                        filter,
                        provenance,
                        absorbed,
                    }
                }));
            }
            relay_reqs.sort_by(|a, b| a.sub_id.cmp(&b.sub_id));
            reqs.insert(relay, relay_reqs);
        }

        let next_plan = RelayPlan {
            reqs,
            limited,
            refused_relays,
        };

        // Step 7: diff vs previous plan.
        let delta = diff_plans(&self.prev_plan, &next_plan);

        self.last_diag = diag::build(
            &next_plan,
            uncovered_authors,
            self.rules.dropped_rules().to_vec(),
        );
        self.prev_plan = next_plan;
        delta
    }

    pub fn diagnostics(&self) -> &Diagnostics {
        &self.last_diag
    }

    pub fn plan(&self) -> &RelayPlan {
        &self.prev_plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{test_relay, FixtureDirectory, Lane};
    use crate::solver::ShortfallReason;
    use nmp_grammar::RoutingEvidenceKind;

    fn pk(c: char) -> PubkeyHex {
        c.to_string().repeat(64)
    }

    fn cf(kind: u16, authors: &[&str]) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            authors: Some(authors.iter().map(|s| s.to_string()).collect()),
            ..ConcreteFilter::default()
        }
    }

    /// Wrap an already-built filter into a demand atom under `source` (fixed
    /// `access: Public` -- these tests don't exercise the access axis).
    fn as_atom(filter: ConcreteFilter, source: SourceAuthority) -> ContextualAtom {
        ContextualAtom {
            filter,
            source,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        }
    }

    /// An `AuthorOutboxes`-sourced demand atom -- what `Demand::from_filter`
    /// would produce for any `cf(...)` above (its `authors` is always
    /// `Some`), spelled out explicitly since these tests build
    /// `ContextualAtom`s directly rather than through a `Demand`.
    fn outbox(kind: u16, authors: &[&str]) -> ContextualAtom {
        as_atom(cf(kind, authors), SourceAuthority::AuthorOutboxes)
    }

    /// A `Public`-sourced demand atom for an already-built (typically
    /// authorless) filter.
    fn pinned(filter: ConcreteFilter) -> ContextualAtom {
        as_atom(filter, SourceAuthority::Public)
    }

    #[test]
    fn outbox_maps_authors_to_own_write_relays() {
        let dir = FixtureDirectory::new()
            .with_write(pk('a'), [test_relay(0), test_relay(1)])
            .with_write(pk('b'), [test_relay(2), test_relay(3)]);
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        let demand = BTreeSet::from([
            outbox(1, &[pk('a').as_str()]),
            outbox(1, &[pk('b').as_str()]),
        ]);
        let _ = router.compile(&demand, &dir, 10);
        let plan = router.plan();
        assert!(plan.reqs.contains_key(&test_relay(0)));
        assert!(plan.reqs.contains_key(&test_relay(1)));
        assert!(plan.reqs.contains_key(&test_relay(2)));
        assert!(plan.reqs.contains_key(&test_relay(3)));
        for req in &plan.reqs[&test_relay(0)] {
            assert!(req.provenance.iter().all(|p| p.lane == Lane::Nip65Write));
        }
    }

    #[test]
    fn per_relay_diff_is_surgical() {
        let dir = FixtureDirectory::new()
            .with_write(pk('a'), [test_relay(0), test_relay(1)])
            .with_write(pk('b'), [test_relay(0), test_relay(1)])
            .with_write(pk('c'), [test_relay(2), test_relay(3)])
            .with_write(pk('d'), [test_relay(2), test_relay(3)]);
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        let demand1 = BTreeSet::from([
            outbox(1, &[pk('a').as_str()]),
            outbox(1, &[pk('b').as_str()]),
            outbox(1, &[pk('c').as_str()]),
        ]);
        let _ = router.compile(&demand1, &dir, 10);

        let demand2 = BTreeSet::from([
            outbox(1, &[pk('a').as_str()]),
            outbox(1, &[pk('b').as_str()]),
            outbox(1, &[pk('d').as_str()]),
        ]);
        let delta = router.compile(&demand2, &dir, 10);

        let touched: BTreeSet<RelayUrl> = delta.ops.iter().map(|(r, _)| r.clone()).collect();
        assert!(touched.contains(&test_relay(2)));
        assert!(touched.contains(&test_relay(3)));
        assert!(!touched.contains(&test_relay(0)));
        assert!(!touched.contains(&test_relay(1)));
    }

    #[test]
    fn every_wire_req_traces_to_a_route() {
        let dir = FixtureDirectory::new().with_write(pk('a'), [test_relay(0), test_relay(1)]);
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        let demand = BTreeSet::from([outbox(1, &[pk('a').as_str()])]);
        let _ = router.compile(&demand, &dir, 10);
        for reqs in router.plan().reqs.values() {
            for req in reqs {
                assert!(!req.provenance.is_empty());
            }
        }
    }

    /// Owner clarification (relay roles are ADDITIVE, not mutually
    /// exclusive): a relay that is BOTH an author's own kind:10002 write
    /// relay AND one of the operator's configured indexers must receive
    /// BOTH that author's content kinds (kind:1) AND discovery-kind reads
    /// (kind:3/kind:0/kind:1xxxx) -- `compile` solves the content group from
    /// `write_relays` (`build_candidates`) and, independently, applies the
    /// discovery group's `indexer_lane_routes` OUTSIDE the solve (Unit B),
    /// so the SAME relay legitimately shows up as a covering candidate for
    /// both an author's outbox group AND their discovery group without
    /// either lane excluding the other.
    #[test]
    fn additive_relay_roles_union_not_exclusive() {
        let shared = test_relay(0);
        let dir = FixtureDirectory::new()
            .with_write(pk('a'), [shared.clone()])
            .with_indexer(shared.clone());
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        // kind:1 (content, never discovery-eligible) + kind:3 (discovery)
        // for the SAME author -- both must route to `shared`.
        let demand = BTreeSet::from([
            outbox(1, &[pk('a').as_str()]),
            outbox(3, &[pk('a').as_str()]),
        ]);
        let _ = router.compile(&demand, &dir, 10);
        let plan = router.plan();

        assert!(
            plan.reqs.contains_key(&shared),
            "the relay serving both roles must appear in the plan at all"
        );
        let covered_kinds: BTreeSet<u16> = plan.reqs[&shared]
            .iter()
            .flat_map(|req| req.filter.kinds.clone().unwrap_or_default())
            .collect();
        assert!(
            covered_kinds.contains(&1u16),
            "the write-relay role must still route the author's content kind: {covered_kinds:?}"
        );
        assert!(
            covered_kinds.contains(&3u16),
            "the indexer role must still route the author's discovery kind: {covered_kinds:?}"
        );

        // `shared` qualifies via BOTH lanes (the content group's own-relay
        // solve picks it up via `write_relays`; the discovery group's
        // additive `indexer_lane_routes` picks it up independently, outside
        // the solve) -- `route::lane_of`'s own doc records this is a deliberate,
        // documented tie-break (write_relays listed first => Nip65Write
        // wins the label when a relay qualifies both ways), not a dedup
        // that drops the indexer role's eligibility. What matters -- and
        // what the kind-coverage assertions above already prove -- is that
        // BOTH roles' kinds still route to `shared` regardless of which
        // single lane the tie-break happens to label it with.
        let lanes: BTreeSet<Lane> = plan.reqs[&shared]
            .iter()
            .flat_map(|req| req.provenance.iter().map(|p| p.lane))
            .collect();
        assert!(
            lanes.contains(&Lane::Nip65Write),
            "must carry Nip65Write provenance: {lanes:?}"
        );

        // Sanity check that `IndexerDiscovery` attribution itself still
        // works in general (i.e. the tie-break above is a labeling nuance
        // for the double-qualifying relay, not a broken lane): an
        // indexer-ONLY relay (not in `a`'s write-relay list) covering the
        // SAME discovery atom must be labeled `IndexerDiscovery`.
        let indexer_only = test_relay(1);
        let dir2 = FixtureDirectory::new()
            .with_write(pk('a'), [shared.clone()])
            .with_indexer(shared.clone())
            .with_indexer(indexer_only.clone());
        let mut router2 = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        let _ = router2.compile(&demand, &dir2, 10);
        let plan2 = router2.plan();
        let indexer_only_lanes: BTreeSet<Lane> = plan2
            .reqs
            .get(&indexer_only)
            .into_iter()
            .flatten()
            .flat_map(|req| req.provenance.iter().map(|p| p.lane))
            .collect();
        assert!(
            indexer_only_lanes.contains(&Lane::IndexerDiscovery),
            "an indexer-only relay covering the discovery atom must still be \
             labeled IndexerDiscovery: {indexer_only_lanes:?}"
        );
    }

    /// Unit B's headline pre-fix falsifier (`routing-build-plan.md` Unit B /
    /// issue #29): today `build_candidates` folds a configured indexer into
    /// a discovery-kind atom's own candidate list, so an author with ONE own
    /// write relay reaches `k=2` (indexer counted) and no shortfall is ever
    /// reported -- fallback can never fire, and "this author is under-
    /// covered" is invisible. Post-fix the indexer is an additive lane
    /// applied OUTSIDE the solve; the solver's input is the author's own
    /// relays only, so this author never reaches `k` and the shortfall must
    /// surface (even though the indexer still legitimately routes the
    /// discovery atom, just not as a k-counting candidate).
    #[test]
    fn solver_counts_only_own_relays_toward_k() {
        let a = pk('a');
        let indexer = test_relay(99);
        let dir = FixtureDirectory::new()
            .with_write(a.clone(), [test_relay(0)])
            .with_indexer(indexer.clone());
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );

        // kind:0 -- discovery-kind, the only shape under which the OLD code
        // ever folded the indexer into the candidate list at all.
        let demand = BTreeSet::from([outbox(0, &[a.as_str()])]);
        let _ = router.compile(&demand, &dir, 10);

        let shortfall = router
            .diagnostics()
            .uncovered_authors
            .get(&a)
            .expect("the indexer must NOT count toward k -- author must be under-min");
        assert_eq!(shortfall.reason, ShortfallReason::FewerCandidatesThanK);
        assert_eq!(shortfall.achieved, 1);

        // The indexer still routes the discovery atom (additive lane) --
        // narrowing the solver's input doesn't remove the route, only its
        // contribution to `k`.
        assert!(router.plan().reqs.contains_key(&indexer));
    }

    /// The app lane routes EVERY atom -- author-bearing (all authors) and
    /// authorless/pinned alike (this closes #7, the authorless-routing-lane
    /// gap: an atom with no other pinned fact previously had zero routes) --
    /// and it is purely additive: it never satisfies the k-min for an
    /// author-bearing atom.
    #[test]
    fn app_lane_routes_all_authors_and_authorless_additively_never_toward_k() {
        let a = pk('a');
        let app_relay = test_relay(50);
        let dir = FixtureDirectory::new()
            .with_write(a.clone(), [test_relay(0)]) // deliberately under-min
            .with_app([app_relay.clone()]);
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );

        let authored = cf(1, &[a.as_str()]);
        // No pinned fact registered for this atom -- the app lane is its
        // ONLY possible route.
        let authorless = ConcreteFilter {
            kinds: Some(BTreeSet::from([39_000u16])),
            ..ConcreteFilter::default()
        };
        let demand = BTreeSet::from([
            as_atom(authored.clone(), SourceAuthority::AuthorOutboxes),
            pinned(authorless.clone()),
        ]);
        let _ = router.compile(&demand, &dir, 10);

        let app_reqs = &router.plan().reqs[&app_relay];
        assert!(app_reqs.iter().any(|r| r.filter == authored));
        assert!(app_reqs.iter().any(|r| r.filter == authorless));
        assert!(app_reqs
            .iter()
            .flat_map(|r| r.provenance.iter())
            .all(|p| p.lane == Lane::AppRelay));

        // Additive, never toward k: 'a' still shows FewerCandidatesThanK
        // despite having a route to the app relay.
        let shortfall = router
            .diagnostics()
            .uncovered_authors
            .get(&a)
            .expect("an appRelay route must not satisfy k");
        assert_eq!(shortfall.reason, ShortfallReason::FewerCandidatesThanK);
    }

    /// Both branches of the fallback lane (`routing-and-ownership.md`
    /// §2.1/§2.2 item 5): it fires for an under-min author when no appRelay
    /// is configured, and is suppressed entirely once one is -- in both
    /// cases the shortfall stays REPORTED (fallback is a lane, not
    /// coverage).
    #[test]
    fn fallback_fires_for_under_min_authors_and_is_suppressed_by_apprelay() {
        let a = pk('a');
        let fallback_relay = test_relay(60);
        let demand = BTreeSet::from([outbox(1, &[a.as_str()])]);

        // Branch 1: no appRelay configured -- fallback fires.
        let dir = FixtureDirectory::new()
            .with_write(a.clone(), [test_relay(0)]) // under-min
            .with_fallback([fallback_relay.clone()]);
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        let _ = router.compile(&demand, &dir, 10);
        let plan = router.plan();
        assert!(
            plan.reqs.contains_key(&fallback_relay),
            "fallback must fire for the under-min author when no appRelay is configured"
        );
        assert!(plan.reqs[&fallback_relay]
            .iter()
            .flat_map(|r| r.provenance.iter())
            .all(|p| p.lane == Lane::Fallback));
        assert_eq!(
            router.diagnostics().uncovered_authors[&a].reason,
            ShortfallReason::FewerCandidatesThanK
        );

        // Branch 2: an appRelay is ALSO configured -- fallback is suppressed
        // entirely, even though the author is STILL under-min.
        let app_relay = test_relay(70);
        let dir2 = FixtureDirectory::new()
            .with_write(a.clone(), [test_relay(0)])
            .with_fallback([fallback_relay.clone()])
            .with_app([app_relay.clone()]);
        let mut router2 = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        let _ = router2.compile(&demand, &dir2, 10);
        let plan2 = router2.plan();
        assert!(
            !plan2.reqs.contains_key(&fallback_relay),
            "an appRelay must suppress the fallback lane entirely"
        );
        assert!(plan2.reqs.contains_key(&app_relay));
        assert_eq!(
            router2.diagnostics().uncovered_authors[&a].reason,
            ShortfallReason::FewerCandidatesThanK,
            "shortfall stays reported even when appRelay/fallback top the author up"
        );
    }

    /// Regression guard on the moved logic (Unit B relocated the indexer
    /// lane from `build_candidates` into `compile`'s `indexer_lane_routes`)
    /// -- re-asserts the invariant the old route.rs-level
    /// `indexer_candidates_only_for_discovery_kinds` test pinned survives
    /// the move: an indexer relay routes discovery-kind atoms only, and
    /// NEVER becomes a content-atom fallback even when the SAME author has
    /// zero own relays for both atoms.
    #[test]
    fn indexer_lane_still_discovery_only_never_content_fallback() {
        let a = pk('a');
        let indexer = test_relay(99);
        let dir = FixtureDirectory::new().with_indexer(indexer.clone());
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );

        let discovery_atom = cf(3, &[a.as_str()]);
        let content_atom = cf(1, &[a.as_str()]);
        let demand = BTreeSet::from([
            as_atom(discovery_atom.clone(), SourceAuthority::AuthorOutboxes),
            as_atom(content_atom.clone(), SourceAuthority::AuthorOutboxes),
        ]);
        let _ = router.compile(&demand, &dir, 10);

        let plan = router.plan();
        let indexer_reqs = &plan.reqs[&indexer];
        assert!(indexer_reqs.iter().all(|r| r.filter == discovery_atom));
        assert!(indexer_reqs
            .iter()
            .flat_map(|r| r.provenance.iter())
            .all(|p| p.lane == Lane::IndexerDiscovery));

        // Both atoms are NoCandidates (the author has zero own relays for
        // either) -- but the content atom's shortfall is never topped up by
        // the indexer: it never appears at `indexer` at all.
        assert_eq!(
            router.diagnostics().uncovered_authors[&a].reason,
            ShortfallReason::NoCandidates
        );
    }

    /// #106/Fable's falsifier 6 (coalescing correctness), re-homed here per
    /// Fable D's "locus fixed" -- equal-context-only coalescing is a
    /// property of `Router::compile`'s per-relay CONTEXT PARTITIONING, not
    /// of `coalesce.rs` itself (which stays pure and untouched). The
    /// IDENTICAL selection, declared under two different `SourceAuthority`s
    /// (the new expressible behavior Fable's owner-flag names: `Public` on
    /// an author-bearing selection is legal), routes to the SAME relay via
    /// two entirely different lanes (outbox-solve vs pinned/group-host
    /// lookup) -- they must ship as TWO separate `WireReq`s with distinct
    /// `SubId`s, never merged into one.
    #[test]
    fn different_context_same_relay_same_filter_never_merges_into_one_wire_req() {
        let a = pk('a');
        let shared = test_relay(0);
        let filter = cf(1, &[a.as_str()]);
        let dir = FixtureDirectory::new()
            .with_write(a.clone(), [shared.clone()])
            .with_group_host(filter.clone(), shared.clone());
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );

        let demand = BTreeSet::from([
            as_atom(filter.clone(), SourceAuthority::AuthorOutboxes),
            as_atom(filter.clone(), SourceAuthority::Public),
        ]);
        let _ = router.compile(&demand, &dir, 10);

        let reqs = &router.plan().reqs[&shared];
        assert_eq!(
            reqs.len(),
            2,
            "identical selection under different SourceAuthority must ship as two separate WireReqs, never merged: {reqs:?}"
        );
        let sub_ids: BTreeSet<_> = reqs.iter().map(|r| r.sub_id.clone()).collect();
        assert_eq!(
            sub_ids.len(),
            2,
            "the two WireReqs must carry distinct SubIds (the wire-side anti-alias fix)"
        );
    }

    /// #107's Contract, the core guarantee: "Pinned author filters never
    /// contact directory, author-outbox, app, fallback, or indexer relays."
    /// Rigs a directory where EVERY other lane would happily route this
    /// exact (author-bearing) filter -- own write relay, a directory-fact
    /// group-host pinned route for the SAME filter, an app relay, a
    /// fallback relay, and an indexer -- so a route to any of them would
    /// prove a lane leaked through. Only the atom's OWN declared
    /// `SourceAuthority::Pinned` relay is ever touched.
    #[test]
    fn explicit_pinned_never_contacts_directory_outbox_app_fallback_or_indexer_relays() {
        let a = pk('a');
        let filter = cf(1, &[a.as_str()]);
        let own_write = test_relay(0);
        let group_host = test_relay(1);
        let app_relay = test_relay(50);
        let fallback_relay = test_relay(60);
        let indexer = test_relay(99);
        let explicit_relay = test_relay(200);

        let dir = FixtureDirectory::new()
            .with_write(a.clone(), [own_write.clone()])
            .with_group_host(filter.clone(), group_host.clone())
            .with_app([app_relay.clone()])
            .with_fallback([fallback_relay.clone()])
            .with_indexer(indexer.clone());
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );

        let demand = BTreeSet::from([as_atom(
            filter,
            SourceAuthority::Pinned(BTreeSet::from([explicit_relay.clone()])),
        )]);
        let _ = router.compile(&demand, &dir, 10);

        let plan = router.plan();
        assert_eq!(
            plan.reqs.keys().collect::<Vec<_>>(),
            vec![&explicit_relay],
            "an ExplicitPinned atom must touch exactly its own declared relay set, \
             never own-write/group-host/app/fallback/indexer relays: {:?}",
            plan.reqs.keys().collect::<Vec<_>>()
        );
        assert!(plan.reqs[&explicit_relay]
            .iter()
            .flat_map(|r| r.provenance.iter())
            .all(|p| p.lane == Lane::ExplicitPinned));
    }

    /// #20 structural falsifier: two different skeletons used to spend the
    /// full cap independently (`2 + 2` relays under a cap of `2`). The
    /// assembled plan now has one ceiling, is deterministic, and records the
    /// refused half as local-limit evidence instead of silently truncating.
    #[test]
    fn whole_demand_cap_is_shared_across_skeletons_and_deterministic() {
        let a = pk('a');
        let b = pk('b');
        let dir = FixtureDirectory::new()
            .with_write(a.clone(), [test_relay(0), test_relay(1)])
            .with_write(b.clone(), [test_relay(2), test_relay(3)]);
        let demand = BTreeSet::from([outbox(1, &[a.as_str()]), outbox(2, &[b.as_str()])]);

        let mut first = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        first.compile(&demand, &dir, 2);
        let first_relays: BTreeSet<_> = first.plan().reqs.keys().cloned().collect();

        let mut second = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        second.compile(&demand, &dir, 2);

        assert_eq!(first.plan().reqs.len(), 2);
        assert_eq!(first.plan().refused_relays.len(), 2);
        assert!(!first.plan().limited.is_empty());
        assert_eq!(
            first_relays,
            second.plan().reqs.keys().cloned().collect(),
            "whole-demand selection and tie-breaking must be reproducible"
        );
        assert_eq!(
            first.plan().limited,
            second.plan().limited,
            "the explicit shortfall evidence must be deterministic too"
        );
        assert_eq!(first.diagnostics().relays_refused_by_cap, 2);
        assert!(first
            .diagnostics()
            .uncovered_authors
            .values()
            .any(|shortfall| shortfall.reason == ShortfallReason::CapExhausted));
    }

    /// The ceiling is over the FINAL plan, not merely the author-outbox
    /// solver. Operator app lanes and explicit pinned authority consume the
    /// same finite budget and any omitted route stays visible as a limit.
    #[test]
    fn additive_and_explicit_pinned_routes_share_the_same_global_cap() {
        let a = pk('a');
        let app = test_relay(10);
        let explicit_a = test_relay(20);
        let explicit_b = test_relay(21);
        let dir = FixtureDirectory::new()
            .with_write(a.clone(), [test_relay(0), test_relay(1)])
            .with_app([app]);
        let demand = BTreeSet::from([
            outbox(1, &[a.as_str()]),
            as_atom(
                cf(9, &[]),
                SourceAuthority::Pinned(BTreeSet::from([explicit_a, explicit_b])),
            ),
        ]);
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );

        router.compile(&demand, &dir, 2);

        assert_eq!(router.plan().reqs.len(), 2);
        assert_eq!(router.plan().refused_relays.len(), 3);
        assert!(!router.plan().limited.is_empty());
        assert_eq!(router.diagnostics().relays_refused_by_cap, 3);
    }

    /// A zero budget is not an uncapped escape hatch at the router seam: it
    /// produces an empty executable plan plus explicit evidence for every
    /// otherwise-routable atom.
    #[test]
    fn zero_budget_refuses_every_route_with_explicit_limit_evidence() {
        let a = pk('a');
        let dir = FixtureDirectory::new().with_write(a.clone(), [test_relay(0), test_relay(1)]);
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );

        router.compile(&BTreeSet::from([outbox(1, &[a.as_str()])]), &dir, 0);

        assert!(router.plan().reqs.is_empty());
        assert_eq!(router.plan().refused_relays.len(), 2);
        assert!(!router.plan().limited.is_empty());
        assert_eq!(
            router.diagnostics().uncovered_authors[&a].reason,
            ShortfallReason::CapExhausted
        );
    }

    #[test]
    fn projected_evidence_routes_authorless_atoms_with_typed_lane() {
        let relay = test_relay(44);
        let mut atom = pinned(ConcreteFilter {
            ids: Some(BTreeSet::from(["11".repeat(32)])),
            ..ConcreteFilter::default()
        });
        atom.routing_evidence.insert(RoutingEvidence {
            relay: relay.clone(),
            origin: RoutingEvidenceKind::Hint,
        });
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );

        router.compile(
            &BTreeSet::from([atom]),
            &FixtureDirectory::new(),
            usize::MAX,
        );

        let req = &router.plan().reqs[&relay][0];
        assert!(req.provenance.iter().any(|fact| fact.lane == Lane::Hint));
    }

    #[test]
    fn projected_author_evidence_participates_in_own_relay_cover() {
        let author = pk('a');
        let directory_relay = test_relay(1);
        let hint_relay = test_relay(2);
        let dir = FixtureDirectory::new().with_write(author.clone(), [directory_relay.clone()]);
        let mut atom = outbox(1, &[author.as_str()]);
        atom.routing_evidence.insert(RoutingEvidence {
            relay: hint_relay.clone(),
            origin: RoutingEvidenceKind::Hint,
        });
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );

        router.compile(&BTreeSet::from([atom]), &dir, usize::MAX);

        assert_eq!(
            router.plan().reqs.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([directory_relay, hint_relay])
        );
        assert!(router.diagnostics().uncovered_authors.is_empty());
    }
}
