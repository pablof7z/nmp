use super::commit::commit_prepared;
use super::publish_queue::{
    alloc_counter_in_txn, alloc_receipt_id_in_txn, lane_deadline, mark_terminal_receipt,
    read_meta_u64, remove_terminal_receipt_index, replace_lane_in_txn,
    update_publish_queue_receipt, PublishQueueReceiptRecord,
};
use super::publish_queue_codec::{
    attempt_key, attempt_range, canonical_route_ids, codec_error, deadline_by_intent_key,
    deadline_due_range, deadline_intent_range, deadline_key, decode_attempt,
    decode_attempt_details, decode_deadline, decode_displaced, decode_intent, decode_lane,
    decode_meta_u64, decode_receipt, decode_relay, decode_route, encode_attempt,
    encode_attempt_details, encode_lane, encode_receipt, encode_relay, encode_route, intent_key,
    lane_key, lane_range, parse_attempt_key, parse_deadline_by_intent_key, parse_deadline_key,
    parse_intent_key, parse_lane_key, parse_route_revision_key, parse_terminal_receipt_key,
    receipt_key, relay_key, route_revision_key, route_revision_range, terminal_receipt_range,
    PublishQueueRelayId, NEXT_RELAY_ID_KEY, TERMINAL_RECEIPT_BYTES_KEY, TERMINAL_RECEIPT_COUNT_KEY,
};
use super::schema::{
    persist_err, PUBLISH_QUEUE_ATTEMPTS, PUBLISH_QUEUE_ATTEMPT_DETAILS, PUBLISH_QUEUE_CORRELATIONS,
    PUBLISH_QUEUE_DEADLINES, PUBLISH_QUEUE_DEADLINES_BY_INTENT, PUBLISH_QUEUE_DISPLACED,
    PUBLISH_QUEUE_INTENTS, PUBLISH_QUEUE_KIND5_CLAIMS, PUBLISH_QUEUE_LANES, PUBLISH_QUEUE_META,
    PUBLISH_QUEUE_RECEIPTS, PUBLISH_QUEUE_RELAYS, PUBLISH_QUEUE_RELAY_IDS,
    PUBLISH_QUEUE_ROUTE_REVISIONS,
};
#[cfg(test)]
use super::store::RedbCrashPoint;
use super::store::RedbStore;
#[cfg(test)]
use super::Ordering;
use super::{
    AuthDenial, BTreeMap, BTreeSet, CloseIntentOutcome, Event, EventId, IntentId, IntentSigState,
    PersistenceError, PublicKey, PublishQueueAttempt, PublishQueueAttemptDetails,
    PublishQueueAttemptHandoff, PublishQueueAttemptOutcome, PublishQueueAttemptTransient,
    PublishQueueDeadline, PublishQueueInFlightPhase, PublishQueueIntent, PublishQueueLane,
    PublishQueueLaneKey, PublishQueueLaneState, PublishQueuePostHandoffState, PublishQueueReceipt,
    PublishQueueRouteRevision, PublishQueueTerminalOutcome, PublishQueueTransientCause,
    ReceiptState, RelayUrl, Timestamp,
};
use crate::terminal_retention::{wall_clock_now, TerminalRetentionLimits};
use crate::EventStore;
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
pub(super) fn recover_publish_queue(
    store: &RedbStore,
) -> Result<Vec<PublishQueueIntent>, PersistenceError> {
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let publish_queue_intents = read_txn
        .open_table(PUBLISH_QUEUE_INTENTS)
        .map_err(persist_err)?;
    let publish_queue_displaced = read_txn
        .open_table(PUBLISH_QUEUE_DISPLACED)
        .map_err(persist_err)?;

    let mut out = Vec::new();
    for entry in publish_queue_intents.iter().map_err(persist_err)? {
        let (key, value) = entry.map_err(persist_err)?;
        let intent_id =
            parse_intent_key(key.value()).map_err(|error| codec_error("intent key", error))?;
        let record = decode_intent(value.value())
            .map_err(|error| codec_error(&format!("intent {}", intent_id.0), error))?;

        let work = match record.work {
            super::publish_queue::PublishQueueIntentRecordWork::Event {
                frozen,
                routing,
                sig_state,
            } => {
                let displaced = publish_queue_displaced
                    .get(key.value())
                    .map_err(persist_err)?
                    .map(|guard| decode_displaced(guard.value()))
                    .transpose()
                    .map_err(|error| codec_error("displaced event", error))?;
                crate::PublishQueueWork::Event {
                    frozen,
                    displaced: displaced.map(Box::new),
                    routing,
                    sig_state,
                }
            }
            super::publish_queue::PublishQueueIntentRecordWork::ReplaceableOperation {
                coordinate,
                materialization,
            } => crate::PublishQueueWork::ReplaceableOperation {
                coordinate,
                materialization: materialization.map(|current| crate::MaterializationWork {
                    receipt: crate::MaterializationReceipt {
                        materialization: current.current,
                        sig_state: current.sig_state,
                    },
                    routing: current.routing,
                }),
            },
        };

        out.push(PublishQueueIntent {
            intent_id,
            receipt_id: record.receipt_id,
            work,
            expected_pubkey: record.expected_pubkey,
            signing_identity_ref: record.signing_identity_ref,
            accepted_at: record.accepted_at,
        });
    }
    Ok(out)
}

pub(super) fn reattach_receipt(
    store: &RedbStore,
    receipt_id: u64,
) -> Result<Option<PublishQueueReceipt>, PersistenceError> {
    // NOT a Q4 "always empty" door: retention (not crash-survival) is
    // the contract — `PUBLISH_QUEUE_RECEIPTS` rows are never deleted by this
    // unit, so this is an ordinary durable read.
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let publish_queue_receipts = read_txn
        .open_table(PUBLISH_QUEUE_RECEIPTS)
        .map_err(persist_err)?;
    let key = receipt_key(receipt_id);
    let Some(encoded) = publish_queue_receipts
        .get(&key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_vec())
    else {
        return Ok(None);
    };
    let record =
        decode_receipt(&encoded).map_err(|error| codec_error("retained receipt", error))?;
    Ok(Some(PublishQueueReceipt {
        receipt_id,
        intent_id: record.intent_id,
        expected_pubkey: record.expected_pubkey,
        accepted_at: record.accepted_at,
        payload: record.payload,
    }))
}

pub(super) fn lookup_correlation(
    store: &RedbStore,
    token: &str,
) -> Result<Option<u64>, PersistenceError> {
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    // A store that has never accepted ANY correlated write never
    // created this table at all -- `ReadTransaction::open_table`
    // returns `TableDoesNotExist` in that case (unlike a write
    // transaction, a read transaction never creates tables). That is
    // exactly "no token has ever been journaled here", not a
    // persistence failure.
    let table = match read_txn.open_table(PUBLISH_QUEUE_CORRELATIONS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(err) => return Err(persist_err(err)),
    };
    let Some(encoded) = table
        .get(token.as_bytes())
        .map_err(persist_err)?
        .map(|guard| *guard.value())
    else {
        return Ok(None);
    };
    Ok(Some(u64::from_be_bytes(encoded)))
}

fn intern_relay_in_txn(
    publish_queue_meta: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    publish_queue_relays: &mut redb::Table<'_, &'static [u8; 4], &'static [u8]>,
    publish_queue_relay_ids: &mut redb::Table<'_, &'static [u8], &'static [u8; 4]>,
    relay: &RelayUrl,
) -> Result<PublishQueueRelayId, PersistenceError> {
    if let Some(encoded) = publish_queue_relay_ids
        .get(relay.as_str().as_bytes())
        .map_err(persist_err)?
    {
        let id = u32::from_be_bytes(*encoded.value());
        let key = relay_key(id);
        let forward = publish_queue_relays
            .get(&key)
            .map_err(persist_err)?
            .ok_or_else(|| {
                PersistenceError::invariant(
                    "delivery relay reverse map points at missing dictionary row",
                )
            })?;
        if decode_relay(forward.value()).map_err(|error| codec_error("relay", error))? != *relay {
            return Err(PersistenceError::invariant(
                "delivery relay dictionary directions disagree",
            ));
        }
        return Ok(id);
    }

    let raw = alloc_counter_in_txn(publish_queue_meta, NEXT_RELAY_ID_KEY)?;
    let id = PublishQueueRelayId::try_from(raw)
        .map_err(|_| PersistenceError::invariant("delivery relay id namespace exhausted"))?;
    let key = relay_key(id);
    let encoded = encode_relay(relay).map_err(|error| codec_error("relay", error))?;
    publish_queue_relays
        .insert(&key, encoded.as_slice())
        .map_err(persist_err)?;
    publish_queue_relay_ids
        .insert(relay.as_str().as_bytes(), &key)
        .map_err(persist_err)?;
    Ok(id)
}

