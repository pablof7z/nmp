//! Replaceable and semantic-source successor installation, including recovery-time reconciliation.

use super::*;

impl CoreState {
    /// Rebuild volatile ownership from the journal without reinserting a
    /// single row. Called by the runtime before its first command and again
    /// after a failed database generation is replaced. Retry clocks are
    /// reconstructed only from persisted lane facts.
    pub(super) fn reconcile_recovered_semantic_sources(
        &mut self,
        recovered: &[nmp_store::PublishQueueIntent],
    ) -> Result<bool, nmp_store::PersistenceError> {
        let mut coordinates = Vec::new();
        for intent in recovered {
            let nmp_store::PublishQueueWork::ReplaceableOperation { coordinate, .. } = &intent.work
            else {
                continue;
            };
            if !coordinates.contains(coordinate) {
                coordinates.push(coordinate.clone());
            }
        }

        let mut changed = false;
        for coordinate in coordinates {
            let Some(snapshot) = self.store.replaceable_operation_snapshot(&coordinate)? else {
                continue;
            };
            let Some(generation) = snapshot.current.generation.as_ref() else {
                continue;
            };
            if !self
                .store
                .query(&nostr::Filter::new().id(generation.materialization.event_id))?
                .is_empty()
            {
                continue;
            }

            let mut filter = nostr::Filter::new()
                .kind(coordinate.kind)
                .author(coordinate.public_key);
            if coordinate.kind.is_addressable() {
                filter = filter.identifier(coordinate.identifier.clone());
            }
            let Some(source) = self.store.query_newest(&filter, 1)?.into_iter().next() else {
                continue;
            };
            if source.provenance.seen.is_empty() {
                continue;
            }

            let Some(first) = snapshot.operations.first() else {
                continue;
            };
            if snapshot.operations.iter().any(|operation| {
                operation.program != first.program || operation.format != first.format
            }) {
                return Err(nmp_store::PersistenceError::new(
                    "one semantic resource retained mixed capability identities",
                ));
            }
            let registration = self
                .replaceable_materializers
                .get(&(first.program.0, first.format.0))
                .ok_or_else(|| {
                    nmp_store::PersistenceError::new(
                        "boot recovery is missing a preflighted replaceable capability",
                    )
                })?;
            let source_unsigned = UnsignedEvent::from(source.event.clone());
            let call = ReplaceableMaterializationCall::new(
                registration.materializer.clone(),
                source_unsigned,
                snapshot
                    .operations
                    .iter()
                    .map(|operation| operation.plan.bytes().to_vec())
                    .collect(),
            );
            let builder = match self.run_replaceable_materialization(call) {
                ReplaceableMaterializationOutcome::Materialized(builder) => builder,
                ReplaceableMaterializationOutcome::Refused(reason) => {
                    return Err(nmp_store::PersistenceError::new(format!(
                        "replaceable capability refused boot reconciliation: {reason}"
                    )))
                }
            };
            if builder.kind != coordinate.kind
                || nostr::Tags::from_list(builder.tags.clone())
                    .identifier()
                    .unwrap_or("")
                    != coordinate.identifier
            {
                return Err(nmp_store::PersistenceError::new(
                    "replaceable capability changed its coordinate during boot reconciliation",
                ));
            }

            let operation_time = snapshot
                .operations
                .iter()
                .map(|operation| operation.accepted_at.as_secs())
                .max()
                .unwrap_or(0);
            let source_time = source
                .event
                .created_at
                .as_secs()
                .checked_add(1)
                .ok_or_else(|| {
                    nmp_store::PersistenceError::new(
                        "source timestamp cannot advance during boot reconciliation",
                    )
                })?;
            let prior_time = generation
                .created_at
                .as_secs()
                .checked_add(1)
                .ok_or_else(|| {
                    nmp_store::PersistenceError::new(
                        "generation timestamp cannot advance during boot reconciliation",
                    )
                })?;
            let mut event = UnsignedEvent::new(
                source.event.pubkey,
                Timestamp::from(operation_time.max(source_time).max(prior_time)),
                builder.kind,
                builder.tags,
                builder.content,
            );
            event.ensure_id();

            let routing = recovered
                .iter()
                .find_map(|intent| match &intent.work {
                    nmp_store::PublishQueueWork::ReplaceableOperation {
                        coordinate: retained_coordinate,
                        materialization: Some(materialization),
                    } if retained_coordinate == &coordinate => {
                        Self::parse_routing_snapshot(&materialization.routing)
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    nmp_store::PersistenceError::new(
                        "semantic boot reconciliation has no readable persisted routing",
                    )
                })?;
            let evidence = snapshot.current.source_revision.evidence();
            let install = SemanticSourceInstall {
                source: source.clone(),
                successor: SemanticRematerialize {
                    coordinate: coordinate.clone(),
                    expected_source_revision: snapshot.current.source_revision.clone(),
                    expected_program_digest: snapshot.current.program_digest,
                    expected_current_materialization: Some(
                        generation.materialization.materialization_id,
                    ),
                    source: SemanticSourceEvidence {
                        plan: evidence.plan,
                        access: evidence.access,
                        qualified: QualifiedSource::Event {
                            event_id: source.event.id,
                            created_at: source.event.created_at,
                        },
                    },
                    evaluated_at: self.clock,
                    materialized: Some(MaterializationCandidate {
                        event,
                        routing: Self::routing_snapshot(&routing),
                        sig_state: PendingMaterializationState::AwaitingSigner,
                    }),
                    contributing_operations: snapshot
                        .operations
                        .iter()
                        .map(|operation| operation.intent_id)
                        .collect(),
                    resolved_operations: Vec::new(),
                },
            };
            match self
                .store
                .install_replaceable_source_materialization(install)?
            {
                nmp_store::SemanticInstallOutcome::Installed { .. } => changed = true,
                nmp_store::SemanticInstallOutcome::Stale => {
                    return Err(nmp_store::PersistenceError::new(
                        "boot source reconciliation lost its exact store fence",
                    ))
                }
                nmp_store::SemanticInstallOutcome::Refused(refusal) => {
                    return Err(nmp_store::PersistenceError::new(format!(
                        "boot source reconciliation was refused: {refusal:?}"
                    )))
                }
                nmp_store::SemanticInstallOutcome::Waiting(_)
                | nmp_store::SemanticInstallOutcome::Resolved => {
                    return Err(nmp_store::PersistenceError::new(
                        "boot source reconciliation returned no complete successor",
                    ))
                }
            }
        }
        Ok(changed)
    }

    // ---- publish queue (D: intent -> signed -> routed -> sent -> acked) --

    /// Prepare a complete successor from one verified relay source, then ask
    /// the store to adopt the source and effective body in one CAS commit.
    /// Returns `true` when this active semantic resource consumed the relay
    /// event (installed or deliberately retained the prior complete value).
    pub(in crate::core) fn install_semantic_source_successor(
        &mut self,
        source: SignedEvent,
        observed: RelayObserved,
        candidate: Option<CommittedObservationCandidate>,
        attribution: Option<(RelaySessionKey, String)>,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let kind = source.kind.as_u16();
        if !matches!(kind, 0 | 3 | 10_000..=19_999 | 30_000..=39_999) {
            return false;
        }
        let coordinate = Coordinate {
            kind: source.kind,
            public_key: source.pubkey,
            identifier: source.tags.identifier().unwrap_or("").to_owned(),
        };
        let snapshot = match self.store.replaceable_operation_snapshot(&coordinate) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return false,
            Err(_error) => {
                return true;
            }
        };
        let Some(first) = snapshot.operations.first() else {
            return false;
        };
        if snapshot
            .current
            .generation
            .as_ref()
            .is_some_and(|generation| generation.materialization.event_id == source.id)
        {
            // A relay echo of the current generation is signature/provenance
            // evidence for that exact generation, not a new semantic base.
            // Let ordinary ingest adopt it and satisfy the existing owners.
            return false;
        }
        if let Some(generation) = snapshot.current.generation.as_ref() {
            for member in &generation.members {
                let attempts = match self.store.recover_attempts(*member) {
                    Ok(attempts) => attempts,
                    Err(_error) => {
                        return true;
                    }
                };
                if attempts.iter().any(|attempt| attempt.event_id == source.id) {
                    // Successor lanes retain predecessor attempts as immutable
                    // delivery history. A relay may replay one of those
                    // previously published materializations after the public
                    // source session reconnects; it is local-generation
                    // evidence, not a new semantic base.
                    return true;
                }
            }
        }
        let members = snapshot
            .current
            .generation
            .as_ref()
            .map(|generation| generation.members.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for member in &members {
            let Some(receipt) = self.pending.receipt_for_intent(*member) else {
                return true;
            };
            let Some(pending) = self.pending.get(&receipt) else {
                return true;
            };
            if pending.intent_id != *member {
                return true;
            }
        }
        let Some(registration) = self
            .replaceable_materializers
            .get(&(first.program.0, first.format.0))
        else {
            effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
            return true;
        };
        let program = ReplayProgramId(registration.program);
        let format = ReplayFormatId(registration.format);
        let materializer = registration.materializer.clone();
        let source_unsigned = UnsignedEvent::from(source.clone());
        let operations = snapshot
            .operations
            .iter()
            .map(|operation| operation.plan.bytes().to_vec())
            .collect::<Vec<_>>();
        let call = ReplaceableMaterializationCall::new(materializer, source_unsigned, operations);
        let outcome = self.run_replaceable_materialization(call);
        self.install_materialized_replaceable_successor(
            ReplaceableSuccessorInput {
                program,
                format,
                coordinate,
                observation: (source, observed, candidate, attribution),
            },
            outcome,
            effects,
        );
        true
    }

    fn install_materialized_replaceable_successor(
        &mut self,
        input: ReplaceableSuccessorInput,
        outcome: ReplaceableMaterializationOutcome,
        effects: &mut Vec<Effect>,
    ) {
        let snapshot = match self.store.replaceable_operation_snapshot(&input.coordinate) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                self.ingest_ordinary_relay_observations(vec![input.observation], effects);
                return;
            }
            Err(_error) => {
                return;
            }
        };
        if snapshot
            .operations
            .iter()
            .any(|operation| operation.program != input.program || operation.format != input.format)
        {
            effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
            return;
        }

