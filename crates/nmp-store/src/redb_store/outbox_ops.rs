use super::canonical::try_decode_stored_event;
use super::outbox::{
    alloc_receipt_id_in_txn, attempt_key, deadline_intent_key, deadline_key, deadline_upper,
    decode_attempt, decode_attempt_details, decode_deadline, decode_deadline_by_intent,
    decode_lane, decode_route_revision, encode_json, increment_pending_ephemeral_in_txn,
    intent_key, intent_row_prefix, lane_deadline, lane_key, prefix_range, receipt_key,
    replace_lane_in_txn, route_revision_key, OutboxAttemptRecord, OutboxIntentRecord,
    OutboxReceiptRecord, OutboxRouteRevisionRecord,
};
use super::schema::{
    persist_err, OUTBOX_ATTEMPTS, OUTBOX_ATTEMPT_DETAILS, OUTBOX_CORRELATIONS, OUTBOX_DEADLINES,
    OUTBOX_DEADLINES_BY_INTENT, OUTBOX_DISPLACED, OUTBOX_INTENTS, OUTBOX_LANES, OUTBOX_META,
    OUTBOX_RECEIPTS, OUTBOX_ROUTE_REVISIONS,
};
#[cfg(test)]
use super::store::RedbCrashPoint;
use super::store::RedbStore;
#[cfg(test)]
use super::Ordering;
use super::{
    AttemptHandoffDetail, AttemptOutcome, AttemptTransientDetail, BTreeMap, BTreeSet,
    CloseIntentOutcome, Event, EventId, EventStore, InFlightPhase, IntentId, IntentSigState,
    LaneDeadline, LaneKey, LaneState, PersistenceError, PostHandoffState, PublicKey, ReceiptState,
    RecoveredAttempt, RecoveredAttemptDetails, RecoveredIntent, RecoveredLane, RecoveredReceipt,
    RecoveredRouteRevision, RelayUrl, Timestamp, TransientCause,
};
use nostr::JsonUtil;
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};

/// Replay every still-open intent (#3 §2.3), fallible end to end (#790).
///
/// Every step — begin, open, iterate, key parse, intent JSON, frozen event
/// JSON, displaced snapshot — reports the exact table and key it failed on
/// rather than panicking the host mid-boot. There is deliberately no
/// skip-the-bad-row branch and no good-prefix return: an intent that cannot
/// be decoded is still an obligation this store accepted, and quietly
/// dropping it would let the engine rebuild a `pending` set that is missing
/// real durable work.
pub(super) fn recover_outbox(store: &RedbStore) -> Result<Vec<RecoveredIntent>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let outbox_intents = read_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
    let outbox_displaced = read_txn.open_table(OUTBOX_DISPLACED).map_err(persist_err)?;

    let mut out = Vec::new();
    for entry in outbox_intents.iter().map_err(persist_err)? {
        let (key, value) = entry.map_err(persist_err)?;
        let intent_id = IntentId(key.value().parse::<u64>().map_err(|error| {
            PersistenceError::invariant(format!(
                "decode outbox intent key {:?}: {error}",
                key.value()
            ))
        })?);
        let record: OutboxIntentRecord = serde_json::from_str(value.value()).map_err(|error| {
            PersistenceError::invariant(format!("decode outbox intent {}: {error}", intent_id.0))
        })?;
        let frozen = Event::from_json(&record.frozen_json).map_err(|error| {
            PersistenceError::invariant(format!(
                "decode frozen event for intent {}: {error}",
                intent_id.0
            ))
        })?;

        let displaced = outbox_displaced
            .get(key.value())
            .map_err(persist_err)?
            .map(|guard| try_decode_stored_event(guard.value()))
            .transpose()?;

        out.push(RecoveredIntent {
            intent_id,
            receipt_id: record.receipt_id,
            frozen,
            expected_pubkey: record.expected_pubkey,
            signing_identity_ref: record.signing_identity_ref,
            durability: record.durability,
            routing: record.routing,
            sig_state: record.sig_state,
            displaced,
            accepted_at: record.accepted_at,
        });
    }
    Ok(out)
}

pub(super) fn reattach_receipt(
    store: &RedbStore,
    receipt_id: u64,
) -> Result<Option<RecoveredReceipt>, PersistenceError> {
    // NOT a Q4 "always empty" door: retention (not crash-survival) is
    // the contract — `OUTBOX_RECEIPTS` rows are never deleted by this
    // unit, so this is an ordinary durable read.
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let outbox_receipts = read_txn.open_table(OUTBOX_RECEIPTS).map_err(persist_err)?;
    let Some(json) = outbox_receipts
        .get(receipt_key(receipt_id).as_str())
        .map_err(persist_err)?
        .map(|guard| guard.value().to_string())
    else {
        return Ok(None);
    };
    let record: OutboxReceiptRecord = serde_json::from_str(&json)
        .map_err(|err| PersistenceError::invariant(format!("decode retained receipt: {err}")))?;
    Ok(Some(RecoveredReceipt {
        receipt_id,
        intent_id: record.intent_id,
        frozen_id: record.frozen_id,
        expected_pubkey: record.expected_pubkey,
        state: record.state,
    }))
}

