//! Hidden ownership census and structural work counters.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::RelaySessionKey;
use nmp_store::CoverageKey;

use crate::plan::{DemandKey, SubId};
use crate::{PublicKey, Router, Shortfall};

use super::{AdmissionWork, FullMetadataWork, RouterOwnershipCensus, WithdrawalWork};

impl Router {
    #[doc(hidden)]
    pub fn reset_withdrawal_work(&mut self) {
        self.withdrawal_work = WithdrawalWork::default();
    }

    #[doc(hidden)]
    pub fn withdrawal_work(&self) -> WithdrawalWork {
        self.withdrawal_work
    }

    #[doc(hidden)]
    pub fn reset_admission_work(&mut self) {
        self.admission_work = AdmissionWork::default();
    }

    #[doc(hidden)]
    pub fn admission_work(&self) -> AdmissionWork {
        self.admission_work
    }

    #[doc(hidden)]
    pub fn reset_full_metadata_work(&mut self) {
        self.full_metadata_work = FullMetadataWork::default();
    }

    #[doc(hidden)]
    pub fn full_metadata_work(&self) -> FullMetadataWork {
        self.full_metadata_work
    }

    #[doc(hidden)]
    pub fn active_demand_count(&self) -> usize {
        self.active_demands.len()
    }

    #[doc(hidden)]
    pub fn ownership_census(&self) -> RouterOwnershipCensus {
        RouterOwnershipCensus {
            active_demands: self.active_demands.len(),
            requests_by_demand_keys: self.requests_by_demand.len(),
            requests_by_demand_edges: self.requests_by_demand.values().map(BTreeSet::len).sum(),
            active_by_request: self.active_by_request.len(),
            request_coverage_keys: self.request_coverage_by_key.len(),
            request_position_keys: self.request_position_by_key.len(),
            request_exact_filter_keys: self.request_by_exact_filter.len(),
            physical_request_claim_keys: self.physical_claims_by_request.len(),
            physical_claim_keys: self.requests_by_physical_claim.len(),
            physical_claim_edges: self
                .requests_by_physical_claim
                .values()
                .map(BTreeSet::len)
                .sum(),
            physical_request_contribution_keys: self
                .physical_contributions_by_request
                .values()
                .map(BTreeMap::len)
                .sum(),
            physical_demand_keys: self.requests_by_physical_demand.len(),
            physical_demand_edges: self
                .requests_by_physical_demand
                .values()
                .map(BTreeSet::len)
                .sum(),
            request_owner_contribution_keys: self
                .request_owner_contributions
                .values()
                .map(BTreeMap::len)
                .sum(),
            request_claim_owner_count_keys: self.request_claim_owner_counts.len(),
            request_provenance_owner_count_keys: self.request_provenance_owner_counts.len(),
            request_demand_coverage_owner_count_keys: self
                .request_demand_coverage_owner_counts
                .len(),
            coverage_assignment_keys: self.coverage_assignment_requests.len(),
            coverage_assignment_edges: self
                .coverage_assignment_requests
                .values()
                .map(BTreeSet::len)
                .sum(),
            refused_coverage_assignment_demands: self.refused_coverage_assignments_by_demand.len(),
            refused_coverage_assignment_authors: self
                .refused_coverage_assignments_by_demand
                .values()
                .map(BTreeSet::len)
                .sum(),
            active_outbox_authors: self.active_outbox_authors.len(),
            refusal_demand_keys: self.refusals_by_demand.len(),
            refusal_demand_edges: self.refusals_by_demand.values().map(BTreeMap::len).sum(),
            refused_request_owner_keys: self.refused_request_owner_counts.len(),
            refused_session_owner_keys: self.refused_owner_counts_by_session.len(),
            diagnostic_author_session_keys: self.diagnostic_author_refs.len(),
            diagnostic_author_edges: self
                .diagnostic_author_refs
                .values()
                .map(BTreeMap::len)
                .sum(),
            uncovered_demand_keys: self.uncovered_by_demand.len(),
            uncovered_author_keys: self.uncovered_owners_by_author.len(),
            uncovered_author_refs: self
                .uncovered_owners_by_author
                .values()
                .map(BTreeMap::len)
                .sum(),
            plan_sessions: self.prev_plan.reqs.len(),
            plan_requests: self.prev_plan.reqs.values().map(Vec::len).sum(),
            plan_limited_demands: self.prev_plan.limited_demands.len(),
            plan_refused_sessions: self.prev_plan.refused_sessions.len(),
            plan_subscription_shortfalls: self.prev_plan.subscription_shortfalls.len(),
            diagnostic_sessions: self.last_diag.per_session.len(),
            diagnostic_uncovered_authors: self.last_diag.uncovered_authors.len(),
            diagnostic_sessions_refused_by_cap: self.last_diag.sessions_refused_by_cap,
            diagnostic_sessions_refused_by_subscription_budget: self
                .last_diag
                .sessions_refused_by_subscription_budget,
            diagnostic_dropped_merge_rules: self.last_diag.dropped_merge_rules.len(),
        }
    }

    pub fn physically_covers(&self, demand: DemandKey) -> bool {
        self.requests_by_demand.contains_key(&demand)
    }

    /// Whether one exact demand still has relay work or routing shortfall
    /// eligible for a later pending-only admission cohort.
    pub fn admission_incomplete(&self, demand: DemandKey) -> bool {
        !self.physically_covers(demand)
            || self.prev_plan.limited_demands.contains(&demand)
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
        Some(current.union(physical).copied().collect())
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
    /// only sound key for routing request-execution facts back to observers.
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

    /// Exact per-demand diagnostic ownership retained before public
    /// same-author reduction. This is a falsifier seam, not a routing input.
    #[doc(hidden)]
    pub fn demand_shortfalls(&self, demand: DemandKey) -> Option<&BTreeMap<PublicKey, Shortfall>> {
        self.uncovered_by_demand.get(&demand)
    }
}