        let builder = match outcome {
            ReplaceableMaterializationOutcome::Materialized(builder) => builder,
            ReplaceableMaterializationOutcome::Refused(_) => {
                effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
                return;
            }
        };
        if builder.kind != input.coordinate.kind
            || nostr::Tags::from_list(builder.tags.clone())
                .identifier()
                .unwrap_or("")
                != input.coordinate.identifier
        {
            return;
        }

        let members = snapshot
            .current
            .generation
            .as_ref()
            .map(|generation| generation.members.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut member_receipts = Vec::with_capacity(members.len());
        for member in &members {
            let Some(receipt) = self.pending.receipt_for_intent(*member) else {
                return;
            };
            let Some(pending) = self.pending.get(&receipt) else {
                return;
            };
            if pending.intent_id != *member {
                return;
            }
            member_receipts.push((*member, receipt));
        }

        let (source, observed, _, _) = &input.observation;
        let operation_time = snapshot
            .operations
            .iter()
            .map(|operation| operation.accepted_at.as_secs())
            .max()
            .unwrap_or(0);
        let Some(source_time) = source.created_at.as_secs().checked_add(1) else {
            return;
        };
        let Some(prior_time) = snapshot
            .current
            .generation
            .as_ref()
            .map(|generation| generation.created_at.as_secs().checked_add(1))
            .unwrap_or(Some(0))
        else {
            return;
        };
        let mut event = UnsignedEvent::new(
            source.pubkey,
            Timestamp::from(operation_time.max(source_time).max(prior_time)),
            builder.kind,
            builder.tags,
            builder.content,
        );
        event.ensure_id();
        let evidence = snapshot.current.source_revision.evidence();
        let source_evidence = SemanticSourceEvidence {
            plan: evidence.plan,
            access: evidence.access,
            qualified: QualifiedSource::Event {
                event_id: source.id,
                created_at: source.created_at,
            },
        };
        let routing = snapshot
            .operations
            .iter()
            .find_map(|operation| {
                self.pending
                    .receipt_for_intent(operation.intent_id)
                    .and_then(|receipt| self.pending.get(&receipt))
                    .map(|pending| Self::routing_snapshot(&pending.routing))
            })
            .unwrap_or_else(|| Self::routing_snapshot(&WriteRouting::Auto));
        let mut seen = BTreeMap::new();
        seen.insert(observed.relay.clone(), observed.at);
        let install = SemanticSourceInstall {
            source: nmp_store::StoredEvent {
                event: source.clone(),
                provenance: nmp_store::Provenance { seen, local: None },
            },
            successor: SemanticRematerialize {
                coordinate: input.coordinate.clone(),
                expected_source_revision: snapshot.current.source_revision.clone(),
                expected_program_digest: snapshot.current.program_digest,
                expected_current_materialization: snapshot
                    .current
                    .generation
                    .as_ref()
                    .map(|generation| generation.materialization.materialization_id),
                source: source_evidence,
                evaluated_at: self.clock,
                materialized: Some(MaterializationCandidate {
                    event,
                    routing,
                    sig_state: PendingMaterializationState::AwaitingSigner,
                }),
                contributing_operations: snapshot
                    .operations
                    .iter()
                    .map(|operation| operation.intent_id)
                    .collect(),
                resolved_operations: Vec::new(),
            },
        };
        let installed = match self
            .resolver
            .install_replaceable_source_materialization(&mut self.store, install)
        {
            Ok(installed) => installed,
            Err(_error) => {
                return;
            }
        };
        let nmp_resolver::SemanticInstallResult { outcome, committed } = installed;
        let nmp_store::SemanticInstallOutcome::Installed {
            current, installed, ..
        } = outcome
        else {
            if matches!(outcome, nmp_store::SemanticInstallOutcome::Stale) {
                self.ingest_ordinary_relay_observations(vec![input.observation], effects);
            }
            return;
        };
        let Some(generation) = current.generation.as_ref() else {
            return;
        };
        let Some(delivery_owner) = super::semantic_delivery::MaterializationDeliveryOwner::new(
            generation.materialization,
            generation.members.iter().copied(),
        ) else {
            return;
        };
        debug_assert_eq!(delivery_owner.materialization(), generation.materialization);
        debug_assert_eq!(delivery_owner.members().count(), generation.members.len());
        let target =
            PendingWriteTarget::ReplaceableOperation(Box::new(ReplaceableMaterializationTarget {
                coordinate: input.coordinate.clone(),
                expected_source_revision: current.source_revision,
                expected_program_digest: current.program_digest,
                expected_materialization: generation.materialization.materialization_id,
                expected_event_id: generation.materialization.event_id,
            }));
        for (member, receipt) in member_receipts {
            debug_assert!(generation.members.contains(&member));
            // Releases every relay this member's old generation had
            // persisted lanes on from `receipts_by_lane_relay`, through the
            // one diff `replace_lane_projection` also uses (#1606). Run
            // first, as its own `&mut self` call, because it cannot run
            // while `pending` below holds `self.pending`'s only mutable
            // borrow.
            self.reset_lane_projection_for_successor(receipt);
            // Release the retired generation's bytes through the one door,
            // read and released before taking `pending` because the door
            // borrows all of `self`.
            let old_event_id = self
                .pending
                .get(&receipt)
                .expect("semantic runtime members were preflighted before commit")
                .frozen
                .id;
            self.pending
                .unindex_receipt_from_event(old_event_id, receipt);
            let pending = self
                .pending
                .get_mut(&receipt)
                .expect("semantic runtime members were preflighted before commit");
            pending.frozen = installed.event.clone();
            pending.target = target.clone();
            pending.already_signed = false;
            pending.sign_request_in_flight = false;
            pending.sign_generation = pending.sign_generation.saturating_add(1);
            pending.event_id = None;
            pending.pending_relays.clear();
            pending.attempt_ordinals.clear();
            // `lane_projection` was already reset above.
            self.pending
                .index_receipt_under_event(installed.event.id, receipt);
        }
        let owner_receipt = self
            .pending
            .receipt_for_intent(delivery_owner.physical_owner())
            .expect("semantic delivery owner was runtime-preflighted");
        if let Some(pending) = self.pending.get_mut(&owner_receipt) {
            pending.sign_request_in_flight = true;
            effects.push(Effect::RequestSign(
                owner_receipt,
                pending.sign_generation,
                unsigned_from_frozen(&pending.frozen),
            ));
        }
        self.apply_committed_mutation(committed, effects);
    }

