//! Pending-only admission and immutable withdrawal for relay plans.
//!
//! A cohort is compiled in an empty incumbent namespace, then appended to
//! the running plan. Existing requests are therefore candidates for exact
//! coverage reuse, never candidates for widening or identity reassignment.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::ContextualAtom;

use crate::budget::CompileBudget;
use crate::facts::RoutingFacts;
use crate::ownership::{
    reduce_outbox_shortfall, refresh_refusal_diagnostics, refused_session_class, RefusalKind,
};
use crate::plan::{BudgetShortfall, DemandKey, WireDelta, WireOp};
use crate::route::{self, AtomClass};
use crate::{AdmissionOutcome, Router};

mod metadata;

use metadata::rank_new_sessions;

impl Router {
    /// Reactivate one exact logical owner already covered by an immutable
    /// physical request. Reattachment updates only that demand's retained
    /// request edges; it never recompiles or mutates the request itself.
    pub fn activate(&mut self, atom: ContextualAtom) {
        let key = DemandKey::for_atom(&atom);
        if let Some(active) = self.active_demands.get_mut(&key) {
            // Routing evidence is deliberately absent from `DemandKey`:
            // multiple exact app owners share one logical selection while
            // contributing independently-lived route facts. Refresh only
            // the retained logical atom; immutable request edges and their
            // active counts do not change.
            *active = atom;
            return;
        }
        if let AtomClass::Coverage { authors, .. } = route::classify(&atom.filter, &atom.source) {
            for author in authors {
                *self.active_outbox_authors.entry(author).or_insert(0) += 1;
            }
        }
        self.active_demands.insert(key, atom);
        for request in self.requests_by_demand.get(&key).into_iter().flatten() {
            let count = self
                .active_by_request
                .get_mut(request)
                .expect("physical demand edge names a live request");
            *count = count.saturating_add(1);
        }
    }

