//! Exact-filter metadata attachment and incremental request projection.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ConcreteFilter, RelaySessionKey};

use crate::budget::CompileBudget;
use crate::plan::WireReq;
use crate::{AdmissionOutcome, RequestMetadataUpdate, Router, WireDelta};

pub(super) struct ExactMetadataAttach {
    pub(super) update: Option<RequestMetadataUpdate>,
    pub(super) diagnostics_changed: bool,
}

fn option_set_covers<T: Ord>(
    physical: &Option<BTreeSet<T>>,
    candidate: &Option<BTreeSet<T>>,
) -> bool {
    match (physical, candidate) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(physical), Some(candidate)) => candidate.is_subset(physical),
    }
}

/// Whether one already-sent immutable filter contains every event selected by
/// a candidate. Limited requests stay exact-only: their result-count boundary
/// is not a set axis and cannot safely be reconstructed for a later owner.
fn physical_filter_covers(physical: &ConcreteFilter, candidate: &ConcreteFilter) -> bool {
    if physical.limit.is_some() || candidate.limit.is_some() {
        return false;
    }
    if !option_set_covers(&physical.kinds, &candidate.kinds)
        || !option_set_covers(&physical.authors, &candidate.authors)
        || !option_set_covers(&physical.ids, &candidate.ids)
    {
        return false;
    }
    for (name, physical_values) in &physical.tags {
        let Some(candidate_values) = candidate.tags.get(name) else {
            return false;
        };
        if !candidate_values.is_subset(physical_values) {
            return false;
        }
    }
    let since_covered = match (physical.since, candidate.since) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(physical), Some(candidate)) => physical <= candidate,
    };
    let until_covered = match (physical.until, candidate.until) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(physical), Some(candidate)) => physical >= candidate,
    };
    since_covered && until_covered
}

pub(super) fn rank_new_sessions(
    reqs: &BTreeMap<RelaySessionKey, Vec<WireReq>>,
    sessions: impl IntoIterator<Item = RelaySessionKey>,
) -> Vec<RelaySessionKey> {
    let mut ranked: Vec<_> = sessions
        .into_iter()
        .map(|session| {
            let requests = reqs.get(&session).into_iter().flatten();
            let coverage: BTreeSet<_> = requests
                .clone()
                .flat_map(|request| request.coverage_assignments.iter().copied())
                .collect();
            let secondary = requests
                .into_iter()
                .map(|request| {
                    request
                        .coverage_claims
                        .len()
                        .max(request.provenance.len())
                        .max(1)
                })
                .sum::<usize>();
            (session, (coverage.len(), secondary))
        })
        .collect();
    ranked.sort_by(|(a, a_score), (b, b_score)| b_score.cmp(a_score).then_with(|| a.cmp(b)));
    ranked.into_iter().map(|(session, _)| session).collect()
}

impl Router {
    /// Reactivate one exact original owner of an immutable physical request
    /// without compiling a cohort or scheduling wire work. Only ownership
    /// captured when the request was first installed is eligible here; later
    /// metadata-only attachments remain detachable and cannot grow this
    /// bounded sidecar.
    pub fn reactivate_covered_atom(
        &mut self,
        atom: &nmp_grammar::ContextualAtom,
    ) -> Option<AdmissionOutcome> {
        let demand = crate::DemandKey::for_atom(atom);
        let request_key = self
            .requests_by_physical_demand
            .get(&demand)
            .into_iter()
            .flatten()
            .find(|request_key| {
                let position = self.request_position_by_key[*request_key];
                let incumbent = &self.prev_plan.reqs[&request_key.0][position];
                incumbent.source == atom.source
                    && physical_filter_covers(&incumbent.filter, &atom.filter)
            })
            .cloned()?;
        let contribution = self
            .physical_contributions_by_request
            .get(&request_key)?
            .get(&demand)?
            .clone();
        let position = self.request_position_by_key[&request_key];
        let incumbent = &self.prev_plan.reqs[&request_key.0][position];
        let candidate = WireReq {
            sub_id: incumbent.sub_id.clone(),
            filter: atom.filter.clone(),
            source: atom.source.clone(),
            provenance: contribution.provenance,
            coverage_claims: contribution.coverage_claims,
            owner_demands: BTreeSet::from([demand]),
            coverage_assignments: contribution.coverage_assignments,
        };
        let mut changed_coverage = BTreeSet::new();
        let attached = self.attach_request_metadata(request_key, candidate, &mut changed_coverage);
        Some(AdmissionOutcome {
            wire: WireDelta::default(),
            changed_coverage,
            diagnostics_changed: attached.diagnostics_changed,
            request_metadata_updates: attached.update.into_iter().collect(),
        })
    }

