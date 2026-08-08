//! Pending-only admission and immutable withdrawal for relay plans.
//!
//! A cohort is compiled in an empty incumbent namespace, then appended to
//! the running plan. Existing requests are therefore candidates for exact
//! coverage reuse, never candidates for widening or identity reassignment.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ContextualAtom, RelaySessionKey};

use crate::budget::CompileBudget;
use crate::diag;
use crate::facts::{PublicKey, RoutingFacts};
use crate::plan::{diff_plans, BudgetShortfall, DemandKey, RelayPlan, WireDelta, WireOp, WireReq};
use crate::route::{self, AtomClass};
use crate::Router;

fn owned(plan: &RelayPlan) -> BTreeSet<DemandKey> {
    plan.reqs
        .values()
        .flatten()
        .flat_map(|request| request.owners.iter().copied())
        .collect()
}

fn owned_by_session(plan: &RelayPlan) -> BTreeMap<RelaySessionKey, BTreeSet<DemandKey>> {
    plan.reqs
        .iter()
        .map(|(session, requests)| {
            (
                session.clone(),
                requests
                    .iter()
                    .flat_map(|request| request.owners.iter().copied())
                    .collect(),
            )
        })
        .collect()
}

fn active_outbox_authors(demand: &BTreeSet<ContextualAtom>) -> BTreeSet<PublicKey> {
    demand
        .iter()
        .filter_map(|atom| match route::classify(&atom.filter, &atom.source) {
            AtomClass::Coverage { authors, .. } => Some(authors),
            AtomClass::Supplemental { .. } | AtomClass::Exact(_) => None,
        })
        .flatten()
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
        let already_owned = owned(&self.prev_plan);
        let pending: BTreeSet<_> = cohort
            .iter()
            .filter(|atom| {
                let key = DemandKey::for_atom(atom);
                !already_owned.contains(&key) || self.prev_plan.limited_demands.contains(&key)
            })
            .cloned()
            .collect();
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

        // A partially-limited atom may already be served on one of its
        // required sessions. Remove only that session/key pair from the
        // candidate; the same key remains eligible on every missing session.
        // The retained filter may be wider than its retained keys, which is
        // safe under the router's existing local-refilter contract.
        let running_by_session = owned_by_session(&self.prev_plan);
        candidate.reqs.retain(|session, requests| {
            if let Some(running) = running_by_session.get(session) {
                for request in requests.iter_mut() {
                    request.owners.retain(|key| !running.contains(key));
                    request.absorbed = request
                        .owners
                        .iter()
                        .map(|owner| owner.coverage())
                        .collect();
                }
                requests.retain(|request| !request.owners.is_empty());
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
                        .flat_map(|request| request.owners.iter().copied()),
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
                    .flat_map(|request| request.owners.iter().copied()),
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

    /// Withdraw inactive demand without narrowing or replacing a request
    /// that still serves at least one active coverage key.
    pub fn withdraw(
        &mut self,
        active_demand: &BTreeSet<ContextualAtom>,
        budget: impl Into<CompileBudget>,
    ) -> WireDelta {
        let budget = budget.into();
        let active_keys: BTreeSet<_> = active_demand.iter().map(DemandKey::for_atom).collect();
        let previous = self.prev_plan.clone();
        self.prev_plan.reqs.retain(|_, requests| {
            requests.retain(|request| !request.owners.is_disjoint(&active_keys));
            !requests.is_empty()
        });
        self.prev_plan
            .limited_demands
            .retain(|key| active_keys.contains(key));
        self.prev_plan.limited = self
            .prev_plan
            .limited_demands
            .iter()
            .map(|key| key.coverage())
            .collect();
        self.prev_plan
            .subscription_shortfalls
            .retain(|session, _| self.prev_plan.reqs.contains_key(session));
        self.prev_plan.refused_sessions.retain(|session| {
            self.prev_plan.reqs.contains_key(session)
                || self.prev_plan.subscription_shortfalls.contains_key(session)
        });

        let active_authors = active_outbox_authors(active_demand);
        let mut uncovered = self.last_diag.uncovered_authors.clone();
        uncovered.retain(|author, _| active_authors.contains(author));
        self.last_diag = diag::build(
            &self.prev_plan,
            &budget,
            uncovered,
            self.last_diag.dropped_merge_rules.clone(),
        );
        diff_plans(&previous, &self.prev_plan)
    }
}
