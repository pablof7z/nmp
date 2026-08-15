use super::*;
use std::sync::Arc;

pub(crate) enum PublishPreparation {
    Complete(Vec<Effect>),
    Materialize(Box<PreparedReplaceableMaterialization>),
}

pub(crate) struct PreparedReplaceableMaterialization {
    pub(crate) call: ReplaceableMaterializationCall,
    pub(crate) continuation: ReplaceableMaterializationContinuation,
}

pub struct PreparedReplaceableSuccessor {
    pub(crate) call: ReplaceableMaterializationCall,
    pub(crate) continuation: ReplaceableSuccessorContinuation,
}

impl std::fmt::Debug for PreparedReplaceableSuccessor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedReplaceableSuccessor")
            .finish_non_exhaustive()
    }
}

pub(crate) struct ReplaceableMaterializationCall {
    materializer: Arc<dyn crate::ReplaceableMaterializer>,
    input: ReplaceableMaterializationInput,
    operations: Vec<Vec<u8>>,
}

enum ReplaceableMaterializationInput {
    Existing {
        source: Box<UnsignedEvent>,
        current: Box<UnsignedEvent>,
    },
    CapabilityDefault {
        coordinate: Coordinate,
    },
}

pub(crate) enum ReplaceableMaterializationOutcome {
    Materialized(nmp_grammar::EventBuilder),
    Refused(String),
}

pub(crate) struct ReplaceableMaterializationContinuation {
    program: ReplayProgramId,
    format: ReplayFormatId,
    materializer: Arc<dyn crate::ReplaceableMaterializer>,
    start: nmp_grammar::ReplaceableOperationStart,
    declared_source_policy: nmp_grammar::ReplaceableSourcePolicy,
    operation_bytes: Vec<u8>,
    signing_pubkey: PublicKey,
    coordinate: Coordinate,
    routing: WriteRouting,
    correlation: Option<nmp_grammar::CorrelationToken>,
    fence: ReplaceableMaterializationFence,
    canonical_source_id: Option<EventId>,
}

pub(crate) struct ReplaceableSuccessorContinuation {
    pub(super) program: ReplayProgramId,
    pub(super) format: ReplayFormatId,
    pub(super) materializer: Arc<dyn crate::ReplaceableMaterializer>,
    pub(super) coordinate: Coordinate,
    pub(super) fence: ReplaceableMaterializationFence,
    pub(super) observation: AttributedRelayObservation,
    pub(super) source_request: Option<(
        super::super::semantic_sources::SemanticSourceRequestKey,
        super::super::semantic_sources::OwnedSemanticSourceRequest,
    )>,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ReplaceableMaterializationFence {
    source_revision: Option<nmp_store::SourceRevision>,
    program_digest: Option<nmp_store::SemanticProgramDigest>,
    current_materialization: Option<nmp_store::MaterializationId>,
    operations: Vec<nmp_store::SemanticOperation>,
}

impl ReplaceableMaterializationCall {
    pub(super) fn new(
        materializer: Arc<dyn crate::ReplaceableMaterializer>,
        source: UnsignedEvent,
        current: UnsignedEvent,
        operations: Vec<Vec<u8>>,
    ) -> Self {
        Self {
            materializer,
            input: ReplaceableMaterializationInput::Existing {
                source: Box::new(source),
                current: Box::new(current),
            },
            operations,
        }
    }