    /// Rebuild the current semantic owner's worker projection when successor
    /// installation already persisted its route union.
    ///
    /// The atomic source transition replaces E1 lanes with E2 lanes and the
    /// reducer clears every member's stale E1 projection. If E2 resolves to
    /// no new route, [`Self::apply_route_answer`] correctly appends nothing;
    /// it therefore has no fresh-lane result from which to rebuild the
    /// projection. Reacquire the already-persisted current lanes after E2 is
    /// signed so worker ownership and connection wakeups cannot disappear
    /// with the predecessor.
    pub(super) fn reacquire_semantic_successor_lanes(&mut self, id: ReceiptId, effects: &mut Vec<Effect>) {
        let Some((intent_id, signing_pubkey, durable_routes, projection_missing)) =
            self.pending.get(&id).map(|pending| {
                (
                    pending.intent_id,
                    pending.signing_pubkey,
                    pending.durable_routes.clone(),
                    pending.lane_projection.persisted.is_empty(),
                )
            })
        else {
            return;
        };
        if !projection_missing || durable_routes.is_empty() {
            return;
        }
        let event_id = self.pending[&id].frozen.id;
        // A failed recovery costs this pass only: the durable lane rows are
        // untouched, and the next engine message re-reads them.
        if let Ok(lanes) = self.recover_semantic_generation_lanes(intent_id, event_id) {
            self.open_fresh_lanes(id, signing_pubkey, lanes, effects);
        }
    }
}
