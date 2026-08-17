//! Exact per-demand contributions to one immutable physical request.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::ContextualAtom;
use nmp_store::{coverage_claim_atoms, coverage_key};

use crate::plan::{DemandKey, WireReq};
use crate::route::{self, AtomClass};
use crate::Router;

use super::{RequestContributionDelta, RequestKey, RequestOwnerContribution};

impl Router {
    pub(crate) fn derive_request_owner_contribution(
        atom: &ContextualAtom,
        request: &WireReq,
    ) -> RequestOwnerContribution {
        let demand = DemandKey::for_atom(atom);
        let coverage_claims = coverage_claim_atoms(atom)
            .iter()
            .map(coverage_key)
            .filter(|claim| request.coverage_claims.contains(claim))
            .collect();
        let coverage_assignments = request
            .coverage_assignments
            .iter()
            .filter(|(owner, _)| owner == &demand)
            .copied()
            .collect();
        let authors = match route::classify(&atom.filter, &atom.source) {
            AtomClass::Coverage { authors, .. } | AtomClass::Supplemental { authors } => authors,
            AtomClass::Exact(_) => BTreeSet::new(),
        };
        let provenance = request
            .provenance
            .iter()
            .filter(|fact| {
                fact.covers_authors.is_empty()
                    || authors.is_empty()
                    || !fact.covers_authors.is_disjoint(&authors)
            })
            .cloned()
            .collect();
        RequestOwnerContribution {
            coverage_claims,
            coverage_assignments,
            provenance,
        }
    }

    pub(crate) fn add_request_owner_contribution(
        &mut self,
        request_key: &RequestKey,
        demand: DemandKey,
        contribution: RequestOwnerContribution,
    ) -> RequestContributionDelta {
        let owner_added = !self
            .request_owner_contributions
            .get(request_key)
            .is_some_and(|owners| owners.contains_key(&demand));
        self.request_owner_contributions
            .entry(request_key.clone())
            .or_default()
            .entry(demand)
            .or_default();
        let mut delta = RequestContributionDelta {
            owner_added,
            ..RequestContributionDelta::default()
        };

        for claim in contribution.coverage_claims {
            let inserted = self
                .request_owner_contributions
                .get_mut(request_key)
                .and_then(|owners| owners.get_mut(&demand))
                .expect("request owner contribution was installed")
                .coverage_claims
                .insert(claim);
            if !inserted {
                continue;
            }
            let count = self
                .request_claim_owner_counts
                .entry((request_key.clone(), claim))
                .or_insert(0);
            if *count == 0 {
                delta.coverage_claims.insert(claim);
            }
            *count += 1;
        }
        for assignment in contribution.coverage_assignments {
            if self
                .request_owner_contributions
                .get_mut(request_key)
                .and_then(|owners| owners.get_mut(&demand))
                .expect("request owner contribution was installed")
                .coverage_assignments
                .insert(assignment)
            {
                delta.coverage_assignments.insert(assignment);
            }
        }
        for provenance in contribution.provenance {
            let inserted = self
                .request_owner_contributions
                .get_mut(request_key)
                .and_then(|owners| owners.get_mut(&demand))
                .expect("request owner contribution was installed")
                .provenance
                .insert(provenance.clone());
            if !inserted {
                continue;
            }
            let count = self
                .request_provenance_owner_counts
                .entry((request_key.clone(), provenance.clone()))
                .or_insert(0);
            if *count == 0 {
                delta.provenance.insert(provenance);
            }
            *count += 1;
        }
        if owner_added {
            *self
                .request_demand_coverage_owner_counts
                .entry((request_key.clone(), demand.coverage()))
                .or_insert(0) += 1;
        }
        delta
    }

