use nmp_grammar::{ConcreteFilter, DescriptorHash, RelaySessionKey};
use nmp_resolver::{HandleId, ResolutionNodeKind};
use nmp_router::SubId;
use nmp_store::CoverageInterval;
use nmp_transport::RelayHandle as TransportRelayHandle;
use std::collections::{BTreeMap, BTreeSet};

use super::coordinate_coverage::RequestReturnEvidence;
use super::request_targets::ActiveRequestTarget;
use super::{
    AttributionSendId, CoreState, Effect, RequestAttemptId, RequestAttemptState,
    RequestHandoffOutcome, RequestSend,
};

#[derive(Debug, Clone)]
pub(super) struct PendingRequestEvidence {
    pub(super) attempt_id: RequestAttemptId,
    pub(super) request_revision: u64,
    pub(super) session: RelaySessionKey,
    pub(super) sub_id: SubId,
    pub(super) filter: ConcreteFilter,
    pub(super) owner_demands: BTreeSet<nmp_router::DemandKey>,
}

#[derive(Debug, Clone)]
pub(super) struct ActiveRequestEvidence {
    pub(super) request_revision: u64,
    pub(super) session: RelaySessionKey,
    pub(super) sub_id: SubId,
    pub(super) owner_demands: BTreeSet<nmp_router::DemandKey>,
    pub(super) handle: TransportRelayHandle,
}

/// One REQ accepted by the exact live transport generation.
///
/// This is deliberately separate from [`ActiveRequestEvidence`]: an EOSE
/// settles request evidence, but it does not close the relay subscription.
/// The wire owner remains live until replacement, CLOSE, or exact-session
/// disconnect. That outliving is exactly why the stored-events phase is
/// retained HERE (#1235): request evidence is REMOVED when the terminal
/// arrives, so it can report that a request is outstanding but never that one
/// finished.
#[derive(Debug, Clone)]
pub(super) struct LiveWireRequest {
    pub(super) filter: ConcreteFilter,
    pub(super) handle: TransportRelayHandle,
    pub(super) stored_events: StoredEvents,
    /// What this relay actually returned under this request, for the
    /// replaceable-coordinate reuse check (#1630). Created with the request
    /// and dropped with it; a replacement REQ reusing this key starts a
    /// fresh one, and nothing here is ever persisted.
    pub(super) returns: RequestReturnEvidence,
}

/// Which half of NIP-01's REQ lifecycle the wire request under one
/// `(session, sub_id)` is in.
///
/// `Streaming` carries the exact request revision it is speaking for because
/// a replacement REQ REUSES this key: a straggler terminal belonging to the
/// request that was displaced must not mark the live one finished. This is
/// the wire-owner counterpart of the FIFO intersection rule
/// `AttributionState::attribute_eose_detailed` enforces for coverage, and it
/// fails closed the same way — an unrecognised revision leaves the phase
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoredEvents {
    Streaming {
        request_revision: u64,
        committed_interval: Option<CoverageInterval>,
    },
    Finished {
        request_revision: u64,
        committed_interval: Option<CoverageInterval>,
    },
}

impl CoreState {
    pub(in crate::core) fn record_observed_request(
        &mut self,
        request: RequestSend<'_>,
    ) -> AttributionSendId {
        self.record_observed_request_detailed(request).0
    }

    /// The retry dispatcher needs the minted attempt id back; every other
    /// caller only needs the attribution send.
    pub(in crate::core) fn record_observed_request_attempt(
        &mut self,
        request: RequestSend<'_>,
    ) -> RequestAttemptId {
        self.record_observed_request_detailed(request).1
    }

