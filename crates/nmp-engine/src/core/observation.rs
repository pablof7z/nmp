use nmp_grammar::{AccessContext, ConcreteFilter, DescriptorHash, IdentityField, RelaySessionKey};
use nmp_resolver::{HandleId, ResolutionNodeKind, ResolvedValue};
use nmp_router::SubId;
use nmp_store::{coverage_key, EventStore};
use nmp_transport::RelayHandle as TransportRelayHandle;
use nostr::{RelayUrl, Timestamp};
use std::collections::{BTreeMap, BTreeSet};

use super::{AttributionSendId, Effect, EngineCore};

/// Ordered execution evidence for one live observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationEvidence {
    /// Monotonic within this observation. Sequence numbers are assigned by
    /// the reducer that owns the observation, never by a delivery adapter.
    pub sequence: u64,
    pub fact: ObservationFact,
}

/// Why a resolver-owned value/filter transition was evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionCause {
    Initial,
    ActiveAccountChanged,
    DependencyChanged,
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
        access: AccessContext,
        transport_generation: u64,
        request_revision: u64,
        filter: ConcreteFilter,
        replay: bool,
    },
    RelayEose {
        path: String,
        filter_revision: u64,
        relay: RelayUrl,
        access: AccessContext,
        transport_generation: u64,
        request_revision: u64,
        observed_at: Timestamp,
    },
    RelayClosed {
        path: String,
        filter_revision: u64,
        relay: RelayUrl,
        access: AccessContext,
        transport_generation: u64,
        request_revision: Option<u64>,
        reason: String,
    },
    RelayRefused {
        path: String,
        filter_revision: u64,
        relay: RelayUrl,
        access: AccessContext,
        transport_generation: Option<u64>,
        request_revision: u64,
        reason: String,
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

#[derive(Debug, Default)]
pub(super) struct ObservationExecutionState {
    next_sequence: u64,
    nodes: BTreeMap<String, RememberedResolution>,
}

#[derive(Debug, Clone)]
pub(super) struct PendingRequestEvidence {
    pub(super) request_revision: u64,
    pub(super) session: RelaySessionKey,
    pub(super) sub_id: SubId,
    pub(super) filter: ConcreteFilter,
    pub(super) targets: Vec<(HandleId, String, u64)>,
    pub(super) replay: bool,
}

#[derive(Debug, Clone)]
pub(super) struct ActiveRequestEvidence {
    pub(super) request_revision: u64,
    pub(super) session: RelaySessionKey,
    pub(super) sub_id: SubId,
    pub(super) targets: Vec<(HandleId, String, u64)>,
    pub(super) handle: TransportRelayHandle,
}

impl ObservationExecutionState {
    fn issue(&mut self, fact: ObservationFact) -> ObservationEvidence {
        self.next_sequence = self.next_sequence.saturating_add(1);
        ObservationEvidence {
            sequence: self.next_sequence,
            fact,
        }
    }

    pub(super) fn request_targets(
        &self,
        absorbed: &std::collections::BTreeSet<nmp_store::CoverageKey>,
    ) -> Vec<(String, u64)> {
        self.nodes
            .iter()
            .filter_map(|(path, node)| {
                let RememberedResolution::Filter {
                    revision, atoms, ..
                } = node
                else {
                    return None;
                };
                atoms
                    .iter()
                    .any(|atom| absorbed.contains(&coverage_key(atom)))
                    .then(|| (path.clone(), *revision))
            })
            .collect()
    }
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

impl<S: EventStore> EngineCore<S> {
    pub(super) fn record_observed_request(
        &mut self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        filter: &ConcreteFilter,
        absorbed: BTreeSet<nmp_store::CoverageKey>,
        replay: bool,
    ) -> AttributionSendId {
        let send = self
            .attribution
            .record_send(session, sub_id, filter, absorbed.clone());
        let targets = self
            .handles
            .iter()
            .flat_map(|(id, state)| {
                state
                    .execution
                    .request_targets(&absorbed)
                    .into_iter()
                    .map(|(path, revision)| (*id, path, revision))
            })
            .collect();
        self.pending_request_evidence
            .entry((session.clone(), sub_id.clone()))
            .or_default()
            .push_back(PendingRequestEvidence {
                request_revision: send.revision(),
                session: session.clone(),
                sub_id: sub_id.clone(),
                filter: filter.clone(),
                targets,
                replay,
            });
        send
    }

    pub(crate) fn on_wire_request_handoff(
        &mut self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        filter_hash: DescriptorHash,
        handle: Option<TransportRelayHandle>,
        accepted: bool,
        reason: Option<String>,
    ) -> Vec<Effect> {
        let key = (session.clone(), sub_id.clone());
        let Some(queue) = self.pending_request_evidence.get_mut(&key) else {
            return Vec::new();
        };
        let Some(position) = queue
            .iter()
            .position(|request| request.filter.hash() == filter_hash)
        else {
            return Vec::new();
        };
        let request = queue
            .remove(position)
            .expect("position came from pending request queue");
        if queue.is_empty() {
            self.pending_request_evidence.remove(&key);
        }
        let mut effects = Vec::new();
        match (accepted, handle) {
            (true, Some(handle)) => {
                for (id, path, filter_revision) in &request.targets {
                    self.emit_observation_fact(
                        *id,
                        ObservationFact::RelayRequest {
                            path: path.clone(),
                            filter_revision: *filter_revision,
                            relay: request.session.relay.clone(),
                            access: request.session.access,
                            transport_generation: handle.generation,
                            request_revision: request.request_revision,
                            filter: request.filter.clone(),
                            replay: request.replay,
                        },
                        &mut effects,
                    );
                }
                self.active_request_evidence.insert(
                    request.request_revision,
                    ActiveRequestEvidence {
                        request_revision: request.request_revision,
                        session: request.session,
                        sub_id: request.sub_id,
                        targets: request.targets,
                        handle,
                    },
                );
            }
            (_, handle) => {
                let reason = reason.unwrap_or_else(|| "transport refused request".to_string());
                for (id, path, filter_revision) in request.targets {
                    self.emit_observation_fact(
                        id,
                        ObservationFact::RelayRefused {
                            path,
                            filter_revision,
                            relay: request.session.relay.clone(),
                            access: request.session.access,
                            transport_generation: handle.map(|value| value.generation),
                            request_revision: request.request_revision,
                            reason: reason.clone(),
                        },
                        &mut effects,
                    );
                }
            }
        }
        effects
    }

    pub(super) fn emit_request_eose(
        &mut self,
        send: AttributionSendId,
        observed_at: Timestamp,
        effects: &mut Vec<Effect>,
    ) {
        let Some(request) = self.active_request_evidence.remove(&send.revision()) else {
            return;
        };
        for (id, path, filter_revision) in request.targets {
            self.emit_observation_fact(
                id,
                ObservationFact::RelayEose {
                    path,
                    filter_revision,
                    relay: request.session.relay.clone(),
                    access: request.session.access,
                    transport_generation: request.handle.generation,
                    request_revision: request.request_revision,
                    observed_at,
                },
                effects,
            );
        }
    }

    pub(super) fn close_requests_for_session(
        &mut self,
        session: &RelaySessionKey,
        handle: TransportRelayHandle,
        reason: String,
        effects: &mut Vec<Effect>,
    ) {
        let revisions: Vec<_> = self
            .active_request_evidence
            .iter()
            .filter_map(|(revision, request)| {
                (&request.session == session && request.handle == handle).then_some(*revision)
            })
            .collect();
        for revision in revisions {
            let Some(request) = self.active_request_evidence.remove(&revision) else {
                continue;
            };
            for (id, path, filter_revision) in request.targets {
                self.emit_observation_fact(
                    id,
                    ObservationFact::RelayClosed {
                        path,
                        filter_revision,
                        relay: request.session.relay.clone(),
                        access: request.session.access,
                        transport_generation: handle.generation,
                        request_revision: Some(request.request_revision),
                        reason: reason.clone(),
                    },
                    effects,
                );
            }
        }
    }

    pub(super) fn close_requests_for_sub(
        &mut self,
        session: &RelaySessionKey,
        handle: TransportRelayHandle,
        sub_id: &SubId,
        reason: String,
        effects: &mut Vec<Effect>,
    ) {
        let revisions: Vec<_> = self
            .active_request_evidence
            .iter()
            .filter_map(|(revision, request)| {
                (&request.session == session
                    && request.handle == handle
                    && &request.sub_id == sub_id)
                    .then_some(*revision)
            })
            .collect();
        for revision in revisions {
            let Some(request) = self.active_request_evidence.remove(&revision) else {
                continue;
            };
            for (id, path, filter_revision) in request.targets {
                self.emit_observation_fact(
                    id,
                    ObservationFact::RelayClosed {
                        path,
                        filter_revision,
                        relay: request.session.relay.clone(),
                        access: request.session.access,
                        transport_generation: handle.generation,
                        request_revision: Some(request.request_revision),
                        reason: reason.clone(),
                    },
                    effects,
                );
            }
        }
    }

    pub(super) fn reconcile_observation_resolution(
        &mut self,
        id: HandleId,
        cause: ResolutionCause,
        effects: &mut Vec<Effect>,
    ) {
        let snapshot = self.resolver.resolution_snapshot(id);
        let Some(state) = self.handles.get_mut(&id) else {
            return;
        };
        let mut evidence = Vec::new();
        for node in snapshot {
            match node.kind {
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
                        evidence.push(state.execution.issue(ObservationFact::ReactiveInput {
                            path: node.path,
                            field,
                            revision,
                            values,
                            fingerprint,
                            cause,
                        }));
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
                        evidence.push(state.execution.issue(ObservationFact::DerivedSet {
                            path: node.path,
                            revision,
                            values,
                            fingerprint,
                            cause,
                        }));
                    }
                }
                ResolutionNodeKind::Filter { atoms } => {
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
                    state.execution.nodes.insert(
                        node.path.clone(),
                        RememberedResolution::Filter {
                            revision,
                            fingerprint: fingerprint.clone(),
                            atoms,
                        },
                    );
                    if changed {
                        evidence.push(state.execution.issue(ObservationFact::ConcreteFilter {
                            path: node.path,
                            revision,
                            filters,
                            fingerprint,
                            cause,
                        }));
                    }
                }
            }
        }
        if !evidence.is_empty() {
            effects.push(Effect::EmitObservationEvidence(id, evidence));
        }
    }

    pub(super) fn emit_observation_fact(
        &mut self,
        id: HandleId,
        fact: ObservationFact,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(state) = self.handles.get_mut(&id) {
            effects.push(Effect::EmitObservationEvidence(
                id,
                vec![state.execution.issue(fact)],
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{EngineMsg, RowSink};
    use nmp_grammar::{Binding, Demand, Derived, Filter, Selector};
    use nmp_resolver::LiveQuery;
    use nmp_router::FixtureDirectory;
    use nmp_store::{MemoryStore, RelayObserved};
    use nostr::{EventBuilder, Keys, Kind, Tag};

    struct NullRows;

    impl RowSink for NullRows {
        fn on_rows(&self, _rows: Vec<crate::core::RowDelta>) {}
    }

    fn articles_by_follows() -> LiveQuery {
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([30_023])),
            authors: Some(Binding::Derived(Box::new(Derived {
                inner: Demand::from_filter(Filter {
                    kinds: Some(BTreeSet::from([3])),
                    authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                    ..Filter::default()
                }),
                project: Selector::Tag("p".to_string()),
            }))),
            ..Filter::default()
        })
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
    fn active_account_and_external_kind3_changes_emit_only_real_resolution_changes() {
        let account_a = Keys::generate();
        let account_b = Keys::generate();
        let followed_a = Keys::generate();
        let followed_b = Keys::generate();
        let relay = RelayUrl::parse("wss://evidence.fixture").unwrap();
        let mut store = MemoryStore::new();
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
        let directory = FixtureDirectory::new()
            .with_write(account_a.public_key().to_hex(), [relay.clone()])
            .with_write(account_b.public_key().to_hex(), [relay.clone()])
            .with_write(followed_a.public_key().to_hex(), [relay.clone()])
            .with_write(followed_b.public_key().to_hex(), [relay.clone()]);
        let mut core = EngineCore::new(store, Box::new(directory), 20);
        core.handle(EngineMsg::SetActivePubkey(Some(account_a.public_key())));

        let opened = core.handle(EngineMsg::Subscribe(
            articles_by_follows(),
            Box::new(NullRows),
        ));
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
            (handle, RelaySessionKey::public(relay.clone())),
        );
        core.connected_relays
            .insert(RelaySessionKey::public(relay.clone()));
        let same_effective_set = EventBuilder::new(Kind::ContactList, "")
            .tag(Tag::public_key(followed_b.public_key()))
            .custom_created_at(Timestamp::from(30))
            .sign_with_keys(&account_b)
            .unwrap();
        let unchanged = core.handle(EngineMsg::RelayFrame(
            handle,
            RelaySessionKey::public(relay.clone()),
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
            RelaySessionKey::public(relay),
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
}
