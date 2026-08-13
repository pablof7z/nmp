use std::collections::BTreeSet;

use redb::{ReadableDatabase, ReadableTable};

use crate::semantic_edit::{
    plan_accept, plan_rematerialize, promote_current, recovered, validate_active_receipts,
    validate_resource_state, OperationId, SemanticAccept, SemanticCurrentState,
    SemanticEditReceipt, SemanticPromotion, SemanticPromotionOutcome, SemanticRematerialize,
    SemanticResourceState, SemanticStoreError,
};
#[cfg(any(test, feature = "bench-instrumentation"))]
use crate::SemanticStoreCounters;

use super::commit::commit_prepared;
use super::schema::{
    persist_err, SEMANTIC_MATERIALIZATION_HIGH_WATER, SEMANTIC_META, SEMANTIC_OPERATIONS,
    SEMANTIC_RECEIPTS, SEMANTIC_RESOURCES,
};
use super::semantic_edit_codec::{
    coordinate_key, decode_coordinate_key, decode_operation, decode_receipt, decode_resource,
    encode_operation, encode_receipt, encode_resource, operation_id_from_key, operation_key,
    operation_range, receipt_key,
};
#[cfg(test)]
use super::store::RedbCrashPoint;
use super::store::RedbStore;

const NEXT_OPERATION_ID: &str = "next_operation_id";

fn load_write_state(
    resources: &redb::Table<'_, &'static [u8], &'static [u8]>,
    operations: &redb::Table<'_, &'static [u8], &'static [u8]>,
    coordinate: &nostr::nips::nip01::Coordinate,
) -> Result<Option<SemanticResourceState>, SemanticStoreError> {
    let coordinate_encoded = coordinate_key(coordinate)?;
    let Some(resource) = resources
        .get(coordinate_encoded.as_slice())
        .map_err(persist_err)?
    else {
        return Ok(None);
    };
    let mut state = decode_resource(coordinate.clone(), resource.value())?;
    let (lower, upper) = operation_range(coordinate)?;
    for row in operations
        .range(lower.as_slice()..=upper.as_slice())
        .map_err(persist_err)?
    {
        let (key, value) = row.map_err(persist_err)?;
        let operation_id = operation_id_from_key(key.value())?;
        state
            .operations
            .push(decode_operation(operation_id, value.value())?);
    }
    validate_resource_state(&state)?;
    Ok(Some(state))
}

fn load_receipts(
    table: &redb::Table<'_, &'static [u8; 8], &'static [u8]>,
    state: Option<&SemanticResourceState>,
) -> Result<Vec<SemanticEditReceipt>, SemanticStoreError> {
    state
        .into_iter()
        .flat_map(|state| &state.operations)
        .map(|operation| {
            let key = receipt_key(operation.operation_id);
            let value = table
                .get(&key)
                .map_err(persist_err)?
                .ok_or(SemanticStoreError::UnknownOperation(operation.operation_id))?;
            decode_receipt(operation.operation_id, value.value())
        })
        .collect()
}

