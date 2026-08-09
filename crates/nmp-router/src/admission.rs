//! Pending-only admission and immutable withdrawal for relay plans.
//!
//! A cohort is compiled in an empty incumbent namespace, then appended to
//! the running plan. Existing requests are therefore candidates for exact
//! coverage reuse, never candidates for widening or identity reassignment.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ContextualAtom, RelaySessionKey};

use crate::budget::CompileBudget;
use crate::diag;
use crate::facts::RoutingFacts;
use crate::plan::{BudgetShortfall, DemandKey, RelayPlan, WireDelta, WireOp, WireReq};
use crate::route::{self, AtomClass};
use crate::{Router, WithdrawalOutcome};

fn owned_by_session(plan: &RelayPlan) -> BTreeMap<RelaySessionKey, BTreeSet<DemandKey>> {
    plan.reqs
        .iter()
        .map(|(session, requests)| {
            (
                session.clone(),
                requests
                    .iter()
                    .flat_map(|request| request.covered_demands.iter().copied())
                    .collect(),
            )
        })
        .collect()
}

fn rank_new_sessions(
    reqs: &BTreeMap<RelaySessionKey, Vec<WireReq>>,
    sessions: impl IntoIterator<Item = RelaySessionKey>,
) -> Vec<RelaySessionKey> {
    let mut ranked: Vec<_> = sessions.into_iter().collect();
    ranked.sort_by(|a, b| {
        let score = |session: &RelaySessionKey| {
            reqs.get(session)
                .into_iter()
                .flatten()
                .map(|request| request.absorbed.len().max(request.provenance.len()).max(1))
                .sum::<usize>()
        };
        score(b).cmp(&score(a)).then_with(|| a.cmp(b))
    });
    ranked
}