    fn record_observed_request_detailed(
        &mut self,
        request: RequestSend<'_>,
    ) -> (AttributionSendId, RequestAttemptId) {
        // Every outgoing REQ this engine ever places -- planned and replayed
        // alike -- passes through
        let send = self.attribution.record_send(
            request.session,
            request.sub_id,
            request.filter,
            request.coverage_claims.clone(),
        );
        let attempt_id = self.attempts.mint(RequestAttemptState {
            session: request.session.clone(),
            sub_id: request.sub_id.clone(),
            filter_hash: request.filter.hash(),
            filter: request.filter.clone(),
            coverage_claims: request.coverage_claims,
            owner_demands: request.owner_demands.clone(),
            request_revision: Some(send.revision()),
            retry_failures: 0,
        });
        self.pending_request_evidence
            .entry((request.session.clone(), request.sub_id.clone()))
            .or_default()
            .push_back(PendingRequestEvidence {
                attempt_id,
                request_revision: send.revision(),
                session: request.session.clone(),
                sub_id: request.sub_id.clone(),
                filter: request.filter.clone(),
                owner_demands: request.owner_demands,
            });
        (send, attempt_id)
    }

    /// Attach exact logical owners to the current byte-identical request
    /// generation without replaying a historical request fact.
    pub(in crate::core) fn extend_current_request_owner_demands(
        &mut self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        filter_hash: DescriptorHash,
        owner_demands: &BTreeSet<nmp_router::DemandKey>,
    ) {
        let key = (session.clone(), sub_id.clone());
        if let Some(current) = self
            .pending_request_evidence
            .get_mut(&key)
            .and_then(|queue| queue.back_mut())
            .filter(|request| request.filter.hash() == filter_hash)
        {
            current.owner_demands.extend(owner_demands.iter().cloned());
        }

        let current_revision = self.live_wire_requests.get(&key).and_then(|request| {
            (request.filter.hash() == filter_hash)
                .then_some(request.stored_events)
                .and_then(|stored_events| match stored_events {
                    StoredEvents::Streaming {
                        request_revision, ..
                    } => Some(request_revision),
                    StoredEvents::Finished { .. } => None,
                })
        });
        if let Some(request) =
            current_revision.and_then(|revision| self.active_request_evidence.get_mut(&revision))
        {
            request.owner_demands.extend(owner_demands.iter().cloned());
        }
    }

    /// Detach exact local execution owners from the current pending or
    /// accepted generation without changing its wire filter or subscription
    /// id. Attribution prunes the matching current claim membership in the
    /// same metadata-removal transition.
    pub(in crate::core) fn remove_current_request_owner_demands(
        &mut self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        filter_hash: DescriptorHash,
        owner_demands: &BTreeSet<nmp_router::DemandKey>,
    ) {
        let key = (session.clone(), sub_id.clone());
        if let Some(current) = self
            .pending_request_evidence
            .get_mut(&key)
            .and_then(|queue| queue.back_mut())
            .filter(|request| request.filter.hash() == filter_hash)
        {
            current
                .owner_demands
                .retain(|demand| !owner_demands.contains(demand));
        }

        let current_revision = self.live_wire_requests.get(&key).and_then(|request| {
            (request.filter.hash() == filter_hash)
                .then_some(request.stored_events)
                .and_then(|stored_events| match stored_events {
                    StoredEvents::Streaming {
                        request_revision, ..
                    } => Some(request_revision),
                    StoredEvents::Finished { .. } => None,
                })
        });
        if let Some(request) =
            current_revision.and_then(|revision| self.active_request_evidence.get_mut(&revision))
        {
            request
                .owner_demands
                .retain(|demand| !owner_demands.contains(demand));
        }
    }

    /// Every observation with a live execution target under any of
    /// `owner_demands` -- the exact set a settlement on that request is a
    /// fact about.
    fn observations_for_demands(
        &self,
        owner_demands: &BTreeSet<nmp_router::DemandKey>,
    ) -> BTreeSet<super::ObservationId> {
        let (targets, _walk) = self.request_targets.live_targets_for_demands(owner_demands);
        targets
            .into_iter()
            .filter_map(|id| self.branch_observation(id))
            .collect()
    }

