use std::collections::BTreeMap;

use nostr::nips::nip01::Coordinate;
use nostr::{Event, PublicKey, Timestamp};
use redb::{ReadableDatabase, ReadableTable};

use crate::semantic_edit::{
    plan_accept, plan_rematerialize, recovered, validate_resource_state, SemanticAccept,
    SemanticInstallOutcome, SemanticOperation, SemanticRematerialize, SemanticResourceState,
    SemanticTransitionPlan,
};
use crate::{
    AcceptOutcome, IntentId, LocalOrigin, PersistenceError, PromoteOutcome, Provenance,
    PublishQueueReceiptPayload, ReplaceableOperationReceiptState, SigState, VerifiedSignature,
};

use super::ingest_txn::{GovernedIngestTxn, GovernedWrite, RedbIngestTxn};
use super::mutation::{remove_row_in_txn, tombstone_refuses};
use super::publish_queue::{
    alloc_intent_id_in_txn, alloc_receipt_id_in_txn, PublishQueueIntentRecord,
    PublishQueueIntentRecordWork, PublishQueueMaterializationRecord, PublishQueueReceiptRecord,
};
use super::publish_queue_codec::{
    codec_error, decode_intent, decode_receipt, encode_intent, encode_receipt, intent_key,
    receipt_key,
};
use super::query::expiration_key;
use super::schema::{
    persist_err, PUBLISH_QUEUE_CORRELATIONS, SEMANTIC_MATERIALIZATION_HIGH_WATER,
    SEMANTIC_OPERATIONS, SEMANTIC_RESOURCES,
};
use super::semantic_edit_codec::{
    coordinate_key, decode_operation, decode_resource, encode_operation, encode_resource,
    operation_key, operation_range,
};
use super::store::RedbStore;

fn load_operations(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    coordinate: &Coordinate,
) -> Result<Vec<SemanticOperation>, PersistenceError> {
    let (lower, upper) = operation_range(coordinate)?;
    let mut operations = Vec::new();
    for row in table
        .range::<&[u8]>(lower.as_slice()..=upper.as_slice())
        .map_err(persist_err)?
    {
        let (_key, value) = row.map_err(persist_err)?;
        operations.push(decode_operation(value.value())?);
    }
    Ok(operations)
}

fn load_resource_from_tables(
    resources: &impl ReadableTable<&'static [u8], &'static [u8]>,
    operations: &impl ReadableTable<&'static [u8], &'static [u8]>,
    coordinate: &Coordinate,
) -> Result<Option<SemanticResourceState>, PersistenceError> {
    let key = coordinate_key(coordinate)?;
    let Some(bytes) = resources
        .get(key.as_slice())
        .map_err(persist_err)?
        .map(|value| value.value().to_vec())
    else {
        return Ok(None);
    };
    let mut state = decode_resource(coordinate.clone(), &bytes)?;
    state.operations = load_operations(operations, coordinate)?;
    validate_resource_state(&state)?;
    Ok(Some(state))
}

fn receipt_state(
    update: &crate::semantic_edit::SemanticReceiptUpdate,
    materialization: Option<&PublishQueueMaterializationRecord>,
) -> ReplaceableOperationReceiptState {
    match &update.resolution {
        crate::OperationResolution::Contributing => {
            ReplaceableOperationReceiptState::Contributing {
                current: update.current.and_then(|current| {
                    materialization
                        .filter(|record| record.current == current)
                        .map(|record| crate::MaterializationReceipt {
                            materialization: record.current,
                            sig_state: record.sig_state,
                        })
                }),
            }
        }
        crate::OperationResolution::Resolved => ReplaceableOperationReceiptState::Resolved,
        crate::OperationResolution::Cancelled => ReplaceableOperationReceiptState::Cancelled,
        crate::OperationResolution::Refused(reason) => {
            ReplaceableOperationReceiptState::Refused(reason.clone())
        }
    }
}

fn load_member_records(
    ingest: &RedbIngestTxn<'_, '_>,
    updates: &[crate::semantic_edit::SemanticReceiptUpdate],
    new_intent: Option<IntentId>,
    coordinate: &Coordinate,
) -> Result<
    BTreeMap<IntentId, (PublishQueueIntentRecord, PublishQueueReceiptRecord)>,
    PersistenceError,