pub(super) fn accept(
    store: &mut RedbStore,
    accept: SemanticAccept,
) -> Result<(SemanticEditReceipt, SemanticCurrentState), SemanticStoreError> {
    let coordinate = accept.coordinate.clone();
    let write = store.database()?.begin_write().map_err(persist_err)?;
    let (plan, receipt, current) = {
        let mut resources = write.open_table(SEMANTIC_RESOURCES).map_err(persist_err)?;
        let mut operations = write.open_table(SEMANTIC_OPERATIONS).map_err(persist_err)?;
        let mut receipts = write.open_table(SEMANTIC_RECEIPTS).map_err(persist_err)?;
        let mut high_water = write
            .open_table(SEMANTIC_MATERIALIZATION_HIGH_WATER)
            .map_err(persist_err)?;
        let mut meta = write.open_table(SEMANTIC_META).map_err(persist_err)?;

        let previous = load_write_state(&resources, &operations, &coordinate)?;
        let prior_receipts = load_receipts(&receipts, previous.as_ref())?;
        let previous_operation_id = meta
            .get(NEXT_OPERATION_ID)
            .map_err(persist_err)?
            .map(|value| value.value())
            .unwrap_or(0);
        let operation_id = OperationId(
            previous_operation_id
                .checked_add(1)
                .ok_or(SemanticStoreError::OperationIdExhausted)?,
        );
        let resource_key = coordinate_key(&coordinate)?;
        let materialization_high_water = high_water
            .get(resource_key.as_slice())
            .map_err(persist_err)?
            .map(|value| crate::MaterializationId(value.value()));
        if previous
            .as_ref()
            .is_some_and(|state| state.last_materialization_id != materialization_high_water)
        {
            return Err(crate::PersistenceError::invariant(
                "semantic materialization high-water disagrees with active resource",
            )
            .into());
        }
        let plan = plan_accept(
            previous,
            &prior_receipts,
            materialization_high_water,
            operation_id,
            accept,
        )?;
        let next = plan
            .next
            .as_ref()
            .expect("accept planner creates a resource");
        let current = next.current()?;
        let receipt = plan
            .receipt_updates
            .iter()
            .find(|receipt| receipt.operation_id == operation_id)
            .cloned()
            .expect("accept planner creates the new operation receipt");

        if let Some(previous) = &plan.previous {
            let retained = plan
                .next
                .as_ref()
                .expect("accept planner creates a resource")
                .operations
                .iter()
                .map(|operation| operation.operation_id)
                .collect::<BTreeSet<_>>();
            for operation in &previous.operations {
                if !retained.contains(&operation.operation_id) {
                    let key = operation_key(&coordinate, operation.operation_id)?;
                    operations.remove(key.as_slice()).map_err(persist_err)?;
                }
            }
        }
        let operation = plan
            .next
            .as_ref()
            .expect("accept planner creates a resource")
            .operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .expect("accept planner retains the new operation");
        let operation_encoded = encode_operation(operation)?;
        let operation_encoded_key = operation_key(&coordinate, operation_id)?;
        operations
            .insert(
                operation_encoded_key.as_slice(),
                operation_encoded.as_slice(),
            )
            .map_err(persist_err)?;
        let resource_encoded = encode_resource(
            plan.next
                .as_ref()
                .expect("accept planner creates a resource"),
        )?;
        resources
            .insert(resource_key.as_slice(), resource_encoded.as_slice())
            .map_err(persist_err)?;
        if let Some(materialization_high_water) = plan.materialization_high_water {
            high_water
                .insert(resource_key.as_slice(), materialization_high_water.0)
                .map_err(persist_err)?;
        }
        for update in &plan.receipt_updates {
            let key = receipt_key(update.operation_id);
            let encoded = encode_receipt(update)?;
            receipts
                .insert(&key, encoded.as_slice())
                .map_err(persist_err)?;
        }
        meta.insert(NEXT_OPERATION_ID, operation_id.0)
            .map_err(persist_err)?;
        (plan, receipt, current)
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::SemanticAcceptBeforeCommit);
    commit_prepared(write, ()).map_err(SemanticStoreError::from)?;
    record_commit(store, &plan);
    Ok((receipt, current))
}

pub(super) fn rematerialize(
    store: &mut RedbStore,
    rematerialize: SemanticRematerialize,
) -> Result<Option<SemanticCurrentState>, SemanticStoreError> {
    let coordinate = rematerialize.coordinate.clone();
    let write = store.database()?.begin_write().map_err(persist_err)?;
    let (plan, current) = {
        let mut resources = write.open_table(SEMANTIC_RESOURCES).map_err(persist_err)?;
        let mut operations = write.open_table(SEMANTIC_OPERATIONS).map_err(persist_err)?;
        let mut receipts = write.open_table(SEMANTIC_RECEIPTS).map_err(persist_err)?;
        let mut high_water = write
            .open_table(SEMANTIC_MATERIALIZATION_HIGH_WATER)
            .map_err(persist_err)?;
        let previous = load_write_state(&resources, &operations, &coordinate)?
            .ok_or(SemanticStoreError::CurrentMaterializationChanged)?;
        let prior_receipts = load_receipts(&receipts, Some(&previous))?;
        let resource_key = coordinate_key(&coordinate)?;
        let persisted_high_water = high_water
            .get(resource_key.as_slice())
            .map_err(persist_err)?
            .map(|value| crate::MaterializationId(value.value()));
        if persisted_high_water != previous.last_materialization_id {
            return Err(crate::PersistenceError::invariant(
                "semantic materialization high-water disagrees with active resource",
            )
            .into());
        }
        let plan = plan_rematerialize(previous, rematerialize, &prior_receipts)?;
        let current = plan
            .next
            .as_ref()
            .map(SemanticResourceState::current)
            .transpose()?;
        let retained = plan
            .next
            .as_ref()
            .into_iter()
            .flat_map(|next| &next.operations)
            .map(|operation| operation.operation_id)
            .collect::<BTreeSet<_>>();
        for operation in &plan
            .previous
            .as_ref()
            .expect("rematerialize planner has prior state")
            .operations
        {
            if !retained.contains(&operation.operation_id) {
                let key = operation_key(&coordinate, operation.operation_id)?;
                operations.remove(key.as_slice()).map_err(persist_err)?;
            }
        }
        match &plan.next {
            Some(next) => {
                let resource_encoded = encode_resource(next)?;
                resources
                    .insert(resource_key.as_slice(), resource_encoded.as_slice())
                    .map_err(persist_err)?;
            }
            None => {
                resources
                    .remove(resource_key.as_slice())
                    .map_err(persist_err)?;
            }
        }
        if let Some(materialization_high_water) = plan.materialization_high_water {
            high_water
                .insert(resource_key.as_slice(), materialization_high_water.0)
                .map_err(persist_err)?;
        }
        for update in &plan.receipt_updates {
            let key = receipt_key(update.operation_id);
            let encoded = encode_receipt(update)?;
            receipts
                .insert(&key, encoded.as_slice())
                .map_err(persist_err)?;
        }
        (plan, current)
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::SemanticRematerializeBeforeCommit);
    commit_prepared(write, ()).map_err(SemanticStoreError::from)?;
    record_commit(store, &plan);
    Ok(current)
}