    /// Runtime/mechanism acknowledgement for one attempted REQ handoff.
    ///
    /// Public only through the doc-hidden mechanism surface so headless
    /// reducer falsifiers can drive the same acceptance edge as the runtime.
    #[doc(hidden)]
    pub(in crate::core) fn on_wire_request_handoff(
        &mut self,
        outcome: RequestHandoffOutcome,
    ) -> Vec<Effect> {
        let (mut effects, evidence_demands) = self.consume_wire_request_handoff(outcome);
        self.refresh_evidence_for_coverage_and_demand_keys(
            &BTreeSet::new(),
            &evidence_demands,
            &mut effects,
        );
        effects
    }

    pub(in crate::core) fn consume_wire_request_handoff(
        &mut self,
        outcome: RequestHandoffOutcome,
    ) -> (Vec<Effect>, BTreeSet<nmp_router::DemandKey>) {
        let Some(attempt) = self.attempts.take(&outcome) else {
            return (Vec::new(), BTreeSet::new());
        };
        let key = (attempt.session.clone(), attempt.sub_id.clone());
        let Some(queue) = self.pending_request_evidence.get_mut(&key) else {
            return (Vec::new(), BTreeSet::new());
        };
        let Some(position) = queue
            .iter()
            .position(|request| request.attempt_id == outcome.attempt_id())
        else {
            return (Vec::new(), BTreeSet::new());
        };
        let request = queue
            .remove(position)
            .expect("position came from pending request queue");
        let evidence_demands = request.owner_demands.clone();
        let replacement_successor = Some(attempt.sub_id.clone());
        debug_assert_eq!(attempt.filter_hash, request.filter.hash());
        debug_assert_eq!(attempt.request_revision, Some(request.request_revision));
        if queue.is_empty() {
            self.pending_request_evidence.remove(&key);
        }
        let mut effects = Vec::new();
        match outcome {
            RequestHandoffOutcome::Accepted { handle, .. } => {
                self.attempts.clear_retry_for_attempt(&attempt);
                self.live_wire_requests.insert(
                    (request.session.clone(), request.sub_id.clone()),
                    LiveWireRequest {
                        filter: request.filter.clone(),
                        handle,
                        stored_events: StoredEvents::Streaming {
                            request_revision: request.request_revision,
                            committed_interval: None,
                        },
                        returns: RequestReturnEvidence::default(),
                    },
                );
                self.active_request_revisions_by_sub
                    .entry((request.session.clone(), request.sub_id.clone()))
                    .or_default()
                    .insert(request.request_revision);
                self.active_request_evidence.insert(
                    request.request_revision,
                    ActiveRequestEvidence {
                        request_revision: request.request_revision,
                        session: request.session,
                        sub_id: request.sub_id,
                        owner_demands: request.owner_demands,
                        handle,
                    },
                );
                if let Some(successor) = replacement_successor {
                    self.complete_request_replacement(&successor, &mut effects);
                }
            }
            RequestHandoffOutcome::Refused { .. } => {
                self.attribution
                    .discard_send_revision(&request.sub_id, request.request_revision);
                if !self.attribution.has_inflight(&request.sub_id)
                    && !self.live_wire_requests.contains_key(&key)
                {
                    self.attribution
                        .discard_wire_mapping(&request.session, &request.sub_id);
                }
                let now = self.clock;
                self.attempts.schedule_retry(attempt, now);
            }
        }
        (effects, evidence_demands)
    }

    fn take_active_request_evidence(&mut self, revision: u64) -> Option<ActiveRequestEvidence> {
        let request = self.active_request_evidence.remove(&revision)?;
        let key = (request.session.clone(), request.sub_id.clone());
        if let Some(revisions) = self.active_request_revisions_by_sub.get_mut(&key) {
            revisions.remove(&revision);
            if revisions.is_empty() {
                self.active_request_revisions_by_sub.remove(&key);
            }
        }
        Some(request)
    }

