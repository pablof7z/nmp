//! Engine ownership for finite replaceable-operation source rounds.
//!
//! Each unfinished durable source member owns one hidden ordinary
//! [`LiveQuery`]. Router/transport request evidence is translated back into
//! the exact durable round; no relay URL, cached row, or unrelated request is
//! enough on its own.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use nmp_grammar::{
    Binding, CacheMode, Demand, Filter, Freshness, IndexedTagName, LiveQuery, SourceAuthority,
};
use nmp_store::{
    QualifiedSource, SemanticCohortClose, SemanticCohortCloseOutcome,
    SemanticDestinationPlanClosure, SemanticSource, SemanticSourceMemberState,
    SemanticSourcePolicy, SemanticSourceRequest, SemanticSourceRoundFact,
    SemanticSourceRoundOutcome, SemanticSourceTerminal, SourceRoundId,
};
use nostr::nips::nip01::Coordinate;

use super::*;

#[derive(Debug, Clone)]
pub(super) struct SemanticSourceOwner {
    pub(super) coordinate: Coordinate,
    pub(super) round: SourceRoundId,
    pub(super) source: SemanticSource,
    filter: ConcreteFilter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OwnedSemanticSourceRequest {
    pub(super) coordinate: Coordinate,
    pub(super) request: SemanticSourceRequest,
}

pub(super) type SemanticSourceRequestKey = (RelaySessionKey, SubId);

impl EngineCore {
    pub(super) fn owns_semantic_source_demand(
        &self,
        demands: &BTreeSet<nmp_router::DemandKey>,
    ) -> bool {
        self.semantic_source_observations.keys().any(|id| {
            self.observations.get(id).is_some_and(|observation| {
                observation
                    .branches
                    .iter()
                    .filter_map(|branch| self.request_targets_by_handle.get(branch))
                    .flat_map(|targets| targets.keys())
                    .any(|target| demands.contains(&target.demand))
            })
        })
    }

    /// Reconcile hidden request ownership with one resource's durable finite
    /// round. Pending and interrupted-open members own a query; settled
    /// members do not. Reopening an interrupted member produces a fresh
    /// request identity, so stale callbacks from the prior process cannot
    /// settle the new request.
    pub(super) fn sync_semantic_source_owners(
        &mut self,
        coordinate: &Coordinate,
        effects: &mut Vec<Effect>,
    ) {
        let snapshot = match self.store.replaceable_operation_snapshot(coordinate) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.degrade_store(error, effects);
                return;
            }
        };
        let desired = snapshot
            .as_ref()
            .and_then(|snapshot| match &snapshot.source_policy {
                SemanticSourcePolicy::Continuing => None,
                SemanticSourcePolicy::Finite(round) => Some(
                    round
                        .sources
                        .iter()
                        .filter_map(|(source, state)| {
                            (!matches!(state, SemanticSourceMemberState::Settled { .. }))
                                .then_some((round.id, source.clone()))
                        })
                        .collect::<Vec<_>>(),
                ),
            })
            .unwrap_or_default();

        let stale: Vec<_> = self
            .semantic_source_observations
            .iter()
            .filter_map(|(id, owner)| {
                (owner.coordinate == *coordinate
                    && !desired
                        .iter()
                        .any(|(round, source)| *round == owner.round && *source == owner.source))
                .then_some(*id)
            })
            .collect();
        for id in stale {
            self.retire_semantic_source_owner(id, effects);
        }