pub(super) fn promote(
    store: &mut RedbStore,
    promotion: SemanticPromotion,
) -> Result<SemanticPromotionOutcome, SemanticStoreError> {
    let coordinate = promotion.coordinate.clone();
    let write = store.database()?.begin_write().map_err(persist_err)?;
    let (outcome, staged) = {
        let mut resources = write.open_table(SEMANTIC_RESOURCES).map_err(persist_err)?;
        let high_water = write
            .open_table(SEMANTIC_MATERIALIZATION_HIGH_WATER)
            .map_err(persist_err)?;
        let resource_key = coordinate_key(&coordinate)?;
        let Some(encoded) = resources
            .get(resource_key.as_slice())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
        else {
            return Ok(SemanticPromotionOutcome::Stale);
        };
        let current = decode_resource(coordinate, &encoded)?;
        let persisted_high_water = high_water
            .get(resource_key.as_slice())
            .map_err(persist_err)?
            .map(|value| crate::MaterializationId(value.value()));
        if persisted_high_water != current.last_materialization_id {
            return Err(crate::PersistenceError::invariant(
                "semantic materialization high-water disagrees with active resource",
            )
            .into());
        }
        let (outcome, next) = promote_current(current, &promotion)?;
        if let Some(next) = next {
            let encoded = encode_resource(&next)?;
            resources
                .insert(resource_key.as_slice(), encoded.as_slice())
                .map_err(persist_err)?;
            (outcome, true)
        } else {
            (outcome, false)
        }
    };
    record_point_read(store);
    if !staged {
        return Ok(outcome);
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::SemanticPromoteBeforeCommit);
    commit_prepared(write, ()).map_err(SemanticStoreError::from)?;
    record_promotion_commit(store);
    Ok(outcome)
}

pub(super) fn recover(
    store: &RedbStore,
) -> Result<Vec<crate::RecoveredSemanticResource>, SemanticStoreError> {
    let read = store.database()?.begin_read().map_err(persist_err)?;
    let resources = read.open_table(SEMANTIC_RESOURCES).map_err(persist_err)?;
    let operations = read.open_table(SEMANTIC_OPERATIONS).map_err(persist_err)?;
    let receipts = read.open_table(SEMANTIC_RECEIPTS).map_err(persist_err)?;
    let mut recovered_resources = Vec::new();
    let mut operation_count = 0u64;
    for row in resources.iter().map_err(persist_err)? {
        let (key, value) = row.map_err(persist_err)?;
        let coordinate = decode_coordinate_key(key.value())?;
        let mut state = decode_resource(coordinate.clone(), value.value())?;
        let (lower, upper) = operation_range(&coordinate)?;
        for operation in operations
            .range(lower.as_slice()..=upper.as_slice())
            .map_err(persist_err)?
        {
            let (key, value) = operation.map_err(persist_err)?;
            let operation_id = operation_id_from_key(key.value())?;
            state
                .operations
                .push(decode_operation(operation_id, value.value())?);
            operation_count += 1;
        }
        validate_resource_state(&state)?;
        let active_receipts = state
            .operations
            .iter()
            .map(|operation| {
                let key = receipt_key(operation.operation_id);
                let value = receipts
                    .get(&key)
                    .map_err(persist_err)?
                    .ok_or(SemanticStoreError::UnknownOperation(operation.operation_id))?;
                decode_receipt(operation.operation_id, value.value())
            })
            .collect::<Result<Vec<_>, _>>()?;
        validate_active_receipts(&state, &active_receipts)?;
        recovered_resources.push(recovered(state)?);
    }
    record_recovery(store, recovered_resources.len() as u64, operation_count);
    Ok(recovered_resources)
}

