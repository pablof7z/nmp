//! [`Router`] — the entry point (M2 plan §2.7, §4.1). Full-recompile-then-
//! diff, not delta-threading: `compile` recomputes the whole per-relay plan
//! from the engine's CURRENT demand set each call, diffs it against the
//! previous plan, stores the new plan + diagnostics, and returns the
//! surgical wire delta. This also discharges M1 nit #2 by construction: a
//! withdrawn atom simply vanishes from `demand`, so the next `compile`
//! emits its `Close` (see `dropped_handle_close_reaches_wire`, test 15).

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::ConcreteFilter;
use nmp_store::{coverage_key, CoverageKey};

use crate::coalesce::RuleRegistry;
use crate::diag::{self, Diagnostics};
use crate::facts::{DiscoveryKinds, PubkeyHex, RelayDirectory, RelayLimits, RelayUrl};
use crate::plan::{diff_plans, RelayPlan, SubId, WireDelta, WireReq};
use crate::route::{self, AtomClass, RouteProvenance, Skeleton};
use crate::solver::{self, CoverageInput, Shortfall};

pub struct Router {
    #[allow(dead_code)] // carried for API completeness / future limit-enforcement (M3)
    limits: RelayLimits,
    discovery: DiscoveryKinds,
    rules: RuleRegistry,
    prev_plan: RelayPlan,
    last_diag: Diagnostics,
}

impl Router {
    pub fn new(limits: RelayLimits, discovery: DiscoveryKinds, rules: RuleRegistry) -> Self {
        Self {
            limits,
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
        demand: &BTreeSet<ConcreteFilter>,
        dir: &dyn RelayDirectory,
        cap: usize,
    ) -> WireDelta {
        // Step 1: group demand by Skeleton (outbox) / classify pinned.
        let mut outbox_groups: BTreeMap<Skeleton, BTreeSet<PubkeyHex>> = BTreeMap::new();
        let mut pinned_atoms: BTreeSet<ConcreteFilter> = BTreeSet::new();
        for atom in demand {
            match route::classify(atom) {
                AtomClass::Outbox { skeleton, authors } => {
                    outbox_groups.entry(skeleton).or_default().extend(authors);
                }
                AtomClass::Pinned => {
                    pinned_atoms.insert(atom.clone());
                }
            }
        }

        // Step 2 + 3: route (coverage-solve outbox groups / pinned lookup)
        // and materialize each relay's bag of (filter, provenance, absorbed)
        // entries. `absorbed` is the coverage-attribution ruling's per-atom
        // `CoverageKey` (§2): each entry here is exactly one pre-coalesce
        // demand atom (one author, for outbox; the pinned atom itself, for
        // pinned), so it contributes exactly one key, later unioned by
        // `coalesce_with` alongside provenance as same-skeleton atoms merge.
        type BagEntry = (ConcreteFilter, Vec<RouteProvenance>, BTreeSet<CoverageKey>);
        let mut bag: BTreeMap<RelayUrl, Vec<BagEntry>> = BTreeMap::new();
        let mut uncovered_authors: BTreeMap<PubkeyHex, Shortfall> = BTreeMap::new();

        for (skeleton, authors) in &outbox_groups {
            let (candidates, indexer_relays) =
                route::build_candidates(authors, dir, &self.discovery, skeleton);
            let coverage = solver::solve(&CoverageInput {
                candidates: candidates.clone(),
                k: 2,
                cap,
                indexer_eligible_relays: indexer_relays,
            });
            uncovered_authors.extend(coverage.shortfall.clone());

            for (relay, prov) in route::provenance_for_outbox(&coverage, &candidates) {
                let filter = skeleton.with_authors(prov.covers_authors.clone());
                let key = coverage_key(&filter);
                bag.entry(relay)
                    .or_default()
                    .push((filter, vec![prov], BTreeSet::from([key])));
            }
        }

        for atom in &pinned_atoms {
            let key = coverage_key(atom);
            for (relay, prov) in route::provenance_for_pinned(atom, dir) {
                bag.entry(relay).or_default().push((
                    atom.clone(),
                    vec![prov],
                    BTreeSet::from([key]),
                ));
            }
        }

        // Step 4 + 5: per relay, dedup + widen-only coalesce, then assign
        // stable sub-ids.
        let mut reqs: BTreeMap<RelayUrl, Vec<WireReq>> = BTreeMap::new();
        for (relay, entries) in bag {
            let merged = self.rules.coalesce_with(entries);
            let mut relay_reqs: Vec<WireReq> = merged
                .into_iter()
                .map(|(filter, provenance, absorbed)| {
                    let sub_id = SubId::for_filter(relay.clone(), &filter);
                    WireReq {
                        sub_id,
                        filter,
                        provenance,
                        absorbed,
                    }
                })
                .collect();
            relay_reqs.sort_by(|a, b| a.sub_id.cmp(&b.sub_id));
            reqs.insert(relay, relay_reqs);
        }

        let next_plan = RelayPlan { reqs };

        // Step 6: diff vs previous plan.
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

    #[test]
    fn outbox_maps_authors_to_own_write_relays() {
        let dir = FixtureDirectory::new()
            .with_write(pk('a'), [test_relay(0), test_relay(1)])
            .with_write(pk('b'), [test_relay(2), test_relay(3)]);
        let mut router = Router::new(
            RelayLimits::default(),
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        let demand = BTreeSet::from([cf(1, &[pk('a').as_str()]), cf(1, &[pk('b').as_str()])]);
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
            RelayLimits::default(),
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        let demand1 = BTreeSet::from([
            cf(1, &[pk('a').as_str()]),
            cf(1, &[pk('b').as_str()]),
            cf(1, &[pk('c').as_str()]),
        ]);
        let _ = router.compile(&demand1, &dir, 10);

        let demand2 = BTreeSet::from([
            cf(1, &[pk('a').as_str()]),
            cf(1, &[pk('b').as_str()]),
            cf(1, &[pk('d').as_str()]),
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
            RelayLimits::default(),
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        let demand = BTreeSet::from([cf(1, &[pk('a').as_str()])]);
        let _ = router.compile(&demand, &dir, 10);
        for reqs in router.plan().reqs.values() {
            for req in reqs {
                assert!(!req.provenance.is_empty());
            }
        }
    }
}
