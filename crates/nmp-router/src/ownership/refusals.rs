//! Exact refusal, limit, and shortfall ownership.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::RelaySessionKey;

use crate::plan::{DemandKey, WireReq};
use crate::{PublicKey, Router};

use super::{RefusalKind, RefusalOwner, RefusedSessionClass};

pub(crate) fn refused_session_class(
    router: &Router,
    session: &RelaySessionKey,
) -> Option<RefusedSessionClass> {
    router
        .prev_plan
        .refused_sessions
        .contains(session)
        .then(|| {
            if router
                .prev_plan
                .subscription_shortfalls
                .contains_key(session)
            {
                RefusedSessionClass::SubscriptionBudget
            } else {
                RefusedSessionClass::RelayCap
            }
        })
}

pub(crate) fn refresh_refusal_diagnostics(
    router: &mut Router,
    session: &RelaySessionKey,
    before: Option<RefusedSessionClass>,
) {
    if let Some(diagnostics) = router.last_diag.per_session.get_mut(session) {
        diagnostics.subscriptions_refused = router
            .prev_plan
            .subscription_shortfalls
            .get(session)
            .map_or(0, |shortfall| shortfall.refused);
    }
    let after = refused_session_class(router, session);
    adjust_count(
        &mut router.last_diag.sessions_refused_by_cap,
        before == Some(RefusedSessionClass::RelayCap),
        after == Some(RefusedSessionClass::RelayCap),
    );
    adjust_count(
        &mut router.last_diag.sessions_refused_by_subscription_budget,
        before == Some(RefusedSessionClass::SubscriptionBudget),
        after == Some(RefusedSessionClass::SubscriptionBudget),
    );
}

fn adjust_count(count: &mut usize, before: bool, after: bool) {
    match (before, after) {
        (true, false) => *count = count.saturating_sub(1),
        (false, true) => *count = count.saturating_add(1),
        _ => {}
    }
}

impl Router {
    pub(crate) fn rebuild_refusal_indexes(
        &mut self,
        cap_refused_demands: BTreeMap<RelaySessionKey, BTreeSet<DemandKey>>,
        cap_refused_coverage_assignments: BTreeSet<(DemandKey, PublicKey)>,
        budget_refused_requests: Vec<(RelaySessionKey, WireReq)>,
    ) {
        // `rebuild_refusal_indexes` always wipes and rebuilds from this
        // call's own `cap_refused_demands`/`budget_refused_requests`, never
        // merging forward what was already indexed. Count every incumbent
        // refusal-owner entry about to be discarded this way. Isolated
        // cohort admission detaches `refusals_by_demand` to empty first, so
        // this is 0 there; a full `compile()` runs against the real
        // incumbent set.
        self.admission_work.incumbent_refusal_entries_visited = self
            .admission_work
            .incumbent_refusal_entries_visited
            .saturating_add(
                self.refusals_by_demand
                    .values()
                    .map(BTreeMap::len)
                    .sum::<usize>() as u64,
            );
        self.refusals_by_demand.clear();
        self.refused_request_owner_counts.clear();
        self.refused_owner_counts_by_session.clear();
        self.refused_coverage_assignments_by_demand.clear();
        for (demand, author) in cap_refused_coverage_assignments {
            self.refused_coverage_assignments_by_demand
                .entry(demand)
                .or_default()
                .insert(author);
        }
        for (session, demands) in cap_refused_demands {
            for demand in demands {
                self.index_refusal_owner(
                    demand,
                    session.clone(),
                    RefusalOwner {
                        refusal_kind: RefusalKind::RelayCap,
                        request: None,
                    },
                );
            }
        }
        for (session, request) in budget_refused_requests {
            for (demand, author) in &request.coverage_assignments {
                self.refused_coverage_assignments_by_demand
                    .entry(*demand)
                    .or_default()
                    .insert(*author);
            }
            let request_key = (session.clone(), request.sub_id.clone());
            self.refused_request_owner_counts
                .insert(request_key, request.owner_demands.len());
            for demand in request.owner_demands {
                self.index_refusal_owner(
                    demand,
                    session.clone(),
                    RefusalOwner {
                        refusal_kind: RefusalKind::SubscriptionBudget,
                        request: Some(request.sub_id.clone()),
                    },
                );
            }
        }
    }

