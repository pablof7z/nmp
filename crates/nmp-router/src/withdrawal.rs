//! Exact delta withdrawal for active and refused relay ownership.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ContextualAtom, RelaySessionKey};

use crate::budget::CompileBudget;
use crate::ownership::{refresh_refusal_diagnostics, refused_session_class};
use crate::plan::{DemandKey, WireDelta, WireOp, WireReq};
use crate::route;
use crate::{RequestMetadataRemoval, Router, WithdrawalOutcome};

impl Router {
    fn remove_request_diagnostics(
        &mut self,
        session: &RelaySessionKey,
        position: usize,
        request: &WireReq,
        budget: &CompileBudget,
    ) {
        let mut remove_session_authors = false;
        if let Some(author_refs) = self.diagnostic_author_refs.get_mut(session) {
            for author in request
                .provenance
                .iter()
                .flat_map(|provenance| provenance.covers_authors.iter())
            {
                let remove_author = author_refs.get_mut(author).is_some_and(|count| {
                    *count = count.saturating_sub(1);
                    *count == 0
                });
                if remove_author {
                    author_refs.remove(author);
                }
            }
            remove_session_authors = author_refs.is_empty();
        }
        if remove_session_authors {
            self.diagnostic_author_refs.remove(session);
        }

        let session_is_empty = !self.prev_plan.reqs.contains_key(session);
        let Some(diagnostics) = self.last_diag.per_session.get_mut(session) else {
            return;
        };
        diagnostics.filters.swap_remove(position);
        diagnostics.wire_sub_count = diagnostics.wire_sub_count.saturating_sub(1);
        for provenance in &request.provenance {
            let remove_lane = diagnostics
                .by_lane
                .get_mut(&provenance.lane)
                .is_some_and(|count| {
                    *count = count.saturating_sub(1);
                    *count == 0
                });
            if remove_lane {
                diagnostics.by_lane.remove(&provenance.lane);
            }
        }
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
        if session_is_empty {
            self.last_diag.per_session.remove(session);
        }
    }