        let Some(snapshot) = snapshot else { return };
        for (round, source) in desired {
            if self.semantic_source_observations.values().any(|owner| {
                owner.coordinate == *coordinate && owner.round == round && owner.source == source
            }) {
                continue;
            }
            let filter = semantic_source_filter(coordinate, &snapshot.current.source_revision);
            let concrete_filter =
                semantic_source_concrete_filter(coordinate, &snapshot.current.source_revision);
            let demand = Demand::new(
                filter.clone(),
                SourceAuthority::Pinned(BTreeSet::from([source.relay.clone()])),
                source.access,
            )
            .expect("one exact finite source is a nonempty pinned demand");
            let demand = Demand {
                cache: CacheMode::Strict,
                freshness: Freshness::Live,
                ..demand
            };
            let opened = self.on_subscribe(LiveQuery::single(demand));
            let Some(id) = opened.iter().find_map(|effect| match effect {
                Effect::EmitRows(id, ..) => Some(*id),
                _ => None,
            }) else {
                effects.extend(opened);
                continue;
            };
            self.semantic_source_observations.insert(
                id,
                SemanticSourceOwner {
                    coordinate: coordinate.clone(),
                    round,
                    source,
                    filter: concrete_filter,
                },
            );
            effects.extend(opened);
        }
    }

    fn retire_semantic_source_owner(&mut self, id: ObservationId, effects: &mut Vec<Effect>) {
        let Some(owner) = self.semantic_source_observations.remove(&id) else {
            return;
        };
        self.semantic_source_retired_observations.insert(id);
        let retired_requests = self
            .semantic_source_requests
            .iter()
            .filter(|(_, request)| {
                request.coordinate == owner.coordinate
                    && request.request.round == owner.round
                    && request.request.source == owner.source
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in retired_requests {
            self.semantic_source_requests.remove(&key);
        }
        effects.extend(self.on_unsubscribe(id));
    }

    /// Remove private observation delivery and reduce its authoritative
    /// request facts. All other effects keep their ordinary runtime path.
    pub(super) fn consume_semantic_source_effects(&mut self, effects: Vec<Effect>) -> Vec<Effect> {
        let mut queue = VecDeque::from(effects);
        let mut outward = Vec::new();
        while let Some(effect) = queue.pop_front() {
            match effect {
                Effect::EmitRows(id, _, _)
                    if self.semantic_source_observations.contains_key(&id)
                        || self.semantic_source_retired_observations.contains(&id) => {}
                Effect::EmitObservationEvidence(id, evidence)
                    if self.semantic_source_observations.contains_key(&id)
                        || self.semantic_source_retired_observations.contains(&id) =>
                {
                    if let Some(owner) = self.semantic_source_observations.get(&id).cloned() {
                        let mut followups = Vec::new();
                        self.consume_semantic_source_evidence(&owner, evidence, &mut followups);
                        queue.extend(followups);
                    }
                }
                other => outward.push(other),
            }
        }
        self.semantic_source_retired_observations.clear();
        outward
    }

    fn consume_semantic_source_evidence(
        &mut self,
        owner: &SemanticSourceOwner,
        evidence: Vec<ObservationEvidence>,
        effects: &mut Vec<Effect>,
    ) {
        for item in evidence {
            match item.fact {
                ObservationFact::RelayRequest {
                    relay,
                    access,
                    transport_generation,
                    request_revision,
                    filter,
                    ..
                } if relay == owner.source.relay
                    && access == owner.source.access
                    && filter.as_ref() == &owner.filter =>
                {
                    let Some(active) = self.active_request_evidence.get(&request_revision).cloned()
                    else {
                        continue;
                    };
                    if active.session.relay != relay
                        || active.session.access != access
                        || active.handle.generation != transport_generation
                    {
                        continue;
                    }
                    let request = SemanticSourceRequest {
                        round: owner.round,
                        source: owner.source.clone(),
                        transport_generation,
                        request_revision,
                    };
                    let existing_key =
                        self.semantic_source_requests
                            .iter()
                            .find_map(|(key, owned)| {
                                (owned.coordinate == owner.coordinate
                                    && owned.request.round == owner.round
                                    && owned.request.source == owner.source)
                                    .then(|| key.clone())
                            });
                    if let Some(existing_key) = existing_key {
                        let existing = self.semantic_source_requests[&existing_key].clone();
                        if existing.request == request {
                            continue;
                        }
                        self.semantic_source_requests.remove(&existing_key);
                    }
                    match self.store.advance_replaceable_source_round(
                        &owner.coordinate,
                        SemanticSourceRoundFact::RequestOpened(request.clone()),
                    ) {
                        Ok(SemanticSourceRoundOutcome::Advanced)
                        | Ok(SemanticSourceRoundOutcome::AlreadyApplied) => {
                            self.semantic_source_requests.insert(
                                (active.session, active.sub_id),
                                OwnedSemanticSourceRequest {
                                    coordinate: owner.coordinate.clone(),
                                    request,
                                },
                            );
                        }
                        Ok(SemanticSourceRoundOutcome::Stale) => {}
                        Err(error) => self.degrade_store(error, effects),
                    }
                }
                ObservationFact::RequestSettled {
                    relay,
                    access,
                    transport_generation,
                    request_revision,
                    terminal: RequestTerminal::Eose | RequestTerminal::Nip77,
                    ..
                } if relay == owner.source.relay && access == owner.source.access => {
                    if let Some(key) = self.semantic_source_request_key(
                        owner,
                        transport_generation,
                        request_revision,
                    ) {
                        self.settle_semantic_source_request_key(
                            key,
                            SemanticSourceTerminal::Eose,
                            effects,
                        );
                    }
                }
                ObservationFact::RelayClosed {
                    relay,
                    access,
                    transport_generation,
                    request_revision: Some(request_revision),
                    reason,
                    ..
                } if relay == owner.source.relay && access == owner.source.access => {
                    if let Some(key) = self.semantic_source_request_key(
                        owner,
                        transport_generation,
                        request_revision,
                    ) {
                        self.settle_semantic_source_request_key(
                            key,
                            SemanticSourceTerminal::Failed(reason),
                            effects,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn semantic_source_request_key(
        &self,
        owner: &SemanticSourceOwner,
        transport_generation: u64,
        request_revision: u64,
    ) -> Option<SemanticSourceRequestKey> {
        self.semantic_source_requests
            .iter()
            .find_map(|(key, owned)| {
                (owned.coordinate == owner.coordinate
                    && owned.request.round == owner.round
                    && owned.request.source == owner.source
                    && owned.request.transport_generation == transport_generation
                    && owned.request.request_revision == request_revision)
                    .then(|| key.clone())
            })
    }

    fn settle_semantic_source_request_key(
        &mut self,
        key: SemanticSourceRequestKey,
        terminal: SemanticSourceTerminal,
        effects: &mut Vec<Effect>,
    ) {
        let owned = self
            .semantic_source_requests
            .get(&key)
            .cloned()
            .filter(|owned| {
                self.semantic_source_observations.values().any(|owner| {
                    owner.coordinate == owned.coordinate
                        && owner.round == owned.request.round
                        && owner.source == owned.request.source
                })
            });
        let Some(owned) = owned else {
            self.semantic_source_requests.remove(&key);
            return;
        };
        match self.store.advance_replaceable_source_round(
            &owned.coordinate,
            SemanticSourceRoundFact::RequestSettled {
                request: owned.request.clone(),
                terminal,
            },
        ) {
            Ok(SemanticSourceRoundOutcome::Advanced)
            | Ok(SemanticSourceRoundOutcome::AlreadyApplied) => {
                self.semantic_source_requests.remove(&key);
                self.sync_semantic_source_owners(&owned.coordinate, effects);
                self.try_close_semantic_cohort(&owned.coordinate, effects);
            }
            Ok(SemanticSourceRoundOutcome::Stale) => {
                self.semantic_source_requests.remove(&key);
            }
            Err(error) => self.degrade_store(error, effects),
        }
    }

    pub(super) fn semantic_source_request_with_key_for_wire(
        &self,
        session: &RelaySessionKey,
        wire_sub_id: &str,
    ) -> Option<(SemanticSourceRequestKey, OwnedSemanticSourceRequest)> {
        let key = self.semantic_source_request_key_for_wire(session, wire_sub_id)?;
        self.semantic_source_requests
            .get(&key)
            .cloned()
            .map(|owned| (key, owned))
    }

    pub(super) fn semantic_source_request_key_for_wire(
        &self,
        session: &RelaySessionKey,
        wire_sub_id: &str,
    ) -> Option<SemanticSourceRequestKey> {
        let sub_id = self.attribution.sub_id_for_wire(session, wire_sub_id)?;
        let key = (session.clone(), sub_id);
        self.semantic_source_requests
            .contains_key(&key)
            .then_some(key)
    }

    pub(super) fn settle_owned_semantic_source_terminal(
        &mut self,
        key: SemanticSourceRequestKey,
        terminal: SemanticSourceTerminal,
        effects: &mut Vec<Effect>,
    ) {
        if self.semantic_source_requests.contains_key(&key) {
            self.settle_semantic_source_request_key(key, terminal, effects);
        }
    }

    /// Ask the existing atomic store door to close the complete cohort. The
    /// store revalidates both the finite round and every route/lane fact; the
    /// reducer only supplies the current CAS witnesses and then removes the
    /// volatile receipt owners after a committed close.
    pub(super) fn try_close_semantic_cohort(
        &mut self,
        coordinate: &Coordinate,
        effects: &mut Vec<Effect>,
    ) {
        let snapshot = match self.store.replaceable_operation_snapshot(coordinate) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(error) => {
                self.degrade_store(error, effects);
                return;
            }
        };
        if !matches!(
            &snapshot.source_policy,
            SemanticSourcePolicy::Finite(round) if round.is_closed()
        ) {
            return;
        }
        let Some(generation) = snapshot.current.generation.as_ref() else {
            return;
        };
        let Some(owner_intent) = generation.members.first().copied() else {
            return;
        };
        let Some(owner_receipt) = self.intent_receipts.get(&owner_intent).copied() else {
            return;
        };
        let Some(pending) = self.pending.get(&owner_receipt) else {
            return;
        };
        if !pending.route_complete
            || !pending.route_blocked_relays.is_empty()
            || !pending.lane_projection.can_close()
        {
            return;
        }
        let destination = if pending.durable_routes.is_empty() {
            SemanticDestinationPlanClosure::NoDestinations
        } else {
            SemanticDestinationPlanClosure::AllCurrentDestinationsTerminal
        };
        let close = SemanticCohortClose {
            coordinate: coordinate.clone(),
            expected_source_revision: snapshot.current.source_revision,
            expected_program_digest: snapshot.current.program_digest,
            expected_materialization: generation.materialization,
            destination,
        };
        match self.store.close_replaceable_operation_cohort(close) {
            Ok(SemanticCohortCloseOutcome::Closed { members }) => {
                for member in members {
                    let Some(receipt) = self.intent_receipts.get(&member).copied() else {
                        continue;
                    };
                    if let Some(pending) = self.pending.remove(&receipt) {
                        self.forget_pending_indexes(receipt, &pending);
                    }
                    effects.push(Effect::EmitReceipt(
                        receipt,
                        WriteFact::Outcome(WriteOutcome::Settled),
                    ));
                }
                self.sync_semantic_source_owners(coordinate, effects);
            }
            Ok(
                SemanticCohortCloseOutcome::SourceRoundOpen
                | SemanticCohortCloseOutcome::DestinationOpen
                | SemanticCohortCloseOutcome::Stale,
            ) => {}
            Err(error) => self.degrade_store(error, effects),
        }
    }
}

fn semantic_source_filter(
    coordinate: &Coordinate,
    source_revision: &nmp_store::SourceRevision,
) -> Filter {
    let mut tags = BTreeMap::new();
    if (30_000..=39_999).contains(&coordinate.kind.as_u16()) {
        tags.insert(
            IndexedTagName::new('d').expect("d is an indexed Nostr tag"),
            Binding::Literal(BTreeSet::from([coordinate.identifier.clone()])),
        );
    }
    let since = match source_revision.evidence().qualified {
        QualifiedSource::Event { created_at, .. } => Some(created_at.as_secs()),
        QualifiedSource::Absent | QualifiedSource::Unresolved => None,
    };
    Filter {
        kinds: Some(BTreeSet::from([coordinate.kind.as_u16()])),
        authors: Some(Binding::Literal(BTreeSet::from([coordinate
            .public_key
            .to_hex()]))),
        tags,
        since,
        ..Filter::default()
    }
}

fn semantic_source_concrete_filter(
    coordinate: &Coordinate,
    source_revision: &nmp_store::SourceRevision,
) -> ConcreteFilter {
    let mut tags = BTreeMap::new();
    if (30_000..=39_999).contains(&coordinate.kind.as_u16()) {
        tags.insert(
            IndexedTagName::new('d').expect("d is an indexed Nostr tag"),
            BTreeSet::from([coordinate.identifier.clone()]),
        );
    }
    let since = match source_revision.evidence().qualified {
        QualifiedSource::Event { created_at, .. } => Some(created_at.as_secs()),
        QualifiedSource::Absent | QualifiedSource::Unresolved => None,
    };
    ConcreteFilter {
        kinds: Some(BTreeSet::from([coordinate.kind.as_u16()])),
        authors: Some(BTreeSet::from([coordinate.public_key.to_hex()])),
        tags,
        since,
        ..ConcreteFilter::default()
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;

    use ::negentropy::{Negentropy as RawNegentropy, NegentropyStorageVector as RawStorage};
    use nmp_store::{RedbStore, RelayObserved};
    use nostr::{EventBuilder as NostrEventBuilder, Keys, Kind, RelayMessage, SubscriptionId, Tag};

    use crate::replaceable_materializer::{
        ReplaceableMaterializer, ReplaceableMaterializerOperation, ReplaceableMaterializerRefusal,
        ReplaceableMaterializerRegistration,
    };

    use super::*;

    const PROGRAM: [u8; 16] = [42; 16];
    const FORMAT: [u8; 16] = [43; 16];

    struct AddPerson;

    impl ReplaceableMaterializer for AddPerson {
        fn materialize(
            &self,
            _source: &UnsignedEvent,
            current: &UnsignedEvent,
            operations: &[ReplaceableMaterializerOperation<'_>],
        ) -> Result<nmp_grammar::EventBuilder, ReplaceableMaterializerRefusal> {
            let mut tags = current.tags.clone().to_vec();
            for operation in operations {
                let person = PublicKey::from_slice(operation.bytes()).map_err(|error| {
                    ReplaceableMaterializerRefusal {
                        reason: error.to_string(),
                    }
                })?;
                if !tags
                    .iter()
                    .any(|tag| tag.as_slice() == ["p", &person.to_hex()])
                {
                    tags.push(Tag::public_key(person));
                }
            }
            Ok(nmp_grammar::EventBuilder {
                kind: current.kind,
                tags,
                content: current.content.clone(),
                created_at: None,
            })
        }

        fn materialize_default(
            &self,
            _coordinate: &Coordinate,
            _operations: &[ReplaceableMaterializerOperation<'_>],
        ) -> Result<nmp_grammar::EventBuilder, ReplaceableMaterializerRefusal> {
            Err(ReplaceableMaterializerRefusal {
                reason: "fixture requires an existing source".to_string(),
            })
        }
    }

    fn source(author: &Keys, at: u64, content: &str) -> SignedEvent {
        NostrEventBuilder::new(Kind::ContactList, content)
            .custom_created_at(Timestamp::from(at))
            .sign_with_keys(author)
            .expect("source fixture signs")
    }

    fn register(core: &mut EngineCore) {
        core.install_replaceable_materializer(ReplaceableMaterializerRegistration {
            program: PROGRAM,
            format: FORMAT,
            materializer: Arc::new(AddPerson),
        });
    }

    fn coordinate(author: &Keys) -> Coordinate {
        Coordinate {
            kind: Kind::ContactList,
            public_key: author.public_key(),
            identifier: String::new(),
        }
    }

    fn requests(
        effects: &[Effect],
    ) -> Vec<(RelaySessionKey, SubId, ConcreteFilter, RequestAttemptId)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Wire(delta) => Some(delta),
                _ => None,
            })
            .flat_map(|delta| {
                delta.ops.iter().flat_map(move |(session, ops)| {
                    ops.iter().filter_map(move |op| {
                        let WireOp::Req(sub_id, filter) = op else {
                            return None;
                        };
                        Some((
                            session.clone(),
                            sub_id.clone(),
                            filter.clone(),
                            delta.attempt_id(session, sub_id, filter),
                        ))
                    })
                })
            })
            .collect()
    }

    fn eose(
        core: &mut EngineCore,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
        sub_id: &SubId,
    ) -> Vec<Effect> {
        core.handle(EngineMsg::RelayFrame(
            handle,
            session,
            RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
                SubscriptionId::new(wire_sub_id_string(sub_id)),
            ))),
        ))
    }

    fn send_event(
        core: &mut EngineCore,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
        sub_id: &SubId,
        event: SignedEvent,
    ) -> Vec<Effect> {
        core.handle(EngineMsg::RelayFrame(
            handle,
            session,
            RelayFrame::from_message(RelayMessage::Event {
                subscription_id: Cow::Owned(SubscriptionId::new(wire_sub_id_string(sub_id))),
                event: Cow::Owned(event),
            }),
        ))
    }


    fn member_state(
        core: &EngineCore,
        coordinate: &Coordinate,
        relay: &RelayUrl,
    ) -> SemanticSourceMemberState {
        let snapshot = core
            .store
            .replaceable_operation_snapshot(coordinate)
            .unwrap()
            .unwrap();
        let SemanticSourcePolicy::Finite(round) = snapshot.source_policy else {
            panic!("fixture must retain a finite source round");
        };
        round.sources[&SemanticSource::new(relay.clone(), AccessContext::Public)].clone()
    }

    #[test]
    fn finite_sources_are_exact_requests_restart_unfinished_and_close_with_destinations() {
        let author = Keys::generate();
        let person = Keys::generate().public_key();
        let relay_one = RelayUrl::parse("wss://finite-source-one.example").unwrap();
        let relay_two = RelayUrl::parse("wss://finite-source-two.example").unwrap();
        let source_one = SemanticSource::new(relay_one.clone(), AccessContext::Public);
        let source_two = SemanticSource::new(relay_two.clone(), AccessContext::Public);
        let session_one = RelaySessionKey::public(relay_one.clone());
        let session_two = RelaySessionKey::public(relay_two.clone());
        let handle_one = TransportRelayHandle {
            slot: 21,
            generation: 1,
        };
        let handle_two = TransportRelayHandle {
            slot: 22,
            generation: 1,
        };
        let base = source(&author, 10, "base");
        let successor = source(&author, 20, "relay two successor");
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("finite-source-round.redb");
        let mut store = RedbStore::open(&path).unwrap();
        store
            .insert(
                base.clone(),
                RelayObserved::new(relay_one.clone(), Timestamp::from(10u64)),
            )
            .unwrap();

        let mut core = EngineCore::new(store, 10);
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        register(&mut core);
        core.handle(EngineMsg::RelayConnected(handle_one, session_one.clone()));
        core.handle(EngineMsg::RelayConnected(handle_two, session_two.clone()));
        let payload = nmp_grammar::ReplaceableOperation::from_registered_parts(
            PROGRAM,
            FORMAT,
            UnsignedEvent::from(base.clone()),
            UnsignedEvent::from(base),
            nmp_grammar::ReplaceableSourcePolicy::Finite {
                relays: BTreeSet::from([relay_one.clone(), relay_two.clone()]),
                access: AccessContext::Public,
            },
            person.to_bytes().to_vec(),
        )
        .unwrap();
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::ReplaceableOperation(payload),
            routing: WriteRouting::Explicit(vec![relay_one.clone()]),
            identity: Identity::Active,
            correlation: None,
        }));
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::WriteAccepted(receipt, _) => Some(*receipt),
                _ => None,
            })
            .expect("finite operation enters custody");
        assert_eq!(core.semantic_source_observations.len(), 2);

        let admitted = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(11u64)));
        let mut source_requests = requests(&admitted);
        source_requests.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(source_requests.len(), 2, "one exact REQ per source member");
        for (session, _, filter, attempt) in &source_requests {
            let handle = if *session == session_one {
                handle_one
            } else {
                handle_two
            };
            core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
                attempt_id: *attempt,
                handle,
            });
            assert_eq!(
                filter.kinds,
                Some(BTreeSet::from([Kind::ContactList.as_u16()]))
            );
            assert_eq!(
                filter.authors,
                Some(BTreeSet::from([author.public_key().to_hex()]))
            );
        }
        assert!(matches!(
            member_state(&core, &coordinate(&author), &relay_one),
            SemanticSourceMemberState::Open(_)
        ));
        assert!(matches!(
            member_state(&core, &coordinate(&author), &relay_two),
            SemanticSourceMemberState::Open(_)
        ));

        // An unrelated request on relay one receives its own EOSE. It cannot
        // settle either source member merely because relay/session match.
        let unrelated = LiveQuery::single(
            Demand::new(
                Filter {
                    kinds: Some(BTreeSet::from([Kind::TextNote.as_u16()])),
                    ..Filter::default()
                },
                SourceAuthority::Pinned(BTreeSet::from([relay_one.clone()])),
                AccessContext::Public,
            )
            .unwrap(),
        );
        core.handle(EngineMsg::Subscribe(unrelated));
        let unrelated_wire = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(12u64)));
        let unrelated_request = requests(&unrelated_wire)
            .into_iter()
            .find(|(session, _, _, _)| *session == session_one)
            .expect("unrelated query opens its own request");
        core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id: unrelated_request.3,
            handle: handle_one,
        });
        eose(
            &mut core,
            handle_one,
            session_one.clone(),
            &unrelated_request.1,
        );
        assert!(matches!(
            member_state(&core, &coordinate(&author), &relay_one),
            SemanticSourceMemberState::Open(_)
        ));

        let request_one = source_requests
            .iter()
            .find(|(session, _, _, _)| *session == session_one)
            .cloned()
            .unwrap();
        let request_two = source_requests
            .iter()
            .find(|(session, _, _, _)| *session == session_two)
            .cloned()
            .unwrap();
        let owner_one_id = core
            .semantic_source_observations
            .iter()
            .find_map(|(id, owner)| (owner.source == source_one).then_some(*id))
            .expect("relay one has a hidden observation owner");
        let settled_one = eose(&mut core, handle_one, session_one.clone(), &request_one.1);
        assert!(settled_one.iter().all(|effect| !matches!(
            effect,
            Effect::EmitRows(id, ..) | Effect::EmitObservationEvidence(id, _) if *id == owner_one_id
        )), "the retired hidden observation never leaks rows or evidence");
        assert!(
            settled_one.iter().any(|effect| matches!(
                effect,
                Effect::Wire(delta) if delta.ops.iter().any(|(_, ops)| ops.iter().any(
                    |op| matches!(op, WireOp::Close(sub_id) if sub_id == &request_one.1)
                ))
            )),
            "retiring the hidden observation preserves its outward wire close"
        );
        assert!(matches!(
            member_state(&core, &coordinate(&author), &relay_one),
            SemanticSourceMemberState::Settled { .. }
        ));
        assert!(matches!(
            member_state(&core, &coordinate(&author), &relay_two),
            SemanticSourceMemberState::Open(_)
        ));

        send_event(
            &mut core,
            handle_two,
            session_two.clone(),
            &request_two.1,
            successor.clone(),
        );
        let before_restart = core
            .store
            .replaceable_operation_snapshot(&coordinate(&author))
            .unwrap()
            .unwrap();
        assert!(matches!(
            before_restart.current.source_revision.evidence().qualified,
            QualifiedSource::Event { event_id, .. } if event_id == successor.id
        ));
        drop(core);

        let reopened_store = RedbStore::open(&path).unwrap();
        let mut reopened = EngineCore::new(reopened_store, 10);
        reopened.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
        register(&mut reopened);
        let new_handle_two = TransportRelayHandle {
            slot: 32,
            generation: 2,
        };
        reopened.handle(EngineMsg::RelayConnected(
            new_handle_two,
            session_two.clone(),
        ));
        let recovered = reopened.recover_on_boot();
        let (sign_receipt, sign_generation, unsigned) = recovered
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned) => {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("the current successor generation resumes signing");
        assert_eq!(sign_receipt, receipt);
        assert_eq!(reopened.semantic_source_observations.len(), 1);
        assert_eq!(
            reopened
                .semantic_source_observations
                .values()
                .next()
                .unwrap()
                .source,
            source_two
        );
        assert!(!reopened
            .semantic_source_observations
            .values()
            .any(|owner| owner.source == source_one));

        let readmitted = reopened.handle(EngineMsg::FlushWireAdmission(Timestamp::from(21u64)));
        let resumed = requests(&readmitted)
            .into_iter()
            .find(|(session, _, _, _)| *session == session_two)
            .expect("only unfinished relay two is requested after restart");
        let resumed_owner = reopened
            .semantic_source_observations
            .values()
            .next()
            .expect("unfinished source keeps its hidden owner");
        assert_eq!(resumed.2, resumed_owner.filter);
        reopened.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id: resumed.3,
            handle: new_handle_two,
        });
        assert_eq!(reopened.semantic_source_requests.len(), 1);
        let resumed_state = member_state(&reopened, &coordinate(&author), &relay_two);
        let SemanticSourceMemberState::Open(resumed_request) = resumed_state else {
            panic!("relay two remains open under its new request identity");
        };
        assert_eq!(resumed_request.transport_generation, 2);

        // A callback from the dead generation is ignored before attribution
        // and therefore cannot settle the reconstructed request.
        eose(
            &mut reopened,
            handle_two,
            session_two.clone(),
            &request_two.1,
        );
        assert_eq!(
            member_state(&reopened, &coordinate(&author), &relay_two),
            SemanticSourceMemberState::Open(resumed_request.clone())
        );
        eose(
            &mut reopened,
            new_handle_two,
            session_two.clone(),
            &resumed.1,
        );
        assert!(matches!(
            member_state(&reopened, &coordinate(&author), &relay_two),
            SemanticSourceMemberState::Settled { .. }
        ));

        // Even an exact-coordinate event on a different ordinary request has
        // no finite-round authority after settlement and cannot resurrect the
        // source or replace the generated successor.
        let new_handle_one = TransportRelayHandle {
            slot: 31,
            generation: 2,
        };
        reopened.handle(EngineMsg::RelayConnected(
            new_handle_one,
            session_one.clone(),
        ));
        let exact_unrelated = LiveQuery::single(
            Demand::new(
                semantic_source_filter(
                    &coordinate(&author),
                    &before_restart.current.source_revision,
                ),
                SourceAuthority::Pinned(BTreeSet::from([relay_one.clone()])),
                AccessContext::Public,
            )
            .unwrap(),
        );
        reopened.handle(EngineMsg::Subscribe(exact_unrelated));
        let unrelated_admitted =
            reopened.handle(EngineMsg::FlushWireAdmission(Timestamp::from(22u64)));
        let unrelated_exact = requests(&unrelated_admitted)
            .into_iter()
            .find(|(session, _, _, _)| *session == session_one)
            .expect("unrelated exact-coordinate request opens");
        reopened.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id: unrelated_exact.3,
            handle: new_handle_one,
        });
        let later_unrelated = source(&author, 30, "unrelated later source");
        send_event(
            &mut reopened,
            new_handle_one,
            session_one.clone(),
            &unrelated_exact.1,
            later_unrelated,
        );
        let unchanged = reopened
            .store
            .replaceable_operation_snapshot(&coordinate(&author))
            .unwrap()
            .unwrap();
        assert!(matches!(
            unchanged.current.source_revision.evidence().qualified,
            QualifiedSource::Event { event_id, .. } if event_id == successor.id
        ));

        let write_session =
            RelaySessionKey::new(relay_one.clone(), AccessContext::Nip42(author.public_key()));
        let write_handle = TransportRelayHandle {
            slot: 33,
            generation: 1,
        };
        reopened.handle(EngineMsg::RelayConnected(
            write_handle,
            write_session.clone(),
        ));
        reopened.handle(EngineMsg::AuthProbeReleased(
            write_handle,
            write_session.clone(),
        ));
        let signed = unsigned
            .sign_with_keys(&author)
            .expect("sign current successor");
        let promoted = reopened.handle(EngineMsg::SignerCompleted(
            sign_receipt,
            sign_generation,
            Ok(signed.clone()),
        ));
        let correlation = promoted
            .iter()
            .find_map(|effect| match effect {
                Effect::PublishEvent(session, event, correlation)
                    if session == &write_session && event.id == signed.id =>
                {
                    Some(*correlation)
                }
                _ => None,
            })
            .expect("signed current generation starts its destination lane");
        reopened.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
        let closed = reopened.handle(EngineMsg::RelayFrame(
            write_handle,
            write_session,
            RelayFrame::from(RelayMessage::ok(signed.id, true, "")),
        ));
        assert!(closed.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(id, WriteFact::Outcome(WriteOutcome::Settled)) if *id == receipt
        )));
        assert!(reopened
            .store
            .replaceable_operation_snapshot(&coordinate(&author))
            .unwrap()
            .is_none());
        assert!(matches!(
            reopened
                .store
                .reattach_receipt(receipt.0)
                .unwrap()
                .unwrap()
                .payload,
            PublishQueueReceiptPayload::ReplaceableOperation {
                state: nmp_store::ReplaceableOperationReceiptState::Settled,
                ..
            }
        ));
    }
}