pub(super) fn receipt(
    store: &RedbStore,
    operation_id: OperationId,
) -> Result<Option<SemanticEditReceipt>, SemanticStoreError> {
    let read = store.database()?.begin_read().map_err(persist_err)?;
    let receipts = read.open_table(SEMANTIC_RECEIPTS).map_err(persist_err)?;
    let key = receipt_key(operation_id);
    receipts
        .get(&key)
        .map_err(persist_err)?
        .map(|value| decode_receipt(operation_id, value.value()))
        .transpose()
}

#[cfg(any(test, feature = "bench-instrumentation"))]
fn record_point_read(store: &RedbStore) {
    use std::sync::atomic::Ordering;
    store
        .semantic_coordinate_point_reads
        .fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(any(test, feature = "bench-instrumentation")))]
fn record_point_read(_: &RedbStore) {}

#[cfg(any(test, feature = "bench-instrumentation"))]
fn record_commit(store: &RedbStore, plan: &crate::semantic_edit::SemanticTransitionPlan) {
    use std::sync::atomic::Ordering;
    record_point_read(store);
    store
        .semantic_operation_bodies_examined
        .fetch_add(plan.operation_bodies_examined, Ordering::Relaxed);
    store
        .semantic_operation_bodies_written
        .fetch_add(plan.operation_bodies_written, Ordering::Relaxed);
    store
        .semantic_operation_bodies_removed
        .fetch_add(plan.operation_bodies_removed, Ordering::Relaxed);
    store.semantic_materializations_written.fetch_add(
        u64::from(
            plan.next
                .as_ref()
                .is_some_and(|state| state.generation.is_some()),
        ),
        Ordering::Relaxed,
    );
    store.semantic_materializations_removed.fetch_add(
        u64::from(
            plan.previous
                .as_ref()
                .is_some_and(|state| state.generation.is_some()),
        ),
        Ordering::Relaxed,
    );
    store.semantic_commits.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(any(test, feature = "bench-instrumentation")))]
fn record_commit(_: &RedbStore, _: &crate::semantic_edit::SemanticTransitionPlan) {}

#[cfg(any(test, feature = "bench-instrumentation"))]
fn record_promotion_commit(store: &RedbStore) {
    use std::sync::atomic::Ordering;
    store
        .semantic_materializations_written
        .fetch_add(1, Ordering::Relaxed);
    store
        .semantic_materializations_removed
        .fetch_add(1, Ordering::Relaxed);
    store.semantic_commits.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(any(test, feature = "bench-instrumentation")))]
fn record_promotion_commit(_: &RedbStore) {}

#[cfg(any(test, feature = "bench-instrumentation"))]
fn record_recovery(store: &RedbStore, rows: u64, operations: u64) {
    use std::sync::atomic::Ordering;
    store
        .semantic_recovery_rows
        .fetch_add(rows, Ordering::Relaxed);
    store
        .semantic_operation_bodies_examined
        .fetch_add(operations, Ordering::Relaxed);
}

#[cfg(not(any(test, feature = "bench-instrumentation")))]
fn record_recovery(_: &RedbStore, _: u64, _: u64) {}

#[cfg(any(test, feature = "bench-instrumentation"))]
pub(super) fn counters(store: &RedbStore) -> SemanticStoreCounters {
    use std::sync::atomic::Ordering;
    SemanticStoreCounters {
        coordinate_point_reads: store
            .semantic_coordinate_point_reads
            .load(Ordering::Relaxed),
        operation_bodies_examined: store
            .semantic_operation_bodies_examined
            .load(Ordering::Relaxed),
        operation_bodies_written: store
            .semantic_operation_bodies_written
            .load(Ordering::Relaxed),
        operation_bodies_removed: store
            .semantic_operation_bodies_removed
            .load(Ordering::Relaxed),
        materializations_written: store
            .semantic_materializations_written
            .load(Ordering::Relaxed),
        materializations_removed: store
            .semantic_materializations_removed
            .load(Ordering::Relaxed),
        commits: store.semantic_commits.load(Ordering::Relaxed),
        recovery_rows: store.semantic_recovery_rows.load(Ordering::Relaxed),
    }
}

#[cfg(any(test, feature = "bench-instrumentation"))]
pub(super) fn reset_counters(store: &RedbStore) {
    use std::sync::atomic::Ordering;
    for counter in [
        &store.semantic_coordinate_point_reads,
        &store.semantic_operation_bodies_examined,
        &store.semantic_operation_bodies_written,
        &store.semantic_operation_bodies_removed,
        &store.semantic_materializations_written,
        &store.semantic_materializations_removed,
        &store.semantic_commits,
        &store.semantic_recovery_rows,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}
