//! Hidden ownership census and structural work counters.

use std::collections::BTreeSet;

use nmp_grammar::RelaySessionKey;
use nmp_store::CoverageKey;

use crate::plan::{DemandKey, SubId};
use crate::Router;



impl Router {

    pub fn physically_covers(&self, demand: DemandKey) -> bool {
        self.requests_by_demand.contains_key(&demand)
    }

    /// Whether one exact demand still has relay work or routing shortfall
    /// eligible for a later pending-only admission cohort.
    pub fn admission_incomplete(&self, demand: DemandKey) -> bool {
        !self.physically_covers(demand.clone())
            || self.prev_plan.limited_demands.contains(&demand.clone())
            || self.uncovered_by_demand.contains_key(&demand)
    }

    /// Exact claims eligible on the next send of one immutable physical
    /// request: its original sent claims plus currently attached local claims.
    pub fn request_claims(
        &self,
        session: &RelaySessionKey,
        sub_id: &SubId,
    ) -> Option<BTreeSet<CoverageKey>> {
        let request_key = (session.clone(), sub_id.clone());
        let position = self.request_position_by_key.get(&request_key)?;
        let current = &self
            .prev_plan
            .reqs
            .get(session)?
            .get(*position)?
            .coverage_claims;
        let physical = self.physical_claims_by_request.get(&request_key)?;
        Some(current.union(physical).cloned().collect())
    }

    /// Claims present when the immutable physical request entered the plan.
    /// These remain coverage authority until physical CLOSE even when their
    /// current local owner detaches.
    pub fn physical_request_claims(
        &self,
        session: &RelaySessionKey,
        sub_id: &SubId,
    ) -> Option<&BTreeSet<CoverageKey>> {
        self.physical_claims_by_request
            .get(&(session.clone(), sub_id.clone()))
    }

    /// Exact immutable logical demands owned by one physical request.
    /// Unlike coverage identity, these retain since/until/limit and are the
    /// only sound key for routing a request's settlement back to the
    /// observations that own it.
    pub fn request_demands(
        &self,
        session: &RelaySessionKey,
        sub_id: &SubId,
    ) -> Option<&BTreeSet<DemandKey>> {
        let position = self
            .request_position_by_key
            .get(&(session.clone(), sub_id.clone()))?;
        self.prev_plan
            .reqs
            .get(session)?
            .get(*position)
            .map(|request| &request.owner_demands)
    }

}