    /// One actually-sent REQ reached NIP-01's end of stored events with
    /// trustworthy settlement evidence.
    ///
    /// The only execution fact this engine reports to anything outside it: an
    /// [`AuthorRouteProvider`](super::AuthorRouteProvider) learns that its own
    /// source relay answered, which is how "this author has no relay list"
    /// becomes a settled negative instead of a silence. Nothing else about
    /// how the request got there is reported, and nothing rides the row
    /// channel.
    pub(in crate::core) fn emit_request_settled(
        &mut self,
        send: AttributionSendId,
        effects: &mut Vec<Effect>,
    ) -> BTreeSet<nmp_router::DemandKey> {
        let Some(request) = self.take_active_request_evidence(send.revision()) else {
            return BTreeSet::new();
        };
        let observations = self.observations_for_demands(&request.owner_demands);
        self.finish_stored_events(&request);
        for observation in observations {
            effects.push(Effect::RequestSettled(
                observation,
                request.session.relay.clone(),
            ));
        }
        request.owner_demands
    }

    /// Retire an actually-finished request whose local facts-before-claims
    /// transaction could not establish trustworthy settlement evidence.
    ///
    /// The terminal wire frame still ends this exact request, but reporting
    /// it through [`CoreState::emit_request_settled`] would let a route
    /// provider derive absence from a locally incomplete view.
    ///
    /// The stored-events phase is a different claim and does end here (#1235):
    /// "this relay sent everything it had for this request" is a delivery fact
    /// that survives a withheld coverage claim intact. A router-bounded REQ is
    /// the case that separates them — it finishes without ever earning a
    /// watermark, and reporting neither fact is what left an app with only a
    /// wall clock to end a bounded read on.
    pub(in crate::core) fn retire_request_evidence(
        &mut self,
        send: AttributionSendId,
    ) -> BTreeSet<nmp_router::DemandKey> {
        let Some(request) = self.take_active_request_evidence(send.revision()) else {
            return BTreeSet::new();
        };
        self.finish_stored_events(&request);
        request.owner_demands
    }

    /// End the stored-events phase of the wire request `request` speaks for,
    /// and only that one. A live wire owner under this key that belongs to a
    /// LATER request revision is a replacement, and the terminal being handled
    /// belongs to the REQ it displaced.
    fn finish_stored_events(&mut self, request: &ActiveRequestEvidence) {
        let Some(live) = self
            .live_wire_requests
            .get_mut(&(request.session.clone(), request.sub_id.clone()))
        else {
            return;
        };
        if let StoredEvents::Streaming {
            request_revision,
            committed_interval,
        } = live.stored_events
        {
            if request_revision == request.request_revision {
                live.stored_events = StoredEvents::Finished {
                    request_revision,
                    committed_interval,
                };
            }
        }
    }

    /// Every `(session, sub_id)` whose wire request has reached NIP-01's end
    /// of stored events, in the shape `evidence::acquisition_evidence` reads.
    pub(in crate::core) fn finished_stored_events(&self) -> BTreeSet<(RelaySessionKey, SubId)> {
        self.live_wire_requests
            .iter()
            .filter(|(_, live)| matches!(live.stored_events, StoredEvents::Finished { .. }))
            .map(|((session, sub_id), _)| (session.clone(), sub_id.clone()))
            .collect()
    }

    pub(in crate::core) fn placed_request_keys(&self) -> BTreeSet<(RelaySessionKey, SubId)> {
        self.live_wire_requests
            .keys()
            .cloned()
            .collect()
    }

    pub(in crate::core) fn awaiting_request_keys(&self) -> BTreeSet<(RelaySessionKey, SubId)> {
        self.attempts.awaiting_evidence_keys()
    }