    pub(super) fn append_active_request(
        &mut self,
        session: &RelaySessionKey,
        position: usize,
        request: &WireReq,
    ) {
        let request_key = (session.clone(), request.sub_id.clone());
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
        let mut active = 0;
        for demand in &request.owner_demands {
            self.requests_by_demand
                .entry(*demand)
                .or_default()
                .insert(request_key.clone());
            active += usize::from(self.active_demands.contains_key(demand));
            self.admission_work.request_edges_appended =
                self.admission_work.request_edges_appended.saturating_add(1);
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
        self.index_physical_request_ownership(&request_key, request);
        for (demand, contribution) in contributions {
            self.add_request_owner_contribution(&request_key, demand, contribution);
        }
        for assignment in &request.coverage_assignments {
            self.coverage_assignment_requests
                .entry(*assignment)
                .or_default()
                .insert(request_key.clone());
        }
        self.request_position_by_key
            .insert(request_key.clone(), position);
        self.request_by_exact_filter.insert(
            (
                session.clone(),
                request.source.clone(),
                request.filter.clone(),
            ),
            (session.clone(), request.sub_id.clone()),
        );
    }

    pub(super) fn attach_exact_request_metadata(
        &mut self,
        session: &RelaySessionKey,
        candidate: &mut Option<WireReq>,
        changed_coverage: &mut BTreeSet<nmp_store::CoverageKey>,
    ) -> Option<ExactMetadataAttach> {
        let candidate_ref = candidate.as_ref()?;
        let exact = (
            session.clone(),
            candidate_ref.source.clone(),
            candidate_ref.filter.clone(),
        );
        let request_key = self.request_by_exact_filter.get(&exact).cloned()?;
        let candidate = candidate
            .take()
            .expect("an exact metadata candidate is consumed at most once");
        Some(self.attach_request_metadata(request_key, candidate, changed_coverage))
    }

    /// Reattach a departed local owner to one immutable request that still
    /// physically contains its exact claim shapes. The reverse claim index
    /// bounds lookup to requests that carried the claim when sent; current
    /// local metadata remains independently detachable.
    pub(super) fn attach_physically_covered_request_metadata(
        &mut self,
        session: &RelaySessionKey,
        candidate: &mut Option<WireReq>,
        changed_coverage: &mut BTreeSet<nmp_store::CoverageKey>,
    ) -> Option<ExactMetadataAttach> {
        let candidate_ref = candidate.as_ref()?;
        let first_claim = candidate_ref.coverage_claims.first()?;
        let request_key = self
            .requests_by_physical_claim
            .get(first_claim)
            .into_iter()
            .flatten()
            .find(|request_key| {
                if &request_key.0 != session {
                    return false;
                }
                let Some(physical_claims) = self.physical_claims_by_request.get(*request_key)
                else {
                    return false;
                };
                if !candidate_ref.coverage_claims.is_subset(physical_claims) {
                    return false;
                }
                let position = self.request_position_by_key[*request_key];
                let incumbent = &self.prev_plan.reqs[session][position];
                incumbent.source == candidate_ref.source
                    && physical_filter_covers(&incumbent.filter, &candidate_ref.filter)
            })
            .cloned()?;
        let candidate = candidate
            .take()
            .expect("a physical metadata candidate is consumed at most once");
        Some(self.attach_request_metadata(request_key, candidate, changed_coverage))
    }

    fn attach_request_metadata(
        &mut self,
        request_key: crate::ownership::RequestKey,
        candidate: WireReq,
        changed_coverage: &mut BTreeSet<nmp_store::CoverageKey>,
    ) -> ExactMetadataAttach {
        let session = &request_key.0;
        let position = self.request_position_by_key[&request_key];
        self.admission_work.metadata_entries_examined = self
            .admission_work
            .metadata_entries_examined
            .saturating_add(
                (candidate.owner_demands.len()
                    + candidate.coverage_claims.len()
                    + candidate.coverage_assignments.len()
                    + candidate.provenance.len()) as u64,
            );
        let contributions: Vec<_> = candidate
            .owner_demands
            .iter()
            .filter_map(|demand| {
                self.active_demands.get(demand).map(|atom| {
                    (
                        *demand,
                        Self::derive_request_owner_contribution(atom, &candidate),
                    )
                })
            })
            .collect();
        let mut new_demands = BTreeSet::new();
        let mut new_claims = BTreeSet::new();
        let mut new_assignments = BTreeSet::new();
        let mut new_provenance = BTreeSet::new();
        for (demand, contribution) in contributions {
            let delta = self.add_request_owner_contribution(&request_key, demand, contribution);
            if delta.owner_added {
                new_demands.insert(demand);
            }
            new_claims.extend(delta.coverage_claims);
            new_assignments.extend(delta.coverage_assignments);
            new_provenance.extend(delta.provenance);
        }
        let metadata_changed = !new_demands.is_empty() || !new_claims.is_empty();

        for demand in &new_demands {
            self.requests_by_demand
                .entry(*demand)
                .or_default()
                .insert(request_key.clone());
            if self.active_demands.contains_key(demand) {
                *self
                    .active_by_request
                    .get_mut(&request_key)
                    .expect("exact incumbent has an active-count index") += 1;
            }
            self.request_coverage_by_key
                .entry(request_key.clone())
                .or_default()
                .insert(demand.coverage());
            changed_coverage.insert(demand.coverage());
            self.admission_work.request_edges_appended =
                self.admission_work.request_edges_appended.saturating_add(1);
        }
        for assignment in &new_assignments {
            self.coverage_assignment_requests
                .entry(*assignment)
                .or_default()
                .insert(request_key.clone());
        }
        if !new_provenance.is_empty() {
            let diagnostics = self
                .last_diag
                .per_session
                .get_mut(session)
                .expect("an exact incumbent has session diagnostics");
            for provenance in &new_provenance {
                *diagnostics.by_lane.entry(provenance.lane).or_insert(0) += 1;
                for author in &provenance.covers_authors {
                    *self
                        .diagnostic_author_refs
                        .entry(session.clone())
                        .or_default()
                        .entry(*author)
                        .or_insert(0) += 1;
                }
            }
            diagnostics.authors_served = self
                .diagnostic_author_refs
                .get(session)
                .map_or(0, BTreeMap::len);
        }

        let diagnostics_changed = !new_provenance.is_empty();
        let incumbent = &mut self.prev_plan.reqs.get_mut(session).unwrap()[position];
        incumbent.coverage_claims.extend(new_claims.iter().copied());
        incumbent.owner_demands.extend(new_demands.iter().copied());
        incumbent
            .coverage_assignments
            .extend(new_assignments.iter().copied());
        incumbent.provenance.extend(new_provenance);
        let update = metadata_changed.then(|| RequestMetadataUpdate {
            session: session.clone(),
            sub_id: incumbent.sub_id.clone(),
            filter_hash: incumbent.filter.hash(),
            added_coverage_claims: new_claims,
            added_owner_demands: new_demands,
        });
        ExactMetadataAttach {
            update,
            diagnostics_changed,
        }
    }

    pub(super) fn append_request_diagnostics(
        &mut self,
        session: &RelaySessionKey,
        requests: &[WireReq],
        budget: &CompileBudget,
    ) {
        let diagnostics = self
            .last_diag
            .per_session
            .entry(session.clone())
            .or_insert_with(|| crate::RelayDiagnostics {
                session: session.clone(),
                wire_sub_count: 0,
                by_lane: BTreeMap::new(),
                authors_served: 0,
                filters: Vec::new(),
                subscription_budget: budget.max_subscriptions(&session.relay),
                subscriptions_refused: 0,
                subid_length_limit: budget.max_subid_length(&session.relay),
                subid_length_rejects_our_ids: budget.rejects_our_subscription_ids(&session.relay),
            });
        diagnostics.subscription_budget = budget.max_subscriptions(&session.relay);
        diagnostics.subid_length_limit = budget.max_subid_length(&session.relay);
        diagnostics.subid_length_rejects_our_ids =
            budget.rejects_our_subscription_ids(&session.relay);

        for request in requests {
            diagnostics.filters.push(request.filter.clone());
            diagnostics.wire_sub_count = diagnostics.wire_sub_count.saturating_add(1);
            for provenance in &request.provenance {
                *diagnostics.by_lane.entry(provenance.lane).or_insert(0) += 1;
                for author in &provenance.covers_authors {
                    *self
                        .diagnostic_author_refs
                        .entry(session.clone())
                        .or_default()
                        .entry(*author)
                        .or_insert(0) += 1;
                }
            }
        }
        diagnostics.authors_served = self
            .diagnostic_author_refs
            .get(session)
            .map_or(0, BTreeMap::len);
        diagnostics.subscriptions_refused = self
            .prev_plan
            .subscription_shortfalls
            .get(session)
            .map_or(0, |shortfall| shortfall.refused);
    }
}
