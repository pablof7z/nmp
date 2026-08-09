//! Exact uncovered-demand ownership and active-demand indexes.

use std::collections::BTreeMap;

#[cfg(test)]
use nmp_grammar::ContextualAtom;

use crate::plan::DemandKey;
#[cfg(test)]
use crate::route::{self, AtomClass};
use crate::{PublicKey, Router, Shortfall};

use super::strongest_shortfall;

impl Router {
    pub(crate) fn install_uncovered_ownership(
        &mut self,
        uncovered_by_demand: BTreeMap<DemandKey, BTreeMap<PublicKey, Shortfall>>,
    ) {
        self.uncovered_by_demand = uncovered_by_demand;
        self.uncovered_owners_by_author.clear();
        for (demand, facts) in &self.uncovered_by_demand {
            for (author, fact) in facts {
                self.uncovered_owners_by_author
                    .entry(*author)
                    .or_default()
                    .insert(*demand, *fact);
            }
        }
        self.refresh_uncovered_diagnostics();
    }

    pub(crate) fn replace_uncovered_demand(
        &mut self,
        demand: DemandKey,
        facts: BTreeMap<PublicKey, Shortfall>,
    ) -> bool {
        let unchanged = self
            .uncovered_by_demand
            .get(&demand)
            .is_some_and(|current| current == &facts);
        if unchanged {
            return false;
        }
        let mut changed = self.remove_uncovered_demand(demand);
        if facts.is_empty() {
            return changed;
        }
        for (author, fact) in &facts {
            self.uncovered_owners_by_author
                .entry(*author)
                .or_default()
                .insert(demand, *fact);
            changed |= self.refresh_uncovered_author(*author);
        }
        self.uncovered_by_demand.insert(demand, facts);
        changed
    }

    pub(crate) fn remove_uncovered_demand(&mut self, demand: DemandKey) -> bool {
        let Some(facts) = self.uncovered_by_demand.remove(&demand) else {
            return false;
        };
        let mut changed = false;
        for author in facts.keys() {
            if let Some(owners) = self.uncovered_owners_by_author.get_mut(author) {
                owners.remove(&demand);
                if owners.is_empty() {
                    self.uncovered_owners_by_author.remove(author);
                }
            }
            changed |= self.refresh_uncovered_author(*author);
        }
        changed
    }

    fn refresh_uncovered_diagnostics(&mut self) {
        self.last_diag.uncovered_authors = self
            .uncovered_owners_by_author
            .iter()
            .filter_map(|(author, owners)| {
                strongest_shortfall(owners.values()).map(|fact| (*author, fact))
            })
            .collect();
    }

    fn refresh_uncovered_author(&mut self, author: PublicKey) -> bool {
        let next = self
            .uncovered_owners_by_author
            .get(&author)
            .and_then(|owners| strongest_shortfall(owners.values()));
        let previous = self.last_diag.uncovered_authors.get(&author).copied();
        match next {
            Some(fact) => {
                self.last_diag.uncovered_authors.insert(author, fact);
            }
            None => {
                self.last_diag.uncovered_authors.remove(&author);
            }
        }
        previous != next
    }

    #[cfg(test)]
    pub(crate) fn rebuild_active_indexes(
        &mut self,
        demand: impl IntoIterator<Item = ContextualAtom>,
    ) {
        self.active_demands = demand
            .into_iter()
            .map(|atom| (DemandKey::for_atom(&atom), atom))
            .collect();
        self.requests_by_demand.clear();
        self.active_by_request.clear();
        self.request_coverage_by_key.clear();
        self.request_position_by_key.clear();
        self.request_by_exact_filter.clear();
        self.physical_claims_by_request.clear();
        self.requests_by_physical_claim.clear();
        self.physical_contributions_by_request.clear();
        self.requests_by_physical_demand.clear();
        self.request_owner_contributions.clear();
        self.request_claim_owner_counts.clear();
        self.request_provenance_owner_counts.clear();
        self.request_demand_coverage_owner_counts.clear();
        self.coverage_assignment_requests.clear();
        self.diagnostic_author_refs.clear();
        let mut contributions = Vec::new();
        for (session, requests) in &self.prev_plan.reqs {
            for request in requests {
                for demand in &request.owner_demands {
                    let Some(atom) = self.active_demands.get(demand) else {
                        continue;
                    };
                    contributions.push((
                        (session.clone(), request.sub_id.clone()),
                        *demand,
                        Self::derive_request_owner_contribution(atom, request),
                    ));
                }
            }
        }
        for (session, requests) in &self.prev_plan.reqs {
            for (position, request) in requests.iter().enumerate() {
                let request_key = (session.clone(), request.sub_id.clone());
                let mut active = 0;
                for demand in &request.owner_demands {
                    self.full_metadata_work.owner_edges_visited = self
                        .full_metadata_work
                        .owner_edges_visited
                        .saturating_add(1);
                    self.requests_by_demand
                        .entry(*demand)
                        .or_default()
                        .insert(request_key.clone());
                    active += usize::from(self.active_demands.contains_key(demand));
                }
                self.active_by_request.insert(request_key.clone(), active);
                self.request_coverage_by_key.insert(
                    request_key.clone(),
                    request
                        .owner_demands
                        .iter()
                        .map(|demand| demand.coverage())
                        .collect(),
                );
                self.physical_claims_by_request
                    .insert(request_key.clone(), request.coverage_claims.clone());
                for claim in &request.coverage_claims {
                    self.requests_by_physical_claim
                        .entry(*claim)
                        .or_default()
                        .insert(request_key.clone());
                }
                self.request_position_by_key.insert(request_key, position);
                self.request_by_exact_filter.insert(
                    (
                        session.clone(),
                        request.source.clone(),
                        request.filter.clone(),
                    ),
                    (session.clone(), request.sub_id.clone()),
                );
                for assignment in &request.coverage_assignments {
                    self.full_metadata_work.assignment_edges_visited = self
                        .full_metadata_work
                        .assignment_edges_visited
                        .saturating_add(1);
                    self.coverage_assignment_requests
                        .entry(*assignment)
                        .or_default()
                        .insert((session.clone(), request.sub_id.clone()));
                }
                for author in request
                    .provenance
                    .iter()
                    .flat_map(|provenance| provenance.covers_authors.iter().copied())
                {
                    self.full_metadata_work.provenance_author_edges_visited = self
                        .full_metadata_work
                        .provenance_author_edges_visited
                        .saturating_add(1);
                    *self
                        .diagnostic_author_refs
                        .entry(session.clone())
                        .or_default()
                        .entry(author)
                        .or_insert(0) += 1;
                }
            }
        }
        for (request_key, demand, contribution) in &contributions {
            self.physical_contributions_by_request
                .entry(request_key.clone())
                .or_default()
                .insert(*demand, contribution.clone());
            self.requests_by_physical_demand
                .entry(*demand)
                .or_default()
                .insert(request_key.clone());
        }
        for (request_key, demand, contribution) in contributions {
            self.add_request_owner_contribution(&request_key, demand, contribution);
        }
        self.active_outbox_authors.clear();
        for atom in self.active_demands.values() {
            if let AtomClass::Coverage { authors, .. } = route::classify(&atom.filter, &atom.source)
            {
                for author in authors {
                    *self.active_outbox_authors.entry(author).or_insert(0) += 1;
                }
            }
        }
    }
}