> {
    let mut records = BTreeMap::new();
    for update in updates {
        if Some(update.intent_id) == new_intent {
            continue;
        }
        let key = intent_key(update.intent_id);
        let Some(intent) = ingest
            .publish_queue_intents
            .get(&key)
            .map_err(persist_err)?
            .map(|value| value.value().to_vec())
        else {
            return Err(PersistenceError::invariant(format!(
                "semantic member intent {} is missing",
                update.intent_id.0
            )));
        };
        let intent =
            decode_intent(&intent).map_err(|error| codec_error("semantic member intent", error))?;
        if !matches!(
            &intent.work,
            PublishQueueIntentRecordWork::ReplaceableOperation {
                coordinate: journal_coordinate,
                ..
            } if journal_coordinate == coordinate
        ) {
            return Err(PersistenceError::invariant(
                "semantic member journal has wrong work or coordinate",
            ));
        }
        let receipt_key = receipt_key(intent.receipt_id);
        let receipt = ingest
            .publish_queue_receipts
            .get(&receipt_key)
            .map_err(persist_err)?
            .map(|value| value.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("semantic member receipt is missing"))?;
        let receipt = decode_receipt(&receipt)
            .map_err(|error| codec_error("semantic member receipt", error))?;
        if receipt.intent_id != Some(update.intent_id)
            || !matches!(
                &receipt.payload,
                PublishQueueReceiptPayload::ReplaceableOperation {
                    coordinate: receipt_coordinate,
                    ..
                } if receipt_coordinate == coordinate
            )
        {
            return Err(PersistenceError::invariant(
                "semantic member receipt does not match journal",
            ));
        }
        records.insert(update.intent_id, (intent, receipt));
    }
    Ok(records)
}