    pub(in crate::core) fn close_requests_for_session(
        &mut self,
        session: &RelaySessionKey,
        handle: TransportRelayHandle,
    ) {
        self.live_wire_requests
            .retain(|(request_session, _), request| {
                request_session != session || request.handle != handle
            });
        let revisions: Vec<_> = self
            .active_request_evidence
            .iter()
            .filter_map(|(revision, request)| {
                (&request.session == session && request.handle == handle).then_some(*revision)
            })
            .collect();
        for revision in revisions {
            self.take_active_request_evidence(revision);
        }
    }

    pub(in crate::core) fn close_requests_for_sub(
        &mut self,
        session: &RelaySessionKey,
        handle: TransportRelayHandle,
        sub_id: &SubId,
    ) {
        let key = (session.clone(), sub_id.clone());
        if self
            .live_wire_requests
            .get(&key)
            .is_some_and(|request| request.handle == handle)
        {
            self.live_wire_requests.remove(&key);
        }
        let revisions: Vec<_> = self
            .active_request_revisions_by_sub
            .get(&key)
            .into_iter()
            .flatten()
            .filter(|revision| {
                self.active_request_evidence
                    .get(revision)
                    .is_some_and(|request| request.handle == handle)
            })
            .copied()
            .collect();
        for revision in revisions {
            self.take_active_request_evidence(revision);
        }
    }

    /// Re-derive which logical demands one branch's resolved filter nodes
    /// currently execute against, and hand the result to the target owner.
    pub(in crate::core) fn reconcile_observation_resolution(&mut self, id: HandleId) {
        let snapshot = self.resolver.resolution_snapshot(id);
        let mut current_targets = BTreeMap::new();
        for node in snapshot {
            let ResolutionNodeKind::Filter { scope, atoms } = node.node_type else {
                continue;
            };
            for atom in &atoms {
                *current_targets
                    .entry(super::ActiveRequestTarget {
                        demand: nmp_router::DemandKey::for_atom(atom),
                        scope,
                    })
                    .or_insert(0) += 1;
            }
        }
        self.replace_request_targets_for_handle(id, current_targets);
    }

    pub(in crate::core) fn remove_request_targets_for_handle(&mut self, id: HandleId) {
        self.replace_request_targets_for_handle(id, BTreeMap::new());
    }

    fn replace_request_targets_for_handle(
        &mut self,
        id: HandleId,
        current: BTreeMap<ActiveRequestTarget, usize>,
    ) {
        // Only a wire-attached branch has anything active to re-derive. The
        // scope set is a freshness decision the branch owns; the target owner
        // is handed it rather than reaching for the branch's acquisition.
        let active_scopes = self
            .wire
            .is_attached(id)
            .then(|| self.wire_contributing_scopes(id));
        self.request_targets
            .replace_for_handle(id, current, active_scopes.as_ref());
    }

    /// Which of one branch's Demand scopes currently contribute to the wire.
    pub(in crate::core) fn wire_contributing_scopes(&self, id: HandleId) -> BTreeSet<usize> {
        self.handles
            .get(&id)
            .into_iter()
            .flat_map(|state| state.acquisition.scopes.iter().enumerate())
            .filter_map(|(scope, acquisition)| acquisition.contributes_wire().then_some(scope))
            .collect()
    }

    pub(in crate::core) fn activate_request_targets_for_handle(&mut self, id: HandleId) {
        let scopes = self.wire_contributing_scopes(id);
        self.request_targets.activate_handle(id, &scopes);
    }

    pub(in crate::core) fn deactivate_request_targets_for_handle(&mut self, id: HandleId) {
        self.request_targets.deactivate_handle(id);
    }

    pub(in crate::core) fn deactivate_request_targets_for_handle_demand(
        &mut self,
        id: HandleId,
        demand: nmp_router::DemandKey,
    ) {
        self.request_targets.deactivate_handle_demand(id, demand);
    }

    /// Which observation a resolver branch handle serves. An engine-internal
    /// handle that belongs to none answers `None`, exactly as its row emits
    /// are already dropped.
    fn branch_observation(&self, id: HandleId) -> Option<super::ObservationId> {
        self.handles.get(&id).map(|state| state.observation)
    }
}
