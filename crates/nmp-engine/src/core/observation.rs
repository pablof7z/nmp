use nmp_grammar::{ConcreteFilter, DescriptorHash, IdentityField, RelaySessionKey};
use nmp_resolver::{HandleId, ResolutionNodeKind, ResolvedValue};
use nmp_router::SubId;
use nmp_store::CoverageInterval;
use nmp_transport::RelayHandle as TransportRelayHandle;
use nostr::{RelayUrl, Timestamp};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::coordinate_coverage::RequestReturnEvidence;
use super::request_targets::ActiveRequestTarget;
use super::{
    AttributionSendId, CoreState, Effect, LocalSendRefusal, RequestAttemptId,
    RequestAttemptPurpose, RequestAttemptState, RequestHandoffOutcome, RequestSend,
};

/// Ordered execution evidence for one live observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationEvidence {
    /// Monotonic within this OBSERVATION, across all of its branches.
    /// Sequence numbers are assigned by the reducer that owns the
    /// observation, never by a delivery adapter and never per branch.
    pub sequence: u64,
    /// Which canonical branch produced this fact, or `None` for a fact that
    /// belongs to the observation as a whole (withdrawal, mailbox overflow).
    /// Two branches can resolve identical values at identical paths; without
    /// this, their traces would be indistinguishable.
    pub branch: Option<usize>,
    pub fact: ObservationFact,
}

/// Why a resolver-owned value/filter transition was evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionCause {
    Initial,
    CurrentAccountChanged,
    DependencyChanged,
}

/// The protocol-neutral terminal that settled one actually-sent request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestTerminal {
    Eose,
    Nip77,
}

/// One exact value already resolved by the query graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolvedBindingValue {
    Scalar(String),
    AddressCoordinate {
        kind: u16,
        author: String,
        identifier: String,
    },
}

