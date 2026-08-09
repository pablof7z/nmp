//! Exact physical-request indexes and incremental diagnostic projection.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::RelaySessionKey;

use crate::plan::{DemandKey, WireReq};
use crate::{CompileBudget, PublicKey, RelayDiagnostics, RouteProvenance, Router};

use super::RequestKey;

impl Router {
    pub(crate) fn index_physical_request_ownership(
        &mut self,
        request_key: &RequestKey,
        request: &WireReq,
    ) {
        self.index_physical_request_claims(request_key, &request.coverage_claims);
        let contributions: BTreeMap<_, _> = request
            .owner_demands
            .iter()
            .map(|demand| {
                let contribution = self
                    .active_demands
                    .get(demand)
                    .map(|atom| Self::derive_request_owner_contribution(atom, request))
                    .unwrap_or_default();
                (*demand, contribution)
            })
            .collect();
        for demand in contributions.keys() {
            self.requests_by_physical_demand
                .entry(*demand)
                .or_default()
                .insert(request_key.clone());
        }
        let previous = self
            .physical_contributions_by_request
            .insert(request_key.clone(), contributions);
        assert!(
            previous.is_none(),
            "physical request contributions must be indexed exactly once"
        );
    }

    pub(crate) fn index_physical_request_claims(
        &mut self,
        request_key: &RequestKey,
        claims: &BTreeSet<nmp_store::CoverageKey>,
    ) {
        let previous = self
            .physical_claims_by_request
            .insert(request_key.clone(), claims.clone());
        assert!(
            previous.is_none(),
            "physical request claim ownership must be indexed exactly once"
        );
        for claim in claims {
            self.requests_by_physical_claim
                .entry(*claim)
                .or_default()
                .insert(request_key.clone());
        }
    }

    pub(crate) fn remove_physical_request_claims(&mut self, request_key: &RequestKey) -> usize {
        let claims = self
            .physical_claims_by_request
            .remove(request_key)
            .expect("live request must retain immutable physical claim ownership");
        for claim in &claims {
            let requests = self
                .requests_by_physical_claim
                .get_mut(claim)
                .expect("physical claim reverse edge must exist");
            assert!(
                requests.remove(request_key),
                "physical claim reverse edge must name its request"
            );
            if requests.is_empty() {
                self.requests_by_physical_claim.remove(claim);
            }
        }
        if let Some(contributions) = self.physical_contributions_by_request.remove(request_key) {
            for demand in contributions.keys() {
                let requests = self
                    .requests_by_physical_demand
                    .get_mut(demand)
                    .expect("physical demand reverse edge must exist");
                assert!(
                    requests.remove(request_key),
                    "physical demand reverse edge must name its request"
                );
                if requests.is_empty() {
                    self.requests_by_physical_demand.remove(demand);
                }
            }
        }
        claims.len()
    }