impl Router {
    /// Admit one already-routed logical cohort without rewriting running
    /// requests. Exact coverage already present in the plan is a no-op.
    pub fn admit(
        &mut self,
        cohort: &BTreeSet<ContextualAtom>,
        facts: &dyn RoutingFacts,
        budget: impl Into<CompileBudget>,
    ) -> WireDelta {
        let budget = budget.into();
        let pending: BTreeSet<_> = cohort
            .iter()
            .filter(|atom| {
                let key = DemandKey::for_atom(atom);
                !self.requests_by_demand.contains_key(&key)
                    || self.prev_plan.limited_demands.contains(&key)
            })
            .cloned()
            .collect();
        for atom in cohort {
            self.active_demands
                .insert(DemandKey::for_atom(atom), atom.clone());
        }
        let active: Vec<_> = self.active_demands.values().cloned().collect();
        self.rebuild_active_indexes(active.clone());
        if pending.is_empty() {
            return WireDelta::default();
        }

        // Reuse the one canonical routing/coalescing compiler with an empty
        // incumbent view. Its newly minted ids remain unique because the
        // router's monotonic token counter is not swapped or reset.
        let running_plan = std::mem::take(&mut self.prev_plan);
        let running_diag = std::mem::take(&mut self.last_diag);
        let _ = self.compile(&pending, facts, CompileBudget::with_relay_cap(usize::MAX));
        let mut candidate = std::mem::take(&mut self.prev_plan);
        let candidate_diag = std::mem::take(&mut self.last_diag);
        self.prev_plan = running_plan;
        self.last_diag = running_diag;
        self.rebuild_active_indexes(active.clone());

        // A partially-limited atom may already be served on one of its
        // required sessions. Remove only that session/key pair from the
        // candidate; the same key remains eligible on every missing session.
        // The retained filter may be wider than its retained keys, which is
        // safe under the router's existing local-refilter contract.
        let running_by_session = owned_by_session(&self.prev_plan);
        candidate.reqs.retain(|session, requests| {
            if let Some(running) = running_by_session.get(session) {
                for request in requests.iter_mut() {
                    request.covered_demands.retain(|key| !running.contains(key));
                    request.absorbed = request
                        .covered_demands
                        .iter()
                        .map(|owner| owner.coverage())
                        .collect();
                }
                requests.retain(|request| !request.covered_demands.is_empty());
            }
            !requests.is_empty()
        });

        // Existing sessions do not consume another global-relay slot. New
        // sessions compete only for the slots that remain.
        let existing_sessions: BTreeSet<_> = self.prev_plan.reqs.keys().cloned().collect();
        let available_new_sessions = budget.relay_cap().saturating_sub(existing_sessions.len());
        let new_sessions = rank_new_sessions(
            &candidate.reqs,
            candidate
                .reqs
                .keys()
                .filter(|session| !existing_sessions.contains(*session))
                .cloned(),
        );
        for session in new_sessions.into_iter().skip(available_new_sessions) {
            if let Some(refused) = candidate.reqs.remove(&session) {
                candidate.limited.extend(
                    refused
                        .iter()
                        .flat_map(|request| request.absorbed.iter().copied()),
                );
                candidate.limited_demands.extend(
                    refused
                        .iter()
                        .flat_map(|request| request.covered_demands.iter().copied()),
                );
            }
            candidate.refused_sessions.insert(session);
        }

        // A relay's advertised concurrent-subscription budget is reduced by
        // the immutable requests already consuming slots there.
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
            let planned = existing.saturating_add(requests.len());
            if requests.len() <= allowed {
                continue;
            }
            requests.sort_by(|a, b| {
                b.absorbed
                    .len()
                    .cmp(&a.absorbed.len())
                    .then_with(|| b.provenance.len().cmp(&a.provenance.len()))
                    .then_with(|| a.filter.hash().cmp(&b.filter.hash()))
            });
            let refused = requests.split_off(allowed);
            requests.sort_by(|a, b| a.sub_id.cmp(&b.sub_id));
            candidate.limited.extend(
                refused
                    .iter()
                    .flat_map(|request| request.absorbed.iter().copied()),
            );
            candidate.limited_demands.extend(
                refused
                    .iter()
                    .flat_map(|request| request.covered_demands.iter().copied()),
            );
            candidate.subscription_shortfalls.insert(
                session.clone(),
                BudgetShortfall {
                    budget: limit,
                    planned,
                    refused: refused.len(),
                },
            );
            if requests.is_empty() {
                candidate.reqs.remove(&session);
                if existing == 0 {
                    candidate.refused_sessions.insert(session);
                }
            }
        }

        let cohort_demands: BTreeSet<_> = pending.iter().map(DemandKey::for_atom).collect();
        let cohort_keys: BTreeSet<_> = cohort_demands
            .iter()
            .map(|demand| demand.coverage())
            .collect();
        let admitted = candidate.reqs.clone();
        let admitted_sessions: BTreeSet<_> = admitted.keys().cloned().collect();
        for (session, mut requests) in candidate.reqs {
            self.prev_plan
                .reqs
                .entry(session)
                .or_default()
                .append(&mut requests);
        }
        for requests in self.prev_plan.reqs.values_mut() {
            requests.sort_by(|a, b| a.sub_id.cmp(&b.sub_id));
        }
        self.prev_plan
            .limited
            .retain(|key| !cohort_keys.contains(key));
        self.prev_plan.limited.extend(candidate.limited);
        self.prev_plan
            .limited_demands
            .retain(|key| !cohort_demands.contains(key));
        self.prev_plan
            .limited_demands
            .extend(candidate.limited_demands);
        self.prev_plan.refused_sessions.clear();
        self.prev_plan
            .refused_sessions
            .extend(candidate.refused_sessions);
        self.prev_plan.subscription_shortfalls.clear();
        for (session, shortfall) in candidate.subscription_shortfalls {
            self.prev_plan
                .subscription_shortfalls
                .insert(session, shortfall);
        }
        self.prev_plan
            .refused_sessions
            .retain(|session| !admitted_sessions.contains(session));

        let mut uncovered = self.last_diag.uncovered_authors.clone();
        uncovered.extend(candidate_diag.uncovered_authors);
        self.last_diag = diag::build(
            &self.prev_plan,
            &budget,
            uncovered,
            candidate_diag.dropped_merge_rules,
        );
        self.rebuild_active_indexes(active);

