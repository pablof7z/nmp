//! Candidate-local, read-only admission preview.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ContextualAtom, RelaySessionKey};
use nmp_store::{coverage_claim_atoms, coverage_key, CoverageKey};

use crate::budget::CompileBudget;
use crate::facts::RoutingFacts;
use crate::ownership::{AdmissionPreview, AdmissionPreviewWork, RefusalKind, RequestKey};
use crate::plan::{DemandKey, RelayPlan, WireReq};
use crate::Router;

use super::metadata::rank_new_sessions;

pub(super) struct ResidualBudget {
    pub(super) refused_requests: Vec<(RelaySessionKey, WireReq, RefusalKind)>,
    pub(super) candidate_request_totals: BTreeMap<RelaySessionKey, usize>,
    pub(super) existing_request_counts: BTreeMap<RelaySessionKey, usize>,
    pub(super) budget_refused_counts: BTreeMap<RelaySessionKey, usize>,
}

pub(super) fn apply_residual_budget(
    running: &RelayPlan,
    candidate: &mut RelayPlan,
    budget: &CompileBudget,
) -> ResidualBudget {
    let available_new_sessions = budget.relay_cap().saturating_sub(running.reqs.len());
    let new_sessions = rank_new_sessions(
        &candidate.reqs,
        candidate
            .reqs
            .keys()
            .filter(|session| !running.reqs.contains_key(*session))
            .cloned(),
    );
    let mut refused_requests = Vec::new();
    for session in new_sessions.into_iter().skip(available_new_sessions) {
        if let Some(refused) = candidate.reqs.remove(&session) {
            refused_requests.extend(
                refused
                    .into_iter()
                    .map(|request| (session.clone(), request, RefusalKind::RelayCap)),
            );
        }
    }

    let candidate_request_totals = candidate
        .reqs
        .iter()
        .map(|(session, requests)| (session.clone(), requests.len()))
        .collect();
    let existing_request_counts = candidate
        .reqs
        .keys()
        .map(|session| {
            (
                session.clone(),
                running.reqs.get(session).map_or(0, Vec::len),
            )
        })
        .collect();
    let mut budget_refused_counts = BTreeMap::new();
    let sessions: Vec<_> = candidate.reqs.keys().cloned().collect();
    for session in sessions {
        let existing = running.reqs.get(&session).map_or(0, Vec::len);
        let Some(limit) = budget.max_subscriptions(&session.relay) else {
            continue;
        };
        let allowed = limit.saturating_sub(existing);
        let requests = candidate
            .reqs
            .get_mut(&session)
            .expect("session came from candidate plan");
        if requests.len() <= allowed {
            continue;
        }
        requests.sort_by(|a, b| {
            b.coverage_assignments
                .len()
                .cmp(&a.coverage_assignments.len())
                .then_with(|| b.coverage_claims.len().cmp(&a.coverage_claims.len()))
                .then_with(|| b.provenance.len().cmp(&a.provenance.len()))
                .then_with(|| a.filter.hash().cmp(&b.filter.hash()))
        });
        let refused = requests.split_off(allowed);
        requests.sort_by(|a, b| a.sub_id.cmp(&b.sub_id));
        budget_refused_counts.insert(session.clone(), refused.len());
        refused_requests.extend(
            refused
                .into_iter()
                .map(|request| (session.clone(), request, RefusalKind::SubscriptionBudget)),
        );
        if requests.is_empty() {
            candidate.reqs.remove(&session);
        }
    }

    ResidualBudget {
        refused_requests,
        candidate_request_totals,
        existing_request_counts,
        budget_refused_counts,
    }
}

fn claims_by_demand(
    cohort: &BTreeSet<ContextualAtom>,
) -> BTreeMap<DemandKey, BTreeSet<CoverageKey>> {
    cohort
        .iter()
        .map(|atom| {
            (
                DemandKey::for_atom(atom),
                coverage_claim_atoms(atom)
                    .into_iter()
                    .map(|claim| coverage_key(&claim))
                    .collect(),
            )
        })
        .collect()
}

fn extend_scoped_request(
    plan: &mut RelayPlan,
    session: RelaySessionKey,
    incumbent: &WireReq,
    demands: BTreeSet<DemandKey>,
    claims: BTreeSet<CoverageKey>,
) {
    let requests = plan.reqs.entry(session).or_default();
    if let Some(existing) = requests
        .iter_mut()
        .find(|request| request.sub_id == incumbent.sub_id)
    {
        existing.owner_demands.extend(demands);
        existing.coverage_claims.extend(claims);
        return;
    }
    requests.push(WireReq {
        sub_id: incumbent.sub_id.clone(),
        filter: incumbent.filter.clone(),
        routing: incumbent.routing.clone(),
        provenance: BTreeSet::new(),
        coverage_claims: claims,
        owner_demands: demands,
        coverage_assignments: BTreeSet::new(),
    });
}