    pub(crate) fn execute(self) -> ReplaceableMaterializationOutcome {
        let operations = self
            .operations
            .iter()
            .map(|operation| ReplaceableMaterializerOperation::new(operation))
            .collect::<Vec<_>>();
        let result = match &self.input {
            ReplaceableMaterializationInput::Existing { source, current } => {
                self.materializer.materialize(source, current, &operations)
            }
            ReplaceableMaterializationInput::CapabilityDefault { coordinate } => self
                .materializer
                .materialize_default(coordinate, &operations),
        };
        match result {
            Ok(builder) => ReplaceableMaterializationOutcome::Materialized(builder),
            Err(refusal) => ReplaceableMaterializationOutcome::Refused(refusal.reason),
        }
    }
}

impl EngineCore {
    pub(super) fn prepare_body_complete_replaceable_operation(
        &mut self,
        operation: nmp_grammar::ReplaceableOperation,
        routing: WriteRouting,
        identity: Identity,
        correlation: Option<nmp_grammar::CorrelationToken>,
    ) -> PublishPreparation {
        let (program, format, start, declared_source_policy, operation_bytes) =
            operation.into_registered_parts();
        let Some(registration) = self.replaceable_materializers.get(&(program, format)) else {
            return PublishPreparation::Complete(self.refuse_publish(
                PublishError::ReplaceableOperationRefused {
                    reason: "replaceable materializer is not configured".to_string(),
                },
            ));
        };
        let program = ReplayProgramId(program);
        let format = ReplayFormatId(format);
        let materializer = registration.materializer.clone();

        let existing_author = match &start {
            nmp_grammar::ReplaceableOperationStart::Existing {
                original_source, ..
            } => Some(original_source.pubkey),
            nmp_grammar::ReplaceableOperationStart::CapabilityDefault { .. } => None,
        };
        let signing_pubkey = match identity {
            Identity::Explicit(pubkey)
                if existing_author.is_some_and(|author| author != pubkey) =>
            {
                return PublishPreparation::Complete(self.refuse_publish(
                    PublishError::IdentityContradictsSignedAuthor {
                        identity: pubkey,
                        author: existing_author.expect("mismatching existing author"),
                    },
                ));
            }
            Identity::Explicit(pubkey) => pubkey,
            Identity::Active => match self.active_pubkey {
                Some(pubkey)
                    if existing_author.is_none_or(|existing_author| existing_author == pubkey) =>
                {
                    pubkey
                }
                Some(pubkey) => {
                    return PublishPreparation::Complete(self.refuse_publish(
                        PublishError::IdentityContradictsSignedAuthor {
                            identity: pubkey,
                            author: existing_author.expect("mismatching existing author"),
                        },
                    ));
                }
                None => {
                    return PublishPreparation::Complete(
                        self.refuse_publish(PublishError::NoCurrentAccount),
                    )
                }
            },
        };
        let (kind, identifier) = match &start {
            nmp_grammar::ReplaceableOperationStart::Existing {
                original_source, ..
            } => (
                original_source.kind,
                original_source.tags.identifier().unwrap_or("").to_owned(),
            ),
            nmp_grammar::ReplaceableOperationStart::CapabilityDefault { kind, identifier } => {
                (*kind, identifier.clone())
            }
        };
        if kind == nostr::Kind::Authentication {
            return PublishPreparation::Complete(self.refuse_publish(PublishError::ReservedKind {
                kind: kind.as_u16(),
            }));
        }

        let coordinate = Coordinate {
            kind,
            public_key: signing_pubkey,
            identifier,
        };
        let snapshot = match self.store.replaceable_operation_snapshot(&coordinate) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return PublishPreparation::Complete(self.refuse_publish(
                    PublishError::PersistenceFailed {
                        reason: error.to_string(),
                    },
                ));
            }
        };