    fn add_request_diagnostic_ownership(
        &mut self,
        session: &RelaySessionKey,
        provenance: impl IntoIterator<Item = RouteProvenance>,
    ) {
        let diagnostics = self
            .last_diag
            .per_session
            .entry(session.clone())
            .or_insert_with(|| RelayDiagnostics {
                session: session.clone(),
                wire_sub_count: 0,
                by_lane: BTreeMap::new(),
                authors_served: 0,
                filters: Vec::new(),
                subscription_budget: None,
                subscriptions_refused: 0,
                subid_length_limit: None,
                subid_length_rejects_our_ids: false,
            });
        for provenance in provenance {
            self.full_metadata_work.diagnostic_provenance_edges_visited = self
                .full_metadata_work
                .diagnostic_provenance_edges_visited
                .saturating_add(1);
            *diagnostics.by_lane.entry(provenance.lane).or_insert(0) += 1;
            for author in provenance.covers_authors {
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
        diagnostics.authors_served = self
            .diagnostic_author_refs
            .get(session)
            .map_or(0, BTreeMap::len);
    }

    pub(crate) fn remove_request_diagnostic_ownership(
        &mut self,
        session: &RelaySessionKey,
        provenance: impl IntoIterator<Item = RouteProvenance>,
    ) {
        let mut remove_author_session = false;
        for provenance in provenance {
            self.full_metadata_work.diagnostic_provenance_edges_visited = self
                .full_metadata_work
                .diagnostic_provenance_edges_visited
                .saturating_add(1);
            if let Some(diagnostics) = self.last_diag.per_session.get_mut(session) {
                let remove_lane =
                    diagnostics
                        .by_lane
                        .get_mut(&provenance.lane)
                        .is_some_and(|count| {
                            *count = count
                                .checked_sub(1)
                                .expect("diagnostic lane refcount cannot underflow");
                            *count == 0
                        });
                if remove_lane {
                    diagnostics.by_lane.remove(&provenance.lane);
                }
            }
            for author in provenance.covers_authors {
                self.full_metadata_work.provenance_author_edges_visited = self
                    .full_metadata_work
                    .provenance_author_edges_visited
                    .saturating_add(1);
                if let Some(author_refs) = self.diagnostic_author_refs.get_mut(session) {
                    let remove_author = author_refs.get_mut(&author).is_some_and(|count| {
                        *count = count
                            .checked_sub(1)
                            .expect("diagnostic author refcount cannot underflow");
                        *count == 0
                    });
                    if remove_author {
                        author_refs.remove(&author);
                    }
                    remove_author_session = author_refs.is_empty();
                }
            }
        }
        if remove_author_session {
            self.diagnostic_author_refs.remove(session);
        }
        if let Some(diagnostics) = self.last_diag.per_session.get_mut(session) {
            diagnostics.authors_served = self
                .diagnostic_author_refs
                .get(session)
                .map_or(0, BTreeMap::len);
        }
    }

    pub(crate) fn remove_full_request_indexes(
        &mut self,
        session: &RelaySessionKey,
        request: &WireReq,
    ) {
        let request_key = (session.clone(), request.sub_id.clone());
        self.request_by_exact_filter.remove(&(
            session.clone(),
            request.source.clone(),
            request.filter.clone(),
        ));
        self.request_position_by_key.remove(&request_key);
        self.request_coverage_by_key.remove(&request_key);
        self.remove_physical_request_claims(&request_key);
        self.active_by_request.remove(&request_key);
        for demand in &request.owner_demands {
            self.remove_request_owner_contribution(&request_key, *demand);
            self.full_metadata_work.owner_edges_visited = self
                .full_metadata_work
                .owner_edges_visited
                .saturating_add(1);
            if let Some(requests) = self.requests_by_demand.get_mut(demand) {
                requests.remove(&request_key);
                if requests.is_empty() {
                    self.requests_by_demand.remove(demand);
                }
            }
        }
        debug_assert!(!self.request_owner_contributions.contains_key(&request_key));
        for assignment in &request.coverage_assignments {
            self.full_metadata_work.assignment_edges_visited = self
                .full_metadata_work
                .assignment_edges_visited
                .saturating_add(1);
            if let Some(requests) = self.coverage_assignment_requests.get_mut(assignment) {
                requests.remove(&request_key);
                if requests.is_empty() {
                    self.coverage_assignment_requests.remove(assignment);
                }
            }
        }
        self.remove_request_diagnostic_ownership(session, request.provenance.iter().cloned());
    }

    pub(crate) fn add_full_request_indexes(
        &mut self,
        session: &RelaySessionKey,
        request: &WireReq,
    ) {
        let request_key = (session.clone(), request.sub_id.clone());
        let mut active = 0;
        let mut coverage = BTreeSet::new();
        let contributions: Vec<_> = request
            .owner_demands
            .iter()
            .filter_map(|demand| {
                self.active_demands.get(demand).map(|atom| {
                    (
                        *demand,
                        Self::derive_request_owner_contribution(atom, request),
                    )
                })
            })
            .collect();
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
            coverage.insert(demand.coverage());
        }
        self.active_by_request.insert(request_key.clone(), active);
        self.request_coverage_by_key
            .insert(request_key.clone(), coverage);
        self.index_physical_request_ownership(&request_key, request);
        for (demand, contribution) in contributions {
            self.add_request_owner_contribution(&request_key, demand, contribution);
        }
        self.request_by_exact_filter.insert(
            (
                session.clone(),
                request.source.clone(),
                request.filter.clone(),
            ),
            request_key.clone(),
        );
        for assignment in &request.coverage_assignments {
            self.full_metadata_work.assignment_edges_visited = self
                .full_metadata_work
                .assignment_edges_visited
                .saturating_add(1);
            self.coverage_assignment_requests
                .entry(*assignment)
                .or_default()
                .insert(request_key.clone());
        }
        self.add_request_diagnostic_ownership(session, request.provenance.iter().cloned());
    }

    pub(crate) fn add_full_request_metadata_indexes(
        &mut self,
        request_key: &RequestKey,
        owner_demands: &BTreeSet<DemandKey>,
        assignments: &BTreeSet<(DemandKey, PublicKey)>,
        provenance: &BTreeSet<RouteProvenance>,
    ) {
        for demand in owner_demands {
            self.full_metadata_work.owner_edges_visited = self
                .full_metadata_work
                .owner_edges_visited
                .saturating_add(1);
            self.requests_by_demand
                .entry(*demand)
                .or_default()
                .insert(request_key.clone());
            if self.active_demands.contains_key(demand) {
                *self
                    .active_by_request
                    .get_mut(request_key)
                    .expect("unchanged request retains its active-count index") += 1;
            }
            self.request_coverage_by_key
                .entry(request_key.clone())
                .or_default()
                .insert(demand.coverage());
        }
        for assignment in assignments {
            self.full_metadata_work.assignment_edges_visited = self
                .full_metadata_work
                .assignment_edges_visited
                .saturating_add(1);
            self.coverage_assignment_requests
                .entry(*assignment)
                .or_default()
                .insert(request_key.clone());
        }
        self.add_request_diagnostic_ownership(&request_key.0, provenance.iter().cloned());
    }

    pub(crate) fn rebuild_request_positions(&mut self) {
        self.request_position_by_key.clear();
        for (session, requests) in &self.prev_plan.reqs {
            for (position, request) in requests.iter().enumerate() {
                self.request_position_by_key
                    .insert((session.clone(), request.sub_id.clone()), position);
            }
        }
    }

    pub(crate) fn project_full_diagnostics(
        &mut self,
        budget: &CompileBudget,
        dropped_merge_rules: Vec<&'static str>,
    ) {
        self.last_diag
            .per_session
            .retain(|session, _| self.prev_plan.reqs.contains_key(session));
        for (session, requests) in &self.prev_plan.reqs {
            let diagnostics = self
                .last_diag
                .per_session
                .entry(session.clone())
                .or_insert_with(|| RelayDiagnostics {
                    session: session.clone(),
                    wire_sub_count: 0,
                    by_lane: BTreeMap::new(),
                    authors_served: 0,
                    filters: Vec::new(),
                    subscription_budget: None,
                    subscriptions_refused: 0,
                    subid_length_limit: None,
                    subid_length_rejects_our_ids: false,
                });
            diagnostics.wire_sub_count = requests.len();
            diagnostics.filters = requests
                .iter()
                .map(|request| request.filter.clone())
                .collect();
            diagnostics.authors_served = self
                .diagnostic_author_refs
                .get(session)
                .map_or(0, BTreeMap::len);
            diagnostics.subscription_budget = budget.max_subscriptions(&session.relay);
            diagnostics.subscriptions_refused = self
                .prev_plan
                .subscription_shortfalls
                .get(session)
                .map_or(0, |shortfall| shortfall.refused);
            diagnostics.subid_length_limit = budget.max_subid_length(&session.relay);
            diagnostics.subid_length_rejects_our_ids =
                budget.rejects_our_subscription_ids(&session.relay);
        }
        let refused_by_budget = self
            .prev_plan
            .refused_sessions
            .iter()
            .filter(|session| {
                self.prev_plan
                    .subscription_shortfalls
                    .contains_key(*session)
            })
            .count();
        self.last_diag.sessions_refused_by_cap = self
            .prev_plan
            .refused_sessions
            .len()
            .saturating_sub(refused_by_budget);
        self.last_diag.sessions_refused_by_subscription_budget = refused_by_budget;
        self.last_diag.dropped_merge_rules = dropped_merge_rules;
    }
}