        WireDelta {
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
        }
    }

    /// Consume exact resolver-style closes without inspecting any sibling
    /// demand. A physical request closes only when its incremental active
    /// owner count reaches zero; its immutable coverage proof is released
    /// only with that request.
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
        for atom in closing_atoms {
            self.withdrawal_work.dropped_atoms =
                self.withdrawal_work.dropped_atoms.saturating_add(1);
            let key = DemandKey::for_atom(&atom);
            let Some(active_atom) = self.active_demands.remove(&key) else {
                continue;
            };
            if let AtomClass::Coverage { authors, .. } =
                route::classify(&active_atom.filter, &active_atom.source)
            {
                for author in authors {
                    let remove = self
                        .active_outbox_authors
                        .get_mut(&author)
                        .is_some_and(|count| {
                            *count = count.saturating_sub(1);
                            *count == 0
                        });
                    if remove {
                        self.active_outbox_authors.remove(&author);
                        uncovered_authors_changed |=
                            self.last_diag.uncovered_authors.remove(&author).is_some();
                    }
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
                *count = count.saturating_sub(1);
                if *count == 0 {
                    closing.insert(request);
                }
            }
            if self.prev_plan.limited_demands.remove(&key) {
                limited_changed = true;
                let coverage = key.coverage();
                changed_coverage.insert(coverage);
                if !self
                    .prev_plan
                    .limited_demands
                    .iter()
                    .any(|remaining| remaining.coverage() == coverage)
                {
                    self.prev_plan.limited.remove(&coverage);
                }
            }
        }

        let mut wire_by_session: BTreeMap<RelaySessionKey, Vec<WireOp>> = BTreeMap::new();
        let mut touched_sessions = BTreeSet::new();
        for (session, sub_id) in closing {
            let request_key = (session.clone(), sub_id.clone());
            let mut released = BTreeSet::new();
            if let Some(requests) = self.prev_plan.reqs.get_mut(&session) {
                requests.retain(|request| {
                    if request.sub_id == sub_id {
                        released.extend(request.covered_demands.iter().copied());
                        changed_coverage.extend(request.absorbed.iter().copied());
                        false
                    } else {
                        true
                    }
                });
                if requests.is_empty() {
                    self.prev_plan.reqs.remove(&session);
                }
            }
            if released.is_empty() {
                continue;
            }
            self.withdrawal_work.requests_closed =
                self.withdrawal_work.requests_closed.saturating_add(1);
            self.withdrawal_work.physical_coverage_edges_released = self
                .withdrawal_work
                .physical_coverage_edges_released
                .saturating_add(released.len() as u64);
            for demand in released {
                if let Some(requests) = self.requests_by_demand.get_mut(&demand) {
                    requests.remove(&request_key);
                    if requests.is_empty() {
                        self.requests_by_demand.remove(&demand);
                    }
                }
            }
            self.active_by_request.remove(&request_key);
            touched_sessions.insert(session.clone());
            wire_by_session
                .entry(session)
                .or_default()
                .push(WireOp::Close(sub_id));
        }

        self.prev_plan
            .subscription_shortfalls
            .retain(|session, _| self.prev_plan.reqs.contains_key(session));
        self.prev_plan.refused_sessions.retain(|session| {
            self.prev_plan.reqs.contains_key(session)
                || self.prev_plan.subscription_shortfalls.contains_key(session)
        });
        let diagnostics_changed =
            !touched_sessions.is_empty() || limited_changed || uncovered_authors_changed;
        if diagnostics_changed {
            diag::refresh_sessions(
                &mut self.last_diag,
                &self.prev_plan,
                &budget,
                &touched_sessions,
            );
            self.withdrawal_work.diagnostic_rebuilds =
                self.withdrawal_work.diagnostic_rebuilds.saturating_add(1);
        }
        WithdrawalOutcome {
            wire: WireDelta {
                ops: wire_by_session.into_iter().collect(),
            },
            changed_coverage,
            diagnostics_changed,
        }
    }
}