        if let nmp_grammar::ReplaceableOperationStart::Existing {
            original_source,
            current,
        } = &start
        {
            let supplied_current_id = current
                .id
                .expect("registered operation validation froze a current id");
            let expected_current_id = snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.current.generation.as_ref())
                .map(|generation| generation.materialization.event_id)
                .unwrap_or_else(|| {
                    original_source
                        .id
                        .expect("registered operation validation froze a source id")
                });
            if supplied_current_id != expected_current_id {
                return PublishPreparation::Complete(
                    self.refuse_publish(PublishError::ReplaceableOperationRefused {
                        reason: "operation was composed over a stale current materialization"
                            .to_string(),
                    }),
                );
            }
        }
        if snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .operations
                .iter()
                .any(|operation| operation.program != program || operation.format != format)
        }) {
            return PublishPreparation::Complete(self.refuse_publish(
                PublishError::ReplaceableOperationRefused {
                    reason: "replaceable coordinate uses another replay program".to_string(),
                },
            ));
        }
        let mut replay_operations = snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .operations
                    .iter()
                    .map(|operation| operation.plan.bytes().to_vec())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        replay_operations.push(operation_bytes.clone());
        let canonical_source =
            match self.canonical_replaceable_source(&coordinate, &declared_source_policy) {
                Ok(source) => source,
                Err(error) => {
                    return PublishPreparation::Complete(self.refuse_publish(
                        PublishError::PersistenceFailed {
                            reason: error.to_string(),
                        },
                    ));
                }
            };
        let input = match self.replaceable_materialization_input(
            &start,
            &coordinate,
            snapshot.as_ref(),
            canonical_source.as_ref(),
        ) {
            Ok(input) => input,
            Err(effects) => return PublishPreparation::Complete(effects),
        };
        let fence = Self::replaceable_materialization_fence(snapshot.as_ref());
        let canonical_source_id = canonical_source.map(|stored| stored.event.id);
        PublishPreparation::Materialize(Box::new(PreparedReplaceableMaterialization {
            call: ReplaceableMaterializationCall {
                materializer: materializer.clone(),
                input,
                operations: replay_operations,
            },
            continuation: ReplaceableMaterializationContinuation {
                program,
                format,
                materializer,
                start,
                declared_source_policy,
                operation_bytes,
                signing_pubkey,
                coordinate,
                routing,
                correlation,
                fence,
                canonical_source_id,
            },
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn retry_body_complete_replaceable_operation(
        &mut self,
        program: ReplayProgramId,
        format: ReplayFormatId,
        materializer: Arc<dyn crate::ReplaceableMaterializer>,
        start: nmp_grammar::ReplaceableOperationStart,
        declared_source_policy: nmp_grammar::ReplaceableSourcePolicy,
        operation_bytes: Vec<u8>,
        signing_pubkey: PublicKey,
        coordinate: Coordinate,
        routing: WriteRouting,
        correlation: Option<nmp_grammar::CorrelationToken>,
        snapshot: Option<nmp_store::RecoveredSemanticResource>,
    ) -> PublishPreparation {
        if snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .operations
                .iter()
                .any(|operation| operation.program != program || operation.format != format)
        }) {
            return PublishPreparation::Complete(self.refuse_publish(
                PublishError::ReplaceableOperationRefused {
                    reason: "replaceable coordinate uses another replay program".to_string(),
                },
            ));
        }
        let mut operations = snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .operations
                    .iter()
                    .map(|operation| operation.plan.bytes().to_vec())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        operations.push(operation_bytes.clone());
        let canonical_source =
            match self.canonical_replaceable_source(&coordinate, &declared_source_policy) {
                Ok(source) => source,
                Err(error) => {
                    return PublishPreparation::Complete(self.refuse_publish(
                        PublishError::PersistenceFailed {
                            reason: error.to_string(),
                        },
                    ));
                }
            };
        let input = match self.replaceable_materialization_input(
            &start,
            &coordinate,
            snapshot.as_ref(),
            canonical_source.as_ref(),
        ) {
            Ok(input) => input,
            Err(effects) => return PublishPreparation::Complete(effects),
        };
        let fence = Self::replaceable_materialization_fence(snapshot.as_ref());
        let canonical_source_id = canonical_source.map(|stored| stored.event.id);
        PublishPreparation::Materialize(Box::new(PreparedReplaceableMaterialization {
            call: ReplaceableMaterializationCall {
                materializer: materializer.clone(),
                input,
                operations,
            },
            continuation: ReplaceableMaterializationContinuation {
                program,
                format,
                materializer,
                start,
                declared_source_policy,
                operation_bytes,
                signing_pubkey,
                coordinate,
                routing,
                correlation,
                fence,
                canonical_source_id,
            },
        }))
    }

    fn replaceable_materialization_input(
        &mut self,
        start: &nmp_grammar::ReplaceableOperationStart,
        coordinate: &Coordinate,
        snapshot: Option<&nmp_store::RecoveredSemanticResource>,
        canonical_source: Option<&nmp_store::StoredEvent>,
    ) -> Result<ReplaceableMaterializationInput, Vec<Effect>> {
        let retained_default = snapshot
            .and_then(|snapshot| snapshot.operations.first())
            .is_some_and(|operation| {
                matches!(
                    &operation.source_requirement,
                    nmp_store::OperationSourceRequirement::CapabilityDefault(_)
                )
            });
        if snapshot.is_some_and(|snapshot| snapshot.source.is_none()) && retained_default {
            return Ok(ReplaceableMaterializationInput::CapabilityDefault {
                coordinate: coordinate.clone(),
            });
        }
        if snapshot.is_none()
            && canonical_source.is_none()
            && matches!(
                start,
                nmp_grammar::ReplaceableOperationStart::CapabilityDefault { .. }
            )
        {
            return Ok(ReplaceableMaterializationInput::CapabilityDefault {
                coordinate: coordinate.clone(),
            });
        }
        let source = snapshot
            .and_then(|snapshot| snapshot.source.as_ref())
            .map(|stored| UnsignedEvent::from(stored.event.clone()))
            .or_else(|| canonical_source.map(|stored| UnsignedEvent::from(stored.event.clone())))
            .or_else(|| match start {
                nmp_grammar::ReplaceableOperationStart::Existing {
                    original_source, ..
                } => Some((**original_source).clone()),
                nmp_grammar::ReplaceableOperationStart::CapabilityDefault { .. } => None,
            });
        let Some(source) = source else {
            return Err(
                self.refuse_publish(PublishError::ReplaceableOperationRefused {
                    reason: "complete original source is no longer retained".to_string(),
                }),
            );
        };
        let current_id = snapshot
            .and_then(|snapshot| snapshot.current.generation.as_ref())
            .map(|generation| generation.materialization.event_id)
            .or(source.id)
            .expect("registered operation retained a complete source id");
        let current = if source.id == Some(current_id) {
            source.clone()
        } else {
            let rows = self
                .store
                .query(&nostr::Filter::new().id(current_id))
                .map_err(|error| {
                    self.refuse_publish(PublishError::PersistenceFailed {
                        reason: error.to_string(),
                    })
                })?;
            let Some(stored) = rows
                .into_iter()
                .find(|stored| stored.event.id == current_id)
            else {
                return Err(
                    self.refuse_publish(PublishError::ReplaceableOperationRefused {
                        reason: "complete current materialization is no longer retained"
                            .to_string(),
                    }),
                );
            };
            UnsignedEvent::from(stored.event)
        };
        Ok(ReplaceableMaterializationInput::Existing {
            source: Box::new(source),
            current: Box::new(current),
        })
    }

    pub(super) fn replaceable_materialization_fence(
        snapshot: Option<&nmp_store::RecoveredSemanticResource>,
    ) -> ReplaceableMaterializationFence {
        ReplaceableMaterializationFence {
            source_revision: snapshot.map(|snapshot| snapshot.current.source_revision.clone()),
            program_digest: snapshot.map(|snapshot| snapshot.current.program_digest),
            current_materialization: snapshot.and_then(|snapshot| {
                snapshot
                    .current
                    .generation
                    .as_ref()
                    .map(|generation| generation.materialization.materialization_id)
            }),
            operations: snapshot
                .map(|snapshot| snapshot.operations.clone())
                .unwrap_or_default(),
        }
    }

    fn canonical_replaceable_source(
        &self,
        coordinate: &Coordinate,
        source_policy: &nmp_grammar::ReplaceableSourcePolicy,
    ) -> Result<Option<nmp_store::StoredEvent>, nmp_store::PersistenceError> {
        let mut filter = nostr::Filter::new()
            .kind(coordinate.kind)
            .author(coordinate.public_key);
        if coordinate.kind.is_addressable() {
            filter = filter.identifier(coordinate.identifier.clone());
        }
        Ok(self
            .store
            .query_newest(&filter, 1)?
            .into_iter()
            .next()
            .filter(|stored| match source_policy {
                nmp_grammar::ReplaceableSourcePolicy::Continuing => {
                    !stored.provenance.seen.is_empty()
                }
                nmp_grammar::ReplaceableSourcePolicy::Finite { relays, .. } => stored
                    .provenance
                    .seen
                    .keys()
                    .any(|relay| relays.contains(relay)),
            }))
    }

    pub(crate) fn complete_body_complete_replaceable_operation(
        &mut self,
        continuation: ReplaceableMaterializationContinuation,
        outcome: ReplaceableMaterializationOutcome,
    ) -> PublishPreparation {
        self.complete_replaceable_materialization(continuation, outcome)
    }

    fn complete_replaceable_materialization(
        &mut self,
        continuation: ReplaceableMaterializationContinuation,
        outcome: ReplaceableMaterializationOutcome,
    ) -> PublishPreparation {
        let ReplaceableMaterializationContinuation {
            program,
            format,
            materializer,
            start,
            declared_source_policy,
            operation_bytes,
            signing_pubkey,
            coordinate,
            routing,
            correlation,
            fence,
            canonical_source_id,
        } = continuation;
        // The first correlation lookup happened before this capability call
        // left the reducer. Another publish can enter custody while the call
        // is blocked, so repeat the exact replay door before consulting stale
        // callback output or accepting a second obligation.
        if let Some(token) = correlation.as_ref() {
            match self.replay_correlated_publish(token, None) {
                Ok(Some(effects)) => return PublishPreparation::Complete(effects),
                Ok(None) => {}
                Err(error) => {
                    return PublishPreparation::Complete(self.refuse_publish(error));
                }
            }
        }
        let Some(registration) = self.replaceable_materializers.get(&(program.0, format.0)) else {
            return PublishPreparation::Complete(self.refuse_publish(
                PublishError::ReplaceableOperationRefused {
                    reason: "replaceable materializer is not configured".to_string(),
                },
            ));
        };
        if !Arc::ptr_eq(&registration.materializer, &materializer) {
            return PublishPreparation::Complete(self.refuse_publish(
                PublishError::ReplaceableOperationRefused {
                    reason: "replaceable materializer is not configured".to_string(),
                },
            ));
        }
        let snapshot = match self.store.replaceable_operation_snapshot(&coordinate) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return PublishPreparation::Complete(self.refuse_publish(
                    PublishError::PersistenceFailed {
                        reason: error.to_string(),
                    },
                ));
            }
        };
        let canonical_source =
            match self.canonical_replaceable_source(&coordinate, &declared_source_policy) {
                Ok(source) => source,
                Err(error) => {
                    return PublishPreparation::Complete(self.refuse_publish(
                        PublishError::PersistenceFailed {
                            reason: error.to_string(),
                        },
                    ));
                }
            };
        if canonical_source.as_ref().map(|stored| stored.event.id) != canonical_source_id
            || Self::replaceable_materialization_fence(snapshot.as_ref()) != fence
        {
            return self.retry_body_complete_replaceable_operation(
                program,
                format,
                materializer,
                start,
                declared_source_policy,
                operation_bytes,
                signing_pubkey,
                coordinate,
                routing,
                correlation,
                snapshot,
            );
        }
        let builder = match outcome {
            ReplaceableMaterializationOutcome::Materialized(builder) => builder,
            ReplaceableMaterializationOutcome::Refused(reason) => {
                return PublishPreparation::Complete(
                    self.refuse_publish(PublishError::ReplaceableOperationRefused { reason }),
                );
            }
        };
        if builder.kind != coordinate.kind
            || nostr::Tags::from_list(builder.tags.clone())
                .identifier()
                .unwrap_or("")
                != coordinate.identifier
        {
            return PublishPreparation::Complete(self.refuse_publish(
                PublishError::ReplaceableOperationRefused {
                    reason: "materializer changed the replaceable coordinate".to_string(),
                },
            ));
        }

        let existing_source = match &start {
            nmp_grammar::ReplaceableOperationStart::Existing {
                original_source, ..
            } => Some((**original_source).clone()),
            nmp_grammar::ReplaceableOperationStart::CapabilityDefault { .. } => canonical_source
                .as_ref()
                .map(|stored| UnsignedEvent::from(stored.event.clone())),
        };
        let source_event_id = existing_source.as_ref().and_then(|source| source.id);
        let source_plan = SourcePlanId(
            *blake3::hash(&[b"nmp-exact-source-v1".as_slice(), program.0.as_slice()].concat())
                .as_bytes(),
        );
        let source_access = match &declared_source_policy {
            nmp_grammar::ReplaceableSourcePolicy::Continuing
            | nmp_grammar::ReplaceableSourcePolicy::Finite {
                access: AccessContext::Public,
                ..
            } => AccessContextId(
                *blake3::hash(&[b"nmp-public-source-v1".as_slice(), format.0.as_slice()].concat())
                    .as_bytes(),
            ),
            nmp_grammar::ReplaceableSourcePolicy::Finite {
                access: AccessContext::Nip42(pubkey),
                ..
            } => {
                let mut hasher = blake3::Hasher::new();
                hasher.update(b"nmp-source-access-v1");
                hasher.update(&[1]);
                hasher.update(pubkey.as_bytes());
                AccessContextId(*hasher.finalize().as_bytes())
            }
        };
        let source = snapshot
            .as_ref()
            .map(|snapshot| snapshot.current.source_revision.evidence().clone())
            .unwrap_or_else(|| SemanticSourceEvidence {
                plan: source_plan,
                access: source_access,
                qualified: existing_source
                    .as_ref()
                    .map_or(QualifiedSource::Unresolved, |source| {
                        QualifiedSource::Event {
                            event_id: source_event_id
                                .expect("registered existing source retained a complete id"),
                            created_at: source.created_at,
                        }
                    }),
            });
        let starting_source = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.operations.first())
            .map(|operation| match &operation.source_requirement {
                nmp_store::OperationSourceRequirement::Awaiting(requirement)
                | nmp_store::OperationSourceRequirement::Qualified(requirement)
                | nmp_store::OperationSourceRequirement::CapabilityDefault(requirement) => {
                    StartingSourceRequirement {
                        plan: requirement.plan,
                        access: requirement.access,
                        source: match source.qualified {
                            QualifiedSource::Event { event_id, .. } => {
                                StartingSource::Event(event_id)
                            }
                            QualifiedSource::Absent => StartingSource::Absent,
                            QualifiedSource::Unresolved => requirement.source,
                        },
                    }
                }
            })
            .unwrap_or(StartingSourceRequirement {
                plan: source_plan,
                access: source_access,
                source: match source.qualified {
                    QualifiedSource::Event { event_id, .. } => StartingSource::Event(event_id),
                    QualifiedSource::Absent => StartingSource::Absent,
                    QualifiedSource::Unresolved => source_event_id
                        .map_or(StartingSource::CapabilityDefault, StartingSource::Event),
                },
            });
        let initial_source_matches = match (source_event_id, source.qualified) {
            (Some(expected), QualifiedSource::Event { event_id, .. }) => expected == event_id,
            (None, QualifiedSource::Unresolved) => true,
            _ => false,
        };
        if snapshot.is_none() && !initial_source_matches {
            return PublishPreparation::Complete(self.refuse_publish(
                PublishError::ReplaceableOperationRefused {
                    reason: "original source does not match retained source evidence".to_string(),
                },
            ));
        }
        let source_event = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.clone())
            .or_else(|| canonical_source.clone())
            .or_else(|| {
                snapshot.is_none().then_some(())?;
                source_event_id.and_then(|source_event_id| {
                    self.store
                        .query(&nostr::Filter::new().id(source_event_id))
                        .ok()
                        .and_then(|rows| {
                            rows.into_iter()
                                .find(|stored| stored.event.id == source_event_id)
                        })
                })
            });
        if matches!(source.qualified, QualifiedSource::Event { .. }) && source_event.is_none() {
            return PublishPreparation::Complete(self.refuse_publish(
                PublishError::ReplaceableOperationRefused {
                    reason: "complete original source is no longer retained".to_string(),
                },
            ));
        };

        let source_floor = match source.qualified {
            QualifiedSource::Event { created_at, .. } => created_at.as_secs().saturating_add(1),
            QualifiedSource::Absent | QualifiedSource::Unresolved => 0,
        };
        let prior_floor = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.current.generation.as_ref())
            .map(|generation| generation.created_at.as_secs().saturating_add(1))
            .unwrap_or(0);
        let created_at = Timestamp::from(self.clock.as_secs().max(source_floor).max(prior_floor));
        let mut event = UnsignedEvent::new(
            signing_pubkey,
            created_at,
            builder.kind,
            builder.tags,
            builder.content,
        );
        event.ensure_id();
        let plan = match SemanticPlan::new(1, operation_bytes) {
            Ok(plan) => plan,
            Err(reason) => {
                return PublishPreparation::Complete(self.refuse_publish(
                    PublishError::ReplaceableOperationRefused {
                        reason: reason.to_string(),
                    },
                ));
            }
        };
        let (expected_source_revision, expected_program_digest, expected_materialization) =
            snapshot
                .as_ref()
                .map(|snapshot| {
                    (
                        Some(snapshot.current.source_revision.clone()),
                        Some(snapshot.current.program_digest),
                        snapshot
                            .current
                            .generation
                            .as_ref()
                            .map(|generation| generation.materialization.materialization_id),
                    )
                })
                .unwrap_or((None, None, None));
        let contributing_operations = snapshot
            .as_ref()
            .map(|snapshot| snapshot.operations.iter().map(|op| op.intent_id).collect())
            .unwrap_or_default();
        let source_policy = match (snapshot.as_ref(), declared_source_policy) {
            (Some(snapshot), nmp_grammar::ReplaceableSourcePolicy::Continuing)
                if matches!(
                    snapshot.source_policy,
                    nmp_store::SemanticSourcePolicy::Continuing
                ) =>
            {
                snapshot.source_policy.clone()
            }
            (Some(snapshot), nmp_grammar::ReplaceableSourcePolicy::Finite { relays, access })
                if matches!(&snapshot.source_policy, nmp_store::SemanticSourcePolicy::Finite(round)
                    if round.sources.keys().cloned().collect::<BTreeSet<_>>()
                        == relays.iter().cloned().map(|relay| nmp_store::SemanticSource::new(relay, access)).collect()) =>
            {
                snapshot.source_policy.clone()
            }
            (Some(_), _) => {
                return PublishPreparation::Complete(self.refuse_publish(
                    PublishError::ReplaceableOperationRefused {
                        reason: nmp_store::SemanticRefusal::IncompatibleSourcePolicy.to_string(),
                    },
                ));
            }
            (None, nmp_grammar::ReplaceableSourcePolicy::Continuing) => {
                nmp_store::SemanticSourcePolicy::Continuing
            }
            (None, nmp_grammar::ReplaceableSourcePolicy::Finite { relays, access }) => {
                let mut hasher = blake3::Hasher::new();
                hasher.update(b"nmp-finite-source-round-v1");
                hasher.update(&coordinate.kind.as_u16().to_be_bytes());
                hasher.update(coordinate.public_key.as_bytes());
                hasher.update(&(coordinate.identifier.len() as u64).to_be_bytes());
                hasher.update(coordinate.identifier.as_bytes());
                hasher.update(source_plan.0.as_slice());
                hasher.update(source_access.0.as_slice());
                for relay in &relays {
                    let relay = relay.as_str().as_bytes();
                    hasher.update(&(relay.len() as u64).to_be_bytes());
                    hasher.update(relay);
                }
                let sources = relays
                    .into_iter()
                    .map(|relay| nmp_store::SemanticSource::new(relay, access))
                    .collect();
                nmp_store::SemanticSourcePolicy::Finite(
                    nmp_store::FiniteSemanticSourceRound::new(
                        nmp_store::SourceRoundId(*hasher.finalize().as_bytes()),
                        sources,
                    )
                    .expect("registered operation rejects an empty finite source set"),
                )
            }
        };
        let accept = AcceptWrite {
            payload: AcceptWritePayload::ReplaceableOperation(Box::new(SemanticAccept {
                coordinate: coordinate.clone(),
                program,
                format,
                expected_source_revision,
                expected_program_digest,
                expected_current_materialization: expected_materialization,
                starting_source,
                source,
                source_policy,
                source_event,
                plan,
                materialized: Some(MaterializationCandidate {
                    event,
                    routing: Self::routing_snapshot(&routing),
                    sig_state: PendingMaterializationState::AwaitingSigner,
                }),
                contributing_operations,
                resolved_operations: Vec::new(),
            })),
            expected_pubkey: signing_pubkey,
            signing_identity_ref: signing_pubkey.to_hex(),
            accepted_at: self.clock,
            correlation,
        };
        let LocalAcceptResult { outcome, committed } =
            match self.resolver.accept_local(&mut self.store, accept) {
                Ok(result) => result,
                Err(error) => {
                    let mut effects = self.refuse_publish(PublishError::PersistenceFailed {
                        reason: error.to_string(),
                    });
                    self.degrade_store(error, &mut effects);
                    return PublishPreparation::Complete(effects);
                }
            };
        let (intent_id, receipt_id, current, installed) = match outcome {
            AcceptOutcome::ReplaceableOperation {
                intent_id,
                receipt_id,
                current,
                installed: Some(installed),
                ..
            } => (intent_id, ReceiptId(receipt_id), current, installed),
            AcceptOutcome::ReplaceableOperationRefused(reason) => {
                return PublishPreparation::Complete(self.refuse_publish(
                    PublishError::ReplaceableOperationRefused {
                        reason: reason.to_string(),
                    },
                ));
            }
            AcceptOutcome::ReplaceableOperation {
                installed: None, ..
            } => {
                return PublishPreparation::Complete(self.refuse_publish(
                    PublishError::PersistenceFailed {
                        reason: "body-complete acceptance produced no canonical row".to_string(),
                    },
                ));
            }
            _ => unreachable!("semantic acceptance returns a semantic outcome"),
        };
        let frozen = installed.event;
        let mut effects = vec![Effect::WriteAccepted(receipt_id, frozen.id)];
        let members = current
            .generation
            .as_ref()
            .map(|generation| generation.members.clone())
            .unwrap_or_default();
        let parked_target =
            PendingWriteTarget::ReplaceableOperation(Box::new(ReplaceableMaterializationTarget {
                coordinate: coordinate.clone(),
                expected_source_revision: current.source_revision.clone(),
                expected_program_digest: current.program_digest,
                expected_materialization: current
                    .generation
                    .as_ref()
                    .expect("body-complete current has a generation")
                    .materialization
                    .materialization_id,
                expected_event_id: frozen.id,
            }));
        for member in members {
            let member_receipt = if member == intent_id {
                Some(receipt_id)
            } else {
                self.intent_receipts.get(&member).copied()
            };
            let Some(member_receipt) = member_receipt else {
                continue;
            };
            if let Some(previous) = self.pending.get_mut(&member_receipt) {
                if let Some(receipts) = self.event_to_receipts.get_mut(&previous.frozen.id) {
                    receipts.remove(&member_receipt);
                }
                previous.frozen = frozen.clone();
                previous.target = parked_target.clone();
                previous.already_signed = false;
                previous.sign_request_in_flight = false;
                previous.sign_generation = previous.sign_generation.saturating_add(1);
                previous.event_id = None;
                previous.pending_relays.clear();
                previous.unstarted_relays.clear();
                previous.route_blocked_relays.clear();
                previous.attempt_ordinals.clear();
                previous.lane_projection = LaneWorkerProjection::default();
                previous.durable_routes.clear();
                previous.route_complete = false;
                previous.destinations_reported = false;
                previous.route_needs.clear();
            } else {
                self.pending.insert(
                    member_receipt,
                    PendingWrite {
                        target: parked_target.clone(),
                        routing: routing.clone(),
                        routing_valid: true,
                        intent_id: member,
                        accepted_at: self.clock,
                        signing_pubkey,
                        frozen: frozen.clone(),
                        already_signed: false,
                        sign_request_in_flight: false,
                        sign_generation: 0,
                        event_id: None,
                        pending_relays: BTreeSet::new(),
                        unstarted_relays: BTreeSet::new(),
                        route_blocked_relays: BTreeSet::new(),
                        attempt_ordinals: BTreeMap::new(),
                        lane_projection: LaneWorkerProjection::default(),
                        durable_routes: BTreeSet::new(),
                        route_complete: false,
                        destinations_reported: false,
                        persistence_fault: None,
                        route_needs: BTreeSet::new(),
                    },
                );
                self.intent_receipts.insert(member, member_receipt);
            }
            self.event_to_receipts
                .entry(frozen.id)
                .or_default()
                .insert(member_receipt);
        }
        if let Some(owner) = current
            .generation
            .as_ref()
            .and_then(|generation| generation.members.first())
            .and_then(|owner| self.intent_receipts.get(owner))
            .copied()
        {
            if let Some(pending) = self.pending.get_mut(&owner) {
                if !pending.sign_request_in_flight && !pending.already_signed {
                    pending.sign_request_in_flight = true;
                    pending.sign_generation = pending.sign_generation.saturating_add(1);
                    effects.push(Effect::RequestSign(
                        owner,
                        pending.sign_generation,
                        unsigned_from_frozen(&pending.frozen),
                    ));
                }
            }
        }
        self.apply_committed_mutation(committed, &mut effects);
        self.sync_semantic_source_owners(&coordinate, &mut effects);
        PublishPreparation::Complete(effects)
    }
}