    pub(crate) fn index_refusal_owner(
        &mut self,
        demand: DemandKey,
        session: RelaySessionKey,
        owner: RefusalOwner,
    ) {
        let replaced = self
            .refusals_by_demand
            .entry(demand)
            .or_default()
            .insert(session.clone(), owner);
        assert!(replaced.is_none(), "duplicate exact demand/session refusal");
        *self
            .refused_owner_counts_by_session
            .entry(session)
            .or_insert(0) += 1;
    }

    pub(crate) fn remove_refusal_owners(&mut self, demand: DemandKey) -> bool {
        let Some(owners) = self.refusals_by_demand.remove(&demand) else {
            return false;
        };
        self.prev_plan.limited_demands.remove(&demand);
        self.refused_coverage_assignments_by_demand.remove(&demand);
        for (session, owner) in owners {
            let before = refused_session_class(self, &session);
            let remove_session = self
                .refused_owner_counts_by_session
                .get_mut(&session)
                .is_some_and(|count| {
                    *count = count.saturating_sub(1);
                    *count == 0
                });
            if remove_session {
                self.refused_owner_counts_by_session.remove(&session);
            }
            if let Some(sub_id) = owner.request {
                debug_assert_eq!(owner.refusal_kind, RefusalKind::SubscriptionBudget);
                let request_key = (session.clone(), sub_id);
                let remove_request = self
                    .refused_request_owner_counts
                    .get_mut(&request_key)
                    .is_some_and(|count| {
                        *count = count.saturating_sub(1);
                        *count == 0
                    });
                if remove_request {
                    self.refused_request_owner_counts.remove(&request_key);
                    let remove_shortfall = self
                        .prev_plan
                        .subscription_shortfalls
                        .get_mut(&session)
                        .is_some_and(|shortfall| {
                            shortfall.planned = shortfall.planned.saturating_sub(1);
                            shortfall.refused = shortfall.refused.saturating_sub(1);
                            shortfall.refused == 0
                        });
                    if remove_shortfall {
                        self.prev_plan.subscription_shortfalls.remove(&session);
                    }
                }
            }
            if self.prev_plan.reqs.contains_key(&session)
                || !self.refused_owner_counts_by_session.contains_key(&session)
            {
                self.prev_plan.refused_sessions.remove(&session);
            } else {
                self.prev_plan.refused_sessions.insert(session.clone());
            }
            refresh_refusal_diagnostics(self, &session, before);
        }
        true
    }

    pub(crate) fn record_refused_request(
        &mut self,
        session: RelaySessionKey,
        request: &WireReq,
        refusal_kind: RefusalKind,
    ) {
        for (demand, author) in &request.coverage_assignments {
            self.refused_coverage_assignments_by_demand
                .entry(*demand)
                .or_default()
                .insert(*author);
        }
        let request_id =
            (refusal_kind == RefusalKind::SubscriptionBudget).then(|| request.sub_id.clone());
        if let Some(sub_id) = &request_id {
            self.refused_request_owner_counts.insert(
                (session.clone(), sub_id.clone()),
                request.owner_demands.len(),
            );
        }
        for demand in &request.owner_demands {
            let first_for_demand = !self.refusals_by_demand.contains_key(demand);
            self.index_refusal_owner(
                *demand,
                session.clone(),
                RefusalOwner {
                    refusal_kind,
                    request: request_id.clone(),
                },
            );
            if first_for_demand {
                self.prev_plan.limited_demands.insert(*demand);
            }
        }
    }
}