pub(super) fn lookup_correlation(
    store: &RedbStore,
    token: &str,
) -> Result<Option<u64>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    // A store that has never accepted ANY correlated write never
    // created this table at all -- `ReadTransaction::open_table`
    // returns `TableDoesNotExist` in that case (unlike a write
    // transaction, a read transaction never creates tables). That is
    // exactly "no token has ever been journaled here", not a
    // persistence failure.
    let table = match read_txn.open_table(OUTBOX_CORRELATIONS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(err) => return Err(persist_err(err)),
    };
    let Some(encoded) = table
        .get(token)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_string())
    else {
        return Ok(None);
    };
    let receipt_id: u64 = encoded.parse().map_err(|err| {
        PersistenceError::invariant(format!("decode correlation receipt id: {err}"))
    })?;
    Ok(Some(receipt_id))
}

pub(super) fn record_route_revision(
    store: &mut RedbStore,
    intent_id: IntentId,
    relays: BTreeSet<RelayUrl>,
) -> Result<RecoveredRouteRevision, PersistenceError> {
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let revision = {
        let intents = write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
        let intent_key = intent_key(intent_id);
        if intents
            .get(intent_key.as_str())
            .map_err(persist_err)?
            .is_none()
        {
            return Err(PersistenceError::invariant(
                "route revision intent is not open",
            ));
        }
        let mut revisions = write_txn
            .open_table(OUTBOX_ROUTE_REVISIONS)
            .map_err(persist_err)?;
        let (lower, upper) = prefix_range(intent_row_prefix(intent_id));
        let mut last = 0;
        for entry in revisions
            .range(lower.as_str()..upper.as_str())
            .map_err(persist_err)?
        {
            #[cfg(test)]
            store
                .route_revision_range_rows
                .fetch_add(1, Ordering::Relaxed);
            let (key, value) = entry.map_err(persist_err)?;
            let recovered = decode_route_revision(key.value(), value.value())?;
            if recovered.intent_id != intent_id {
                return Err(PersistenceError::invariant(
                    "route revision range does not match its value intent",
                ));
            }
            last = last.max(recovered.ordinal);
        }
        let ordinal = last
            .checked_add(1)
            .ok_or_else(|| PersistenceError::invariant("route revision ordinal exhausted"))?;
        let record = OutboxRouteRevisionRecord {
            version: 1,
            intent_id,
            ordinal,
            relays: relays.clone(),
        };
        let encoded = serde_json::to_string(&record)
            .map_err(|err| PersistenceError::invariant(format!("encode route revision: {err}")))?;
        revisions
            .insert(
                route_revision_key(intent_id, ordinal).as_str(),
                encoded.as_str(),
            )
            .map_err(persist_err)?;
        RecoveredRouteRevision {
            version: 1,
            intent_id,
            ordinal,
            relays,
        }
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::RouteRevisionBeforeCommit);
    write_txn.commit().map_err(persist_err)?;
    Ok(revision)
}

pub(super) fn recover_route_revisions(
    store: &RedbStore,
    intent_id: IntentId,
) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let revisions = read_txn
        .open_table(OUTBOX_ROUTE_REVISIONS)
        .map_err(persist_err)?;
    let (lower, upper) = prefix_range(intent_row_prefix(intent_id));
    let mut recovered = Vec::new();
    for entry in revisions
        .range(lower.as_str()..upper.as_str())
        .map_err(persist_err)?
    {
        #[cfg(test)]
        store
            .route_revision_range_rows
            .fetch_add(1, Ordering::Relaxed);
        let (key, value) = entry.map_err(persist_err)?;
        let revision = decode_route_revision(key.value(), value.value())?;
        if revision.intent_id != intent_id {
            return Err(PersistenceError::invariant(
                "route revision range does not match its value intent",
            ));
        }
        recovered.push(revision);
    }
    recovered.sort_by_key(|revision| revision.ordinal);
    Ok(recovered)
}

pub(super) fn recover_attempts(
    store: &RedbStore,
    intent_id: IntentId,
) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let attempts = read_txn.open_table(OUTBOX_ATTEMPTS).map_err(persist_err)?;
    let details = read_txn
        .open_table(OUTBOX_ATTEMPT_DETAILS)
        .map_err(persist_err)?;
    let (lower, upper) = prefix_range(intent_row_prefix(intent_id));
    let mut recovered = Vec::new();
    for entry in attempts
        .range(lower.as_str()..upper.as_str())
        .map_err(persist_err)?
    {
        #[cfg(test)]
        store.attempt_range_rows.fetch_add(1, Ordering::Relaxed);
        let (key, value) = entry.map_err(persist_err)?;
        let mut attempt = decode_attempt(key.value(), value.value())?;
        if attempt.intent_id != intent_id {
            return Err(PersistenceError::invariant(
                "outbox attempt range does not match its value intent",
            ));
        }
        if let Some(detail) = details.get(key.value()).map_err(persist_err)? {
            let detail = decode_attempt_details(key.value(), detail.value())?;
            if let Some(terminal) = detail.terminal {
                attempt.outcome = terminal;
            }
        }
        recovered.push(attempt);
    }
    // Table-key layout is a storage detail (currently length-prefixed
    // relay text), not public recovery order. Match MemoryStore and the
    // typed contract explicitly.
    recovered.sort_by(|left, right| {
        left.relay
            .cmp(&right.relay)
            .then(left.ordinal.cmp(&right.ordinal))
    });
    Ok(recovered)
}

