use super::*;

impl EngineCore {
    pub(super) fn on_body_complete_replaceable_operation(
        &mut self,
        operation: nmp_grammar::ReplaceableOperation,
        routing: WriteRouting,
        identity: Identity,
        correlation: Option<nmp_grammar::CorrelationToken>,
    ) -> Vec<Effect> {
        let (instance, original_source, supplied_current, declared_source_policy, operation_bytes) =
            operation.into_registered_parts();
        let Some(registration) = self.replaceable_materializers.get(&instance) else {
            return self.refuse_publish(PublishError::ReplaceableOperationRefused {
                reason: "replaceable materializer registration is not active".to_string(),
            });
        };
        let program = ReplayProgramId(registration.program);
        let format = ReplayFormatId(registration.format);
        let materializer = registration.materializer.clone();

        let signing_pubkey = match identity {
            Identity::Explicit(pubkey) if pubkey != original_source.pubkey => {
                return self.refuse_publish(PublishError::IdentityContradictsSignedAuthor {
                    identity: pubkey,
                    author: original_source.pubkey,
                });
            }
            Identity::Explicit(pubkey) => pubkey,
            Identity::Active => match self.active_pubkey {
                Some(pubkey) if pubkey == original_source.pubkey => pubkey,
                Some(pubkey) => {
                    return self.refuse_publish(PublishError::IdentityContradictsSignedAuthor {
                        identity: pubkey,
                        author: original_source.pubkey,
                    });
                }
                None => return self.refuse_publish(PublishError::NoCurrentAccount),
            },
        };
        if original_source.kind == nostr::Kind::Authentication {
            return self.refuse_publish(PublishError::ReservedKind {
                kind: original_source.kind.as_u16(),
            });
        }

        let coordinate = Coordinate {
            kind: original_source.kind,
            public_key: original_source.pubkey,
            identifier: original_source.tags.identifier().unwrap_or("").to_owned(),
        };
        let snapshot = match self.store.replaceable_operation_snapshot(&coordinate) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return self.refuse_publish(PublishError::PersistenceFailed {
                    reason: error.to_string(),
                });
            }
        };

        let supplied_current_id = supplied_current
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
            return self.refuse_publish(PublishError::ReplaceableOperationRefused {
                reason: "operation was composed over a stale current materialization".to_string(),
            });
        }
        if snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .operations
                .iter()
                .any(|operation| operation.program != program || operation.format != format)
        }) {
            return self.refuse_publish(PublishError::ReplaceableOperationRefused {
                reason: "replaceable coordinate uses another replay program".to_string(),
            });
        }
        let replay_source = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.as_ref())
            .map(|stored| UnsignedEvent::from(stored.event.clone()))
            .unwrap_or_else(|| original_source.clone());

        let mut replay_operations = snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .operations
                    .iter()
                    .map(|operation| ReplaceableMaterializerOperation::new(operation.plan.bytes()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        replay_operations.push(ReplaceableMaterializerOperation::new(&operation_bytes));
        let builder =
            match materializer.materialize(&replay_source, &supplied_current, &replay_operations) {
                Ok(builder) => builder,
                Err(refusal) => {
                    return self.refuse_publish(PublishError::ReplaceableOperationRefused {
                        reason: refusal.reason,
                    });
                }
            };
        if builder.kind != coordinate.kind
            || nostr::Tags::from_list(builder.tags.clone())
                .identifier()
                .unwrap_or("")
                != coordinate.identifier
        {
            return self.refuse_publish(PublishError::ReplaceableOperationRefused {
                reason: "materializer changed the replaceable coordinate".to_string(),
            });
        }

        let source_event_id = original_source
            .id
            .expect("registered operation validation froze a source id");
        let source_plan = SourcePlanId(
            *blake3::hash(&[b"nmp-exact-source-v1".as_slice(), &registration.program].concat())
                .as_bytes(),
        );
        let source_access = match &declared_source_policy {
            nmp_grammar::ReplaceableSourcePolicy::Continuing
            | nmp_grammar::ReplaceableSourcePolicy::Finite {
                access: AccessContext::Public,
                ..
            } => AccessContextId(
                *blake3::hash(&[b"nmp-public-source-v1".as_slice(), &registration.format].concat())
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
            .unwrap_or(SemanticSourceEvidence {
                plan: source_plan,
                access: source_access,
                qualified: QualifiedSource::Event {
                    event_id: source_event_id,
                    created_at: original_source.created_at,
                },
            });
        let starting_source = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.operations.first())
            .map(|operation| match &operation.source_requirement {
                nmp_store::OperationSourceRequirement::Awaiting(requirement)
                | nmp_store::OperationSourceRequirement::Qualified(requirement) => {
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
                source: StartingSource::Event(source_event_id),
            });
        if snapshot.is_none()
            && !matches!(
                source.qualified,
                QualifiedSource::Event { event_id, .. } if event_id == source_event_id
            )
        {
            return self.refuse_publish(PublishError::ReplaceableOperationRefused {
                reason: "original source does not match retained source evidence".to_string(),
            });
        }
        let source_event = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.clone())
            .or_else(|| {
                self.store
                    .query(&nostr::Filter::new().id(source_event_id))
                    .ok()
                    .and_then(|rows| {
                        rows.into_iter()
                            .find(|stored| stored.event.id == source_event_id)
                    })
            });
        let Some(source_event) = source_event else {
            return self.refuse_publish(PublishError::ReplaceableOperationRefused {
                reason: "complete original source is no longer retained".to_string(),
            });
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
                return self.refuse_publish(PublishError::ReplaceableOperationRefused {
                    reason: reason.to_string(),
                });
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
                return self.refuse_publish(PublishError::ReplaceableOperationRefused {
                    reason: nmp_store::SemanticRefusal::IncompatibleSourcePolicy.to_string(),
                });
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
                source_event: Some(source_event),
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
                    return effects;
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
                return self.refuse_publish(PublishError::ReplaceableOperationRefused {
                    reason: reason.to_string(),
                });
            }
            AcceptOutcome::ReplaceableOperation {
                installed: None, ..
            } => {
                return self.refuse_publish(PublishError::PersistenceFailed {
                    reason: "body-complete acceptance produced no canonical row".to_string(),
                });
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
        effects
    }
}