pub(super) fn record_route_revision(
    store: &mut RedbStore,
    intent_id: IntentId,
    relays: BTreeSet<RelayUrl>,
) -> Result<PublishQueueRouteRevision, PersistenceError> {
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let revision = {
        let intents = write_txn
            .open_table(PUBLISH_QUEUE_INTENTS)
            .map_err(persist_err)?;
        let intent_key_value = intent_key(intent_id);
        if intents
            .get(&intent_key_value)
            .map_err(persist_err)?
            .is_none()
        {
            return Err(PersistenceError::invariant(
                "route revision intent is not open",
            ));
        }
        let mut revisions = write_txn
            .open_table(PUBLISH_QUEUE_ROUTE_REVISIONS)
            .map_err(persist_err)?;
        let (lower, upper) = route_revision_range(intent_id);
        let mut last = 0;
        for entry in revisions
            .range::<&[u8; 16]>(&lower..=&upper)
            .map_err(persist_err)?
        {
            #[cfg(test)]
            store
                .route_revision_range_rows
                .fetch_add(1, Ordering::Relaxed);
            let (key, value) = entry.map_err(persist_err)?;
            let (key_intent, ordinal) = parse_route_revision_key(key.value())
                .map_err(|error| codec_error("route revision key", error))?;
            if key_intent != intent_id {
                return Err(PersistenceError::invariant(
                    "route revision range does not match its value intent",
                ));
            }
            decode_route(value.value()).map_err(|error| codec_error("route revision", error))?;
            last = last.max(ordinal);
        }
        let ordinal = last
            .checked_add(1)
            .ok_or_else(|| PersistenceError::invariant("route revision ordinal exhausted"))?;
        let mut publish_queue_meta = write_txn
            .open_table(PUBLISH_QUEUE_META)
            .map_err(persist_err)?;
        let mut publish_queue_relays = write_txn
            .open_table(PUBLISH_QUEUE_RELAYS)
            .map_err(persist_err)?;
        let mut publish_queue_relay_ids = write_txn
            .open_table(PUBLISH_QUEUE_RELAY_IDS)
            .map_err(persist_err)?;
        let mut interned = Vec::with_capacity(relays.len());
        for relay in &relays {
            let id = intern_relay_in_txn(
                &mut publish_queue_meta,
                &mut publish_queue_relays,
                &mut publish_queue_relay_ids,
                relay,
            )?;
            interned.push((id, relay.clone()));
        }
        let ids = canonical_route_ids(interned.iter().map(|(id, _)| *id));
        let encoded = encode_route(&ids).map_err(|error| codec_error("route revision", error))?;
        revisions
            .insert(&route_revision_key(intent_id, ordinal), encoded.as_slice())
            .map_err(persist_err)?;
        PublishQueueRouteRevision {
            version: 1,
            intent_id,
            ordinal,
            relays,
        }
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::RouteRevisionBeforeCommit);
    commit_prepared(write_txn, revision)
}

pub(super) fn recover_route_revisions(
    store: &RedbStore,
    intent_id: IntentId,
) -> Result<Vec<PublishQueueRouteRevision>, PersistenceError> {
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let revisions = read_txn
        .open_table(PUBLISH_QUEUE_ROUTE_REVISIONS)
        .map_err(persist_err)?;
    let (lower, upper) = route_revision_range(intent_id);
    let mut recovered = Vec::new();
    for entry in revisions
        .range::<&[u8; 16]>(&lower..=&upper)
        .map_err(persist_err)?
    {
        #[cfg(test)]
        store
            .route_revision_range_rows
            .fetch_add(1, Ordering::Relaxed);
        let (key, value) = entry.map_err(persist_err)?;
        let (key_intent, ordinal) = parse_route_revision_key(key.value())
            .map_err(|error| codec_error("route revision key", error))?;
        if key_intent != intent_id {
            return Err(PersistenceError::invariant(
                "route revision range does not match its value intent",
            ));
        }
        let ids =
            decode_route(value.value()).map_err(|error| codec_error("route revision", error))?;
        let mut relays = BTreeSet::new();
        for id in ids {
            relays.insert(store.publish_queue_relay(id)?);
        }
        recovered.push(PublishQueueRouteRevision {
            version: 1,
            intent_id,
            ordinal,
            relays,
        });
    }
    recovered.sort_by_key(|revision| revision.ordinal);
    Ok(recovered)
}

pub(super) fn recover_attempts(
    store: &RedbStore,
    intent_id: IntentId,
) -> Result<Vec<PublishQueueAttempt>, PersistenceError> {
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let attempts = read_txn
        .open_table(PUBLISH_QUEUE_ATTEMPTS)
        .map_err(persist_err)?;
    let details = read_txn
        .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
        .map_err(persist_err)?;
    let (lower, upper) = attempt_range(intent_id);
    let mut recovered = Vec::new();
    for entry in attempts
        .range::<&[u8; 20]>(&lower..=&upper)
        .map_err(persist_err)?
    {
        #[cfg(test)]
        store.attempt_range_rows.fetch_add(1, Ordering::Relaxed);
        let (key, value) = entry.map_err(persist_err)?;
        let (key_intent, relay_id, ordinal) =
            parse_attempt_key(key.value()).map_err(|error| codec_error("attempt key", error))?;
        if key_intent != intent_id {
            return Err(PersistenceError::invariant(
                "delivery attempt range escaped intent prefix",
            ));
        }
        let relay = store.publish_queue_relay(relay_id)?;
        let (event, mut outcome) =
            decode_attempt(value.value()).map_err(|error| codec_error("attempt", error))?;
        if let Some(detail) = details.get(key.value()).map_err(persist_err)? {
            let detail = decode_attempt_details(detail.value(), intent_id, relay.clone(), ordinal)
                .map_err(|error| codec_error("attempt details", error))?;
            if let Some(terminal) = detail.terminal {
                outcome = terminal;
            }
        }
        recovered.push(PublishQueueAttempt {
            version: 1,
            intent_id,
            relay,
            ordinal,
            event,
            outcome,
        });
    }
    // Table-key layout is a storage detail (currently length-prefixed
    // relay text), not public recovery order. Match RedbStore and the
    // typed contract explicitly.
    recovered.sort_by(|left, right| {
        left.relay
            .cmp(&right.relay)
            .then(left.ordinal.cmp(&right.ordinal))
    });
    Ok(recovered)
}