pub(super) fn bootstrap_outbox_lanes(
    store: &mut RedbStore,
    intent_id: IntentId,
) -> Result<Vec<RecoveredLane>, PersistenceError> {
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    {
        let intents = write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
        if intents
            .get(intent_key(intent_id).as_str())
            .map_err(persist_err)?
            .is_none()
        {
            return Err(PersistenceError::invariant(
                "lane bootstrap intent is not open",
            ));
        }
        let route_revisions = write_txn
            .open_table(OUTBOX_ROUTE_REVISIONS)
            .map_err(persist_err)?;
        let attempts_table = write_txn.open_table(OUTBOX_ATTEMPTS).map_err(persist_err)?;
        let mut lanes = write_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
        let details = write_txn
            .open_table(OUTBOX_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let (lower, upper) = prefix_range(intent_row_prefix(intent_id));
        let mut details_by_key = BTreeMap::new();
        for row in details
            .range(lower.as_str()..upper.as_str())
            .map_err(persist_err)?
        {
            let (key, value) = row.map_err(persist_err)?;
            let detail = decode_attempt_details(key.value(), value.value())?;
            details_by_key.insert((detail.relay.clone(), detail.ordinal), detail);
        }
        let mut attempts = Vec::new();
        for row in attempts_table
            .range(lower.as_str()..upper.as_str())
            .map_err(persist_err)?
        {
            #[cfg(test)]
            store.attempt_range_rows.fetch_add(1, Ordering::Relaxed);
            let (key, value) = row.map_err(persist_err)?;
            let mut attempt = decode_attempt(key.value(), value.value())?;
            if let Some(terminal) = details_by_key
                .get(&(attempt.relay.clone(), attempt.ordinal))
                .and_then(|detail| detail.terminal.clone())
            {
                attempt.outcome = terminal;
            }
            attempts.push(attempt);
        }
        attempts.sort_by(|left, right| {
            left.relay
                .cmp(&right.relay)
                .then(left.ordinal.cmp(&right.ordinal))
        });
        let mut relays = BTreeSet::new();
        for row in route_revisions
            .range(lower.as_str()..upper.as_str())
            .map_err(persist_err)?
        {
            #[cfg(test)]
            store
                .route_revision_range_rows
                .fetch_add(1, Ordering::Relaxed);
            let (key, value) = row.map_err(persist_err)?;
            let revision = decode_route_revision(key.value(), value.value())?;
            relays.extend(revision.relays);
        }
        // #867: the current schema writes an attempt's base row, its detail
        // row, and its route revision together. Bootstrap therefore VERIFIES
        // that shape instead of adopting a pre-detail one — a missing detail
        // row is corruption of the current epoch, not an older writer to be
        // accommodated with a synthesized shell.
        let attempt_keys: BTreeSet<_> = attempts
            .iter()
            .map(|attempt| (attempt.relay.clone(), attempt.ordinal))
            .collect();
        for attempt in &attempts {
            if !details_by_key.contains_key(&(attempt.relay.clone(), attempt.ordinal)) {
                return Err(PersistenceError::invariant(
                    "attempt row is missing its detail row",
                ));
            }
            if !relays.contains(&attempt.relay) {
                return Err(PersistenceError::invariant(
                    "attempt relay is absent from every route revision",
                ));
            }
        }
        if details_by_key
            .keys()
            .any(|detail_key| !attempt_keys.contains(detail_key))
        {
            return Err(PersistenceError::invariant(
                "attempt detail row has no base attempt row",
            ));
        }
        for relay in relays {
            let key = LaneKey { intent_id, relay };
            let storage_key = lane_key(&key);
            let lane_attempts: Vec<_> = attempts
                .iter()
                .filter(|attempt| attempt.relay == key.relay)
                .collect();
            let live_count = lane_attempts
                .iter()
                .filter(|attempt| {
                    crate::attempt_is_live(
                        attempt,
                        details_by_key.get(&(attempt.relay.clone(), attempt.ordinal)),
                    )
                })
                .count();
            if live_count > 1
                || (live_count == 1
                    && lane_attempts.last().is_some_and(|attempt| {
                        !crate::attempt_is_live(
                            attempt,
                            details_by_key.get(&(attempt.relay.clone(), attempt.ordinal)),
                        )
                    }))
            {
                return Err(PersistenceError::invariant(
                    "contradictory live attempt history",
                ));
            }
            if let Some(existing) = lanes.get(storage_key.as_str()).map_err(persist_err)? {
                let lane = decode_lane(&storage_key, existing.value())?;
                let max = lane_attempts.last().map_or(0, |attempt| attempt.ordinal);
                if lane.last_ordinal != max {
                    return Err(PersistenceError::invariant(
                        "outbox lane cursor disagrees with retained attempt history",
                    ));
                }
                match lane_attempts.last() {
                    Some(attempt) if attempt.outcome != AttemptOutcome::Started => {
                        if lane.state
                            != (LaneState::Terminal {
                                ordinal: attempt.ordinal,
                                outcome: attempt.outcome.clone(),
                            })
                        {
                            return Err(PersistenceError::invariant(
                                "terminal attempt and lane state disagree",
                            ));
                        }
                    }
                    _ if matches!(lane.state, LaneState::Terminal { .. }) => {
                        return Err(PersistenceError::invariant(
                            "terminal lane lacks matching terminal attempt",
                        ));
                    }
                    _ => {}
                }
                continue;
            }
            // The current schema commits a lane row before any attempt on it
            // may start, so attempt history without a lane is corruption, not
            // an older layout to reconstruct a state for.
            if !lane_attempts.is_empty() {
                return Err(PersistenceError::invariant(
                    "attempt history exists without its lane row",
                ));
            }
            let lane = RecoveredLane {
                version: 1,
                key,
                revision: 1,
                last_ordinal: 0,
                state: LaneState::WaitingConnection,
            };
            let encoded = encode_json(&lane, "outbox lane")?;
            lanes
                .insert(storage_key.as_str(), encoded.as_str())
                .map_err(persist_err)?;
        }
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneBootstrapBeforeCommit);
    write_txn.commit().map_err(persist_err)?;
    store.recover_outbox_lanes(intent_id)
}

pub(super) fn recover_outbox_lanes(
    store: &RedbStore,
    intent_id: IntentId,
) -> Result<Vec<RecoveredLane>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let lanes = read_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
    let (lower, upper) = prefix_range(intent_row_prefix(intent_id));
    let mut recovered = Vec::new();
    for row in lanes
        .range(lower.as_str()..upper.as_str())
        .map_err(persist_err)?
    {
        let (key, value) = row.map_err(persist_err)?;
        let lane = decode_lane(key.value(), value.value())?;
        if lane.key.intent_id != intent_id {
            return Err(PersistenceError::invariant(
                "lane range escaped intent prefix",
            ));
        }
        recovered.push(lane);
    }
    recovered.sort_by(|a, b| a.key.relay.cmp(&b.key.relay));
    Ok(recovered)
}