/// Authoritative facts emitted by the owners of resolution and wire state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationFact {
    ReactiveInput {
        path: String,
        field: IdentityField,
        revision: u64,
        values: Vec<ResolvedBindingValue>,
        fingerprint: String,
        cause: ResolutionCause,
    },
    DerivedSet {
        path: String,
        revision: u64,
        values: Vec<ResolvedBindingValue>,
        fingerprint: String,
        cause: ResolutionCause,
    },
    ConcreteFilter {
        path: String,
        revision: u64,
        filters: Vec<ConcreteFilter>,
        fingerprint: String,
        cause: ResolutionCause,
    },
    RelayRequest {
        path: String,
        filter_revision: u64,
        relay: RelayUrl,
        authenticated_as: Option<nostr::PublicKey>,
        transport_generation: u64,
        request_revision: u64,
        filter: Arc<ConcreteFilter>,
        /// WHY this relay was asked: the routing lanes that put this REQ on
        /// the wire — the author's NIP-65 outbound set, a selector hint,
        /// prior source provenance, or an operator app/fallback lane.
        ///
        /// Without this the trace said which relays were asked and never
        /// why, which is exactly the gap that made the deleted filter-shape
        /// inference invisible in the first place: a default that decides a
        /// route has to report the route it decided, or it is the same
        /// unaccountable magic under a better name.
        ///
        /// A SET because coalescing is real — one REQ can be two authors'
        /// outbox lane and the operator's app lane at once, and reporting a
        /// single lane would be true but partial.
        lanes: BTreeSet<nmp_router::Lane>,
        replay: bool,
    },
    RequestSettled {
        path: String,
        filter_revision: u64,
        relay: RelayUrl,
        authenticated_as: Option<nostr::PublicKey>,
        transport_generation: u64,
        request_revision: u64,
        observed_at: Timestamp,
        terminal: RequestTerminal,
    },
    RelayClosed {
        path: String,
        filter_revision: u64,
        relay: RelayUrl,
        authenticated_as: Option<nostr::PublicKey>,
        transport_generation: u64,
        request_revision: Option<u64>,
        reason: String,
    },
    RequestDeferred {
        path: String,
        filter_revision: u64,
        relay: RelayUrl,
        authenticated_as: Option<nostr::PublicKey>,
        request_revision: u64,
        retry_at: Timestamp,
        cause: LocalSendRefusal,
    },
    Withdrawn,
    /// The bounded delivery mailbox discarded an exact contiguous sequence
    /// range. Loss is therefore visible and never masquerades as a complete
    /// causal trace.
    Overflow {
        first_sequence: u64,
        last_sequence: u64,
        dropped: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RememberedResolution {
    Reactive {
        revision: u64,
        fingerprint: String,
    },
    ValueSet {
        revision: u64,
        fingerprint: String,
    },
    Filter {
        revision: u64,
        fingerprint: String,
        atoms: Vec<nmp_grammar::ContextualAtom>,
    },
}

/// One BRANCH's remembered resolution state. Sequence numbers live on the
/// owning observation, not here: the app receives one ordered trace for the
/// whole live query, not one per branch.
#[derive(Debug, Default)]
pub(super) struct ObservationExecutionState {
    nodes: BTreeMap<String, RememberedResolution>,
}

#[derive(Debug, Clone)]
pub(super) struct PendingRequestEvidence {
    pub(super) attempt_id: RequestAttemptId,
    pub(super) request_revision: u64,
    pub(super) session: RelaySessionKey,
    pub(super) sub_id: SubId,
    pub(super) filter: ConcreteFilter,
    pub(super) owner_demands: BTreeSet<nmp_router::DemandKey>,
    pub(super) lanes: BTreeSet<nmp_router::Lane>,
    pub(super) replay: bool,
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
    /// Router-plan identity used only by acquisition evidence. NIP-77 roles
    /// keep their distinct physical `SubId` in the map key and every wire
    /// owner while projecting the semantic request they execute here.
    pub(super) evidence_sub_id: SubId,
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

fn resolved_values(values: Vec<ResolvedValue>) -> Vec<ResolvedBindingValue> {
    values
        .into_iter()
        .map(|value| match value {
            ResolvedValue::Scalar(value) => ResolvedBindingValue::Scalar(value),
            ResolvedValue::AddressCoordinate {
                kind,
                author,
                identifier,
            } => ResolvedBindingValue::AddressCoordinate {
                kind,
                author,
                identifier,
            },
        })
        .collect()
}

fn value_fingerprint(values: &[ResolvedBindingValue]) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in values {
        match value {
            ResolvedBindingValue::Scalar(value) => {
                hasher.update(&[0]);
                hasher.update(&(value.len() as u64).to_be_bytes());
                hasher.update(value.as_bytes());
            }
            ResolvedBindingValue::AddressCoordinate {
                kind,
                author,
                identifier,
            } => {
                hasher.update(&[1]);
                hasher.update(&kind.to_be_bytes());
                hasher.update(&(author.len() as u64).to_be_bytes());
                hasher.update(author.as_bytes());
                hasher.update(&(identifier.len() as u64).to_be_bytes());
                hasher.update(identifier.as_bytes());
            }
        }
    }
    hasher.finalize().to_hex().to_string()
}

fn filter_fingerprint(filters: &[ConcreteFilter]) -> String {
    let mut hasher = blake3::Hasher::new();
    for filter in filters {
        hasher.update(filter.hash().as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

impl CoreState {
    pub(in crate::core) fn record_observed_request(
        &mut self,
        request: RequestSend<'_>,
    ) -> AttributionSendId {
        self.record_observed_request_with_purpose(request, RequestAttemptPurpose::Ordinary)
            .0
    }

    pub(in crate::core) fn record_observed_request_with_purpose(
        &mut self,
        request: RequestSend<'_>,
        purpose: RequestAttemptPurpose,
    ) -> (AttributionSendId, RequestAttemptId) {
        // Every outgoing REQ this engine ever places -- planned, replayed,
        // NIP-77 live candidate, backlog and backfill alike -- passes through
        let send = self.attribution.record_send(
            request.session,
            request.sub_id,
            request.filter,
            request.coverage_claims.clone(),
            request.event_failure_target,
        );
        let attempt_id = self.attempts.mint(RequestAttemptState {
            session: request.session.clone(),
            sub_id: request.sub_id.clone(),
            filter_hash: request.filter.hash(),
            filter: request.filter.clone(),
            coverage_claims: request.coverage_claims,
            owner_demands: request.owner_demands.clone(),
            lanes: request.lanes.clone(),
            replay: request.replay,
            event_failure_target: request.event_failure_target,
            request_revision: Some(send.revision()),
            retry_failures: 0,
            purpose,
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
                lanes: request.lanes,
                replay: request.replay,
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
            current.owner_demands.extend(owner_demands.iter().copied());
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
            request.owner_demands.extend(owner_demands.iter().copied());
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

    fn current_request_targets(
        &self,
        owner_demands: &BTreeSet<nmp_router::DemandKey>,
    ) -> Vec<(HandleId, String, u64)> {
        let (targets, _walk) = self.request_targets.live_targets_for_demands(owner_demands);
        #[cfg(any(test, feature = "bench-instrumentation"))]
        {
            self.request_target_demand_keys_touched.set(
                self.request_target_demand_keys_touched
                    .get()
                    .saturating_add(_walk.demand_keys_touched),
            );
            self.request_target_candidates_examined.set(
                self.request_target_candidates_examined
                    .get()
                    .saturating_add(_walk.candidates_examined),
            );
        }
        targets
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
        let mut evidence_demands = request.owner_demands.clone();
        if let Some(plan_sub_id) = attempt.purpose.plan_sub_id() {
            if let Some(metadata) = self.plan_execution_metadata.get(plan_sub_id) {
                evidence_demands.extend(metadata.owner_demands.iter().copied());
            }
        }
        let evidence_sub_id = attempt.purpose.evidence_sub_id(&request.sub_id);
        let replacement_successor = matches!(attempt.purpose, RequestAttemptPurpose::Ordinary)
            .then(|| attempt.sub_id.clone());
        debug_assert_eq!(attempt.filter_hash, request.filter.hash());
        debug_assert_eq!(attempt.request_revision, Some(request.request_revision));
        if queue.is_empty() {
            self.pending_request_evidence.remove(&key);
        }
        let targets = self.current_request_targets(&request.owner_demands);
        let mut effects = Vec::new();
        match outcome {
            RequestHandoffOutcome::Accepted { handle, .. } => {
                self.attempts.clear_retry_for_attempt(&attempt);
                let shared_filter = Arc::new(request.filter.clone());
                for (id, path, filter_revision) in &targets {
                    self.emit_observation_fact(
                        *id,
                        ObservationFact::RelayRequest {
                            path: path.clone(),
                            filter_revision: *filter_revision,
                            relay: request.session.relay.clone(),
                            authenticated_as: request.session.authenticated_as,
                            transport_generation: handle.generation,
                            request_revision: request.request_revision,
                            filter: shared_filter.clone(),
                            lanes: request.lanes.clone(),
                            replay: request.replay,
                        },
                        &mut effects,
                    );
                }
                self.live_wire_requests.insert(
                    (request.session.clone(), request.sub_id.clone()),
                    LiveWireRequest {
                        filter: request.filter.clone(),
                        evidence_sub_id,
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
            RequestHandoffOutcome::Refused { cause, .. } => {
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
                let retry_at = self
                    .attempts
                    .retry_due_for_sub(&request.sub_id)
                    .unwrap_or(now);
                for (id, path, filter_revision) in targets {
                    self.emit_observation_fact(
                        id,
                        ObservationFact::RequestDeferred {
                            path,
                            filter_revision,
                            relay: request.session.relay.clone(),
                            authenticated_as: request.session.authenticated_as,
                            request_revision: request.request_revision,
                            retry_at,
                            cause: cause.clone(),
                        },
                        &mut effects,
                    );
                }
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

    pub(in crate::core) fn emit_request_settled(
        &mut self,
        send: AttributionSendId,
        observed_at: Timestamp,
        terminal: RequestTerminal,
        effects: &mut Vec<Effect>,
    ) -> BTreeSet<nmp_router::DemandKey> {
        let Some(request) = self.take_active_request_evidence(send.revision()) else {
            return BTreeSet::new();
        };
        let targets = self.current_request_targets(&request.owner_demands);
        self.finish_stored_events(&request);
        for (id, path, filter_revision) in targets {
            self.emit_observation_fact(
                id,
                ObservationFact::RequestSettled {
                    path,
                    filter_revision,
                    relay: request.session.relay.clone(),
                    authenticated_as: request.session.authenticated_as,
                    transport_generation: request.handle.generation,
                    request_revision: request.request_revision,
                    observed_at,
                    terminal,
                },
                effects,
            );
        }
        request.owner_demands
    }

    /// Retire an actually-finished request whose local facts-before-claims
    /// transaction could not establish trustworthy settlement evidence.
    ///
    /// The terminal wire frame still ends this exact request, but exposing
    /// it as [`ObservationFact::RequestSettled`] would let protocol
    /// consumers derive absence from a locally incomplete view.
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
            .map(|((session, _), live)| (session.clone(), live.evidence_sub_id.clone()))
            .collect()
    }

    pub(in crate::core) fn placed_request_keys(&self) -> BTreeSet<(RelaySessionKey, SubId)> {
        self.live_wire_requests
            .iter()
            .map(|((session, _), live)| (session.clone(), live.evidence_sub_id.clone()))
            .collect()
    }

    pub(in crate::core) fn awaiting_request_keys(&self) -> BTreeSet<(RelaySessionKey, SubId)> {
        self.attempts.awaiting_evidence_keys()
    }

    pub(in crate::core) fn close_requests_for_session(
        &mut self,
        session: &RelaySessionKey,
        handle: TransportRelayHandle,
        reason: String,
        effects: &mut Vec<Effect>,
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
            let Some(request) = self.take_active_request_evidence(revision) else {
                continue;
            };
            let targets = self.current_request_targets(&request.owner_demands);
            for (id, path, filter_revision) in targets {
                self.emit_observation_fact(
                    id,
                    ObservationFact::RelayClosed {
                        path,
                        filter_revision,
                        relay: request.session.relay.clone(),
                        authenticated_as: request.session.authenticated_as,
                        transport_generation: handle.generation,
                        request_revision: Some(request.request_revision),
                        reason: reason.clone(),
                    },
                    effects,
                );
            }
        }
    }

    pub(in crate::core) fn close_requests_for_sub(
        &mut self,
        session: &RelaySessionKey,
        handle: TransportRelayHandle,
        sub_id: &SubId,
        reason: String,
        effects: &mut Vec<Effect>,
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
            let Some(request) = self.take_active_request_evidence(revision) else {
                continue;
            };
            let targets = self.current_request_targets(&request.owner_demands);
            for (id, path, filter_revision) in targets {
                self.emit_observation_fact(
                    id,
                    ObservationFact::RelayClosed {
                        path,
                        filter_revision,
                        relay: request.session.relay.clone(),
                        authenticated_as: request.session.authenticated_as,
                        transport_generation: handle.generation,
                        request_revision: Some(request.request_revision),
                        reason: reason.clone(),
                    },
                    effects,
                );
            }
        }
    }

    pub(in crate::core) fn reconcile_observation_resolution(
        &mut self,
        id: HandleId,
        cause: ResolutionCause,
        effects: &mut Vec<Effect>,
    ) {
        let snapshot = self.resolver.resolution_snapshot(id);
        let Some(&BranchOwner {
            observation,
            index: branch,
        }) = self.branch_owner(id).as_ref()
        else {
            return;
        };
        let mut next_sequence = self
            .observations
            .get(&observation)
            .map(|state| state.next_sequence)
            .unwrap_or_default();
        let Some(state) = self.handles.get_mut(&id) else {
            return;
        };
        let mut evidence = Vec::new();
        let mut current_targets = BTreeMap::new();
        for node in snapshot {
            match node.node_type {
                ResolutionNodeKind::Reactive { field, values } => {
                    let values = resolved_values(values);
                    let fingerprint = value_fingerprint(&values);
                    let prior = state.execution.nodes.get(&node.path);
                    let revision = match prior {
                        Some(RememberedResolution::Reactive {
                            revision,
                            fingerprint: old,
                        }) if old == &fingerprint => *revision,
                        Some(RememberedResolution::Reactive { revision, .. }) => {
                            revision.saturating_add(1)
                        }
                        _ => 1,
                    };
                    let changed = !matches!(
                        prior,
                        Some(RememberedResolution::Reactive {
                            fingerprint: old,
                            ..
                        }) if old == &fingerprint
                    );
                    state.execution.nodes.insert(
                        node.path.clone(),
                        RememberedResolution::Reactive {
                            revision,
                            fingerprint: fingerprint.clone(),
                        },
                    );
                    if changed {
                        evidence.push(issue(
                            &mut next_sequence,
                            branch,
                            ObservationFact::ReactiveInput {
                                path: node.path,
                                field,
                                revision,
                                values,
                                fingerprint,
                                cause,
                            },
                        ));
                    }
                }
                ResolutionNodeKind::Derived { values } | ResolutionNodeKind::SetOp { values } => {
                    let values = resolved_values(values);
                    let fingerprint = value_fingerprint(&values);
                    let prior = state.execution.nodes.get(&node.path);
                    let revision = match prior {
                        Some(RememberedResolution::ValueSet {
                            revision,
                            fingerprint: old,
                        }) if old == &fingerprint => *revision,
                        Some(RememberedResolution::ValueSet { revision, .. }) => {
                            revision.saturating_add(1)
                        }
                        _ => 1,
                    };
                    let changed = !matches!(
                        prior,
                        Some(RememberedResolution::ValueSet {
                            fingerprint: old,
                            ..
                        }) if old == &fingerprint
                    );
                    state.execution.nodes.insert(
                        node.path.clone(),
                        RememberedResolution::ValueSet {
                            revision,
                            fingerprint: fingerprint.clone(),
                        },
                    );
                    if changed {
                        evidence.push(issue(
                            &mut next_sequence,
                            branch,
                            ObservationFact::DerivedSet {
                                path: node.path,
                                revision,
                                values,
                                fingerprint,
                                cause,
                            },
                        ));
                    }
                }
                ResolutionNodeKind::Filter { scope, atoms } => {
                    let filters: Vec<_> = atoms.iter().map(|atom| atom.filter.clone()).collect();
                    let fingerprint = filter_fingerprint(&filters);
                    let prior = state.execution.nodes.get(&node.path);
                    let revision = match prior {
                        Some(RememberedResolution::Filter {
                            revision,
                            fingerprint: old,
                            ..
                        }) if old == &fingerprint => *revision,
                        Some(RememberedResolution::Filter { revision, .. }) => {
                            revision.saturating_add(1)
                        }
                        _ => 1,
                    };
                    let changed = !matches!(
                        prior,
                        Some(RememberedResolution::Filter {
                            fingerprint: old,
                            ..
                        }) if old == &fingerprint
                    );
                    for atom in &atoms {
                        *current_targets
                            .entry(super::ActiveRequestTarget {
                                demand: nmp_router::DemandKey::for_atom(atom),
                                scope,
                                path: node.path.clone(),
                                revision,
                            })
                            .or_insert(0) += 1;
                    }
                    state.execution.nodes.insert(
                        node.path.clone(),
                        RememberedResolution::Filter {
                            revision,
                            fingerprint: fingerprint.clone(),
                            atoms,
                        },
                    );
                    if changed {
                        evidence.push(issue(
                            &mut next_sequence,
                            branch,
                            ObservationFact::ConcreteFilter {
                                path: node.path,
                                revision,
                                filters,
                                fingerprint,
                                cause,
                            },
                        ));
                    }
                }
            }
        }
        self.replace_request_targets_for_handle(id, current_targets);
        if !evidence.is_empty() {
            if let Some(state) = self.observations.get_mut(&observation) {
                state.next_sequence = next_sequence;
            }
            effects.push(Effect::EmitObservationEvidence(observation, evidence));
        }
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

    /// Issue one branch-scoped execution fact into its OBSERVATION's ordered
    /// trace. Facts about an engine-internal handle that belongs to no
    /// observation are dropped, exactly as their row emits already are.
    pub(in crate::core) fn emit_observation_fact(
        &mut self,
        id: HandleId,
        fact: ObservationFact,
        effects: &mut Vec<Effect>,
    ) {
        let Some(BranchOwner { observation, index }) = self.branch_owner(id) else {
            return;
        };
        let Some(state) = self.observations.get_mut(&observation) else {
            return;
        };
        state.next_sequence = state.next_sequence.saturating_add(1);
        let evidence = ObservationEvidence {
            sequence: state.next_sequence,
            branch: Some(index),
            fact,
        };
        effects.push(Effect::EmitObservationEvidence(observation, vec![evidence]));
    }

    /// Which observation and canonical branch index a resolver handle serves.
    pub(in crate::core) fn branch_owner(&self, id: HandleId) -> Option<BranchOwner> {
        self.handles.get(&id).map(|state| BranchOwner {
            observation: state.observation,
            index: state.index,
        })
    }
}

/// The observation and canonical branch index one resolver handle serves.
#[derive(Debug, Clone, Copy)]
pub(super) struct BranchOwner {
    pub(super) observation: super::ObservationId,
    pub(super) index: usize,
}

fn issue(next_sequence: &mut u64, branch: usize, fact: ObservationFact) -> ObservationEvidence {
    *next_sequence = next_sequence.saturating_add(1);
    ObservationEvidence {
        sequence: *next_sequence,
        branch: Some(branch),
        fact,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::EngineMsg;
    use nmp_grammar::LiveQuery;
    use nmp_grammar::{Binding, Demand, Derived, Filter, Freshness, ReadRouting, Selector};
    use nmp_router_testkit::FixtureRoutingFacts;
    use nmp_store::{RedbStore, RelayObserved};
    use nostr::{EventBuilder, Keys, Kind, Tag};

    fn articles_by_follows() -> LiveQuery {
        LiveQuery::single(Demand {
            selection: Filter {
                kinds: Some(BTreeSet::from([30_023])),
                authors: Some(Binding::Derived(Box::new(Derived {
                    inner: Demand {
                        selection: Filter {
                            kinds: Some(BTreeSet::from([3])),
                            authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                            ..Filter::default()
                        },
                        ..Demand::default()
                    },
                    project: Selector::Tag("p".to_string()),
                }))),
                ..Filter::default()
            },
            ..Demand::default()
        })
    }

    fn pinned_articles_by_follows(relay: &RelayUrl, freshness: Freshness) -> LiveQuery {
        let mut inner = Demand {
            selection: Filter {
                kinds: Some(BTreeSet::from([3])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            ..Demand::default()
        };
        inner.freshness = freshness;
        let mut demand = Demand {
            selection: Filter {
                kinds: Some(BTreeSet::from([30_023])),
                authors: Some(Binding::Derived(Box::new(Derived {
                    inner,
                    project: Selector::Tag("p".to_string()),
                }))),
                ..Filter::default()
            },
            ..Demand::default()
        };
        demand.routing = ReadRouting::Explicit(vec![relay.clone()]);
        demand.freshness = freshness;
        LiveQuery::single(demand)
    }

    fn pinned_kind_one(relay: &RelayUrl) -> LiveQuery {
        let mut demand = Demand {
            selection: Filter {
                kinds: Some(BTreeSet::from([1])),
                ..Filter::default()
            },
            ..Demand::default()
        };
        demand.routing = ReadRouting::Explicit(vec![relay.clone()]);
        demand.freshness = Freshness::Live;
        LiveQuery::single(demand)
    }

    fn opened_observation(effects: &[Effect]) -> super::super::ObservationId {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(id, _, _) => Some(*id),
                _ => None,
            })
            .expect("opening an observation emits its cache seed")
    }

    fn observation_facts(effects: &[Effect]) -> Vec<&ObservationEvidence> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::EmitObservationEvidence(_, evidence) => Some(evidence.as_slice()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn current_account_and_external_kind3_changes_emit_only_real_resolution_changes() {
        let account_a = Keys::generate();
        let account_b = Keys::generate();
        let followed_a = Keys::generate();
        let followed_b = Keys::generate();
        let relay = RelayUrl::parse("wss://evidence.fixture").unwrap();
        let mut store = RedbStore::temporary().expect("temporary Redb store");
        for (event, observed_at) in [
            (
                EventBuilder::new(Kind::ContactList, "")
                    .tag(Tag::public_key(followed_a.public_key()))
                    .custom_created_at(Timestamp::from(10))
                    .sign_with_keys(&account_a)
                    .unwrap(),
                11,
            ),
            (
                EventBuilder::new(Kind::ContactList, "")
                    .tag(Tag::public_key(followed_b.public_key()))
                    .custom_created_at(Timestamp::from(20))
                    .sign_with_keys(&account_b)
                    .unwrap(),
                21,
            ),
        ] {
            store
                .insert(
                    event,
                    RelayObserved::new(relay.clone(), Timestamp::from(observed_at)),
                )
                .unwrap();
        }
        let directory = FixtureRoutingFacts::new()
            .with_outbound_routes(account_a.public_key(), [relay.clone()])
            .with_outbound_routes(account_b.public_key(), [relay.clone()])
            .with_outbound_routes(followed_a.public_key(), [relay.clone()])
            .with_outbound_routes(followed_b.public_key(), [relay.clone()]);
        let mut core = CoreState::new_with_fixture_routing_facts(store, directory, 20);
        core.handle(EngineMsg::SetActivePubkey(Some(account_a.public_key())));

        let opened = core.handle(EngineMsg::Subscribe(articles_by_follows()));
        let opened_facts = observation_facts(&opened);
        let paths: Vec<_> = opened_facts
            .iter()
            .map(|evidence| match &evidence.fact {
                ObservationFact::ReactiveInput { path, .. }
                | ObservationFact::DerivedSet { path, .. }
                | ObservationFact::ConcreteFilter { path, .. } => path.as_str(),
                _ => "wire",
            })
            .collect();
        assert_eq!(
            paths,
            [
                "$.authors.inner.authors",
                "$.authors.inner",
                "$.authors",
                "$"
            ]
        );
        assert_eq!(
            opened_facts
                .iter()
                .map(|evidence| evidence.sequence)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );

        let switched = core.handle(EngineMsg::SetActivePubkey(Some(account_b.public_key())));
        let switched_facts = observation_facts(&switched);
        assert!(switched_facts.iter().any(|evidence| matches!(
            evidence.fact,
            ObservationFact::ReactiveInput { revision: 2, .. }
        )));
        assert!(switched_facts.iter().any(|evidence| matches!(
            evidence.fact,
            ObservationFact::DerivedSet { revision: 2, .. }
        )));

        // Drive the real current-generation relay ingest door.
        let handle = nmp_transport::RelayHandle {
            slot: 7,
            generation: 1,
        };
        core.slot_to_relay.insert(
            handle.slot,
            (handle, RelaySessionKey::unauthenticated(relay.clone())),
        );
        core.connected_relays
            .insert(RelaySessionKey::unauthenticated(relay.clone()));
        let same_effective_set = EventBuilder::new(Kind::ContactList, "")
            .tag(Tag::public_key(followed_b.public_key()))
            .custom_created_at(Timestamp::from(30))
            .sign_with_keys(&account_b)
            .unwrap();
        let unchanged = core.handle(EngineMsg::RelayFrame(
            handle,
            RelaySessionKey::unauthenticated(relay.clone()),
            nmp_transport::RelayFrame::from(nostr::RelayMessage::event(
                nostr::SubscriptionId::new("foreign"),
                same_effective_set,
            )),
        ));
        assert!(
            observation_facts(&unchanged).is_empty(),
            "a newer kind:3 with the same effective p set must not fabricate a derived/filter revision"
        );

        let changed_contact = EventBuilder::new(Kind::ContactList, "")
            .tag(Tag::public_key(followed_a.public_key()))
            .custom_created_at(Timestamp::from(40))
            .sign_with_keys(&account_b)
            .unwrap();
        let changed = core.handle(EngineMsg::RelayFrame(
            handle,
            RelaySessionKey::unauthenticated(relay),
            nmp_transport::RelayFrame::from(nostr::RelayMessage::event(
                nostr::SubscriptionId::new("foreign"),
                changed_contact,
            )),
        ));
        let changed_facts = observation_facts(&changed);
        assert!(changed_facts.iter().any(|evidence| matches!(
            &evidence.fact,
            ObservationFact::DerivedSet {
                path,
                revision: 3,
                cause: ResolutionCause::DependencyChanged,
                ..
            } if path == "$.authors"
        )));
        assert!(changed_facts.iter().any(|evidence| matches!(
            &evidence.fact,
            ObservationFact::ConcreteFilter {
                path,
                revision: 3,
                cause: ResolutionCause::DependencyChanged,
                ..
            } if path == "$"
        )));
    }

    #[test]
    fn request_target_multiplicity_replaces_and_tears_down_exactly() {
        let relay = RelayUrl::parse("wss://request-target-multiplicity.example").unwrap();
        let mut core = CoreState::new(RedbStore::temporary().expect("temporary Redb store"), 20);
        let observation =
            opened_observation(&core.handle(EngineMsg::Subscribe(pinned_kind_one(&relay))));
        let handle = core.observations[&observation].branches[0];
        let target = core
            .request_targets
            .declared_for_handle(handle)
            .keys()
            .next()
            .expect("one root filter target")
            .clone();

        core.replace_request_targets_for_handle(handle, BTreeMap::from([(target.clone(), 2)]));
        assert_eq!(core.bench_ownership_census().request_target_edges, 1);
        assert_eq!(core.bench_ownership_census().request_target_refs, 2);
        core.replace_request_targets_for_handle(handle, BTreeMap::from([(target, 1)]));
        assert_eq!(core.bench_ownership_census().request_target_edges, 1);
        assert_eq!(core.bench_ownership_census().request_target_refs, 1);

        core.handle(EngineMsg::Unsubscribe(observation));
        assert_eq!(
            core.bench_ownership_census(),
            super::super::CoreOwnershipCensus::default()
        );
    }

    #[test]
    fn changed_filter_revisions_replace_stale_request_targets_before_send() {
        let account_a = Keys::generate();
        let account_b = Keys::generate();
        let followed_a = Keys::generate();
        let followed_b = Keys::generate();
        let relay = RelayUrl::parse("wss://request-target-revision.example").unwrap();
        let mut store = RedbStore::temporary().expect("temporary Redb store");
        for (event, observed_at) in [
            (
                EventBuilder::new(Kind::ContactList, "")
                    .tag(Tag::public_key(followed_a.public_key()))
                    .custom_created_at(Timestamp::from(10))
                    .sign_with_keys(&account_a)
                    .unwrap(),
                11,
            ),
            (
                EventBuilder::new(Kind::ContactList, "")
                    .tag(Tag::public_key(followed_b.public_key()))
                    .custom_created_at(Timestamp::from(20))
                    .sign_with_keys(&account_b)
                    .unwrap(),
                21,
            ),
        ] {
            store
                .insert(
                    event,
                    RelayObserved::new(relay.clone(), Timestamp::from(observed_at)),
                )
                .unwrap();
        }
        let mut core = CoreState::new(store, 20);
        core.handle(EngineMsg::SetActivePubkey(Some(account_a.public_key())));
        let observation = opened_observation(&core.handle(EngineMsg::Subscribe(
            pinned_articles_by_follows(&relay, Freshness::Live),
        )));
        let handle = core.observations[&observation].branches[0];
        let cache_only_observation = opened_observation(&core.handle(EngineMsg::Subscribe(
            pinned_articles_by_follows(&relay, Freshness::CacheOnly),
        )));
        let cache_only_handle = core.observations[&cache_only_observation].branches[0];
        assert!(core
            .request_targets
            .declared_for_handle(handle)
            .keys()
            .all(|target| target.revision == 1));
        assert!(core
            .request_targets
            .declared_for_handle(cache_only_handle)
            .keys()
            .all(|target| target.revision == 1));

        core.handle(EngineMsg::SetActivePubkey(Some(account_b.public_key())));
        assert!(core
            .request_targets
            .declared_for_handle(handle)
            .keys()
            .all(|target| target.revision == 2));
        assert!(core
            .request_targets
            .declared_for_handle(cache_only_handle)
            .keys()
            .all(|target| target.revision == 2));
        assert_eq!(
            core.request_targets.live_handles(),
            BTreeSet::from([handle])
        );
        let pending_targets: Vec<_> = core
            .pending_request_evidence
            .values()
            .flatten()
            .flat_map(|request| core.current_request_targets(&request.owner_demands))
            .filter(|(target, _, _)| *target == handle)
            .collect();
        assert!(!pending_targets.is_empty());
        assert!(pending_targets
            .iter()
            .all(|(_, _, revision)| *revision == 2));

        core.handle(EngineMsg::Unsubscribe(cache_only_observation));
        core.handle(EngineMsg::Unsubscribe(observation));
        assert_eq!(
            core.bench_ownership_census(),
            super::super::CoreOwnershipCensus::default()
        );
    }
}