/// Seed every lane the intent's route revisions imply, and validate the ones
/// that already exist.
///
/// The transaction commits only when a lane row was actually staged (#889).
/// Bootstrap is called once per open intent on every boot, and on a store
/// carrying thousands of them a lane set that is already complete is by far
/// the common case: committing there spent one fsync-durable transaction per
/// intent to make the database byte-identical to what it already was, which is
/// how a 15,311-intent store turned the engine thread's pre-command rebuild
/// into a 53-second block on the app's first call. Validation still runs
/// against the same isolated snapshot either way; only the barrier is
/// conditional, and an unstaged transaction has nothing for a commit to make
/// durable.
pub(super) fn bootstrap_publish_queue_lanes(
    store: &mut RedbStore,
    intent_id: IntentId,
) -> Result<Vec<PublishQueueLane>, PersistenceError> {
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let mut staged = false;
    let prepared = {
        let intents = write_txn
            .open_table(PUBLISH_QUEUE_INTENTS)
            .map_err(persist_err)?;
        let intent_bytes = intents
            .get(&intent_key(intent_id))
            .map_err(persist_err)?
            .map(|value| value.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("lane bootstrap intent is not open"))?;
        let event_id = decode_intent(&intent_bytes)
            .map_err(|error| codec_error("lane bootstrap intent", error))?
            .current_event_id()
            .ok_or_else(|| PersistenceError::invariant("lane bootstrap has no current event"))?;
        let route_revisions = write_txn
            .open_table(PUBLISH_QUEUE_ROUTE_REVISIONS)
            .map_err(persist_err)?;
        let attempts_table = write_txn
            .open_table(PUBLISH_QUEUE_ATTEMPTS)
            .map_err(persist_err)?;
        let mut lanes = write_txn
            .open_table(PUBLISH_QUEUE_LANES)
            .map_err(persist_err)?;
        let details = write_txn
            .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let (attempt_lower, attempt_upper) = attempt_range(intent_id);
        let mut details_by_key = BTreeMap::new();
        for row in details
            .range::<&[u8; 20]>(&attempt_lower..=&attempt_upper)
            .map_err(persist_err)?
        {
            let (key, value) = row.map_err(persist_err)?;
            let (key_intent, relay_id, ordinal) = parse_attempt_key(key.value())
                .map_err(|error| codec_error("attempt detail key", error))?;
            if key_intent != intent_id {
                return Err(PersistenceError::invariant(
                    "attempt detail range escaped intent prefix",
                ));
            }
            let relay = store.publish_queue_relay(relay_id)?;
            let detail = decode_attempt_details(value.value(), intent_id, relay, ordinal)
                .map_err(|error| codec_error("attempt details", error))?;
            details_by_key.insert((relay_id, ordinal), detail);
        }
        let mut attempts: Vec<(PublishQueueRelayId, PublishQueueAttempt)> = Vec::new();
        for row in attempts_table
            .range::<&[u8; 20]>(&attempt_lower..=&attempt_upper)
            .map_err(persist_err)?
        {
            #[cfg(test)]
            store.attempt_range_rows.fetch_add(1, Ordering::Relaxed);
            let (key, value) = row.map_err(persist_err)?;
            let (key_intent, relay_id, ordinal) = parse_attempt_key(key.value())
                .map_err(|error| codec_error("attempt key", error))?;
            if key_intent != intent_id {
                return Err(PersistenceError::invariant(
                    "attempt range escaped intent prefix",
                ));
            }
            let relay = store.publish_queue_relay(relay_id)?;
            let (event, mut outcome) =
                decode_attempt(value.value()).map_err(|error| codec_error("attempt", error))?;
            if let Some(terminal) = details_by_key
                .get(&(relay_id, ordinal))
                .and_then(|detail| detail.terminal.clone())
            {
                outcome = terminal;
            }
            attempts.push((
                relay_id,
                PublishQueueAttempt {
                    version: 1,
                    intent_id,
                    relay,
                    ordinal,
                    event,
                    outcome,
                },
            ));
        }
        attempts.sort_by(|(_, left), (_, right)| {
            left.relay
                .cmp(&right.relay)
                .then(left.ordinal.cmp(&right.ordinal))
        });
        let (route_lower, route_upper) = route_revision_range(intent_id);
        let mut relay_ids = BTreeSet::new();
        for row in route_revisions
            .range::<&[u8; 16]>(&route_lower..=&route_upper)
            .map_err(persist_err)?
        {
            #[cfg(test)]
            store
                .route_revision_range_rows
                .fetch_add(1, Ordering::Relaxed);
            let (key, value) = row.map_err(persist_err)?;
            let (key_intent, _) = parse_route_revision_key(key.value())
                .map_err(|error| codec_error("route revision key", error))?;
            if key_intent != intent_id {
                return Err(PersistenceError::invariant(
                    "route revision range escaped intent prefix",
                ));
            }
            relay_ids.extend(
                decode_route(value.value())
                    .map_err(|error| codec_error("route revision", error))?,
            );
        }
        // #867: the current schema writes an attempt's base row, its detail
        // row, and its route revision together. Bootstrap therefore VERIFIES
        // that shape instead of adopting a pre-detail one — a missing detail
        // row is corruption of the current epoch, not an older writer to be
        // accommodated with a synthesized shell.
        let attempt_keys: BTreeSet<_> = attempts
            .iter()
            .map(|(relay_id, attempt)| (*relay_id, attempt.ordinal))
            .collect();
        for (relay_id, attempt) in &attempts {
            if !details_by_key.contains_key(&(*relay_id, attempt.ordinal)) {
                return Err(PersistenceError::invariant(
                    "attempt row is missing its detail row",
                ));
            }
            if !relay_ids.contains(relay_id) {
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
        for relay_id in &relay_ids {
            let relay = store.publish_queue_relay(*relay_id)?;
            let key = PublishQueueLaneKey {
                intent_id,
                event_id,
                relay,
            };
            let storage_key = lane_key(intent_id, *relay_id);
            let lane_attempts: Vec<_> = attempts
                .iter()
                .filter(|(attempt_relay_id, _)| attempt_relay_id == relay_id)
                .map(|(_, attempt)| attempt)
                .collect();
            let live_count = lane_attempts
                .iter()
                .filter(|attempt| {
                    crate::attempt_is_live(
                        attempt,
                        details_by_key.get(&(*relay_id, attempt.ordinal)),
                    )
                })
                .count();
            if live_count > 1
                || (live_count == 1
                    && lane_attempts.last().is_some_and(|attempt| {
                        !crate::attempt_is_live(
                            attempt,
                            details_by_key.get(&(*relay_id, attempt.ordinal)),
                        )
                    }))
            {
                return Err(PersistenceError::invariant(
                    "contradictory live attempt history",
                ));
            }
            if let Some(existing) = lanes.get(&storage_key).map_err(persist_err)? {
                let (lane_event_id, revision, last_ordinal, state) =
                    decode_lane(existing.value()).map_err(|error| codec_error("lane", error))?;
                if lane_event_id != event_id {
                    return Err(PersistenceError::invariant(
                        "delivery lane belongs to a predecessor event",
                    ));
                }
                let lane = PublishQueueLane {
                    version: 1,
                    key: key.clone(),
                    revision,
                    last_ordinal,
                    state,
                };
                let max = lane_attempts.last().map_or(0, |attempt| attempt.ordinal);
                if lane.last_ordinal != max {
                    return Err(PersistenceError::invariant(
                        "delivery lane cursor disagrees with retained attempt history",
                    ));
                }
                match lane_attempts.last() {
                    Some(attempt) if attempt.outcome != PublishQueueAttemptOutcome::Started => {
                        if lane.state
                            != (PublishQueueLaneState::Terminal {
                                ordinal: attempt.ordinal,
                                outcome: PublishQueueTerminalOutcome::from_attempt(
                                    attempt.outcome.clone(),
                                )?,
                            })
                        {
                            return Err(PersistenceError::invariant(
                                "terminal attempt and lane state disagree",
                            ));
                        }
                    }
                    _ if matches!(
                        lane.state,
                        PublishQueueLaneState::Terminal {
                            outcome: PublishQueueTerminalOutcome::AuthDenied(_),
                            ..
                        }
                    ) => {}
                    _ if matches!(lane.state, PublishQueueLaneState::Terminal { .. }) => {
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
            let lane = PublishQueueLane {
                version: 1,
                key,
                revision: 1,
                last_ordinal: 0,
                state: PublishQueueLaneState::WaitingConnection,
            };
            let encoded = encode_lane(
                lane.key.event_id,
                lane.revision,
                lane.last_ordinal,
                &lane.state,
            )
            .map_err(|error| codec_error("lane", error))?;
            lanes
                .insert(&storage_key, encoded.as_slice())
                .map_err(persist_err)?;
            staged = true;
        }

        // Construct the complete value this mutating door will return while
        // the transaction is still uncommitted. This deliberately ranges the
        // whole intent prefix rather than returning only the rows staged
        // above: an orphan, malformed, or intent-mismatched row must refuse
        // before any newly prepared lane becomes durable.
        let (lane_lower, lane_upper) = lane_range(intent_id);
        let mut recovered = Vec::new();
        for row in lanes
            .range::<&[u8; 12]>(&lane_lower..=&lane_upper)
            .map_err(persist_err)?
        {
            let (key, value) = row.map_err(persist_err)?;
            let (key_intent, relay_id) =
                parse_lane_key(key.value()).map_err(|error| codec_error("lane key", error))?;
            if key_intent != intent_id {
                return Err(PersistenceError::invariant(
                    "lane range escaped intent prefix",
                ));
            }
            if !relay_ids.contains(&relay_id) {
                return Err(PersistenceError::invariant(
                    "lane relay is absent from every route revision",
                ));
            }
            let relay = store.publish_queue_relay(relay_id)?;
            let (lane_event_id, revision, last_ordinal, state) =
                decode_lane(value.value()).map_err(|error| codec_error("lane", error))?;
            recovered.push(PublishQueueLane {
                version: 1,
                key: PublishQueueLaneKey {
                    intent_id,
                    event_id: lane_event_id,
                    relay,
                },
                revision,
                last_ordinal,
                state,
            });
        }
        recovered.sort_by(|left, right| left.key.relay.cmp(&right.key.relay));
        recovered
    };
    if !staged {
        write_txn.abort().map_err(persist_err)?;
        #[cfg(test)]
        store
            .unstaged_lane_bootstraps
            .fetch_add(1, Ordering::Relaxed);
        return Ok(prepared);
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneBootstrapBeforeCommit);
    commit_prepared(write_txn, prepared)
}

pub(super) fn recover_publish_queue_lanes(
    store: &RedbStore,
    intent_id: IntentId,
) -> Result<Vec<PublishQueueLane>, PersistenceError> {
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let lanes = read_txn
        .open_table(PUBLISH_QUEUE_LANES)
        .map_err(persist_err)?;
    let (lower, upper) = lane_range(intent_id);
    let mut recovered = Vec::new();
    for row in lanes
        .range::<&[u8; 12]>(&lower..=&upper)
        .map_err(persist_err)?
    {
        let (key, value) = row.map_err(persist_err)?;
        let (key_intent, relay_id) =
            parse_lane_key(key.value()).map_err(|error| codec_error("lane key", error))?;
        if key_intent != intent_id {
            return Err(PersistenceError::invariant(
                "lane range escaped intent prefix",
            ));
        }
        let (lane_event_id, revision, last_ordinal, state) =
            decode_lane(value.value()).map_err(|error| codec_error("lane", error))?;
        recovered.push(PublishQueueLane {
            version: 1,
            key: PublishQueueLaneKey {
                intent_id,
                event_id: lane_event_id,
                relay: store.publish_queue_relay(relay_id)?,
            },
            revision,
            last_ordinal,
            state,
        });
    }
    recovered.sort_by(|a, b| a.key.relay.cmp(&b.key.relay));
    Ok(recovered)
}

pub(super) fn due_publish_queue_deadlines(
    store: &RedbStore,
    now: Timestamp,
    limit: usize,
) -> Result<Vec<PublishQueueDeadline>, PersistenceError> {
    if limit > 1_024 {
        return Err(PersistenceError::invariant(
            "deadline read limit exceeds 1024",
        ));
    }
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let deadlines = read_txn
        .open_table(PUBLISH_QUEUE_DEADLINES)
        .map_err(persist_err)?;
    let deadlines_by_intent = read_txn
        .open_table(PUBLISH_QUEUE_DEADLINES_BY_INTENT)
        .map_err(persist_err)?;
    if deadlines.len().map_err(persist_err)? != deadlines_by_intent.len().map_err(persist_err)? {
        return Err(PersistenceError::invariant(
            "deadline index cardinalities disagree",
        ));
    }
    let lanes = read_txn
        .open_table(PUBLISH_QUEUE_LANES)
        .map_err(persist_err)?;
    let (lower, upper) = deadline_due_range(now);
    let mut recovered = Vec::new();
    for row in deadlines
        .range::<&[u8; 20]>(&lower..=&upper)
        .map_err(persist_err)?
    {
        if recovered.len() == limit {
            break;
        }
        let (key, value) = row.map_err(persist_err)?;
        let (at, intent_id, relay_id) =
            parse_deadline_key(key.value()).map_err(|error| codec_error("deadline key", error))?;
        let (lane_revision, kind) =
            decode_deadline(value.value()).map_err(|error| codec_error("deadline", error))?;
        let secondary_key = deadline_by_intent_key(intent_id, at, relay_id);
        let secondary = deadlines_by_intent
            .get(&secondary_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("deadline is missing by-intent index"))?;
        if decode_deadline(&secondary).map_err(|error| codec_error("deadline index", error))?
            != (lane_revision, kind)
        {
            return Err(PersistenceError::invariant("deadline indexes disagree"));
        }
        let lane_storage_key = lane_key(intent_id, relay_id);
        let lane_encoded = lanes
            .get(&lane_storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("deadline references missing lane"))?;
        let (lane_event_id, revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        let relay = store.publish_queue_relay(relay_id)?;
        let lane = PublishQueueLane {
            version: 1,
            key: PublishQueueLaneKey {
                intent_id,
                event_id: lane_event_id,
                relay: relay.clone(),
            },
            revision,
            last_ordinal,
            state,
        };
        let deadline = PublishQueueDeadline {
            at,
            key: PublishQueueLaneKey {
                intent_id,
                event_id: lane_event_id,
                relay,
            },
            lane_revision,
            kind,
        };
        if lane_deadline(&lane).as_ref() != Some(&deadline) {
            return Err(PersistenceError::invariant("deadline and lane disagree"));
        }
        recovered.push(deadline);
    }
    Ok(recovered)
}

pub(super) fn next_publish_queue_deadline(
    store: &RedbStore,
) -> Result<Option<Timestamp>, PersistenceError> {
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let deadlines = read_txn
        .open_table(PUBLISH_QUEUE_DEADLINES)
        .map_err(persist_err)?;
    let deadlines_by_intent = read_txn
        .open_table(PUBLISH_QUEUE_DEADLINES_BY_INTENT)
        .map_err(persist_err)?;
    if deadlines.len().map_err(persist_err)? != deadlines_by_intent.len().map_err(persist_err)? {
        return Err(PersistenceError::invariant(
            "deadline index cardinalities disagree",
        ));
    }
    let lanes = read_txn
        .open_table(PUBLISH_QUEUE_LANES)
        .map_err(persist_err)?;
    let mut rows = deadlines.iter().map_err(persist_err)?;
    let Some(row) = rows.next() else {
        return Ok(None);
    };
    let (key, value) = row.map_err(persist_err)?;
    let (at, intent_id, relay_id) =
        parse_deadline_key(key.value()).map_err(|error| codec_error("deadline key", error))?;
    let (lane_revision, kind) =
        decode_deadline(value.value()).map_err(|error| codec_error("deadline", error))?;
    let secondary_key = deadline_by_intent_key(intent_id, at, relay_id);
    let secondary = deadlines_by_intent
        .get(&secondary_key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_vec())
        .ok_or_else(|| PersistenceError::invariant("deadline is missing by-intent index"))?;
    if decode_deadline(&secondary).map_err(|error| codec_error("deadline index", error))?
        != (lane_revision, kind)
    {
        return Err(PersistenceError::invariant("deadline indexes disagree"));
    }
    let lane_storage_key = lane_key(intent_id, relay_id);
    let lane_encoded = lanes
        .get(&lane_storage_key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_vec())
        .ok_or_else(|| PersistenceError::invariant("deadline references missing lane"))?;
    let (lane_event_id, revision, last_ordinal, state) =
        decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
    let relay = store.publish_queue_relay(relay_id)?;
    let lane = PublishQueueLane {
        version: 1,
        key: PublishQueueLaneKey {
            intent_id,
            event_id: lane_event_id,
            relay: relay.clone(),
        },
        revision,
        last_ordinal,
        state,
    };
    let deadline = PublishQueueDeadline {
        at,
        key: PublishQueueLaneKey {
            intent_id,
            event_id: lane_event_id,
            relay,
        },
        lane_revision,
        kind,
    };
    if lane_deadline(&lane).as_ref() != Some(&deadline) {
        return Err(PersistenceError::invariant("deadline and lane disagree"));
    }
    Ok(Some(at))
}

pub(super) fn set_lane_waiting(
    store: &mut RedbStore,
    key: &PublishQueueLaneKey,
    expected_revision: u64,
    auth: bool,
) -> Result<PublishQueueLane, PersistenceError> {
    store.persist_lane_state(
        key,
        expected_revision,
        if auth {
            PublishQueueLaneState::WaitingAuth
        } else {
            PublishQueueLaneState::WaitingConnection
        },
    )
}

pub(super) fn set_lane_eligible(
    store: &mut RedbStore,
    key: &PublishQueueLaneKey,
    expected_revision: u64,
    since: Timestamp,
) -> Result<PublishQueueLane, PersistenceError> {
    store.persist_lane_state(
        key,
        expected_revision,
        PublishQueueLaneState::Eligible { since },
    )
}

pub(super) fn set_lane_transient(
    store: &mut RedbStore,
    key: &PublishQueueLaneKey,
    expected_revision: u64,
    ordinal: u64,
    eligible_at: Timestamp,
    cause: PublishQueueTransientCause,
    raw_reason: Option<String>,
) -> Result<PublishQueueLane, PersistenceError> {
    if raw_reason
        .as_ref()
        .is_some_and(|reason| reason.len() > 4_096)
    {
        return Err(PersistenceError::invariant(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let lane = {
        let mut lanes = write_txn
            .open_table(PUBLISH_QUEUE_LANES)
            .map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let mut details = write_txn
            .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (lane_event_id, _, last_ordinal, _) =
            decode_lane(&encoded).map_err(|error| codec_error("lane", error))?;
        if lane_event_id != key.event_id || last_ordinal != ordinal {
            return Err(PersistenceError::invariant("stale attempt ordinal"));
        }
        if ordinal > 0 {
            let detail_key = attempt_key(key.intent_id, relay_id, ordinal);
            let detail_encoded = details
                .get(&detail_key)
                .map_err(persist_err)?
                .map(|guard| guard.value().to_vec())
                .ok_or_else(|| PersistenceError::invariant("attempt detail row not found"))?;
            let mut detail =
                decode_attempt_details(&detail_encoded, key.intent_id, key.relay.clone(), ordinal)
                    .map_err(|error| codec_error("attempt details", error))?;
            detail.transient = Some(PublishQueueAttemptTransient {
                eligible_at,
                cause,
                raw_reason: raw_reason.clone(),
            });
            let detail_encoded = encode_attempt_details(&detail)
                .map_err(|error| codec_error("attempt details", error))?;
            details
                .insert(&detail_key, detail_encoded.as_slice())
                .map_err(persist_err)?;
        }
        replace_lane_in_txn(
            &mut lanes,
            &mut deadlines,
            &mut deadlines_by_intent,
            key,
            relay_id,
            expected_revision,
            PublishQueueLaneState::Transient {
                ordinal,
                eligible_at,
                cause,
                raw_reason,
            },
        )?
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneTransitionBeforeCommit);
    commit_prepared(write_txn, lane)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn suspend_lane_attempt(
    store: &mut RedbStore,
    key: &PublishQueueLaneKey,
    expected_revision: u64,
    ordinal: u64,
    at: Timestamp,
    cause: PublishQueueTransientCause,
    raw_reason: Option<String>,
    auth: bool,
) -> Result<PublishQueueLane, PersistenceError> {
    if raw_reason
        .as_ref()
        .is_some_and(|reason| reason.len() > 4_096)
    {
        return Err(PersistenceError::invariant(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let lane = {
        let mut lanes = write_txn
            .open_table(PUBLISH_QUEUE_LANES)
            .map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let mut details = write_txn
            .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (lane_event_id, revision, last_ordinal, _) =
            decode_lane(&encoded).map_err(|error| codec_error("lane", error))?;
        if lane_event_id != key.event_id
            || revision != expected_revision
            || last_ordinal != ordinal
            || ordinal == 0
        {
            return Err(PersistenceError::invariant("stale suspended attempt"));
        }
        let detail_key = attempt_key(key.intent_id, relay_id, ordinal);
        let detail_encoded = details
            .get(&detail_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("attempt detail row not found"))?;
        let mut detail =
            decode_attempt_details(&detail_encoded, key.intent_id, key.relay.clone(), ordinal)
                .map_err(|error| codec_error("attempt details", error))?;
        detail.transient = Some(PublishQueueAttemptTransient {
            eligible_at: at,
            cause,
            raw_reason,
        });
        let detail_encoded = encode_attempt_details(&detail)
            .map_err(|error| codec_error("attempt details", error))?;
        details
            .insert(&detail_key, detail_encoded.as_slice())
            .map_err(persist_err)?;
        replace_lane_in_txn(
            &mut lanes,
            &mut deadlines,
            &mut deadlines_by_intent,
            key,
            relay_id,
            expected_revision,
            if auth {
                PublishQueueLaneState::WaitingAuth
            } else {
                PublishQueueLaneState::WaitingConnection
            },
        )?
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneTransitionBeforeCommit);
    commit_prepared(write_txn, lane)
}

pub(super) fn start_lane_attempt(
    store: &mut RedbStore,
    key: &PublishQueueLaneKey,
    expected_revision: u64,
    event: Event,
    started_at: Timestamp,
) -> Result<(PublishQueueAttempt, PublishQueueLane), PersistenceError> {
    let intent = {
        let read_txn = store.database()?.begin_read().map_err(persist_err)?;
        let intents = read_txn
            .open_table(PUBLISH_QUEUE_INTENTS)
            .map_err(persist_err)?;
        let storage_key = intent_key(key.intent_id);
        let encoded = intents
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("attempt intent is not open"))?;
        decode_intent(&encoded).map_err(|error| codec_error("attempt intent", error))?
    };
    let (intent_event, sig_state) = match &intent.work {
        super::publish_queue::PublishQueueIntentRecordWork::Event {
            frozen, sig_state, ..
        } => (frozen.clone(), *sig_state),
        super::publish_queue::PublishQueueIntentRecordWork::ReplaceableOperation {
            materialization: Some(materialization),
            ..
        } => {
            let rows = store.query(&nostr::Filter::new().id(materialization.current.event_id))?;
            let stored = rows.into_iter().next().ok_or_else(|| {
                PersistenceError::invariant("operation materialization event is missing")
            })?;
            (stored.event, materialization.sig_state)
        }
        super::publish_queue::PublishQueueIntentRecordWork::ReplaceableOperation {
            materialization: None,
            ..
        } => {
            return Err(PersistenceError::invariant(
                "operation intent has no current materialization",
            ))
        }
    };
    if sig_state != IntentSigState::Signed || intent_event != event {
        return Err(PersistenceError::invariant(
            "attempt bytes are not the intent's promoted signed bytes",
        ));
    }
    event
        .verify()
        .map_err(|e| PersistenceError::invariant(format!("attempt event is invalid: {e}")))?;
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let (attempt, lane) = {
        let mut attempts = write_txn
            .open_table(PUBLISH_QUEUE_ATTEMPTS)
            .map_err(persist_err)?;
        let mut details = write_txn
            .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let mut lanes = write_txn
            .open_table(PUBLISH_QUEUE_LANES)
            .map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (lane_event_id, revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        if lane_event_id != key.event_id
            || revision != expected_revision
            || !matches!(state, PublishQueueLaneState::Eligible { .. })
        {
            return Err(PersistenceError::invariant(
                "lane is not expected eligible cursor",
            ));
        }
        let ordinal = last_ordinal
            .checked_add(1)
            .ok_or_else(|| PersistenceError::invariant("attempt ordinal exhausted"))?;
        let attempt = PublishQueueAttempt {
            version: 1,
            intent_id: key.intent_id,
            relay: key.relay.clone(),
            ordinal,
            event,
            outcome: PublishQueueAttemptOutcome::Started,
        };
        let attempt_key_value = attempt_key(key.intent_id, relay_id, ordinal);
        let attempt_encoded = encode_attempt(&attempt.event, &attempt.outcome)
            .map_err(|error| codec_error("attempt", error))?;
        attempts
            .insert(&attempt_key_value, attempt_encoded.as_slice())
            .map_err(persist_err)?;
        let detail = PublishQueueAttemptDetails {
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
        let detail_encoded = encode_attempt_details(&detail)
            .map_err(|error| codec_error("attempt details", error))?;
        details
            .insert(&attempt_key_value, detail_encoded.as_slice())
            .map_err(persist_err)?;
        let advanced = replace_lane_in_txn(
            &mut lanes,
            &mut deadlines,
            &mut deadlines_by_intent,
            key,
            relay_id,
            expected_revision,
            PublishQueueLaneState::InFlight {
                ordinal,
                phase: PublishQueueInFlightPhase::AwaitingHandoff,
            },
        )?;
        (attempt, advanced)
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneStartBeforeCommit);
    commit_prepared(write_txn, (attempt, lane))
}

pub(super) fn record_lane_handoff(
    store: &mut RedbStore,
    key: &PublishQueueLaneKey,
    expected_revision: u64,
    ordinal: u64,
    detail: PublishQueueAttemptHandoff,
    next: PublishQueuePostHandoffState,
) -> Result<PublishQueueLane, PersistenceError> {
    if matches!(
        &next,
        PublishQueuePostHandoffState::Transient {
            raw_reason: Some(reason),
            ..
        } if reason.len() > 4_096
    ) {
        return Err(PersistenceError::invariant(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let lane = {
        let mut details = write_txn
            .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let mut lanes = write_txn
            .open_table(PUBLISH_QUEUE_LANES)
            .map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let lane_storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&lane_storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (lane_event_id, revision, last_ordinal, current_state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        if lane_event_id != key.event_id || revision != expected_revision || last_ordinal != ordinal
        {
            return Err(PersistenceError::invariant("stale lane handoff"));
        }
        if !matches!(
            current_state,
            PublishQueueLaneState::InFlight {
                ordinal: current,
                phase: PublishQueueInFlightPhase::AwaitingHandoff,
            } if current == ordinal
        ) {
            return Err(PersistenceError::invariant("lane is not awaiting handoff"));
        }
        let attempt_key_value = attempt_key(key.intent_id, relay_id, ordinal);
        let detail_encoded = details
            .get(&attempt_key_value)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("attempt detail row not found"))?;
        let mut recovered_detail =
            decode_attempt_details(&detail_encoded, key.intent_id, key.relay.clone(), ordinal)
                .map_err(|error| codec_error("attempt details", error))?;
        if let Some(existing) = &recovered_detail.handoff {
            if existing != &detail {
                return Err(PersistenceError::invariant("conflicting handoff evidence"));
            }
        } else {
            recovered_detail.handoff = Some(detail);
        }
        let state = match next {
            PublishQueuePostHandoffState::WaitingConnection => {
                PublishQueueLaneState::WaitingConnection
            }
            PublishQueuePostHandoffState::WaitingAuth => PublishQueueLaneState::WaitingAuth,
            PublishQueuePostHandoffState::Eligible { since } => {
                PublishQueueLaneState::Eligible { since }
            }
            PublishQueuePostHandoffState::AwaitingAck { deadline } => {
                PublishQueueLaneState::InFlight {
                    ordinal,
                    phase: PublishQueueInFlightPhase::AwaitingAck { deadline },
                }
            }
            PublishQueuePostHandoffState::Transient {
                eligible_at,
                cause,
                raw_reason,
            } => PublishQueueLaneState::Transient {
                ordinal,
                eligible_at,
                cause,
                raw_reason,
            },
            PublishQueuePostHandoffState::Terminal {
                outcome,
                finished_at,
            } => {
                if outcome == PublishQueueAttemptOutcome::Started {
                    return Err(PersistenceError::invariant("Started is not terminal"));
                }
                recovered_detail.finished_at = Some(finished_at);
                recovered_detail.terminal = Some(outcome.clone());
                PublishQueueLaneState::Terminal {
                    ordinal,
                    outcome: PublishQueueTerminalOutcome::from_attempt(outcome)?,
                }
            }
        };
        let lane = replace_lane_in_txn(
            &mut lanes,
            &mut deadlines,
            &mut deadlines_by_intent,
            key,
            relay_id,
            expected_revision,
            state,
        )?;
        if lane.last_ordinal != ordinal {
            return Err(PersistenceError::invariant("stale lane handoff ordinal"));
        }
        let detail_encoded = encode_attempt_details(&recovered_detail)
            .map_err(|error| codec_error("attempt details", error))?;
        details
            .insert(&attempt_key_value, detail_encoded.as_slice())
            .map_err(persist_err)?;
        lane
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneHandoffBeforeCommit);
    commit_prepared(write_txn, lane)
}

pub(super) fn finish_lane_attempt(
    store: &mut RedbStore,
    key: &PublishQueueLaneKey,
    expected_revision: u64,
    ordinal: u64,
    outcome: PublishQueueAttemptOutcome,
    finished_at: Timestamp,
) -> Result<PublishQueueLane, PersistenceError> {
    if outcome == PublishQueueAttemptOutcome::Started {
        return Err(PersistenceError::invariant("Started is not terminal"));
    }
    let lane_outcome = PublishQueueTerminalOutcome::from_attempt(outcome.clone())?;
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let lane = {
        let mut details = write_txn
            .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let mut lanes = write_txn
            .open_table(PUBLISH_QUEUE_LANES)
            .map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (lane_event_id, revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        let current = PublishQueueLane {
            version: 1,
            key: key.clone(),
            revision,
            last_ordinal,
            state,
        };
        if lane_event_id != key.event_id
            || current.revision != expected_revision
            || current.last_ordinal != ordinal
        {
            return Err(PersistenceError::invariant("stale terminal attempt"));
        }
        let detail_key = attempt_key(key.intent_id, relay_id, ordinal);
        let detail_encoded = details
            .get(&detail_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("attempt detail row not found"))?;
        let mut detail =
            decode_attempt_details(&detail_encoded, key.intent_id, key.relay.clone(), ordinal)
                .map_err(|error| codec_error("attempt details", error))?;
        if let Some(existing) = &detail.terminal {
            if existing == &outcome
                && detail.finished_at == Some(finished_at)
                && matches!(
                    current.state,
                    PublishQueueLaneState::Terminal {
                        ordinal: current_ordinal,
                        outcome: ref current_outcome,
                    } if current_ordinal == ordinal && current_outcome == &lane_outcome
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
            let detail_encoded = encode_attempt_details(&detail)
                .map_err(|error| codec_error("attempt details", error))?;
            details
                .insert(&detail_key, detail_encoded.as_slice())
                .map_err(persist_err)?;
            replace_lane_in_txn(
                &mut lanes,
                &mut deadlines,
                &mut deadlines_by_intent,
                key,
                relay_id,
                expected_revision,
                PublishQueueLaneState::Terminal {
                    ordinal,
                    outcome: lane_outcome,
                },
            )?
        }
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::FinishAttemptBeforeCommit);
    commit_prepared(write_txn, lane)
}

pub(super) fn deny_lane_auth(
    store: &mut RedbStore,
    key: &PublishQueueLaneKey,
    expected_revision: u64,
    denial: AuthDenial,
) -> Result<PublishQueueLane, PersistenceError> {
    if denial.reason.len() > 4_096 {
        return Err(PersistenceError::invariant(
            "authentication denial reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let lane = {
        let mut lanes = write_txn
            .open_table(PUBLISH_QUEUE_LANES)
            .map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (lane_event_id, revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        let current = PublishQueueLane {
            version: 1,
            key: key.clone(),
            revision,
            last_ordinal,
            state,
        };
        if lane_event_id != key.event_id || current.revision != expected_revision {
            return Err(PersistenceError::invariant(
                "authentication denial lane revision is stale",
            ));
        }
        if matches!(
            current.state,
            PublishQueueLaneState::Terminal {
                outcome: PublishQueueTerminalOutcome::AuthDenied(ref existing),
                ..
            } if existing == &denial
        ) {
            current
        } else {
            if !matches!(current.state, PublishQueueLaneState::WaitingAuth) {
                return Err(PersistenceError::invariant(
                    "lane is not waiting for authentication",
                ));
            }
            replace_lane_in_txn(
                &mut lanes,
                &mut deadlines,
                &mut deadlines_by_intent,
                key,
                relay_id,
                expected_revision,
                PublishQueueLaneState::Terminal {
                    ordinal: current.last_ordinal,
                    outcome: PublishQueueTerminalOutcome::AuthDenied(denial),
                },
            )?
        }
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::DenyLaneAuthBeforeCommit);
    commit_prepared(write_txn, lane)
}

pub(super) fn recover_attempt_details(
    store: &RedbStore,
    intent_id: IntentId,
) -> Result<Vec<PublishQueueAttemptDetails>, PersistenceError> {
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let details = read_txn
        .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
        .map_err(persist_err)?;
    let (lower, upper) = attempt_range(intent_id);
    let mut recovered = Vec::new();
    for row in details
        .range::<&[u8; 20]>(&lower..=&upper)
        .map_err(persist_err)?
    {
        let (key, value) = row.map_err(persist_err)?;
        let (key_intent, relay_id, ordinal) = parse_attempt_key(key.value())
            .map_err(|error| codec_error("attempt detail key", error))?;
        if key_intent != intent_id {
            return Err(PersistenceError::invariant(
                "attempt detail range escaped intent prefix",
            ));
        }
        recovered.push(
            decode_attempt_details(
                value.value(),
                intent_id,
                store.publish_queue_relay(relay_id)?,
                ordinal,
            )
            .map_err(|error| codec_error("attempt details", error))?,
        );
    }
    recovered.sort_by(|a, b| a.relay.cmp(&b.relay).then(a.ordinal.cmp(&b.ordinal)));
    Ok(recovered)
}

pub(super) fn close_unroutable_intent(
    store: &mut RedbStore,
    intent_id: IntentId,
) -> Result<CloseIntentOutcome, PersistenceError> {
    close_intent(store, intent_id, LaneShape::None)
}

pub(super) fn close_terminal_intent(
    store: &mut RedbStore,
    intent_id: IntentId,
) -> Result<CloseIntentOutcome, PersistenceError> {
    close_intent(store, intent_id, LaneShape::AllTerminal)
}

/// Which lane shape a close door demands. Both are facts this crate checks
/// for itself, so no caller can talk the store into deleting open work.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LaneShape {
    /// No lane exists at all -- the intent resolved to nowhere.
    None,
    /// At least one lane exists and every one of them is terminal.
    AllTerminal,
}

fn terminal_intent_evidence_bytes(
    write_txn: &redb::WriteTransaction,
    intent_id: IntentId,
) -> Result<u64, PersistenceError> {
    let mut total = 0u64;
    let (attempt_lower, attempt_upper) = attempt_range(intent_id);
    let attempts = write_txn
        .open_table(PUBLISH_QUEUE_ATTEMPTS)
        .map_err(persist_err)?;
    for row in attempts
        .range::<&[u8; 20]>(&attempt_lower..=&attempt_upper)
        .map_err(persist_err)?
    {
        let (key, value) = row.map_err(persist_err)?;
        total = total
            .checked_add((key.value().len() + value.value().len()) as u64)
            .ok_or_else(|| PersistenceError::invariant("terminal evidence bytes overflow"))?;
    }
    let details = write_txn
        .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
        .map_err(persist_err)?;
    for row in details
        .range::<&[u8; 20]>(&attempt_lower..=&attempt_upper)
        .map_err(persist_err)?
    {
        let (key, value) = row.map_err(persist_err)?;
        total = total
            .checked_add((key.value().len() + value.value().len()) as u64)
            .ok_or_else(|| PersistenceError::invariant("terminal evidence bytes overflow"))?;
    }
    let (lane_lower, lane_upper) = lane_range(intent_id);
    let lanes = write_txn
        .open_table(PUBLISH_QUEUE_LANES)
        .map_err(persist_err)?;
    for row in lanes
        .range::<&[u8; 12]>(&lane_lower..=&lane_upper)
        .map_err(persist_err)?
    {
        let (key, value) = row.map_err(persist_err)?;
        total = total
            .checked_add((key.value().len() + value.value().len()) as u64)
            .ok_or_else(|| PersistenceError::invariant("terminal evidence bytes overflow"))?;
    }
    let (route_lower, route_upper) = route_revision_range(intent_id);
    let routes = write_txn
        .open_table(PUBLISH_QUEUE_ROUTE_REVISIONS)
        .map_err(persist_err)?;
    for row in routes
        .range::<&[u8; 16]>(&route_lower..=&route_upper)
        .map_err(persist_err)?
    {
        let (key, value) = row.map_err(persist_err)?;
        total = total
            .checked_add((key.value().len() + value.value().len()) as u64)
            .ok_or_else(|| PersistenceError::invariant("terminal evidence bytes overflow"))?;
    }
    Ok(total)
}

fn close_intent(
    store: &mut RedbStore,
    intent_id: IntentId,
    shape: LaneShape,
) -> Result<CloseIntentOutcome, PersistenceError> {
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let result = {
        let mut intents = write_txn
            .open_table(PUBLISH_QUEUE_INTENTS)
            .map_err(persist_err)?;
        let intent_key_value = intent_key(intent_id);
        let existing = intents.get(&intent_key_value).map_err(persist_err)?;
        let encoded_intent = existing.map(|guard| guard.value().to_vec());
        if encoded_intent.is_none() {
            CloseIntentOutcome::AlreadyClosed
        } else {
            let intent_record = decode_intent(
                encoded_intent
                    .as_deref()
                    .expect("close path already proved the intent row exists"),
            )
            .map_err(|error| codec_error("intent", error))?;
            let lanes_table = write_txn
                .open_table(PUBLISH_QUEUE_LANES)
                .map_err(persist_err)?;
            let (lane_lower, lane_upper) = lane_range(intent_id);
            let mut lanes_snapshot = Vec::new();
            for row in lanes_table
                .range::<&[u8; 12]>(&lane_lower..=&lane_upper)
                .map_err(persist_err)?
            {
                let (key, value) = row.map_err(persist_err)?;
                let (key_intent, _) =
                    parse_lane_key(key.value()).map_err(|error| codec_error("lane key", error))?;
                if key_intent != intent_id {
                    return Err(PersistenceError::invariant(
                        "lane close range escaped intent prefix",
                    ));
                }
                let (_, _, _, state) =
                    decode_lane(value.value()).map_err(|error| codec_error("lane", error))?;
                lanes_snapshot.push(state);
            }
            let shape_holds = match shape {
                LaneShape::None => lanes_snapshot.is_empty(),
                LaneShape::AllTerminal => {
                    !lanes_snapshot.is_empty()
                        && lanes_snapshot
                            .iter()
                            .all(|state| matches!(state, PublishQueueLaneState::Terminal { .. }))
                }
            };
            if !shape_holds {
                return Err(PersistenceError::invariant(match shape {
                    LaneShape::None => "intent still owns lanes",
                    LaneShape::AllTerminal => "intent lanes are not non-empty and terminal",
                }));
            }
            drop(lanes_table);
            let evidence_bytes = terminal_intent_evidence_bytes(&write_txn, intent_id)?;
            if shape == LaneShape::None {
                // One transaction: the receipt records WHY the write ended,
                // so a reattaching app is told "nowhere to publish" rather
                // than merely finding its open work gone.
                let mut receipts = write_txn
                    .open_table(PUBLISH_QUEUE_RECEIPTS)
                    .map_err(persist_err)?;
                update_publish_queue_receipt(
                    &mut receipts,
                    intent_record.receipt_id,
                    ReceiptState::NoDestination,
                )?;
            }
            let mut deadlines = write_txn
                .open_table(PUBLISH_QUEUE_DEADLINES)
                .map_err(persist_err)?;
            let mut deadlines_by_intent = write_txn
                .open_table(PUBLISH_QUEUE_DEADLINES_BY_INTENT)
                .map_err(persist_err)?;
            if deadlines.len().map_err(persist_err)?
                != deadlines_by_intent.len().map_err(persist_err)?
            {
                return Err(PersistenceError::invariant(
                    "deadline index cardinalities disagree",
                ));
            }
            let (deadline_lower, deadline_upper) = deadline_intent_range(intent_id);
            let mut stale_rows = Vec::new();
            for row in deadlines_by_intent
                .range::<&[u8; 20]>(&deadline_lower..=&deadline_upper)
                .map_err(persist_err)?
            {
                let (key, value) = row.map_err(persist_err)?;
                let (key_intent, at, relay_id) = parse_deadline_by_intent_key(key.value())
                    .map_err(|error| codec_error("deadline-by-intent key", error))?;
                if key_intent != intent_id {
                    return Err(PersistenceError::invariant(
                        "deadline close range escaped intent prefix",
                    ));
                }
                let decoded = decode_deadline(value.value())
                    .map_err(|error| codec_error("deadline-by-intent", error))?;
                stale_rows.push((*key.value(), at, relay_id, decoded));
            }
            for (by_intent_key, at, relay_id, decoded) in stale_rows {
                let ordered_key = deadline_key(at, intent_id, relay_id);
                let ordered = deadlines
                    .get(&ordered_key)
                    .map_err(persist_err)?
                    .map(|guard| guard.value().to_vec())
                    .ok_or_else(|| {
                        PersistenceError::invariant("by-intent deadline is missing ordered index")
                    })?;
                if decode_deadline(&ordered)
                    .map_err(|error| codec_error("ordered deadline", error))?
                    != decoded
                {
                    return Err(PersistenceError::invariant("deadline indexes disagree"));
                }
                deadlines.remove(&ordered_key).map_err(persist_err)?;
                deadlines_by_intent
                    .remove(&by_intent_key)
                    .map_err(persist_err)?;
            }
            intents.remove(&intent_key_value).map_err(persist_err)?;
            drop(deadlines);
            drop(deadlines_by_intent);
            drop(intents);
            let mut receipts = write_txn
                .open_table(PUBLISH_QUEUE_RECEIPTS)
                .map_err(persist_err)?;
            let mut meta = write_txn
                .open_table(PUBLISH_QUEUE_META)
                .map_err(persist_err)?;
            mark_terminal_receipt(
                &mut receipts,
                &mut meta,
                intent_record.receipt_id,
                wall_clock_now(),
                evidence_bytes,
            )?;
            drop(receipts);
            drop(meta);
            maintain_terminal_receipts_in_txn(
                &write_txn,
                wall_clock_now(),
                TerminalRetentionLimits::PRODUCTION,
            )?;
            CloseIntentOutcome::Closed
        }
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneCloseBeforeCommit);
    commit_prepared(write_txn, result)
}

pub(super) fn accept_refused(
    store: &mut RedbStore,
    frozen_id: EventId,
    expected_pubkey: PublicKey,
    reason: crate::RefuseReason,
) -> Result<u64, PersistenceError> {
    if reason == crate::RefuseReason::AlreadyExpired {
        return Err(PersistenceError::invariant(
            "already-expired writes are refused before receipt custody",
        ));
    }
    // Receipt-ONLY and terminal at birth: touches `PUBLISH_QUEUE_RECEIPTS`
    // (+ `PUBLISH_QUEUE_META` for the id allocation) alone — no `EVENTS` row,
    // no `PUBLISH_QUEUE_INTENTS` row, `intent_id: None` (nothing backs it).
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let receipt_id = {
        let mut publish_queue_meta = write_txn
            .open_table(PUBLISH_QUEUE_META)
            .map_err(persist_err)?;
        let mut publish_queue_receipts = write_txn
            .open_table(PUBLISH_QUEUE_RECEIPTS)
            .map_err(persist_err)?;
        let receipt_id = alloc_receipt_id_in_txn(&mut publish_queue_meta)?;
        let record = PublishQueueReceiptRecord {
            intent_id: None,
            expected_pubkey,
            accepted_at: None,
            payload: crate::PublishQueueReceiptPayload::Event {
                event_id: frozen_id,
                state: ReceiptState::Refused(reason),
            },
            correlation: None,
            terminal_sequence: None,
            terminal_at: None,
            terminal_bytes: None,
        };
        let encoded = encode_receipt(&record);
        let receipt_key_value = receipt_key(receipt_id);
        publish_queue_receipts
            .insert(&receipt_key_value, encoded.as_slice())
            .map_err(persist_err)?;
        mark_terminal_receipt(
            &mut publish_queue_receipts,
            &mut publish_queue_meta,
            receipt_id,
            wall_clock_now(),
            0,
        )?;
        drop(publish_queue_receipts);
        drop(publish_queue_meta);
        maintain_terminal_receipts_in_txn(
            &write_txn,
            wall_clock_now(),
            TerminalRetentionLimits::PRODUCTION,
        )?;
        receipt_id
    };
    commit_prepared(write_txn, receipt_id)
}

/// Read every retained receipt back out in receipt-id order (#1039).
pub(super) fn enumerate_publish_queue_receipts(
    store: &RedbStore,
) -> Result<Vec<crate::PublishQueueReceipt>, PersistenceError> {
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let receipts = match read_txn.open_table(PUBLISH_QUEUE_RECEIPTS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(error) => return Err(persist_err(error)),
    };
    let mut out = Vec::new();
    for row in receipts.iter().map_err(persist_err)? {
        let (key, value) = row.map_err(persist_err)?;
        let receipt_id = u64::from_be_bytes(*key.value());
        let record =
            decode_receipt(value.value()).map_err(|error| codec_error("receipt", error))?;
        out.push(crate::PublishQueueReceipt {
            receipt_id,
            intent_id: record.intent_id,
            expected_pubkey: record.expected_pubkey,
            accepted_at: record.accepted_at,
            payload: record.payload,
        });
    }
    Ok(out)
}

/// Read one bounded page of retained receipts in receipt-id order (#903).
/// The range begins after the exclusive cursor, so a later page does not
/// walk or materialize the prefix the caller already consumed.
pub(super) fn publish_queue_receipts_after(
    store: &RedbStore,
    after: Option<u64>,
    limit: u8,
) -> Result<Vec<crate::PublishQueueReceipt>, PersistenceError> {
    if limit == 0 || after == Some(u64::MAX) {
        return Ok(Vec::new());
    }
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let receipts = match read_txn.open_table(PUBLISH_QUEUE_RECEIPTS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(error) => return Err(persist_err(error)),
    };
    let first = receipt_key(after.map_or(0, |receipt_id| receipt_id + 1));
    let mut out = Vec::with_capacity(usize::from(limit));
    for row in receipts.range::<&[u8; 8]>(&first..).map_err(persist_err)? {
        let (key, value) = row.map_err(persist_err)?;
        let receipt_id = u64::from_be_bytes(*key.value());
        let record =
            decode_receipt(value.value()).map_err(|error| codec_error("receipt", error))?;
        out.push(crate::PublishQueueReceipt {
            receipt_id,
            intent_id: record.intent_id,
            expected_pubkey: record.expected_pubkey,
            accepted_at: record.accepted_at,
            payload: record.payload,
        });
        if out.len() == usize::from(limit) {
            break;
        }
    }
    Ok(out)
}

pub(super) fn maintain_terminal_receipts(
    store: &mut RedbStore,
) -> Result<Vec<u64>, PersistenceError> {
    maintain_terminal_receipts_at(store, wall_clock_now(), TerminalRetentionLimits::PRODUCTION)
}

pub(crate) fn maintain_terminal_receipts_at(
    store: &mut RedbStore,
    now: Timestamp,
    limits: TerminalRetentionLimits,
) -> Result<Vec<u64>, PersistenceError> {
    if !terminal_retention_due(store, now, limits)? {
        return Ok(Vec::new());
    }
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let candidates = maintain_terminal_receipts_in_txn(&write_txn, now, limits)?;
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::TerminalRetentionBeforeCommit);
    commit_prepared(write_txn, candidates)
}

pub(super) fn maintain_terminal_receipts_in_txn(
    write_txn: &redb::WriteTransaction,
    now: Timestamp,
    limits: TerminalRetentionLimits,
) -> Result<Vec<u64>, PersistenceError> {
    let candidates = {
        let meta = write_txn
            .open_table(PUBLISH_QUEUE_META)
            .map_err(persist_err)?;
        let receipts = write_txn
            .open_table(PUBLISH_QUEUE_RECEIPTS)
            .map_err(persist_err)?;
        let mut remaining_count =
            read_meta_u64(&meta, TERMINAL_RECEIPT_COUNT_KEY, "terminal receipt count")?
                .unwrap_or(0);
        let mut remaining_bytes =
            read_meta_u64(&meta, TERMINAL_RECEIPT_BYTES_KEY, "terminal receipt bytes")?
                .unwrap_or(0);
        let (lower, upper) = terminal_receipt_range();
        let mut candidates = Vec::new();
        for row in meta
            .range::<&[u8]>(lower.as_slice()..=upper.as_slice())
            .map_err(persist_err)?
        {
            let (key, _) = row.map_err(persist_err)?;
            let (sequence, receipt_id) = parse_terminal_receipt_key(key.value())
                .map_err(|error| codec_error("terminal receipt index key", error))?;
            let receipt = receipts
                .get(&receipt_key(receipt_id))
                .map_err(persist_err)?
                .map(|guard| guard.value().to_vec())
                .ok_or_else(|| {
                    PersistenceError::invariant("terminal index references missing receipt")
                })?;
            let record =
                decode_receipt(&receipt).map_err(|error| codec_error("terminal receipt", error))?;
            let terminal_bytes = record.terminal_bytes.ok_or_else(|| {
                PersistenceError::invariant("terminal receipt has no byte accounting")
            })?;
            if record.terminal_sequence != Some(sequence) {
                return Err(PersistenceError::invariant(
                    "terminal receipt and FIFO sequence disagree",
                ));
            }
            let terminal_at = record.terminal_at.ok_or_else(|| {
                PersistenceError::invariant("terminal receipt has no completion time")
            })?;
            let age_due =
                now.as_secs() >= terminal_at.as_secs().saturating_add(limits.max_age_secs);
            if !age_due
                && remaining_count <= limits.max_count
                && remaining_bytes <= limits.max_bytes
            {
                break;
            }
            candidates.push(receipt_id);
            remaining_count = remaining_count
                .checked_sub(1)
                .ok_or_else(|| PersistenceError::invariant("terminal receipt count underflow"))?;
            remaining_bytes = remaining_bytes
                .checked_sub(terminal_bytes)
                .ok_or_else(|| PersistenceError::invariant("terminal receipt bytes underflow"))?;
        }
        if remaining_count > limits.max_count || remaining_bytes > limits.max_bytes {
            return Err(PersistenceError::invariant(
                "terminal receipt accounting exceeds its FIFO index",
            ));
        }
        candidates
    };

    for receipt_id in &candidates {
        if remove_publish_queue_entry_in_txn(write_txn, *receipt_id)?
            != crate::RemoveQueueEntryOutcome::Removed
        {
            return Err(PersistenceError::invariant(
                "terminal FIFO references a non-removable receipt",
            ));
        }
    }
    Ok(candidates)
}

fn terminal_retention_due(
    store: &RedbStore,
    now: Timestamp,
    limits: TerminalRetentionLimits,
) -> Result<bool, PersistenceError> {
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let meta = read_txn
        .open_table(PUBLISH_QUEUE_META)
        .map_err(persist_err)?;
    let read_scalar = |key, name| {
        meta.get(key)
            .map_err(persist_err)?
            .map(|guard| decode_meta_u64(guard.value(), name))
            .transpose()
            .map_err(|error| codec_error(name, error))
    };
    let count = read_scalar(TERMINAL_RECEIPT_COUNT_KEY, "terminal receipt count")?.unwrap_or(0);
    let bytes = read_scalar(TERMINAL_RECEIPT_BYTES_KEY, "terminal receipt bytes")?.unwrap_or(0);
    if count > limits.max_count || bytes > limits.max_bytes {
        return Ok(true);
    }
    let (lower, upper) = terminal_receipt_range();
    let Some(row) = meta
        .range::<&[u8]>(lower.as_slice()..=upper.as_slice())
        .map_err(persist_err)?
        .next()
    else {
        return Ok(false);
    };
    let (key, _) = row.map_err(persist_err)?;
    let (sequence, receipt_id) = parse_terminal_receipt_key(key.value())
        .map_err(|error| codec_error("terminal receipt index key", error))?;
    let receipts = read_txn
        .open_table(PUBLISH_QUEUE_RECEIPTS)
        .map_err(persist_err)?;
    let receipt = receipts
        .get(&receipt_key(receipt_id))
        .map_err(persist_err)?
        .map(|guard| guard.value().to_vec())
        .ok_or_else(|| PersistenceError::invariant("terminal index references missing receipt"))?;
    let record =
        decode_receipt(&receipt).map_err(|error| codec_error("terminal receipt", error))?;
    if record.terminal_sequence != Some(sequence) {
        return Err(PersistenceError::invariant(
            "terminal receipt and FIFO sequence disagree",
        ));
    }
    let terminal_at = record
        .terminal_at
        .ok_or_else(|| PersistenceError::invariant("terminal receipt has no completion time"))?;
    Ok(now.as_secs() >= terminal_at.as_secs().saturating_add(limits.max_age_secs))
}

/// Forget one retained receipt and every piece of evidence keyed to it
/// (#1039). Refuses while the receipt still owns an open intent row.
pub(super) fn remove_publish_queue_entry(
    store: &mut RedbStore,
    receipt_id: u64,
) -> Result<crate::RemoveQueueEntryOutcome, PersistenceError> {
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    let outcome = remove_publish_queue_entry_in_txn(&write_txn, receipt_id)?;
    commit_prepared(write_txn, outcome)
}

fn remove_publish_queue_entry_in_txn(
    write_txn: &redb::WriteTransaction,
    receipt_id: u64,
) -> Result<crate::RemoveQueueEntryOutcome, PersistenceError> {
    {
        let mut receipts = write_txn
            .open_table(PUBLISH_QUEUE_RECEIPTS)
            .map_err(persist_err)?;
        let key = receipt_key(receipt_id);
        let existing = receipts.get(&key).map_err(persist_err)?;
        let Some(encoded) = existing.map(|guard| guard.value().to_vec()) else {
            return Ok(crate::RemoveQueueEntryOutcome::NotFound);
        };
        let record = decode_receipt(&encoded).map_err(|error| codec_error("receipt", error))?;
        let intents = write_txn
            .open_table(PUBLISH_QUEUE_INTENTS)
            .map_err(persist_err)?;
        if let Some(intent_id) = record.intent_id {
            if intents
                .get(&intent_key(intent_id))
                .map_err(persist_err)?
                .is_some()
            {
                return Ok(crate::RemoveQueueEntryOutcome::StillOpen);
            }
            let claims = write_txn
                .open_table(PUBLISH_QUEUE_KIND5_CLAIMS)
                .map_err(persist_err)?;
            if claims
                .get(&intent_key(intent_id))
                .map_err(persist_err)?
                .is_some()
            {
                return Err(PersistenceError::invariant(
                    "terminal receipt still owns provisional suppression claims",
                ));
            }
        }
        let mut meta = write_txn
            .open_table(PUBLISH_QUEUE_META)
            .map_err(persist_err)?;
        remove_terminal_receipt_index(&mut meta, receipt_id, &record)?;
        receipts.remove(&key).map_err(persist_err)?;
        // #591 tokens name this receipt id and nothing else; leaving one
        // behind would reattach a caller to a receipt that no longer exists.
        let mut correlations = write_txn
            .open_table(PUBLISH_QUEUE_CORRELATIONS)
            .map_err(persist_err)?;
        if let Some(token) = &record.correlation {
            let mapped = correlations
                .get(token.as_bytes())
                .map_err(persist_err)?
                .map(|guard| u64::from_be_bytes(*guard.value()));
            if mapped != Some(receipt_id) {
                return Err(PersistenceError::invariant(
                    "receipt correlation reverse ownership disagrees",
                ));
            }
            correlations.remove(token.as_bytes()).map_err(persist_err)?;
        }
        // The intent row itself is already gone (checked above); its retained
        // per-relay evidence is what this door reclaims. Two passes (collect
        // then remove) — `redb` does not allow mutating a table while
        // iterating it.
        if let Some(intent_id) = record.intent_id {
            let mut displaced = write_txn
                .open_table(PUBLISH_QUEUE_DISPLACED)
                .map_err(persist_err)?;
            displaced
                .remove(&intent_key(intent_id))
                .map_err(persist_err)?;
            let (attempt_lower, attempt_upper) = attempt_range(intent_id);
            let mut attempts = write_txn
                .open_table(PUBLISH_QUEUE_ATTEMPTS)
                .map_err(persist_err)?;
            let mut victims: Vec<[u8; 20]> = Vec::new();
            for row in attempts
                .range::<&[u8; 20]>(&attempt_lower..=&attempt_upper)
                .map_err(persist_err)?
            {
                let (key, _) = row.map_err(persist_err)?;
                victims.push(*key.value());
            }
            for key in &victims {
                attempts.remove(key).map_err(persist_err)?;
            }
            let mut details = write_txn
                .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
                .map_err(persist_err)?;
            let mut detail_victims: Vec<[u8; 20]> = Vec::new();
            for row in details
                .range::<&[u8; 20]>(&attempt_lower..=&attempt_upper)
                .map_err(persist_err)?
            {
                let (key, _) = row.map_err(persist_err)?;
                detail_victims.push(*key.value());
            }
            for key in &detail_victims {
                details.remove(key).map_err(persist_err)?;
            }
            let (lane_lower, lane_upper) = lane_range(intent_id);
            let mut lanes = write_txn
                .open_table(PUBLISH_QUEUE_LANES)
                .map_err(persist_err)?;
            let mut lane_victims: Vec<[u8; 12]> = Vec::new();
            for row in lanes
                .range::<&[u8; 12]>(&lane_lower..=&lane_upper)
                .map_err(persist_err)?
            {
                let (key, _) = row.map_err(persist_err)?;
                lane_victims.push(*key.value());
            }
            for key in &lane_victims {
                lanes.remove(key).map_err(persist_err)?;
            }
            let (route_lower, route_upper) = route_revision_range(intent_id);
            let mut routes = write_txn
                .open_table(PUBLISH_QUEUE_ROUTE_REVISIONS)
                .map_err(persist_err)?;
            let mut route_victims: Vec<[u8; 16]> = Vec::new();
            for row in routes
                .range::<&[u8; 16]>(&route_lower..=&route_upper)
                .map_err(persist_err)?
            {
                let (key, _) = row.map_err(persist_err)?;
                route_victims.push(*key.value());
            }
            for key in &route_victims {
                routes.remove(key).map_err(persist_err)?;
            }
            let (deadline_lower, deadline_upper) = deadline_intent_range(intent_id);
            let mut deadlines = write_txn
                .open_table(PUBLISH_QUEUE_DEADLINES)
                .map_err(persist_err)?;
            let mut deadlines_by_intent = write_txn
                .open_table(PUBLISH_QUEUE_DEADLINES_BY_INTENT)
                .map_err(persist_err)?;
            let mut deadline_victims = Vec::new();
            for row in deadlines_by_intent
                .range::<&[u8; 20]>(&deadline_lower..=&deadline_upper)
                .map_err(persist_err)?
            {
                let (key, _) = row.map_err(persist_err)?;
                let (_, at, relay_id) = parse_deadline_by_intent_key(key.value())
                    .map_err(|error| codec_error("deadline-by-intent key", error))?;
                deadline_victims.push((*key.value(), deadline_key(at, intent_id, relay_id)));
            }
            for (by_intent, ordered) in deadline_victims {
                deadlines_by_intent
                    .remove(&by_intent)
                    .map_err(persist_err)?;
                deadlines.remove(&ordered).map_err(persist_err)?;
            }
        }
        Ok(crate::RemoveQueueEntryOutcome::Removed)
    }
}