    /// Admit one already-routed logical cohort without rewriting running
    /// requests. Exact coverage already present in the plan is a no-op.
    pub fn admit(
        &mut self,
        cohort: &BTreeSet<ContextualAtom>,
        facts: &dyn RoutingFacts,
        budget: impl Into<CompileBudget>,
    ) -> AdmissionOutcome {
        let budget = budget.into();
        let pending: BTreeSet<_> = cohort
            .iter()
            .filter(|atom| {
                let key = DemandKey::for_atom(atom);
                !self.active_demands.contains_key(&key)
                    || !self.requests_by_demand.contains_key(&key)
                    || self.prev_plan.limited_demands.contains(&key)
                    || self.uncovered_by_demand.contains_key(&key)
            })
            .cloned()
            .collect();
        for atom in cohort {
            if !self.active_demands.contains_key(&DemandKey::for_atom(atom)) {
                self.activate(atom.clone());
                self.admission_work.active_entries_appended = self
                    .admission_work
                    .active_entries_appended
                    .saturating_add(1);
            }
        }
        if pending.is_empty() {
            return AdmissionOutcome {
                wire: WireDelta::default(),
                changed_coverage: BTreeSet::new(),
                diagnostics_changed: false,
                request_metadata_updates: Vec::new(),
            };
        }
        self.admission_work.cohort_compiles = self.admission_work.cohort_compiles.saturating_add(1);
        let mut changed_coverage = BTreeSet::new();
        for demand in pending.iter().map(DemandKey::for_atom) {
            if self.remove_refusal_owners(demand) {
                changed_coverage.insert(demand.coverage());
            }
        }

        // Reuse the one canonical routing/coalescing compiler with an empty
        // incumbent view. The running indexes are detached wholesale, not
        // rebuilt: candidate compilation can visit only `pending`, while the
        // monotonic token counter remains shared and therefore never rewinds.
        let running_plan = std::mem::take(&mut self.prev_plan);
        let running_diag = std::mem::take(&mut self.last_diag);
        let running_active_demands = std::mem::take(&mut self.active_demands);
        let running_requests_by_demand = std::mem::take(&mut self.requests_by_demand);
        let running_active_by_request = std::mem::take(&mut self.active_by_request);
        let running_request_coverage_by_key = std::mem::take(&mut self.request_coverage_by_key);
        let running_request_position_by_key = std::mem::take(&mut self.request_position_by_key);
        let running_request_by_exact_filter = std::mem::take(&mut self.request_by_exact_filter);
        let running_physical_claims_by_request =
            std::mem::take(&mut self.physical_claims_by_request);
        let running_requests_by_physical_claim =
            std::mem::take(&mut self.requests_by_physical_claim);
        let running_physical_contributions_by_request =
            std::mem::take(&mut self.physical_contributions_by_request);
        let running_requests_by_physical_demand =
            std::mem::take(&mut self.requests_by_physical_demand);
        let running_request_owner_contributions =
            std::mem::take(&mut self.request_owner_contributions);
        let running_request_claim_owner_counts =
            std::mem::take(&mut self.request_claim_owner_counts);
        let running_request_provenance_owner_counts =
            std::mem::take(&mut self.request_provenance_owner_counts);
        let running_request_demand_coverage_owner_counts =
            std::mem::take(&mut self.request_demand_coverage_owner_counts);
        let running_coverage_assignment_requests =
            std::mem::take(&mut self.coverage_assignment_requests);
        let running_refused_coverage_assignments_by_demand =
            std::mem::take(&mut self.refused_coverage_assignments_by_demand);
        let running_active_outbox_authors = std::mem::take(&mut self.active_outbox_authors);
        let running_diagnostic_author_refs = std::mem::take(&mut self.diagnostic_author_refs);
        let running_uncovered_by_demand = std::mem::take(&mut self.uncovered_by_demand);
        let running_uncovered_owners_by_author =
            std::mem::take(&mut self.uncovered_owners_by_author);
        let running_refusals_by_demand = std::mem::take(&mut self.refusals_by_demand);
        let running_refused_request_owner_counts =
            std::mem::take(&mut self.refused_request_owner_counts);
        let running_refused_owner_counts_by_session =
            std::mem::take(&mut self.refused_owner_counts_by_session);
        let _ = self.compile(&pending, facts, CompileBudget::with_relay_cap(usize::MAX));
        let mut candidate = std::mem::take(&mut self.prev_plan);
        let _candidate_diag = std::mem::take(&mut self.last_diag);
        let candidate_uncovered_by_demand = std::mem::take(&mut self.uncovered_by_demand);
        self.request_owner_contributions.clear();
        self.request_claim_owner_counts.clear();
        self.request_provenance_owner_counts.clear();
        self.request_demand_coverage_owner_counts.clear();
        self.prev_plan = running_plan;
        self.last_diag = running_diag;
        self.active_demands = running_active_demands;
        self.requests_by_demand = running_requests_by_demand;
        self.active_by_request = running_active_by_request;
        self.request_coverage_by_key = running_request_coverage_by_key;
        self.request_position_by_key = running_request_position_by_key;
        self.request_by_exact_filter = running_request_by_exact_filter;
        self.physical_claims_by_request = running_physical_claims_by_request;
        self.requests_by_physical_claim = running_requests_by_physical_claim;
        self.physical_contributions_by_request = running_physical_contributions_by_request;
        self.requests_by_physical_demand = running_requests_by_physical_demand;
        self.request_owner_contributions = running_request_owner_contributions;
        self.request_claim_owner_counts = running_request_claim_owner_counts;
        self.request_provenance_owner_counts = running_request_provenance_owner_counts;
        self.request_demand_coverage_owner_counts = running_request_demand_coverage_owner_counts;
        self.coverage_assignment_requests = running_coverage_assignment_requests;
        self.refused_coverage_assignments_by_demand =
            running_refused_coverage_assignments_by_demand;
        self.active_outbox_authors = running_active_outbox_authors;
        self.diagnostic_author_refs = running_diagnostic_author_refs;
        self.uncovered_by_demand = running_uncovered_by_demand;
        self.uncovered_owners_by_author = running_uncovered_owners_by_author;
        self.refusals_by_demand = running_refusals_by_demand;
        self.refused_request_owner_counts = running_refused_request_owner_counts;
        self.refused_owner_counts_by_session = running_refused_owner_counts_by_session;

        // A byte-identical incumbent already performs this physical work.
        // Attach all new local metadata before same-session owner pruning:
        // the same DemandKey may be upgrading a supplemental edge to a real
        // typed coverage assignment without adding a new lifecycle owner.
        let mut request_metadata_updates = Vec::new();
        let mut metadata_diagnostics_changed = false;
        let candidate_sessions: Vec<_> = candidate.reqs.keys().cloned().collect();
        for session in candidate_sessions {
            let requests = candidate.reqs.remove(&session).unwrap_or_default();
            let mut remaining = Vec::new();
            for request in requests {
                let mut request = Some(request);
                let attached = self
                    .attach_exact_request_metadata(&session, &mut request, &mut changed_coverage)
                    .or_else(|| {
                        self.attach_physically_covered_request_metadata(
                            &session,
                            &mut request,
                            &mut changed_coverage,
                        )
                    });
                if let Some(attached) = attached {
                    metadata_diagnostics_changed |= attached.diagnostics_changed;
                    if let Some(update) = attached.update {
                        request_metadata_updates.push(update);
                    }
                } else {
                    remaining
                        .push(request.expect("an unattached metadata candidate remains available"));
                }
            }
            if !remaining.is_empty() {
                candidate.reqs.insert(session, remaining);
            }
        }

        // A partially-limited atom may already be served on one of its
        // required sessions. Remove only that session/key pair from the
        // candidate; the same key remains eligible on every missing session.
        // The retained filter may be wider than its retained keys, which is
        // safe under the router's existing local-refilter contract.
        candidate.reqs.retain(|session, requests| {
            for request in requests.iter_mut() {
                request.owner_demands.retain(|key| {
                    !self.requests_by_demand.get(key).is_some_and(|owners| {
                        owners
                            .iter()
                            .any(|(running_session, _)| running_session == session)
                    })
                });
                request
                    .coverage_assignments
                    .retain(|(demand, _)| request.owner_demands.contains(demand));
            }
            requests.retain(|request| !request.owner_demands.is_empty());
            !requests.is_empty()
        });

        // Existing sessions do not consume another global-relay slot. New
        // sessions compete only for the slots that remain.
        let available_new_sessions = budget.relay_cap().saturating_sub(self.prev_plan.reqs.len());
        let new_sessions = rank_new_sessions(
            &candidate.reqs,
            candidate
                .reqs
                .keys()
                .filter(|session| !self.prev_plan.reqs.contains_key(*session))
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

        // A relay's advertised concurrent-subscription budget is reduced by
        // the immutable requests already consuming slots there.
        let candidate_request_totals: BTreeMap<_, _> = candidate
            .reqs
            .iter()
            .map(|(session, requests)| (session.clone(), requests.len()))
            .collect();
        let existing_request_counts: BTreeMap<_, _> = candidate
            .reqs
            .keys()
            .map(|session| {
                (
                    session.clone(),
                    self.prev_plan.reqs.get(session).map_or(0, Vec::len),
                )
            })
            .collect();
        let mut budget_refused_counts = BTreeMap::new();
        let sessions: Vec<_> = candidate.reqs.keys().cloned().collect();
        for session in sessions {
            let existing = self.prev_plan.reqs.get(&session).map_or(0, Vec::len);
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

        let admitted = candidate.reqs.clone();
        changed_coverage.extend(
            admitted
                .values()
                .flatten()
                .flat_map(|request| request.owner_demands.iter().map(|demand| demand.coverage())),
        );
        changed_coverage.extend(refused_requests.iter().flat_map(|(_, request, _)| {
            request.owner_demands.iter().map(|demand| demand.coverage())
        }));
        let admitted_sessions: BTreeSet<_> = admitted.keys().cloned().collect();
        let refused_sessions: BTreeSet<_> = refused_requests
            .iter()
            .map(|(session, _, _)| session.clone())
            .collect();
        let affected_sessions: BTreeSet<_> = admitted_sessions
            .iter()
            .chain(refused_sessions.iter())
            .cloned()
            .collect();
        let refusal_classes_before: BTreeMap<_, _> = affected_sessions
            .iter()
            .map(|session| (session.clone(), refused_session_class(self, session)))
            .collect();
        for (session, requests) in &admitted {
            let start = self.prev_plan.reqs.get(session).map_or(0, Vec::len);
            for (offset, request) in requests.iter().enumerate() {
                self.append_active_request(session, start + offset, request);
            }
        }
        for (session, mut requests) in candidate.reqs {
            let running = self.prev_plan.reqs.entry(session).or_default();
            running.append(&mut requests);
        }

        for (session, total) in candidate_request_totals {
            let refused = budget_refused_counts.get(&session).copied().unwrap_or(0);
            if let Some(shortfall) = self.prev_plan.subscription_shortfalls.get_mut(&session) {
                if let Some(current_budget) = budget.max_subscriptions(&session.relay) {
                    shortfall.budget = current_budget;
                }
                shortfall.planned = shortfall.planned.saturating_add(total);
                shortfall.refused = shortfall.refused.saturating_add(refused);
            } else if refused > 0 {
                self.prev_plan.subscription_shortfalls.insert(
                    session.clone(),
                    BudgetShortfall {
                        budget: budget
                            .max_subscriptions(&session.relay)
                            .expect("a subscription refusal requires an advertised budget"),
                        planned: existing_request_counts
                            .get(&session)
                            .copied()
                            .unwrap_or(0)
                            .saturating_add(total),
                        refused,
                    },
                );
            }
        }
        for (session, request, kind) in &refused_requests {
            self.record_refused_request(session.clone(), request, *kind);
        }
        for session in refused_sessions.iter().chain(admitted_sessions.iter()) {
            if self.prev_plan.reqs.contains_key(session)
                || !self.refused_owner_counts_by_session.contains_key(session)
            {
                self.prev_plan.refused_sessions.remove(session);
            } else {
                self.prev_plan.refused_sessions.insert(session.clone());
            }
        }

        for (session, requests) in &admitted {
            self.append_request_diagnostics(session, requests, &budget);
        }
        for session in &affected_sessions {
            refresh_refusal_diagnostics(
                self,
                session,
                refusal_classes_before.get(session).copied().flatten(),
            );
        }
        let mut uncovered_changed = false;
        for atom in &pending {
            let demand = DemandKey::for_atom(atom);
            let mut facts = candidate_uncovered_by_demand
                .get(&demand)
                .cloned()
                .unwrap_or_default();
            if let AtomClass::Coverage { authors, .. } = route::classify(&atom.filter, &atom.source)
            {
                for author in authors {
                    let assignment = (demand, author);
                    let achieved = self
                        .coverage_assignment_requests
                        .get(&assignment)
                        .into_iter()
                        .flatten()
                        .map(|(session, _)| session)
                        .collect::<BTreeSet<_>>()
                        .len();
                    let refused = self
                        .refused_coverage_assignments_by_demand
                        .get(&demand)
                        .is_some_and(|authors| authors.contains(&author));
                    match reduce_outbox_shortfall(facts.get(&author).copied(), achieved, refused) {
                        Some(fact) => {
                            facts.insert(author, fact);
                        }
                        None => {
                            facts.remove(&author);
                        }
                    }
                }
            }
            uncovered_changed |= self.replace_uncovered_demand(demand, facts);
        }
        let diagnostics_changed =
            !changed_coverage.is_empty() || uncovered_changed || metadata_diagnostics_changed;

        AdmissionOutcome {
            wire: WireDelta {
                ops: admitted
                    .into_iter()
                    .filter_map(|(session, requests)| {
                        (!requests.is_empty()).then(|| {
                            (
                                session,
                                requests
                                    .into_iter()
                                    .map(|request| WireOp::Req(request.sub_id, request.filter))
                                    .collect(),
                            )
                        })
                    })
                    .collect(),
            },
            changed_coverage,
            diagnostics_changed,
            request_metadata_updates,
        }
    }
}