    /// Consume exact resolver-style closes without inspecting any sibling
    /// demand. A physical request closes only when its incremental active
    /// owner count reaches zero.
    pub fn withdraw(
        &mut self,
        closing_atoms: impl IntoIterator<Item = ContextualAtom>,
        budget: impl Into<CompileBudget>,
    ) -> WithdrawalOutcome {
        let budget = budget.into();
        let mut closing = BTreeSet::new();
        let mut limited_changed = false;
        let mut uncovered_authors_changed = false;
        let mut changed_coverage = BTreeSet::new();
        let mut request_metadata_removals = Vec::new();
        let mut metadata_diagnostics_changed = false;
        for atom in closing_atoms {
            self.withdrawal_work.dropped_atoms =
                self.withdrawal_work.dropped_atoms.saturating_add(1);
            let key = DemandKey::for_atom(&atom);
            let Some(active_atom) = self.active_demands.remove(&key) else {
                continue;
            };
            uncovered_authors_changed |= self.remove_uncovered_demand(key);
            for author in route::outbox_authors(&active_atom.filter, &active_atom.routing) {
                let remove = self
                    .active_outbox_authors
                    .get_mut(&author)
                    .is_some_and(|count| {
                        *count = count.saturating_sub(1);
                        *count == 0
                    });
                if remove {
                    self.active_outbox_authors.remove(&author);
                }
            }
            for request in self
                .requests_by_demand
                .get(&key)
                .cloned()
                .unwrap_or_default()
            {
                self.withdrawal_work.request_edges_touched =
                    self.withdrawal_work.request_edges_touched.saturating_add(1);
                let count = self
                    .active_by_request
                    .get_mut(&request)
                    .expect("physical demand edge names a live request");
                *count = count
                    .checked_sub(1)
                    .expect("physical request active-owner count cannot underflow");
                if *count == 0 {
                    closing.insert(request);
                    continue;
                }

                let contribution = self.remove_request_owner_contribution(&request, key);
                debug_assert!(
                    contribution.owner_added,
                    "an active request edge must own exact local metadata"
                );
                self.withdrawal_work.metadata_owner_entries_touched = self
                    .withdrawal_work
                    .metadata_owner_entries_touched
                    .saturating_add(1);
                self.withdrawal_work.metadata_claim_entries_touched = self
                    .withdrawal_work
                    .metadata_claim_entries_touched
                    .saturating_add(contribution.coverage_claims.len() as u64);
                self.withdrawal_work.metadata_assignment_entries_touched = self
                    .withdrawal_work
                    .metadata_assignment_entries_touched
                    .saturating_add(contribution.coverage_assignments.len() as u64);
                self.withdrawal_work.metadata_provenance_entries_touched = self
                    .withdrawal_work
                    .metadata_provenance_entries_touched
                    .saturating_add(contribution.provenance.len() as u64);

                if let Some(requests) = self.requests_by_demand.get_mut(&key) {
                    requests.remove(&request);
                    if requests.is_empty() {
                        self.requests_by_demand.remove(&key);
                    }
                }
                if !self
                    .request_demand_coverage_owner_counts
                    .contains_key(&(request.clone(), key.coverage()))
                {
                    if let Some(coverage) = self.request_coverage_by_key.get_mut(&request) {
                        coverage.remove(&key.coverage());
                    }
                }
                for assignment in &contribution.coverage_assignments {
                    if let Some(requests) = self.coverage_assignment_requests.get_mut(assignment) {
                        requests.remove(&request);
                        if requests.is_empty() {
                            self.coverage_assignment_requests.remove(assignment);
                        }
                    }
                }
                if !contribution.provenance.is_empty() {
                    metadata_diagnostics_changed = true;
                    self.remove_request_diagnostic_ownership(
                        &request.0,
                        contribution.provenance.iter().cloned(),
                    );
                }
                let position = self.request_position_by_key[&request];
                let retained = &mut self.prev_plan.reqs.get_mut(&request.0).unwrap()[position];
                retained.owner_demands.remove(&key);
                for claim in &contribution.coverage_claims {
                    retained.coverage_claims.remove(claim);
                }
                for assignment in &contribution.coverage_assignments {
                    retained.coverage_assignments.remove(assignment);
                }
                for provenance in &contribution.provenance {
                    retained.provenance.remove(provenance);
                }
                request_metadata_removals.push(RequestMetadataRemoval {
                    session: request.0.clone(),
                    sub_id: request.1.clone(),
                    filter_hash: retained.filter.hash(),
                    removed_coverage_claims: contribution.coverage_claims,
                    removed_owner_demands: BTreeSet::from([key]),
                });
            }
            if self.remove_refusal_owners(key) {
                limited_changed = true;
                changed_coverage.insert(key.coverage());
            }
        }

        let mut wire_by_session: BTreeMap<RelaySessionKey, Vec<WireOp>> = BTreeMap::new();
        for (session, sub_id) in closing {
            let before = refused_session_class(self, &session);
            let request_key = (session.clone(), sub_id.clone());
            self.request_by_exact_filter.remove(&(
                session.clone(),
                self.prev_plan.reqs[&session][self.request_position_by_key[&request_key]]
                    .routing
                    .clone(),
                self.prev_plan.reqs[&session][self.request_position_by_key[&request_key]]
                    .filter
                    .clone(),
            ));
            let position = self
                .request_position_by_key
                .remove(&request_key)
                .expect("live request must have an exact plan position");
            let requests = self
                .prev_plan
                .reqs
                .get_mut(&session)
                .expect("live request session must exist");
            let removed = requests.swap_remove(position);
            assert_eq!(removed.sub_id, sub_id, "request position index drifted");
            self.withdrawal_work.plan_request_entries_visited = self
                .withdrawal_work
                .plan_request_entries_visited
                .saturating_add(1);
            if let Some(moved) = requests.get(position) {
                self.request_position_by_key
                    .insert((session.clone(), moved.sub_id.clone()), position);
            }
            if requests.is_empty() {
                self.prev_plan.reqs.remove(&session);
            }
            let released_coverage = self
                .request_coverage_by_key
                .remove(&request_key)
                .expect("live request must have exact retained coverage");
            changed_coverage.extend(released_coverage);

            if let Some(shortfall) = self.prev_plan.subscription_shortfalls.get_mut(&session) {
                shortfall.planned = shortfall.planned.saturating_sub(1);
            }
            self.remove_request_diagnostics(&session, position, &removed, &budget);
            self.withdrawal_work.requests_closed =
                self.withdrawal_work.requests_closed.saturating_add(1);
            self.withdrawal_work.physical_coverage_edges_released = self
                .withdrawal_work
                .physical_coverage_edges_released
                .saturating_add(self.remove_physical_request_claims(&request_key) as u64);
            for demand in &removed.owner_demands {
                self.remove_request_owner_contribution(&request_key, *demand);
            }
            debug_assert!(!self.request_owner_contributions.contains_key(&request_key));
            for assignment in removed.coverage_assignments {
                if let Some(requests) = self.coverage_assignment_requests.get_mut(&assignment) {
                    requests.remove(&request_key);
                    if requests.is_empty() {
                        self.coverage_assignment_requests.remove(&assignment);
                    }
                }
            }
            for demand in removed.owner_demands {
                if let Some(requests) = self.requests_by_demand.get_mut(&demand) {
                    requests.remove(&request_key);
                    if requests.is_empty() {
                        self.requests_by_demand.remove(&demand);
                    }
                }
            }
            self.active_by_request.remove(&request_key);
            if self.prev_plan.reqs.contains_key(&session)
                || !self.refused_owner_counts_by_session.contains_key(&session)
            {
                self.prev_plan.refused_sessions.remove(&session);
            } else {
                self.prev_plan.refused_sessions.insert(session.clone());
            }
            refresh_refusal_diagnostics(self, &session, before);
            wire_by_session
                .entry(session)
                .or_default()
                .push(WireOp::Close(sub_id));
        }

        let diagnostics_changed = !wire_by_session.is_empty()
            || limited_changed
            || uncovered_authors_changed
            || metadata_diagnostics_changed;
        if diagnostics_changed {
            self.withdrawal_work.diagnostic_rebuilds =
                self.withdrawal_work.diagnostic_rebuilds.saturating_add(1);
        }
        WithdrawalOutcome {
            wire: WireDelta {
                ops: wire_by_session.into_iter().collect(),
            },
            changed_coverage,
            diagnostics_changed,
            request_metadata_removals,
        }
    }
}
