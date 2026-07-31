use super::commit::commit_prepared;
use super::delivery::{
    alloc_counter_in_txn, alloc_receipt_id_in_txn, increment_pending_ephemeral_in_txn,
    lane_deadline, replace_lane_in_txn, DeliveryReceiptRecord,
};
use super::delivery_codec::{
    attempt_key, attempt_range, canonical_route_ids, codec_error, deadline_by_intent_key,
    deadline_due_range, deadline_intent_range, deadline_key, decode_attempt,
    decode_attempt_details, decode_deadline, decode_displaced, decode_intent, decode_lane,
    decode_receipt, decode_relay, decode_route, encode_attempt, encode_attempt_details,
    encode_lane, encode_receipt, encode_relay, encode_route, intent_key, lane_key, lane_range,
    parse_attempt_key, parse_deadline_by_intent_key, parse_deadline_key, parse_intent_key,
    parse_lane_key, parse_route_revision_key, receipt_key, relay_key, route_revision_key,
    route_revision_range, DeliveryRelayId, NEXT_RELAY_ID_KEY,
};
use super::schema::{
    persist_err, DELIVERY_ATTEMPTS, DELIVERY_ATTEMPT_DETAILS, DELIVERY_CORRELATIONS,
    DELIVERY_DEADLINES, DELIVERY_DEADLINES_BY_INTENT, DELIVERY_DISPLACED, DELIVERY_INTENTS,
    DELIVERY_LANES, DELIVERY_META, DELIVERY_RECEIPTS, DELIVERY_RELAYS, DELIVERY_RELAY_IDS,
    DELIVERY_ROUTE_REVISIONS,
};
#[cfg(test)]
use super::store::RedbCrashPoint;
use super::store::RedbStore;
#[cfg(test)]
use super::Ordering;
use super::{
    AuthDenial, BTreeMap, BTreeSet, CloseIntentOutcome, DeliveryAttempt, DeliveryAttemptDetails,
    DeliveryAttemptHandoff, DeliveryAttemptOutcome, DeliveryAttemptTransient, DeliveryDeadline,
    DeliveryInFlightPhase, DeliveryIntent, DeliveryLane, DeliveryLaneKey, DeliveryLaneState,
    DeliveryPostHandoffState, DeliveryReceipt, DeliveryRouteRevision, DeliveryTerminalOutcome,
    DeliveryTransientCause, Event, EventId, IntentId, IntentSigState, PersistenceError, PublicKey,
    ReceiptState, RelayUrl, Timestamp,
};
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
pub(super) fn recover_delivery(store: &RedbStore) -> Result<Vec<DeliveryIntent>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let delivery_intents = read_txn.open_table(DELIVERY_INTENTS).map_err(persist_err)?;
    let delivery_displaced = read_txn
        .open_table(DELIVERY_DISPLACED)
        .map_err(persist_err)?;

    let mut out = Vec::new();
    for entry in delivery_intents.iter().map_err(persist_err)? {
        let (key, value) = entry.map_err(persist_err)?;
        let intent_id =
            parse_intent_key(key.value()).map_err(|error| codec_error("intent key", error))?;
        let record = decode_intent(value.value())
            .map_err(|error| codec_error(&format!("intent {}", intent_id.0), error))?;

        let displaced = delivery_displaced
            .get(key.value())
            .map_err(persist_err)?
            .map(|guard| decode_displaced(guard.value()))
            .transpose()
            .map_err(|error| codec_error("displaced event", error))?;

        out.push(DeliveryIntent {
            intent_id,
            receipt_id: record.receipt_id,
            frozen: record.frozen,
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
) -> Result<Option<DeliveryReceipt>, PersistenceError> {
    // NOT a Q4 "always empty" door: retention (not crash-survival) is
    // the contract — `DELIVERY_RECEIPTS` rows are never deleted by this
    // unit, so this is an ordinary durable read.
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let delivery_receipts = read_txn
        .open_table(DELIVERY_RECEIPTS)
        .map_err(persist_err)?;
    let key = receipt_key(receipt_id);
    let Some(encoded) = delivery_receipts
        .get(&key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_vec())
    else {
        return Ok(None);
    };
    let record =
        decode_receipt(&encoded).map_err(|error| codec_error("retained receipt", error))?;
    Ok(Some(DeliveryReceipt {
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
    let table = match read_txn.open_table(DELIVERY_CORRELATIONS) {
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
    delivery_meta: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    delivery_relays: &mut redb::Table<'_, &'static [u8; 4], &'static [u8]>,
    delivery_relay_ids: &mut redb::Table<'_, &'static [u8], &'static [u8; 4]>,
    relay: &RelayUrl,
) -> Result<DeliveryRelayId, PersistenceError> {
    if let Some(encoded) = delivery_relay_ids
        .get(relay.as_str().as_bytes())
        .map_err(persist_err)?
    {
        let id = u32::from_be_bytes(*encoded.value());
        let key = relay_key(id);
        let forward = delivery_relays
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

    let raw = alloc_counter_in_txn(delivery_meta, NEXT_RELAY_ID_KEY)?;
    let id = DeliveryRelayId::try_from(raw)
        .map_err(|_| PersistenceError::invariant("delivery relay id namespace exhausted"))?;
    let key = relay_key(id);
    let encoded = encode_relay(relay).map_err(|error| codec_error("relay", error))?;
    delivery_relays
        .insert(&key, encoded.as_slice())
        .map_err(persist_err)?;
    delivery_relay_ids
        .insert(relay.as_str().as_bytes(), &key)
        .map_err(persist_err)?;
    Ok(id)
}

pub(super) fn record_route_revision(
    store: &mut RedbStore,
    intent_id: IntentId,
    relays: BTreeSet<RelayUrl>,
) -> Result<DeliveryRouteRevision, PersistenceError> {
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let revision = {
        let intents = write_txn
            .open_table(DELIVERY_INTENTS)
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
            .open_table(DELIVERY_ROUTE_REVISIONS)
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
        let mut delivery_meta = write_txn.open_table(DELIVERY_META).map_err(persist_err)?;
        let mut delivery_relays = write_txn.open_table(DELIVERY_RELAYS).map_err(persist_err)?;
        let mut delivery_relay_ids = write_txn
            .open_table(DELIVERY_RELAY_IDS)
            .map_err(persist_err)?;
        let mut interned = Vec::with_capacity(relays.len());
        for relay in &relays {
            let id = intern_relay_in_txn(
                &mut delivery_meta,
                &mut delivery_relays,
                &mut delivery_relay_ids,
                relay,
            )?;
            interned.push((id, relay.clone()));
        }
        let ids = canonical_route_ids(interned.iter().map(|(id, _)| *id));
        let encoded = encode_route(&ids).map_err(|error| codec_error("route revision", error))?;
        revisions
            .insert(&route_revision_key(intent_id, ordinal), encoded.as_slice())
            .map_err(persist_err)?;
        DeliveryRouteRevision {
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
) -> Result<Vec<DeliveryRouteRevision>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let revisions = read_txn
        .open_table(DELIVERY_ROUTE_REVISIONS)
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
            relays.insert(store.delivery_relay(id)?);
        }
        recovered.push(DeliveryRouteRevision {
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
) -> Result<Vec<DeliveryAttempt>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let attempts = read_txn
        .open_table(DELIVERY_ATTEMPTS)
        .map_err(persist_err)?;
    let details = read_txn
        .open_table(DELIVERY_ATTEMPT_DETAILS)
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
        let relay = store.delivery_relay(relay_id)?;
        let (event, mut outcome) =
            decode_attempt(value.value()).map_err(|error| codec_error("attempt", error))?;
        if let Some(detail) = details.get(key.value()).map_err(persist_err)? {
            let detail = decode_attempt_details(detail.value(), intent_id, relay.clone(), ordinal)
                .map_err(|error| codec_error("attempt details", error))?;
            if let Some(terminal) = detail.terminal {
                outcome = terminal;
            }
        }
        recovered.push(DeliveryAttempt {
            version: 1,
            intent_id,
            relay,
            ordinal,
            event,
            outcome,
        });
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

pub(super) fn bootstrap_delivery_lanes(
    store: &mut RedbStore,
    intent_id: IntentId,
) -> Result<Vec<DeliveryLane>, PersistenceError> {
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let prepared = {
        let intents = write_txn
            .open_table(DELIVERY_INTENTS)
            .map_err(persist_err)?;
        if intents
            .get(&intent_key(intent_id))
            .map_err(persist_err)?
            .is_none()
        {
            return Err(PersistenceError::invariant(
                "lane bootstrap intent is not open",
            ));
        }
        let route_revisions = write_txn
            .open_table(DELIVERY_ROUTE_REVISIONS)
            .map_err(persist_err)?;
        let attempts_table = write_txn
            .open_table(DELIVERY_ATTEMPTS)
            .map_err(persist_err)?;
        let mut lanes = write_txn.open_table(DELIVERY_LANES).map_err(persist_err)?;
        let details = write_txn
            .open_table(DELIVERY_ATTEMPT_DETAILS)
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
            let relay = store.delivery_relay(relay_id)?;
            let detail = decode_attempt_details(value.value(), intent_id, relay, ordinal)
                .map_err(|error| codec_error("attempt details", error))?;
            details_by_key.insert((relay_id, ordinal), detail);
        }
        let mut attempts: Vec<(DeliveryRelayId, DeliveryAttempt)> = Vec::new();
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
            let relay = store.delivery_relay(relay_id)?;
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
                DeliveryAttempt {
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
            let relay = store.delivery_relay(*relay_id)?;
            let key = DeliveryLaneKey { intent_id, relay };
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
                let (revision, last_ordinal, state) =
                    decode_lane(existing.value()).map_err(|error| codec_error("lane", error))?;
                let lane = DeliveryLane {
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
                    Some(attempt) if attempt.outcome != DeliveryAttemptOutcome::Started => {
                        if lane.state
                            != (DeliveryLaneState::Terminal {
                                ordinal: attempt.ordinal,
                                outcome: DeliveryTerminalOutcome::from_attempt(
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
                        DeliveryLaneState::Terminal {
                            outcome: DeliveryTerminalOutcome::AuthDenied(_),
                            ..
                        }
                    ) => {}
                    _ if matches!(lane.state, DeliveryLaneState::Terminal { .. }) => {
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
            let lane = DeliveryLane {
                version: 1,
                key,
                revision: 1,
                last_ordinal: 0,
                state: DeliveryLaneState::WaitingConnection,
            };
            let encoded = encode_lane(lane.revision, lane.last_ordinal, &lane.state)
                .map_err(|error| codec_error("lane", error))?;
            lanes
                .insert(&storage_key, encoded.as_slice())
                .map_err(persist_err)?;
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
            let relay = store.delivery_relay(relay_id)?;
            let (revision, last_ordinal, state) =
                decode_lane(value.value()).map_err(|error| codec_error("lane", error))?;
            recovered.push(DeliveryLane {
                version: 1,
                key: DeliveryLaneKey { intent_id, relay },
                revision,
                last_ordinal,
                state,
            });
        }
        recovered.sort_by(|left, right| left.key.relay.cmp(&right.key.relay));
        recovered
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneBootstrapBeforeCommit);
    commit_prepared(write_txn, prepared)
}

pub(super) fn recover_delivery_lanes(
    store: &RedbStore,
    intent_id: IntentId,
) -> Result<Vec<DeliveryLane>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let lanes = read_txn.open_table(DELIVERY_LANES).map_err(persist_err)?;
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
        let (revision, last_ordinal, state) =
            decode_lane(value.value()).map_err(|error| codec_error("lane", error))?;
        recovered.push(DeliveryLane {
            version: 1,
            key: DeliveryLaneKey {
                intent_id,
                relay: store.delivery_relay(relay_id)?,
            },
            revision,
            last_ordinal,
            state,
        });
    }
    recovered.sort_by(|a, b| a.key.relay.cmp(&b.key.relay));
    Ok(recovered)
}

pub(super) fn due_delivery_deadlines(
    store: &RedbStore,
    now: Timestamp,
    limit: usize,
) -> Result<Vec<DeliveryDeadline>, PersistenceError> {
    if limit > 1_024 {
        return Err(PersistenceError::invariant(
            "deadline read limit exceeds 1024",
        ));
    }
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let deadlines = read_txn
        .open_table(DELIVERY_DEADLINES)
        .map_err(persist_err)?;
    let deadlines_by_intent = read_txn
        .open_table(DELIVERY_DEADLINES_BY_INTENT)
        .map_err(persist_err)?;
    if deadlines.len().map_err(persist_err)? != deadlines_by_intent.len().map_err(persist_err)? {
        return Err(PersistenceError::invariant(
            "deadline index cardinalities disagree",
        ));
    }
    let lanes = read_txn.open_table(DELIVERY_LANES).map_err(persist_err)?;
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
        let (revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        let relay = store.delivery_relay(relay_id)?;
        let lane = DeliveryLane {
            version: 1,
            key: DeliveryLaneKey {
                intent_id,
                relay: relay.clone(),
            },
            revision,
            last_ordinal,
            state,
        };
        let deadline = DeliveryDeadline {
            at,
            key: DeliveryLaneKey { intent_id, relay },
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

pub(super) fn next_delivery_deadline(
    store: &RedbStore,
) -> Result<Option<Timestamp>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let deadlines = read_txn
        .open_table(DELIVERY_DEADLINES)
        .map_err(persist_err)?;
    let deadlines_by_intent = read_txn
        .open_table(DELIVERY_DEADLINES_BY_INTENT)
        .map_err(persist_err)?;
    if deadlines.len().map_err(persist_err)? != deadlines_by_intent.len().map_err(persist_err)? {
        return Err(PersistenceError::invariant(
            "deadline index cardinalities disagree",
        ));
    }
    let lanes = read_txn.open_table(DELIVERY_LANES).map_err(persist_err)?;
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
    let (revision, last_ordinal, state) =
        decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
    let relay = store.delivery_relay(relay_id)?;
    let lane = DeliveryLane {
        version: 1,
        key: DeliveryLaneKey {
            intent_id,
            relay: relay.clone(),
        },
        revision,
        last_ordinal,
        state,
    };
    let deadline = DeliveryDeadline {
        at,
        key: DeliveryLaneKey { intent_id, relay },
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
    key: &DeliveryLaneKey,
    expected_revision: u64,
    auth: bool,
) -> Result<DeliveryLane, PersistenceError> {
    store.persist_lane_state(
        key,
        expected_revision,
        if auth {
            DeliveryLaneState::WaitingAuth
        } else {
            DeliveryLaneState::WaitingConnection
        },
    )
}

pub(super) fn set_lane_eligible(
    store: &mut RedbStore,
    key: &DeliveryLaneKey,
    expected_revision: u64,
    since: Timestamp,
) -> Result<DeliveryLane, PersistenceError> {
    store.persist_lane_state(
        key,
        expected_revision,
        DeliveryLaneState::Eligible { since },
    )
}

pub(super) fn set_lane_transient(
    store: &mut RedbStore,
    key: &DeliveryLaneKey,
    expected_revision: u64,
    ordinal: u64,
    eligible_at: Timestamp,
    cause: DeliveryTransientCause,
    raw_reason: Option<String>,
) -> Result<DeliveryLane, PersistenceError> {
    if raw_reason
        .as_ref()
        .is_some_and(|reason| reason.len() > 4_096)
    {
        return Err(PersistenceError::invariant(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.delivery_relay_id(&key.relay)?;
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let lane = {
        let mut lanes = write_txn.open_table(DELIVERY_LANES).map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(DELIVERY_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(DELIVERY_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let mut details = write_txn
            .open_table(DELIVERY_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (_, last_ordinal, _) =
            decode_lane(&encoded).map_err(|error| codec_error("lane", error))?;
        if last_ordinal != ordinal {
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
            detail.transient = Some(DeliveryAttemptTransient {
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
            DeliveryLaneState::Transient {
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
    key: &DeliveryLaneKey,
    expected_revision: u64,
    ordinal: u64,
    at: Timestamp,
    cause: DeliveryTransientCause,
    raw_reason: Option<String>,
    auth: bool,
) -> Result<DeliveryLane, PersistenceError> {
    if raw_reason
        .as_ref()
        .is_some_and(|reason| reason.len() > 4_096)
    {
        return Err(PersistenceError::invariant(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.delivery_relay_id(&key.relay)?;
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let lane = {
        let mut lanes = write_txn.open_table(DELIVERY_LANES).map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(DELIVERY_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(DELIVERY_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let mut details = write_txn
            .open_table(DELIVERY_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (revision, last_ordinal, _) =
            decode_lane(&encoded).map_err(|error| codec_error("lane", error))?;
        if revision != expected_revision || last_ordinal != ordinal || ordinal == 0 {
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
        detail.transient = Some(DeliveryAttemptTransient {
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
                DeliveryLaneState::WaitingAuth
            } else {
                DeliveryLaneState::WaitingConnection
            },
        )?
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneTransitionBeforeCommit);
    commit_prepared(write_txn, lane)
}

pub(super) fn start_lane_attempt(
    store: &mut RedbStore,
    key: &DeliveryLaneKey,
    expected_revision: u64,
    event: Event,
    started_at: Timestamp,
) -> Result<(DeliveryAttempt, DeliveryLane), PersistenceError> {
    let intent = {
        let read_txn = store.db.begin_read().map_err(persist_err)?;
        let intents = read_txn.open_table(DELIVERY_INTENTS).map_err(persist_err)?;
        let storage_key = intent_key(key.intent_id);
        let encoded = intents
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("attempt intent is not open"))?;
        decode_intent(&encoded).map_err(|error| codec_error("attempt intent", error))?
    };
    if intent.sig_state != IntentSigState::Signed || intent.frozen != event {
        return Err(PersistenceError::invariant(
            "attempt bytes are not the intent's promoted signed bytes",
        ));
    }
    event
        .verify()
        .map_err(|e| PersistenceError::invariant(format!("attempt event is invalid: {e}")))?;
    let relay_id = store.delivery_relay_id(&key.relay)?;
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let (attempt, lane) = {
        let mut attempts = write_txn
            .open_table(DELIVERY_ATTEMPTS)
            .map_err(persist_err)?;
        let mut details = write_txn
            .open_table(DELIVERY_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let mut lanes = write_txn.open_table(DELIVERY_LANES).map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(DELIVERY_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(DELIVERY_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        if revision != expected_revision || !matches!(state, DeliveryLaneState::Eligible { .. }) {
            return Err(PersistenceError::invariant(
                "lane is not expected eligible cursor",
            ));
        }
        let ordinal = last_ordinal
            .checked_add(1)
            .ok_or_else(|| PersistenceError::invariant("attempt ordinal exhausted"))?;
        let attempt = DeliveryAttempt {
            version: 1,
            intent_id: key.intent_id,
            relay: key.relay.clone(),
            ordinal,
            event,
            outcome: DeliveryAttemptOutcome::Started,
        };
        let attempt_key_value = attempt_key(key.intent_id, relay_id, ordinal);
        let attempt_encoded = encode_attempt(&attempt.event, &attempt.outcome)
            .map_err(|error| codec_error("attempt", error))?;
        attempts
            .insert(&attempt_key_value, attempt_encoded.as_slice())
            .map_err(persist_err)?;
        let detail = DeliveryAttemptDetails {
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
            DeliveryLaneState::InFlight {
                ordinal,
                phase: DeliveryInFlightPhase::AwaitingHandoff,
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
    key: &DeliveryLaneKey,
    expected_revision: u64,
    ordinal: u64,
    detail: DeliveryAttemptHandoff,
    next: DeliveryPostHandoffState,
) -> Result<DeliveryLane, PersistenceError> {
    if matches!(
        &next,
        DeliveryPostHandoffState::Transient {
            raw_reason: Some(reason),
            ..
        } if reason.len() > 4_096
    ) {
        return Err(PersistenceError::invariant(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.delivery_relay_id(&key.relay)?;
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let lane = {
        let mut details = write_txn
            .open_table(DELIVERY_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let mut lanes = write_txn.open_table(DELIVERY_LANES).map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(DELIVERY_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(DELIVERY_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let lane_storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&lane_storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (revision, last_ordinal, current_state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        if revision != expected_revision || last_ordinal != ordinal {
            return Err(PersistenceError::invariant("stale lane handoff"));
        }
        if !matches!(
            current_state,
            DeliveryLaneState::InFlight {
                ordinal: current,
                phase: DeliveryInFlightPhase::AwaitingHandoff,
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
            DeliveryPostHandoffState::WaitingConnection => DeliveryLaneState::WaitingConnection,
            DeliveryPostHandoffState::WaitingAuth => DeliveryLaneState::WaitingAuth,
            DeliveryPostHandoffState::Eligible { since } => DeliveryLaneState::Eligible { since },
            DeliveryPostHandoffState::AwaitingAck { deadline } => DeliveryLaneState::InFlight {
                ordinal,
                phase: DeliveryInFlightPhase::AwaitingAck { deadline },
            },
            DeliveryPostHandoffState::Transient {
                eligible_at,
                cause,
                raw_reason,
            } => DeliveryLaneState::Transient {
                ordinal,
                eligible_at,
                cause,
                raw_reason,
            },
            DeliveryPostHandoffState::Terminal {
                outcome,
                finished_at,
            } => {
                if outcome == DeliveryAttemptOutcome::Started {
                    return Err(PersistenceError::invariant("Started is not terminal"));
                }
                recovered_detail.finished_at = Some(finished_at);
                recovered_detail.terminal = Some(outcome.clone());
                DeliveryLaneState::Terminal {
                    ordinal,
                    outcome: DeliveryTerminalOutcome::from_attempt(outcome)?,
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
    key: &DeliveryLaneKey,
    expected_revision: u64,
    ordinal: u64,
    outcome: DeliveryAttemptOutcome,
    finished_at: Timestamp,
) -> Result<DeliveryLane, PersistenceError> {
    if outcome == DeliveryAttemptOutcome::Started {
        return Err(PersistenceError::invariant("Started is not terminal"));
    }
    let lane_outcome = DeliveryTerminalOutcome::from_attempt(outcome.clone())?;
    let relay_id = store.delivery_relay_id(&key.relay)?;
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let lane = {
        let mut details = write_txn
            .open_table(DELIVERY_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let mut lanes = write_txn.open_table(DELIVERY_LANES).map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(DELIVERY_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(DELIVERY_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        let current = DeliveryLane {
            version: 1,
            key: key.clone(),
            revision,
            last_ordinal,
            state,
        };
        if current.revision != expected_revision || current.last_ordinal != ordinal {
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
                    DeliveryLaneState::Terminal {
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
                DeliveryLaneState::Terminal {
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
    key: &DeliveryLaneKey,
    expected_revision: u64,
    denial: AuthDenial,
) -> Result<DeliveryLane, PersistenceError> {
    if denial.reason.len() > 4_096 {
        return Err(PersistenceError::invariant(
            "authentication denial reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.delivery_relay_id(&key.relay)?;
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let lane = {
        let mut lanes = write_txn.open_table(DELIVERY_LANES).map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(DELIVERY_DEADLINES)
            .map_err(persist_err)?;
        let mut deadlines_by_intent = write_txn
            .open_table(DELIVERY_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
        let (revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        let current = DeliveryLane {
            version: 1,
            key: key.clone(),
            revision,
            last_ordinal,
            state,
        };
        if current.revision != expected_revision {
            return Err(PersistenceError::invariant(
                "authentication denial lane revision is stale",
            ));
        }
        if matches!(
            current.state,
            DeliveryLaneState::Terminal {
                outcome: DeliveryTerminalOutcome::AuthDenied(ref existing),
                ..
            } if existing == &denial
        ) {
            current
        } else {
            if !matches!(current.state, DeliveryLaneState::WaitingAuth) {
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
                DeliveryLaneState::Terminal {
                    ordinal: current.last_ordinal,
                    outcome: DeliveryTerminalOutcome::AuthDenied(denial),
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
) -> Result<Vec<DeliveryAttemptDetails>, PersistenceError> {
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let details = read_txn
        .open_table(DELIVERY_ATTEMPT_DETAILS)
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
                store.delivery_relay(relay_id)?,
                ordinal,
            )
            .map_err(|error| codec_error("attempt details", error))?,
        );
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
        let mut intents = write_txn
            .open_table(DELIVERY_INTENTS)
            .map_err(persist_err)?;
        let intent_key_value = intent_key(intent_id);
        if intents
            .get(&intent_key_value)
            .map_err(persist_err)?
            .is_none()
        {
            CloseIntentOutcome::AlreadyClosed
        } else {
            let lanes_table = write_txn.open_table(DELIVERY_LANES).map_err(persist_err)?;
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
                let (_, _, state) =
                    decode_lane(value.value()).map_err(|error| codec_error("lane", error))?;
                lanes_snapshot.push(state);
            }
            if lanes_snapshot.is_empty()
                || lanes_snapshot
                    .iter()
                    .any(|state| !matches!(state, DeliveryLaneState::Terminal { .. }))
            {
                return Err(PersistenceError::invariant(
                    "intent lanes are not non-empty and terminal",
                ));
            }
            let mut deadlines = write_txn
                .open_table(DELIVERY_DEADLINES)
                .map_err(persist_err)?;
            let mut deadlines_by_intent = write_txn
                .open_table(DELIVERY_DEADLINES_BY_INTENT)
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
            CloseIntentOutcome::Closed
        }
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneCloseBeforeCommit);
    commit_prepared(write_txn, result)
}

pub(super) fn accept_ephemeral(
    store: &mut RedbStore,
    frozen_id: EventId,
    expected_pubkey: PublicKey,
) -> Result<u64, PersistenceError> {
    // Receipt-ONLY: touches `DELIVERY_RECEIPTS` (+ `DELIVERY_META` for the
    // id allocation) alone — no `EVENTS` row, no `DELIVERY_INTENTS` row,
    // `intent_id: None` (nothing backs it).
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let receipt_id = {
        let mut delivery_meta = write_txn.open_table(DELIVERY_META).map_err(persist_err)?;
        let mut delivery_receipts = write_txn
            .open_table(DELIVERY_RECEIPTS)
            .map_err(persist_err)?;
        let receipt_id = alloc_receipt_id_in_txn(&mut delivery_meta)?;
        let record = DeliveryReceiptRecord {
            intent_id: None,
            frozen_id,
            expected_pubkey,
            state: ReceiptState::Accepted,
        };
        let encoded = encode_receipt(&record);
        let receipt_key_value = receipt_key(receipt_id);
        delivery_receipts
            .insert(&receipt_key_value, encoded.as_slice())
            .map_err(persist_err)?;
        increment_pending_ephemeral_in_txn(&mut delivery_meta)?;
        receipt_id
    };
    commit_prepared(write_txn, receipt_id)
}