pub(super) fn due_outbox_deadlines(
    store: &RedbStore,
    now: Timestamp,
    limit: usize,
) -> Result<Vec<LaneDeadline>, PersistenceError> {
    if limit > 1_024 {
        return Err(PersistenceError::invariant(
            "deadline read limit exceeds 1024",
        ));
    }
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let deadlines = read_txn.open_table(OUTBOX_DEADLINES).map_err(persist_err)?;
    let deadlines_by_intent = read_txn
        .open_table(OUTBOX_DEADLINES_BY_INTENT)
        .map_err(persist_err)?;
    if deadlines.len().map_err(persist_err)? != deadlines_by_intent.len().map_err(persist_err)? {
        return Err(PersistenceError::invariant(
            "deadline index cardinalities disagree",
        ));
    }
    let lanes = read_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
    let upper = deadline_upper(now);
    let mut recovered = Vec::new();
    for row in deadlines
        .range("00000000000000000000:"..upper.as_str())
        .map_err(persist_err)?
    {
        if recovered.len() == limit {
            break;
        }
        let (key, value) = row.map_err(persist_err)?;
        let deadline = decode_deadline(key.value(), value.value())?;
        let secondary_key = deadline_intent_key(&deadline);
        let secondary = deadlines_by_intent
            .get(secondary_key.as_str())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_string())
            .ok_or_else(|| PersistenceError::invariant("deadline is missing by-intent index"))?;
        if decode_deadline_by_intent(&secondary_key, &secondary)? != deadline {
            return Err(PersistenceError::invariant("deadline indexes disagree"));
        }
        let lane_storage_key = lane_key(&deadline.key);
        let lane_json = lanes
            .get(lane_storage_key.as_str())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_string())
            .ok_or_else(|| PersistenceError::invariant("deadline references missing lane"))?;
        let lane = decode_lane(&lane_storage_key, &lane_json)?;
        if lane_deadline(&lane).as_ref() != Some(&deadline) {
            return Err(PersistenceError::invariant("deadline and lane disagree"));
        }
        recovered.push(deadline);
    }
    Ok(recovered)
}

pub(super) fn next_outbox_deadline(
    store: &RedbStore,
) -> Result<Option<Timestamp>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let deadlines = read_txn.open_table(OUTBOX_DEADLINES).map_err(persist_err)?;
    let deadlines_by_intent = read_txn
        .open_table(OUTBOX_DEADLINES_BY_INTENT)
        .map_err(persist_err)?;
    if deadlines.len().map_err(persist_err)? != deadlines_by_intent.len().map_err(persist_err)? {
        return Err(PersistenceError::invariant(
            "deadline index cardinalities disagree",
        ));
    }
    let lanes = read_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
    let mut rows = deadlines.iter().map_err(persist_err)?;
    let Some(row) = rows.next() else {
        return Ok(None);
    };
    let (key, value) = row.map_err(persist_err)?;
    let deadline = decode_deadline(key.value(), value.value())?;
    let secondary_key = deadline_intent_key(&deadline);
    let secondary = deadlines_by_intent
        .get(secondary_key.as_str())
        .map_err(persist_err)?
        .map(|guard| guard.value().to_string())
        .ok_or_else(|| PersistenceError::invariant("deadline is missing by-intent index"))?;
    if decode_deadline_by_intent(&secondary_key, &secondary)? != deadline {
        return Err(PersistenceError::invariant("deadline indexes disagree"));
    }
    let lane_storage_key = lane_key(&deadline.key);
    let lane = lanes
        .get(lane_storage_key.as_str())
        .map_err(persist_err)?
        .map(|guard| guard.value().to_string())
        .ok_or_else(|| PersistenceError::invariant("deadline references missing lane"))?;
    if lane_deadline(&decode_lane(&lane_storage_key, &lane)?).as_ref() != Some(&deadline) {
        return Err(PersistenceError::invariant("deadline and lane disagree"));
    }
    Ok(Some(deadline.at))
}