impl Router {
    fn request(&self, key: &RequestKey) -> Option<&WireReq> {
        let position = self.request_position_by_key.get(key)?;
        self.prev_plan.reqs.get(&key.0)?.get(*position)
    }

    fn demand_requests(&self, demand: DemandKey) -> BTreeSet<RequestKey> {
        self.requests_by_demand
            .get(&demand)
            .into_iter()
            .flatten()
            .chain(
                self.requests_by_physical_demand
                    .get(&demand)
                    .into_iter()
                    .flatten(),
            )
            .cloned()
            .collect()
    }

    /// Evaluate one cohort against current immutable requests and residual
    /// capacity without changing live ownership, diagnostics, or wire state.
    #[doc(hidden)]
    pub fn preview_admission(
        &self,
        cohort: &BTreeSet<ContextualAtom>,
        facts: &dyn RoutingFacts,
        budget: impl Into<CompileBudget>,
    ) -> AdmissionPreview {
        let budget = budget.into();
        let mut work = AdmissionPreviewWork {
            candidate_atoms: cohort.len() as u64,
            ..AdmissionPreviewWork::default()
        };
        let claims = claims_by_demand(cohort);
        let mut plan = RelayPlan::default();

        for atom in cohort {
            let demand = DemandKey::for_atom(atom);
            for request_key in self.demand_requests(demand) {
                work.incumbent_demand_edges_visited =
                    work.incumbent_demand_edges_visited.saturating_add(1);
                let Some(request) = self.request(&request_key) else {
                    continue;
                };
                work.incumbent_request_entries_visited =
                    work.incumbent_request_entries_visited.saturating_add(1);
                let eligible = self
                    .request_claims(&request_key.0, &request_key.1)
                    .unwrap_or_default();
                let scoped_claims = claims[&demand].intersection(&eligible).copied().collect();
                extend_scoped_request(
                    &mut plan,
                    request_key.0,
                    request,
                    BTreeSet::from([demand]),
                    scoped_claims,
                );
            }
        }

        let pending: BTreeSet<_> = cohort
            .iter()
            .filter(|atom| self.admission_incomplete(DemandKey::for_atom(atom)))
            .cloned()
            .collect();
        if pending.is_empty() {
            return AdmissionPreview { plan, work };
        }

        let mut candidate_router = Router::new(self.rules.fork());
        let _ =
            candidate_router.compile(&pending, facts, CompileBudget::with_relay_cap(usize::MAX));
        work.coalesce_pair_attempts = candidate_router.rules.pair_attempts();
        let mut candidate = candidate_router.plan().clone();

        let sessions: Vec<_> = candidate.reqs.keys().cloned().collect();
        for session in sessions {
            let requests = candidate.reqs.remove(&session).unwrap_or_default();
            let mut remaining = Vec::new();
            for mut request in requests {
                if let Some(request_key) = self.covering_request_key(&session, &request) {
                    work.incumbent_request_entries_visited =
                        work.incumbent_request_entries_visited.saturating_add(1);
                    if let Some(incumbent) = self.request(&request_key) {
                        extend_scoped_request(
                            &mut plan,
                            session.clone(),
                            incumbent,
                            request.owner_demands,
                            request.coverage_claims,
                        );
                    }
                    continue;
                }

                request.owner_demands.retain(|demand| {
                    !self
                        .demand_requests(*demand)
                        .iter()
                        .any(|request_key| request_key.0 == session)
                });
                request
                    .coverage_assignments
                    .retain(|(demand, _)| request.owner_demands.contains(demand));
                let retained_claims: BTreeSet<_> = request
                    .owner_demands
                    .iter()
                    .flat_map(|demand| claims.get(demand).into_iter().flatten().copied())
                    .collect();
                request
                    .coverage_claims
                    .retain(|claim| retained_claims.contains(claim));
                if !request.owner_demands.is_empty() {
                    remaining.push(request);
                }
            }
            if !remaining.is_empty() {
                candidate.reqs.insert(session, remaining);
            }
        }

        let residual = apply_residual_budget(&self.prev_plan, &mut candidate, &budget);
        for (_, request, _) in residual.refused_requests {
            plan.limited_demands.extend(request.owner_demands);
        }
        plan.limited_demands.extend(candidate.limited_demands);
        for (session, mut requests) in candidate.reqs {
            plan.reqs.entry(session).or_default().append(&mut requests);
        }
        AdmissionPreview { plan, work }
    }
}