fn apply_plan(
    ingest: &mut RedbIngestTxn<'_, '_>,
    write_txn: &redb::WriteTransaction,
    coordinate: Coordinate,
    new_intent: Option<(
        IntentId,
        u64,
        PublicKey,
        String,
        Timestamp,
        Option<nmp_grammar::CorrelationToken>,
    )>,
    plan: SemanticTransitionPlan,
) -> Result<SemanticInstallOutcome, PersistenceError> {
    let mut resources = write_txn
        .open_table(SEMANTIC_RESOURCES)
        .map_err(persist_err)?;
    let mut operations = write_txn
        .open_table(SEMANTIC_OPERATIONS)
        .map_err(persist_err)?;
    let mut high_water = write_txn
        .open_table(SEMANTIC_MATERIALIZATION_HIGH_WATER)
        .map_err(persist_err)?;
    let mut correlations = write_txn
        .open_table(PUBLISH_QUEUE_CORRELATIONS)
        .map_err(persist_err)?;

    let new_intent_id = new_intent.as_ref().map(|value| value.0);
    let existing_records =
        load_member_records(ingest, &plan.receipt_updates, new_intent_id, &coordinate)?;
    let coordinate_storage_key = coordinate_key(&coordinate)?;
    let address = crate::address_key::address_key_for_coordinate(&coordinate)
        .expect("semantic planner validates coordinate")
        .to_redb_key();
    let current_winner = match ingest.address_get(&address)? {
        None => None,
        Some(key) => Some((
            key,
            ingest.load_by_key(key)?.ok_or_else(|| {
                PersistenceError::invariant(format!(
                    "semantic address index points at missing canonical event {key}"
                ))
            })?,
        )),
    };
    let predecessor = if let Some(old) = &plan.removed_generation {
        let Some((_key, row)) = current_winner.as_ref() else {
            return Ok(SemanticInstallOutcome::Stale);
        };
        if row.event.id != old.materialization.event_id
            || row
                .provenance
                .local
                .as_ref()
                .is_none_or(|local| local.owners != old.members)
        {
            return Ok(SemanticInstallOutcome::Stale);
        }
        Some(old.materialization.event_id)
    } else if plan.candidate.is_some()
        && plan
            .previous
            .as_ref()
            .and_then(|previous| previous.generation.as_ref())
            .is_none()
    {
        let source = plan
            .next
            .as_ref()
            .expect("candidate has next resource")
            .source_revision
            .evidence()
            .qualified;
        match (source, current_winner.as_ref()) {
            (crate::QualifiedSource::Absent, None) => None,
            (
                crate::QualifiedSource::Event {
                    event_id,
                    created_at,
                },
                Some((_key, row)),
            ) if event_id == row.event.id && created_at == row.event.created_at => Some(event_id),
            _ => return Ok(SemanticInstallOutcome::Stale),
        }
    } else {
        None
    };

    let materialization_record = plan.candidate.as_ref().and_then(|candidate| {
        plan.next
            .as_ref()
            .and_then(|next| next.generation.as_ref())
            .map(|generation| PublishQueueMaterializationRecord {
                current: generation.materialization,
                routing: candidate.routing.clone(),
                sig_state: candidate.sig_state.intent_sig_state(),
            })
    });
    let candidate_event = plan.candidate.as_ref().map(|candidate| {
        let unsigned = &candidate.event;
        Event::new(
            unsigned.id.expect("semantic planner validates event id"),
            unsigned.pubkey,
            unsigned.created_at,
            unsigned.kind,
            unsigned.tags.clone(),
            unsigned.content.clone(),
            crate::sentinel_signature(),
        )
    });
    if let Some(event) = &candidate_event {
        if tombstone_refuses(ingest, event)? {
            return Ok(SemanticInstallOutcome::Refused(
                crate::SemanticRefusal::MaterializationTombstoned,
            ));
        }
        let replacing = plan
            .removed_generation
            .as_ref()
            .map(|old| old.materialization.event_id);
        if ingest.key_for_id(&event.id)?.is_some() && replacing != Some(event.id) {
            return Ok(SemanticInstallOutcome::Refused(
                crate::SemanticRefusal::MaterializationEventIdCollision,
            ));
        }
    }

    if let Some(event_id) = predecessor {
        if plan.removed_generation.is_some() || plan.candidate.is_some() {
            remove_row_in_txn(ingest, event_id, |_| true)?;
        }
    }

    if let (Some(event), Some(generation)) = (
        candidate_event.as_ref(),
        plan.next.as_ref().and_then(|next| next.generation.as_ref()),
    ) {
        let provenance = Provenance::local_origin(LocalOrigin {
            owners: generation.members.clone(),
            sig_state: SigState::Pending,
        });
        let event_key = ingest.insert_new(event, &provenance)?;
        ingest.insert_indexes(event, event_key)?;
        ingest.address_put(&address, event_key)?;
        if let Some(expires_at) = event.tags.expiration().copied() {
            ingest.expiration_put(&expiration_key(expires_at, &event.id), event_key)?;
        }
    }

    let (lower, upper) = operation_range(&coordinate)?;
    let old_keys = operations
        .range::<&[u8]>(lower.as_slice()..=upper.as_slice())
        .map_err(persist_err)?
        .map(|row| {
            row.map(|(key, _)| key.value().to_vec())
                .map_err(persist_err)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for key in old_keys {
        operations.remove(key.as_slice()).map_err(persist_err)?;
    }
    if let Some(next) = &plan.next {
        let encoded = encode_resource(next)?;
        resources
            .insert(coordinate_storage_key.as_slice(), encoded.as_slice())
            .map_err(persist_err)?;
        for operation in &next.operations {
            let key = operation_key(&coordinate, operation.intent_id)?;
            let encoded = encode_operation(operation)?;
            operations
                .insert(key.as_slice(), encoded.as_slice())
                .map_err(persist_err)?;
        }
    } else {
        resources
            .remove(coordinate_storage_key.as_slice())
            .map_err(persist_err)?;
    }
    if let Some(value) = plan.materialization_high_water {
        high_water
            .insert(coordinate_storage_key.as_slice(), value.0)
            .map_err(persist_err)?;
    }

    for update in &plan.receipt_updates {
        let Some((mut intent, mut receipt)) = existing_records.get(&update.intent_id).cloned()
        else {
            continue;
        };
        let prior_materialization = match &intent.work {
            PublishQueueIntentRecordWork::ReplaceableOperation {
                materialization, ..
            } => materialization.clone(),
            PublishQueueIntentRecordWork::Event { .. } => {
                return Err(PersistenceError::invariant(
                    "semantic member has ordinary event journal",
                ))
            }
        };
        let chosen = materialization_record
            .clone()
            .filter(|record| update.current == Some(record.current))
            .or_else(|| {
                prior_materialization.filter(|record| update.current == Some(record.current))
            });
        receipt.payload = PublishQueueReceiptPayload::ReplaceableOperation {
            coordinate: coordinate.clone(),
            state: receipt_state(update, chosen.as_ref()),
        };
        let encoded_receipt = encode_receipt(&receipt);
        ingest
            .publish_queue_receipts
            .insert(&receipt_key(intent.receipt_id), encoded_receipt.as_slice())
            .map_err(persist_err)?;
        if matches!(update.resolution, crate::OperationResolution::Contributing) {
            intent.work = PublishQueueIntentRecordWork::ReplaceableOperation {
                coordinate: coordinate.clone(),
                materialization: chosen,
            };
            let encoded_intent =
                encode_intent(&intent).map_err(|error| codec_error("semantic intent", error))?;
            ingest
                .publish_queue_intents
                .insert(&intent_key(update.intent_id), encoded_intent.as_slice())
                .map_err(persist_err)?;
        } else {
            ingest
                .publish_queue_intents
                .remove(&intent_key(update.intent_id))
                .map_err(persist_err)?;
        }
    }

    if let Some((
        intent_id,
        receipt_id,
        expected_pubkey,
        signing_identity_ref,
        accepted_at,
        correlation,
    )) = new_intent
    {
        let update = plan
            .receipt_updates
            .iter()
            .find(|update| update.intent_id == intent_id)
            .expect("accept planner emits new member");
        let materialization = materialization_record
            .clone()
            .filter(|record| update.current == Some(record.current));
        let intent = PublishQueueIntentRecord {
            receipt_id,
            work: PublishQueueIntentRecordWork::ReplaceableOperation {
                coordinate: coordinate.clone(),
                materialization: materialization.clone(),
            },
            expected_pubkey,
            signing_identity_ref,
            accepted_at,
        };
        let encoded =
            encode_intent(&intent).map_err(|error| codec_error("semantic intent", error))?;
        ingest
            .publish_queue_intents
            .insert(&intent_key(intent_id), encoded.as_slice())
            .map_err(persist_err)?;
        let receipt = PublishQueueReceiptRecord {
            intent_id: Some(intent_id),
            expected_pubkey,
            accepted_at: Some(accepted_at),
            payload: PublishQueueReceiptPayload::ReplaceableOperation {
                coordinate: coordinate.clone(),
                state: receipt_state(update, materialization.as_ref()),
            },
            correlation: correlation.as_ref().map(|token| token.as_ref().to_owned()),
            terminal_sequence: None,
            terminal_at: None,
            terminal_bytes: None,
        };
        let encoded = encode_receipt(&receipt);
        ingest
            .publish_queue_receipts
            .insert(&receipt_key(receipt_id), encoded.as_slice())
            .map_err(persist_err)?;
        if let Some(correlation) = correlation {
            correlations
                .insert(correlation.as_ref().as_bytes(), &receipt_key(receipt_id))
                .map_err(persist_err)?;
        }
    }

    Ok(match plan.next {
        None => SemanticInstallOutcome::Resolved,
        Some(next) => {
            let current = next.current();
            if plan.candidate.is_some() {
                SemanticInstallOutcome::Installed {
                    current,
                    predecessor,
                }
            } else {
                SemanticInstallOutcome::Waiting(current)
            }
        }
    })
}

pub(super) fn accept(
    store: &mut RedbStore,
    expected_pubkey: PublicKey,
    signing_identity_ref: String,
    accepted_at: Timestamp,
    correlation: Option<nmp_grammar::CorrelationToken>,
    accept: SemanticAccept,
) -> Result<AcceptOutcome, PersistenceError> {
    if expected_pubkey != accept.coordinate.public_key {
        return Ok(AcceptOutcome::ReplaceableOperationRefused(
            crate::SemanticRefusal::InvalidCoordinate,
        ));
    }
    let coordinate = accept.coordinate.clone();
    let mut write = GovernedWrite::begin(store)?;
    let outcome = write.apply(|ingest, write_txn| {
        let resources = write_txn
            .open_table(SEMANTIC_RESOURCES)
            .map_err(persist_err)?;
        let operations = write_txn
            .open_table(SEMANTIC_OPERATIONS)
            .map_err(persist_err)?;
        let high_water = write_txn
            .open_table(SEMANTIC_MATERIALIZATION_HIGH_WATER)
            .map_err(persist_err)?;
        let previous = load_resource_from_tables(&resources, &operations, &coordinate)?;
        let key = coordinate_key(&coordinate)?;
        let materialization_high_water = high_water
            .get(key.as_slice())
            .map_err(persist_err)?
            .map(|value| crate::MaterializationId(value.value()));
        drop(high_water);
        drop(operations);
        drop(resources);
        let intent_id = alloc_intent_id_in_txn(&mut ingest.publish_queue_meta)?;
        let receipt_id = alloc_receipt_id_in_txn(&mut ingest.publish_queue_meta)?;
        let plan = match plan_accept(
            previous,
            materialization_high_water,
            intent_id,
            accepted_at,
            accept,
        ) {
            Ok(plan) => plan,
            Err(refusal) => return Ok(AcceptOutcome::ReplaceableOperationRefused(refusal)),
        };
        let current = plan
            .next
            .as_ref()
            .expect("accepted operation remains active")
            .current();
        let installed = apply_plan(
            ingest,
            write_txn,
            coordinate.clone(),
            Some((
                intent_id,
                receipt_id,
                expected_pubkey,
                signing_identity_ref,
                accepted_at,
                correlation,
            )),
            plan,
        )?;
        match installed {
            SemanticInstallOutcome::Stale => {
                return Ok(AcceptOutcome::ReplaceableOperationRefused(
                    crate::SemanticRefusal::InvalidSourceRevision,
                ));
            }
            SemanticInstallOutcome::Refused(refusal) => {
                return Ok(AcceptOutcome::ReplaceableOperationRefused(refusal));
            }
            SemanticInstallOutcome::Installed { .. } | SemanticInstallOutcome::Waiting(_) => {}
            SemanticInstallOutcome::Resolved => {
                return Err(PersistenceError::invariant(
                    "newly accepted replaceable operation cannot already be resolved",
                ));
            }
        }
        Ok(AcceptOutcome::ReplaceableOperation {
            intent_id,
            receipt_id,
            current,
        })
    })?;
    if matches!(outcome, AcceptOutcome::ReplaceableOperationRefused(_)) {
        return Ok(outcome);
    }
    #[cfg(test)]
    store.crash_if(super::store::RedbCrashPoint::SemanticAcceptBeforeCommit);
    write.commit_prepared(outcome)
}

pub(super) fn snapshot(
    store: &RedbStore,
    coordinate: &Coordinate,
) -> Result<Option<crate::RecoveredSemanticResource>, PersistenceError> {
    let read = store.database()?.begin_read().map_err(persist_err)?;
    let resources = match read.open_table(SEMANTIC_RESOURCES) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(error) => return Err(persist_err(error)),
    };
    let operations = read.open_table(SEMANTIC_OPERATIONS).map_err(persist_err)?;
    Ok(load_resource_from_tables(&resources, &operations, coordinate)?.map(recovered))
}

pub(super) fn install(
    store: &mut RedbStore,
    rematerialize: SemanticRematerialize,
) -> Result<SemanticInstallOutcome, PersistenceError> {
    let coordinate = rematerialize.coordinate.clone();
    let mut write = GovernedWrite::begin(store)?;
    let outcome = write.apply(|ingest, write_txn| {
        let resources = write_txn
            .open_table(SEMANTIC_RESOURCES)
            .map_err(persist_err)?;
        let operations = write_txn
            .open_table(SEMANTIC_OPERATIONS)
            .map_err(persist_err)?;
        let Some(previous) = load_resource_from_tables(&resources, &operations, &coordinate)?
        else {
            return Ok(SemanticInstallOutcome::Stale);
        };
        let high_water = write_txn
            .open_table(SEMANTIC_MATERIALIZATION_HIGH_WATER)
            .map_err(persist_err)?;
        let coordinate_key = coordinate_key(&coordinate)?;
        let persisted_high_water = high_water
            .get(coordinate_key.as_slice())
            .map_err(persist_err)?
            .map(|value| crate::MaterializationId(value.value()));
        if previous.last_materialization_id != persisted_high_water {
            return Err(PersistenceError::invariant(
                "semantic materialization high-water diverged from resource",
            ));
        }
        drop(high_water);
        drop(operations);
        drop(resources);
        let plan = match plan_rematerialize(previous, rematerialize) {
            Ok(plan) => plan,
            Err(crate::SemanticRefusal::InvalidSourceRevision) => {
                return Ok(SemanticInstallOutcome::Stale)
            }
            Err(refusal) => return Ok(SemanticInstallOutcome::Refused(refusal)),
        };
        apply_plan(ingest, write_txn, coordinate.clone(), None, plan)
    })?;
    if matches!(
        outcome,
        SemanticInstallOutcome::Stale | SemanticInstallOutcome::Refused(_)
    ) {
        return Ok(outcome);
    }
    #[cfg(test)]
    store.crash_if(super::store::RedbCrashPoint::SemanticRematerializeBeforeCommit);
    write.commit_prepared(outcome)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn promote(
    store: &mut RedbStore,
    coordinate: Coordinate,
    expected_source_revision: crate::SourceRevision,
    expected_program_digest: crate::SemanticProgramDigest,
    expected_materialization: crate::MaterializationId,
    expected_event_id: nostr::EventId,
    verified: VerifiedSignature,
) -> Result<PromoteOutcome, PersistenceError> {
    if verified.event_id() != expected_event_id {
        return Err(PersistenceError::invariant(format!(
            "promotion evidence verifies {} but materialization expects {expected_event_id}",
            verified.event_id()
        )));
    }
    let mut write = GovernedWrite::begin(store)?;
    let outcome = write.apply(|ingest, write_txn| {
        let resources = write_txn
            .open_table(SEMANTIC_RESOURCES)
            .map_err(persist_err)?;
        let operations = write_txn
            .open_table(SEMANTIC_OPERATIONS)
            .map_err(persist_err)?;
        let Some(resource) = load_resource_from_tables(&resources, &operations, &coordinate)?
        else {
            return Ok(PromoteOutcome::Stale);
        };
        let Some(generation) = resource.generation.clone() else {
            return Ok(PromoteOutcome::Stale);
        };
        if resource.source_revision != expected_source_revision
            || crate::semantic_edit::semantic_program_digest(&resource.operations)
                != expected_program_digest
            || generation.materialization.materialization_id != expected_materialization
            || generation.materialization.event_id != expected_event_id
            || generation.source_revision != expected_source_revision
            || generation.program_digest != expected_program_digest
        {
            return Ok(PromoteOutcome::Stale);
        }
        drop(operations);
        drop(resources);

        let address = crate::address_key::address_key_for_coordinate(&coordinate)
            .expect("persisted semantic coordinate is valid")
            .to_redb_key();
        let Some(event_key) = ingest.address_get(&address)? else {
            return Ok(PromoteOutcome::Stale);
        };
        let row = ingest.load_by_key(event_key)?.ok_or_else(|| {
            PersistenceError::invariant(format!(
                "semantic address index points at missing canonical event {event_key}"
            ))
        })?;
        if row.event.id != expected_event_id {
            return Ok(PromoteOutcome::Stale);
        }
        let Some(local) = row.provenance.local.as_ref() else {
            return Err(PersistenceError::invariant(
                "current materialization canonical row is not local",
            ));
        };
        if local.owners != generation.members || local.sig_state == SigState::Signed {
            return Ok(PromoteOutcome::Stale);
        }
        if row.event.sig != crate::sentinel_signature() {
            return Err(PersistenceError::invariant(
                "pending materialization carries a non-sentinel signature",
            ));
        }

        let mut member_records = Vec::with_capacity(generation.members.len());
        for member in &generation.members {
            let key = intent_key(*member);
            let intent_bytes = ingest
                .publish_queue_intents
                .get(&key)
                .map_err(persist_err)?
                .map(|value| value.value().to_vec())
                .ok_or_else(|| PersistenceError::invariant("materialization member missing"))?;
            let intent = decode_intent(&intent_bytes)
                .map_err(|error| codec_error("materialization member", error))?;
            let work = match &intent.work {
                PublishQueueIntentRecordWork::ReplaceableOperation {
                    coordinate: journal_coordinate,
                    materialization: Some(work),
                } if journal_coordinate == &coordinate
                    && work.current.materialization_id == expected_materialization
                    && work.current.event_id == expected_event_id
                    && work.sig_state != crate::IntentSigState::Signed =>
                {
                    work.clone()
                }
                _ => return Ok(PromoteOutcome::Stale),
            };
            let receipt_storage_key = receipt_key(intent.receipt_id);
            let receipt_bytes = ingest
                .publish_queue_receipts
                .get(&receipt_storage_key)
                .map_err(persist_err)?
                .map(|value| value.value().to_vec())
                .ok_or_else(|| PersistenceError::invariant("materialization receipt missing"))?;
            let receipt = decode_receipt(&receipt_bytes)
                .map_err(|error| codec_error("materialization receipt", error))?;
            if receipt.intent_id != Some(*member)
                || receipt.expected_pubkey != intent.expected_pubkey
                || receipt.accepted_at != Some(intent.accepted_at)
                || !matches!(
                    &receipt.payload,
                    PublishQueueReceiptPayload::ReplaceableOperation {
                        coordinate: receipt_coordinate,
                        state: ReplaceableOperationReceiptState::Contributing {
                            current: Some(current),
                        },
                    } if receipt_coordinate == &coordinate
                        && current.materialization.materialization_id == expected_materialization
                        && current.materialization.event_id == expected_event_id
                        && current.sig_state != crate::IntentSigState::Signed
                )
            {
                return Ok(PromoteOutcome::Stale);
            }
            member_records.push((*member, work, intent, receipt));
        }

        let mut event = row.event;
        event.sig = verified.signature();
        let mut local = row
            .provenance
            .local
            .expect("materialization local provenance preflighted");
        local.sig_state = SigState::Signed;
        ingest.replace_event(event_key, &event)?;
        ingest.replace_local(event_key, Some(local.clone()))?;

        for (member, mut work, mut intent, mut receipt) in member_records {
            work.sig_state = crate::IntentSigState::Signed;
            intent.work = PublishQueueIntentRecordWork::ReplaceableOperation {
                coordinate: coordinate.clone(),
                materialization: Some(work.clone()),
            };
            let encoded = encode_intent(&intent)
                .map_err(|error| codec_error("materialization member", error))?;
            ingest
                .publish_queue_intents
                .insert(&intent_key(member), encoded.as_slice())
                .map_err(persist_err)?;
            receipt.payload = PublishQueueReceiptPayload::ReplaceableOperation {
                coordinate: coordinate.clone(),
                state: ReplaceableOperationReceiptState::Contributing {
                    current: Some(crate::MaterializationReceipt {
                        materialization: work.current,
                        sig_state: crate::IntentSigState::Signed,
                    }),
                },
            };
            let encoded = encode_receipt(&receipt);
            ingest
                .publish_queue_receipts
                .insert(&receipt_key(intent.receipt_id), encoded.as_slice())
                .map_err(persist_err)?;
        }
        Ok(PromoteOutcome::MaterializationPromoted {
            row: Box::new(crate::StoredEvent {
                event,
                provenance: Provenance {
                    seen: row.provenance.seen,
                    local: Some(local),
                },
            }),
            members: generation.members.iter().copied().collect(),
        })
    })?;
    if matches!(outcome, PromoteOutcome::Stale | PromoteOutcome::NotFound) {
        return Ok(outcome);
    }
    #[cfg(test)]
    store.crash_if(super::store::RedbCrashPoint::SemanticPromoteBeforeCommit);
    write.commit_prepared(outcome)
}