pub(super) fn set_lane_waiting(
    store: &mut RedbStore,
    key: &LaneKey,
    expected_revision: u64,
    auth: bool,
) -> Result<RecoveredLane, PersistenceError> {
    store.persist_lane_state(
        key,
        expected_revision,
        if auth {
            LaneState::WaitingAuth
        } else {
            LaneState::WaitingConnection
        },
    )
}

pub(super) fn set_lane_eligible(
    store: &mut RedbStore,
    key: &LaneKey,
    expected_revision: u64,
    since: Timestamp,
) -> Result<RecoveredLane, PersistenceError> {
    store.persist_lane_state(key, expected_revision, LaneState::Eligible { since })
}

pub(super) fn set_lane_transient(
    store: &mut RedbStore,
    key: &LaneKey,
    expected_revision: u64,
    ordinal: u64,
    eligible_at: Timestamp,
    cause: TransientCause,
    raw_reason: Option<String>,
) -> Result<RecoveredLane, PersistenceError> {
    if raw_reason
        .as_ref()
        .is_some_and(|reason| reason.len() > 4_096)
    {
        return Err(PersistenceError::invariant(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let lane = {
        let mut lanes = write_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(OUTBOX_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(OUTBOX_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let mut details = write_txn
            .open_table(OUTBOX_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let storage_key = lane_key(key);
        let json = lanes
            .get(storage_key.as_str())
            .map_err(persist_err)?
            .map(|g| g.value().to_string())
            .ok_or_else(|| PersistenceError::invariant("outbox lane not found"))?;
        let current = decode_lane(&storage_key, &json)?;
        if current.last_ordinal != ordinal {
            return Err(PersistenceError::invariant("stale attempt ordinal"));
        }
        if ordinal > 0 {
            let detail_key = attempt_key(key.intent_id, &key.relay, ordinal);
            let detail_json = details
                .get(detail_key.as_str())
                .map_err(persist_err)?
                .map(|g| g.value().to_string())
                .ok_or_else(|| PersistenceError::invariant("attempt detail row not found"))?;
            let mut detail = decode_attempt_details(&detail_key, &detail_json)?;
            detail.transient = Some(AttemptTransientDetail {
                eligible_at,
                cause,
                raw_reason: raw_reason.clone(),
            });
            details
                .insert(
                    detail_key.as_str(),
                    encode_json(&detail, "attempt details")?.as_str(),
                )
                .map_err(persist_err)?;
        }
        replace_lane_in_txn(
            &mut lanes,
            &mut deadlines,
            &mut deadlines_by_intent,
            key,
            expected_revision,
            LaneState::Transient {
                ordinal,
                eligible_at,
                cause,
                raw_reason,
            },
        )?
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneTransitionBeforeCommit);
    write_txn.commit().map_err(persist_err)?;
    Ok(lane)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn suspend_lane_attempt(
    store: &mut RedbStore,
    key: &LaneKey,
    expected_revision: u64,
    ordinal: u64,
    at: Timestamp,
    cause: TransientCause,
    raw_reason: Option<String>,
    auth: bool,
) -> Result<RecoveredLane, PersistenceError> {
    if raw_reason
        .as_ref()
        .is_some_and(|reason| reason.len() > 4_096)
    {
        return Err(PersistenceError::invariant(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let lane = {
        let mut lanes = write_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(OUTBOX_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(OUTBOX_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let mut details = write_txn
            .open_table(OUTBOX_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let storage_key = lane_key(key);
        let json = lanes
            .get(storage_key.as_str())
            .map_err(persist_err)?
            .map(|g| g.value().to_string())
            .ok_or_else(|| PersistenceError::invariant("outbox lane not found"))?;
        let current = decode_lane(&storage_key, &json)?;
        if current.revision != expected_revision || current.last_ordinal != ordinal || ordinal == 0
        {
            return Err(PersistenceError::invariant("stale suspended attempt"));
        }
        let detail_key = attempt_key(key.intent_id, &key.relay, ordinal);
        let detail_json = details
            .get(detail_key.as_str())
            .map_err(persist_err)?
            .map(|g| g.value().to_string())
            .ok_or_else(|| PersistenceError::invariant("attempt detail row not found"))?;
        let mut detail = decode_attempt_details(&detail_key, &detail_json)?;
        detail.transient = Some(AttemptTransientDetail {
            eligible_at: at,
            cause,
            raw_reason,
        });
        details
            .insert(
                detail_key.as_str(),
                encode_json(&detail, "attempt details")?.as_str(),
            )
            .map_err(persist_err)?;
        replace_lane_in_txn(
            &mut lanes,
            &mut deadlines,
            &mut deadlines_by_intent,
            key,
            expected_revision,
            if auth {
                LaneState::WaitingAuth
            } else {
                LaneState::WaitingConnection
            },
        )?
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneTransitionBeforeCommit);
    write_txn.commit().map_err(persist_err)?;
    Ok(lane)
}

pub(super) fn start_lane_attempt(
    store: &mut RedbStore,
    key: &LaneKey,
    expected_revision: u64,
    event: Event,
    started_at: Timestamp,
) -> Result<(RecoveredAttempt, RecoveredLane), PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let intents = read_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
    let intent_json = intents
        .get(intent_key(key.intent_id).as_str())
        .map_err(persist_err)?
        .map(|g| g.value().to_string())
        .ok_or_else(|| PersistenceError::invariant("attempt intent is not open"))?;
    let intent: OutboxIntentRecord = serde_json::from_str(&intent_json)
        .map_err(|e| PersistenceError::invariant(format!("decode attempt intent: {e}")))?;
    let frozen = Event::from_json(&intent.frozen_json)
        .map_err(|e| PersistenceError::invariant(format!("decode attempt intent event: {e}")))?;
    if intent.sig_state != IntentSigState::Signed || frozen != event {
        return Err(PersistenceError::invariant(
            "attempt bytes are not the intent's promoted signed bytes",
        ));
    }
    event
        .verify()
        .map_err(|e| PersistenceError::invariant(format!("attempt event is invalid: {e}")))?;
    drop(intents);
    drop(read_txn);
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let (attempt, lane) = {
        let mut attempts = write_txn.open_table(OUTBOX_ATTEMPTS).map_err(persist_err)?;
        let mut details = write_txn
            .open_table(OUTBOX_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let mut lanes = write_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(OUTBOX_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(OUTBOX_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let storage_key = lane_key(key);
        let lane_json = lanes
            .get(storage_key.as_str())
            .map_err(persist_err)?
            .map(|g| g.value().to_string())
            .ok_or_else(|| PersistenceError::invariant("outbox lane not found"))?;
        let current = decode_lane(&storage_key, &lane_json)?;
        if current.revision != expected_revision
            || !matches!(current.state, LaneState::Eligible { .. })
        {
            return Err(PersistenceError::invariant(
                "lane is not expected eligible cursor",
            ));
        }
        let ordinal = current
            .last_ordinal
            .checked_add(1)
            .ok_or_else(|| PersistenceError::invariant("attempt ordinal exhausted"))?;
        let attempt = RecoveredAttempt {
            version: 1,
            intent_id: key.intent_id,
            relay: key.relay.clone(),
            ordinal,
            event,
            outcome: AttemptOutcome::Started,
        };
        let raw = OutboxAttemptRecord {
            version: 1,
            intent_id: key.intent_id,
            relay: key.relay.clone(),
            ordinal,
            event_json: attempt.event.as_json(),
            outcome: AttemptOutcome::Started,
        };
        attempts
            .insert(
                attempt_key(key.intent_id, &key.relay, ordinal).as_str(),
                encode_json(&raw, "outbox attempt")?.as_str(),
            )
            .map_err(persist_err)?;
        let detail = RecoveredAttemptDetails {
            version: 1,
            intent_id: key.intent_id,
            relay: key.relay.clone(),
            ordinal,
            started_at: Some(started_at),
            handoff: None,
            transient: None,
            finished_at: None,
            terminal: None,
        };
        details
            .insert(
                attempt_key(key.intent_id, &key.relay, ordinal).as_str(),
                encode_json(&detail, "attempt details")?.as_str(),
            )
            .map_err(persist_err)?;
        let mut advanced = replace_lane_in_txn(
            &mut lanes,
            &mut deadlines,
            &mut deadlines_by_intent,
            key,
            expected_revision,
            LaneState::InFlight {
                ordinal,
                phase: InFlightPhase::AwaitingHandoff,
            },
        )?;
        advanced.last_ordinal = ordinal;
        lanes
            .insert(
                storage_key.as_str(),
                encode_json(&advanced, "outbox lane")?.as_str(),
            )
            .map_err(persist_err)?;
        (attempt, advanced)
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneStartBeforeCommit);
    write_txn.commit().map_err(persist_err)?;
    Ok((attempt, lane))
}

pub(super) fn record_lane_handoff(
    store: &mut RedbStore,
    key: &LaneKey,
    expected_revision: u64,
    ordinal: u64,
    detail: AttemptHandoffDetail,
    next: PostHandoffState,
) -> Result<RecoveredLane, PersistenceError> {
    if matches!(
        &next,
        PostHandoffState::Transient {
            raw_reason: Some(reason),
            ..
        } if reason.len() > 4_096
    ) {
        return Err(PersistenceError::invariant(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let lane = {
        let mut details = write_txn
            .open_table(OUTBOX_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let mut lanes = write_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(OUTBOX_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(OUTBOX_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let lane_storage_key = lane_key(key);
        let lane_json = lanes
            .get(lane_storage_key.as_str())
            .map_err(persist_err)?
            .map(|g| g.value().to_string())
            .ok_or_else(|| PersistenceError::invariant("outbox lane not found"))?;
        let current_lane = decode_lane(&lane_storage_key, &lane_json)?;
        if current_lane.revision != expected_revision || current_lane.last_ordinal != ordinal {
            return Err(PersistenceError::invariant("stale lane handoff"));
        }
        if !matches!(
            current_lane.state,
            LaneState::InFlight {
                ordinal: current,
                phase: InFlightPhase::AwaitingHandoff,
            } if current == ordinal
        ) {
            return Err(PersistenceError::invariant("lane is not awaiting handoff"));
        }
        let attempt_key_value = attempt_key(key.intent_id, &key.relay, ordinal);
        let detail_json = details
            .get(attempt_key_value.as_str())
            .map_err(persist_err)?
            .map(|g| g.value().to_string())
            .ok_or_else(|| PersistenceError::invariant("attempt detail row not found"))?;
        let mut recovered_detail = decode_attempt_details(&attempt_key_value, &detail_json)?;
        if let Some(existing) = &recovered_detail.handoff {
            if existing != &detail {
                return Err(PersistenceError::invariant("conflicting handoff evidence"));
            }
        } else {
            recovered_detail.handoff = Some(detail);
        }
        let state = match next {
            PostHandoffState::WaitingConnection => LaneState::WaitingConnection,
            PostHandoffState::WaitingAuth => LaneState::WaitingAuth,
            PostHandoffState::Eligible { since } => LaneState::Eligible { since },
            PostHandoffState::AwaitingAck { deadline } => LaneState::InFlight {
                ordinal,
                phase: InFlightPhase::AwaitingAck { deadline },
            },
            PostHandoffState::Transient {
                eligible_at,
                cause,
                raw_reason,
            } => LaneState::Transient {
                ordinal,
                eligible_at,
                cause,
                raw_reason,
            },
            PostHandoffState::Terminal {
                outcome,
                finished_at,
            } => {
                if outcome == AttemptOutcome::Started {
                    return Err(PersistenceError::invariant("Started is not terminal"));
                }
                recovered_detail.finished_at = Some(finished_at);
                recovered_detail.terminal = Some(outcome.clone());
                LaneState::Terminal { ordinal, outcome }
            }
        };
        let lane = replace_lane_in_txn(
            &mut lanes,
            &mut deadlines,
            &mut deadlines_by_intent,
            key,
            expected_revision,
            state,
        )?;
        if lane.last_ordinal != ordinal {
            return Err(PersistenceError::invariant("stale lane handoff ordinal"));
        }
        details
            .insert(
                attempt_key_value.as_str(),
                encode_json(&recovered_detail, "attempt details")?.as_str(),
            )
            .map_err(persist_err)?;
        lane
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneHandoffBeforeCommit);
    write_txn.commit().map_err(persist_err)?;
    Ok(lane)
}

pub(super) fn finish_lane_attempt(
    store: &mut RedbStore,
    key: &LaneKey,
    expected_revision: u64,
    ordinal: u64,
    outcome: AttemptOutcome,
    finished_at: Timestamp,
) -> Result<RecoveredLane, PersistenceError> {
    if outcome == AttemptOutcome::Started {
        return Err(PersistenceError::invariant("Started is not terminal"));
    }
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let lane = {
        let mut details = write_txn
            .open_table(OUTBOX_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let mut lanes = write_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(OUTBOX_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(OUTBOX_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let storage_key = lane_key(key);
        let lane_json = lanes
            .get(storage_key.as_str())
            .map_err(persist_err)?
            .map(|g| g.value().to_string())
            .ok_or_else(|| PersistenceError::invariant("outbox lane not found"))?;
        let current = decode_lane(&storage_key, &lane_json)?;
        if current.revision != expected_revision || current.last_ordinal != ordinal {
            return Err(PersistenceError::invariant("stale terminal attempt"));
        }
        let detail_key = attempt_key(key.intent_id, &key.relay, ordinal);
        let detail_json = details
            .get(detail_key.as_str())
            .map_err(persist_err)?
            .map(|g| g.value().to_string())
            .ok_or_else(|| PersistenceError::invariant("attempt detail row not found"))?;
        let mut detail = decode_attempt_details(&detail_key, &detail_json)?;
        if let Some(existing) = &detail.terminal {
            if existing == &outcome
                && detail.finished_at == Some(finished_at)
                && matches!(
                    current.state,
                    LaneState::Terminal {
                        ordinal: current_ordinal,
                        outcome: ref current_outcome,
                    } if current_ordinal == ordinal && current_outcome == &outcome
                )
            {
                current
            } else {
                return Err(PersistenceError::invariant(
                    "attempt already has conflicting terminal evidence",
                ));
            }
        } else {
            detail.finished_at = Some(finished_at);
            detail.terminal = Some(outcome.clone());
            details
                .insert(
                    detail_key.as_str(),
                    encode_json(&detail, "attempt details")?.as_str(),
                )
                .map_err(persist_err)?;
            replace_lane_in_txn(
                &mut lanes,
                &mut deadlines,
                &mut deadlines_by_intent,
                key,
                expected_revision,
                LaneState::Terminal { ordinal, outcome },
            )?
        }
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::FinishAttemptBeforeCommit);
    write_txn.commit().map_err(persist_err)?;
    Ok(lane)
}

pub(super) fn recover_attempt_details(
    store: &RedbStore,
    intent_id: IntentId,
) -> Result<Vec<RecoveredAttemptDetails>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let details = read_txn
        .open_table(OUTBOX_ATTEMPT_DETAILS)
        .map_err(persist_err)?;
    let (lower, upper) = prefix_range(intent_row_prefix(intent_id));
    let mut recovered = Vec::new();
    for row in details
        .range(lower.as_str()..upper.as_str())
        .map_err(persist_err)?
    {
        let (key, value) = row.map_err(persist_err)?;
        recovered.push(decode_attempt_details(key.value(), value.value())?);
    }
    recovered.sort_by(|a, b| a.relay.cmp(&b.relay).then(a.ordinal.cmp(&b.ordinal)));
    Ok(recovered)
}

pub(super) fn close_terminal_intent(
    store: &mut RedbStore,
    intent_id: IntentId,
) -> Result<CloseIntentOutcome, PersistenceError> {
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let result = {
        let mut intents = write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
        if intents
            .get(intent_key(intent_id).as_str())
            .map_err(persist_err)?
            .is_none()
        {
            CloseIntentOutcome::AlreadyClosed
        } else {
            let lanes_table = write_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
            let (lane_lower, lane_upper) = prefix_range(intent_row_prefix(intent_id));
            let mut lanes_snapshot = Vec::new();
            for row in lanes_table
                .range(lane_lower.as_str()..lane_upper.as_str())
                .map_err(persist_err)?
            {
                let (key, value) = row.map_err(persist_err)?;
                let lane = decode_lane(key.value(), value.value())?;
                if lane.key.intent_id != intent_id {
                    return Err(PersistenceError::invariant(
                        "lane close range escaped intent prefix",
                    ));
                }
                lanes_snapshot.push(lane);
            }
            if lanes_snapshot.is_empty()
                || lanes_snapshot
                    .iter()
                    .any(|lane| !matches!(lane.state, LaneState::Terminal { .. }))
            {
                return Err(PersistenceError::invariant(
                    "intent lanes are not non-empty and terminal",
                ));
            }
            let mut deadlines = write_txn
                .open_table(OUTBOX_DEADLINES)
                .map_err(persist_err)?;
            let mut deadlines_by_intent = write_txn
                .open_table(OUTBOX_DEADLINES_BY_INTENT)
                .map_err(persist_err)?;
            if deadlines.len().map_err(persist_err)?
                != deadlines_by_intent.len().map_err(persist_err)?
            {
                return Err(PersistenceError::invariant(
                    "deadline index cardinalities disagree",
                ));
            }
            let (deadline_lower, deadline_upper) = prefix_range(intent_row_prefix(intent_id));
            let mut stale_rows = Vec::new();
            for row in deadlines_by_intent
                .range(deadline_lower.as_str()..deadline_upper.as_str())
                .map_err(persist_err)?
            {
                let (key, value) = row.map_err(persist_err)?;
                let deadline = decode_deadline_by_intent(key.value(), value.value())?;
                if deadline.key.intent_id != intent_id {
                    return Err(PersistenceError::invariant(
                        "deadline close range escaped intent prefix",
                    ));
                }
                stale_rows.push((key.value().to_string(), deadline));
            }
            for (by_intent_key, deadline) in stale_rows {
                let ordered_key = deadline_key(&deadline);
                let ordered = deadlines
                    .get(ordered_key.as_str())
                    .map_err(persist_err)?
                    .map(|guard| guard.value().to_string())
                    .ok_or_else(|| {
                        PersistenceError::invariant("by-intent deadline is missing ordered index")
                    })?;
                if decode_deadline(&ordered_key, &ordered)? != deadline {
                    return Err(PersistenceError::invariant("deadline indexes disagree"));
                }
                deadlines
                    .remove(ordered_key.as_str())
                    .map_err(persist_err)?;
                deadlines_by_intent
                    .remove(by_intent_key.as_str())
                    .map_err(persist_err)?;
            }
            intents
                .remove(intent_key(intent_id).as_str())
                .map_err(persist_err)?;
            CloseIntentOutcome::Closed
        }
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneCloseBeforeCommit);
    write_txn.commit().map_err(persist_err)?;
    Ok(result)
}

pub(super) fn accept_ephemeral(
    store: &mut RedbStore,
    frozen_id: EventId,
    expected_pubkey: PublicKey,
) -> Result<u64, PersistenceError> {
    // Receipt-ONLY: touches `OUTBOX_RECEIPTS` (+ `OUTBOX_META` for the
    // id allocation) alone — no `EVENTS` row, no `OUTBOX_INTENTS` row,
    // `intent_id: None` (nothing backs it).
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let receipt_id = {
        let mut outbox_meta = write_txn.open_table(OUTBOX_META).map_err(persist_err)?;
        let mut outbox_receipts = write_txn.open_table(OUTBOX_RECEIPTS).map_err(persist_err)?;
        let receipt_id = alloc_receipt_id_in_txn(&mut outbox_meta)?;
        let record = OutboxReceiptRecord {
            intent_id: None,
            frozen_id,
            expected_pubkey,
            state: ReceiptState::Accepted,
        };
        let encoded = serde_json::to_string(&record).expect("redb: encode outbox receipt");
        outbox_receipts
            .insert(receipt_key(receipt_id).as_str(), encoded.as_str())
            .map_err(persist_err)?;
        increment_pending_ephemeral_in_txn(&mut outbox_meta)?;
        receipt_id
    };
    write_txn.commit().map_err(persist_err)?;
    Ok(receipt_id)
}