    pub(crate) fn remove_request_owner_contribution(
        &mut self,
        request_key: &RequestKey,
        demand: DemandKey,
    ) -> RequestContributionDelta {
        let Some(contribution) = self
            .request_owner_contributions
            .get_mut(request_key)
            .and_then(|owners| owners.remove(&demand))
        else {
            return RequestContributionDelta::default();
        };
        if self
            .request_owner_contributions
            .get(request_key)
            .is_some_and(BTreeMap::is_empty)
        {
            self.request_owner_contributions.remove(request_key);
        }
        let mut delta = RequestContributionDelta {
            owner_added: true,
            ..RequestContributionDelta::default()
        };
        for claim in contribution.coverage_claims {
            let count_key = (request_key.clone(), claim);
            let remove = self
                .request_claim_owner_counts
                .get_mut(&count_key)
                .is_some_and(|count| {
                    *count = count
                        .checked_sub(1)
                        .expect("request claim owner count cannot underflow");
                    *count == 0
                });
            if remove {
                self.request_claim_owner_counts.remove(&count_key);
                delta.coverage_claims.insert(claim);
            }
        }
        delta.coverage_assignments = contribution.coverage_assignments;
        for provenance in contribution.provenance {
            let count_key = (request_key.clone(), provenance.clone());
            let remove = self
                .request_provenance_owner_counts
                .get_mut(&count_key)
                .is_some_and(|count| {
                    *count = count
                        .checked_sub(1)
                        .expect("request provenance owner count cannot underflow");
                    *count == 0
                });
            if remove {
                self.request_provenance_owner_counts.remove(&count_key);
                delta.provenance.insert(provenance);
            }
        }
        let coverage_count_key = (request_key.clone(), demand.coverage());
        let remove_coverage = self
            .request_demand_coverage_owner_counts
            .get_mut(&coverage_count_key)
            .is_some_and(|count| {
                *count = count
                    .checked_sub(1)
                    .expect("request demand-coverage owner count cannot underflow");
                *count == 0
            });
        if remove_coverage {
            self.request_demand_coverage_owner_counts
                .remove(&coverage_count_key);
        }
        delta
    }

    pub(crate) fn reconcile_active_demands(&mut self, next: BTreeMap<DemandKey, ContextualAtom>) {
        // Every incumbent active-demand entry is dereferenced here to decide
        // whether `next` still owns it. When admission isolates a pending
        // cohort (`Router::admit`), `self.active_demands` is detached to
        // empty before this runs, so a later cohort visits none of it; a
        // full `compile()` runs this against the real incumbent set. Either
        // way the count is exact, not a proxy.
        let mut removed = Vec::new();
        for demand in self.active_demands.keys().copied() {
            self.admission_work.incumbent_active_entries_visited = self
                .admission_work
                .incumbent_active_entries_visited
                .saturating_add(1);
            if !next.contains_key(&demand) {
                removed.push(demand);
            }
        }
        for demand in removed {
            if let Some(atom) = self.active_demands.get(&demand) {
                if let AtomClass::Coverage { authors, .. } =
                    route::classify(&atom.filter, &atom.source)
                {
                    for author in authors {
                        let remove =
                            self.active_outbox_authors
                                .get_mut(&author)
                                .is_some_and(|count| {
                                    *count = count
                                        .checked_sub(1)
                                        .expect("active outbox author refcount cannot underflow");
                                    *count == 0
                                });
                        if remove {
                            self.active_outbox_authors.remove(&author);
                        }
                    }
                }
            }
            for request in self.requests_by_demand.get(&demand).into_iter().flatten() {
                let count = self
                    .active_by_request
                    .get_mut(request)
                    .expect("active demand edge names an indexed request");
                *count = count
                    .checked_sub(1)
                    .expect("active request owner count cannot underflow");
            }
        }

        for (demand, atom) in &next {
            if self.active_demands.contains_key(demand) {
                continue;
            }
            if let AtomClass::Coverage { authors, .. } = route::classify(&atom.filter, &atom.source)
            {
                for author in authors {
                    *self.active_outbox_authors.entry(author).or_insert(0) += 1;
                }
            }
            for request in self.requests_by_demand.get(demand).into_iter().flatten() {
                *self
                    .active_by_request
                    .get_mut(request)
                    .expect("active demand edge names an indexed request") += 1;
            }
        }
        self.active_demands = next;
    }
}
