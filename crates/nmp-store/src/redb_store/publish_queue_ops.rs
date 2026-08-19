use super::commit::commit_prepared;
use super::publish_queue::{
    alloc_counter_in_txn, alloc_receipt_id_in_txn, clear_intent_deadlines, lane_deadline,
    mark_terminal_receipt, read_meta_u64, remove_terminal_receipt_index, replace_lane_in_txn,
    update_publish_queue_receipt, PublishQueueReceiptRecord,
};
use super::publish_queue_codec::{
    attempt_key, attempt_range, canonical_route_ids, codec_error, deadline_due_range,
    decode_attempt, decode_attempt_details, decode_deadline, decode_displaced, decode_intent,
    decode_lane, decode_meta_u64, decode_receipt, decode_relay, decode_route, encode_attempt,
    encode_attempt_details, encode_lane, encode_receipt, encode_relay, encode_route, intent_key,
    lane_key, lane_range, parse_attempt_key, parse_deadline_key, parse_intent_key, parse_lane_key,
    parse_route_revision_key, parse_terminal_receipt_key, receipt_key, relay_key,
    route_revision_key, route_revision_range, terminal_receipt_range, PublishQueueRelayId,
    NEXT_RELAY_ID_KEY, TERMINAL_RECEIPT_BYTES_KEY, TERMINAL_RECEIPT_COUNT_KEY,
};
use super::schema::{
    persist_err, PUBLISH_QUEUE_ATTEMPTS, PUBLISH_QUEUE_ATTEMPT_DETAILS, PUBLISH_QUEUE_DEADLINES,
    PUBLISH_QUEUE_DISPLACED, PUBLISH_QUEUE_INTENTS, PUBLISH_QUEUE_KIND5_CLAIMS,
    PUBLISH_QUEUE_LANES, PUBLISH_QUEUE_META, PUBLISH_QUEUE_RECEIPTS, PUBLISH_QUEUE_RELAYS,
    PUBLISH_QUEUE_RELAY_IDS, PUBLISH_QUEUE_ROUTE_REVISIONS,
};
#[cfg(test)]
use super::store::RedbCrashPoint;
use super::store::RedbStore;
#[cfg(any(test, feature = "test-instrumentation"))]
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
use redb::ReadableTable;

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
    let read_txn = store.begin_read()?;
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
    let read_txn = store.begin_read()?;
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
                PersistenceError::new("delivery relay reverse map points at missing dictionary row")
            })?;
        if decode_relay(forward.value()).map_err(|error| codec_error("relay", error))? != *relay {
            return Err(PersistenceError::new(
                "delivery relay dictionary directions disagree",
            ));
        }
        return Ok(id);
    }

    let raw = alloc_counter_in_txn(publish_queue_meta, NEXT_RELAY_ID_KEY)?;
    let id = PublishQueueRelayId::try_from(raw)
        .map_err(|_| PersistenceError::new("delivery relay id namespace exhausted"))?;
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
    let write_txn = store.begin_write()?;
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
            return Err(PersistenceError::new("route revision intent is not open"));
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
                return Err(PersistenceError::new(
                    "route revision range does not match its value intent",
                ));
            }
            decode_route(value.value()).map_err(|error| codec_error("route revision", error))?;
            last = last.max(ordinal);
        }
        let ordinal = last
            .checked_add(1)
            .ok_or_else(|| PersistenceError::new("route revision ordinal exhausted"))?;
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
            intent_id,
            ordinal,
            relays,
        }
    };
    #[cfg(any(test, feature = "test-instrumentation"))]
    if store.fail_route_revision_writes {
        return Err(PersistenceError::new("injected route revision failure"));
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::RouteRevisionBeforeCommit);
    commit_prepared(write_txn, revision)
}

pub(super) fn recover_route_revisions(
    store: &RedbStore,
    intent_id: IntentId,
) -> Result<Vec<PublishQueueRouteRevision>, PersistenceError> {
    let read_txn = store.begin_read()?;
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
            return Err(PersistenceError::new(
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
    let read_txn = store.begin_read()?;
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
            return Err(PersistenceError::new(
                "delivery attempt range escaped intent prefix",
            ));
        }
        let relay = store.publish_queue_relay(relay_id)?;
        let (event_id, mut outcome) =
            decode_attempt(value.value()).map_err(|error| codec_error("attempt", error))?;
        if let Some(detail) = details.get(key.value()).map_err(persist_err)? {
            let detail = decode_attempt_details(detail.value(), intent_id, relay.clone(), ordinal)
                .map_err(|error| codec_error("attempt details", error))?;
            if let Some(terminal) = detail.terminal {
                outcome = terminal;
            }
        }
        recovered.push(PublishQueueAttempt {
            intent_id,
            relay,
            ordinal,
            event_id,
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
    let write_txn = store.begin_write()?;
    let mut staged = false;
    let prepared = {
        let intents = write_txn
            .open_table(PUBLISH_QUEUE_INTENTS)
            .map_err(persist_err)?;
        let intent_bytes = intents
            .get(&intent_key(intent_id))
            .map_err(persist_err)?
            .map(|value| value.value().to_vec())
            .ok_or_else(|| PersistenceError::new("lane bootstrap intent is not open"))?;
        let event_id = decode_intent(&intent_bytes)
            .map_err(|error| codec_error("lane bootstrap intent", error))?
            .current_event_id()
            .ok_or_else(|| PersistenceError::new("lane bootstrap has no current event"))?;
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
                return Err(PersistenceError::new(
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
                return Err(PersistenceError::new("attempt range escaped intent prefix"));
            }
            let relay = store.publish_queue_relay(relay_id)?;
            let (event_id, mut outcome) =
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
                    intent_id,
                    relay,
                    ordinal,
                    event_id,
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
                return Err(PersistenceError::new(
                    "route revision range escaped intent prefix",
                ));
            }
            relay_ids.extend(
                decode_route(value.value())
                    .map_err(|error| codec_error("route revision", error))?,
            );
        }
        // Bootstrap checks each representation against ITSELF, never against
        // another one. Attempts, attempt details, lanes and route revisions
        // are four spellings of the same publish, written in one transaction;
        // pairwise agreement checks between them re-derived the same fact four
        // ways and hard-failed the boot when the derivations disagreed, which
        // is a store that has corrupted its own tables — not a shape recovery
        // can do anything about.
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
            // Attempt ordinals remain monotonic across every generation that
            // used this durable lane. Lane state, however, belongs only to
            // the exact current event; retained predecessor attempts are
            // historical evidence and cannot make the successor terminal.
            let current_attempts: Vec<_> = lane_attempts
                .iter()
                .copied()
                .filter(|attempt| attempt.event_id == event_id)
                .collect();
            let live_count = current_attempts
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
                    && current_attempts.last().is_some_and(|attempt| {
                        !crate::attempt_is_live(
                            attempt,
                            details_by_key.get(&(*relay_id, attempt.ordinal)),
                        )
                    }))
            {
                return Err(PersistenceError::new("contradictory live attempt history"));
            }
            if let Some(existing) = lanes.get(&storage_key).map_err(persist_err)? {
                let (lane_event_id, _, _, _) =
                    decode_lane(existing.value()).map_err(|error| codec_error("lane", error))?;
                if lane_event_id != event_id {
                    return Err(PersistenceError::new(
                        "delivery lane belongs to a predecessor event",
                    ));
                }
                continue;
            }
            let lane = PublishQueueLane {
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
                return Err(PersistenceError::new("lane range escaped intent prefix"));
            }
            let relay = store.publish_queue_relay(relay_id)?;
            let (lane_event_id, revision, last_ordinal, state) =
                decode_lane(value.value()).map_err(|error| codec_error("lane", error))?;
            recovered.push(PublishQueueLane {
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
    #[cfg(any(test, feature = "test-instrumentation"))]
    if std::mem::take(&mut store.fail_next_lane_bootstrap) {
        return Err(PersistenceError::new("injected lane bootstrap failure"));
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::LaneBootstrapBeforeCommit);
    commit_prepared(write_txn, prepared)
}

pub(super) fn recover_publish_queue_lanes(
    store: &RedbStore,
    intent_id: IntentId,
) -> Result<Vec<PublishQueueLane>, PersistenceError> {
    #[cfg(any(test, feature = "test-instrumentation"))]
    store
        .publish_queue_lane_recovery_reads
        .fetch_add(1, Ordering::Relaxed);
    let read_txn = store.begin_read()?;
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
            return Err(PersistenceError::new("lane range escaped intent prefix"));
        }
        let (lane_event_id, revision, last_ordinal, state) =
            decode_lane(value.value()).map_err(|error| codec_error("lane", error))?;
        recovered.push(PublishQueueLane {
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
        return Err(PersistenceError::new("deadline read limit exceeds 1024"));
    }
    let read_txn = store.begin_read()?;
    let deadlines = read_txn
        .open_table(PUBLISH_QUEUE_DEADLINES)
        .map_err(persist_err)?;
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
        let lane_storage_key = lane_key(intent_id, relay_id);
        let lane_encoded = lanes
            .get(&lane_storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::new("deadline references missing lane"))?;
        let (lane_event_id, revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        let relay = store.publish_queue_relay(relay_id)?;
        let lane = PublishQueueLane {
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
            return Err(PersistenceError::new("deadline and lane disagree"));
        }
        recovered.push(deadline);
    }
    Ok(recovered)
}

pub(super) fn next_publish_queue_deadline(
    store: &RedbStore,
) -> Result<Option<Timestamp>, PersistenceError> {
    let read_txn = store.begin_read()?;
    let deadlines = read_txn
        .open_table(PUBLISH_QUEUE_DEADLINES)
        .map_err(persist_err)?;
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
    let lane_storage_key = lane_key(intent_id, relay_id);
    let lane_encoded = lanes
        .get(&lane_storage_key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_vec())
        .ok_or_else(|| PersistenceError::new("deadline references missing lane"))?;
    let (lane_event_id, revision, last_ordinal, state) =
        decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
    let relay = store.publish_queue_relay(relay_id)?;
    let lane = PublishQueueLane {
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
        return Err(PersistenceError::new("deadline and lane disagree"));
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
        return Err(PersistenceError::new(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.begin_write()?;
    let lane = {
        let mut lanes = write_txn
            .open_table(PUBLISH_QUEUE_LANES)
            .map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES)
            .map_err(persist_err)?;
        let mut details = write_txn
            .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::new("delivery lane not found"))?;
        let (lane_event_id, _, last_ordinal, _) =
            decode_lane(&encoded).map_err(|error| codec_error("lane", error))?;
        if lane_event_id != key.event_id || last_ordinal != ordinal {
            return Err(PersistenceError::new("stale attempt ordinal"));
        }
        if ordinal > 0 {
            let detail_key = attempt_key(key.intent_id, relay_id, ordinal);
            let detail_encoded = details
                .get(&detail_key)
                .map_err(persist_err)?
                .map(|guard| guard.value().to_vec())
                .ok_or_else(|| PersistenceError::new("attempt detail row not found"))?;
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
        return Err(PersistenceError::new(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.begin_write()?;
    let lane = {
        let mut lanes = write_txn
            .open_table(PUBLISH_QUEUE_LANES)
            .map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES)
            .map_err(persist_err)?;
        let mut details = write_txn
            .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::new("delivery lane not found"))?;
        let (lane_event_id, revision, last_ordinal, _) =
            decode_lane(&encoded).map_err(|error| codec_error("lane", error))?;
        if lane_event_id != key.event_id
            || revision != expected_revision
            || last_ordinal != ordinal
            || ordinal == 0
        {
            return Err(PersistenceError::new("stale suspended attempt"));
        }
        let detail_key = attempt_key(key.intent_id, relay_id, ordinal);
        let detail_encoded = details
            .get(&detail_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::new("attempt detail row not found"))?;
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
        let read_txn = store.begin_read()?;
        let intents = read_txn
            .open_table(PUBLISH_QUEUE_INTENTS)
            .map_err(persist_err)?;
        let storage_key = intent_key(key.intent_id);
        let encoded = intents
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::new("attempt intent is not open"))?;
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
                PersistenceError::new("operation materialization event is missing")
            })?;
            (stored.event, materialization.sig_state)
        }
        super::publish_queue::PublishQueueIntentRecordWork::ReplaceableOperation {
            materialization: None,
            ..
        } => {
            return Err(PersistenceError::new(
                "operation intent has no current materialization",
            ))
        }
    };
    // The stored verdict IS the check (#1782): `Signed` can only have been
    // written by `promote_signed`, which only a `VerifiedSignature` opens,
    // and byte-identity binds these attempt bytes to that same promoted
    // body. Re-running schnorr here proved nothing the two lines above had
    // not already established.
    if sig_state != IntentSigState::Signed || intent_event != event {
        return Err(PersistenceError::new(
            "attempt bytes are not the intent's promoted signed bytes",
        ));
    }
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.begin_write()?;
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
        let storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::new("delivery lane not found"))?;
        let (lane_event_id, revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        if lane_event_id != key.event_id
            || revision != expected_revision
            || !matches!(state, PublishQueueLaneState::Eligible { .. })
        {
            return Err(PersistenceError::new(
                "lane is not expected eligible cursor",
            ));
        }
        let ordinal = last_ordinal
            .checked_add(1)
            .ok_or_else(|| PersistenceError::new("attempt ordinal exhausted"))?;
        let attempt = PublishQueueAttempt {
            intent_id: key.intent_id,
            relay: key.relay.clone(),
            ordinal,
            event_id: event.id,
            outcome: PublishQueueAttemptOutcome::Started,
        };
        let attempt_key_value = attempt_key(key.intent_id, relay_id, ordinal);
        let attempt_encoded = encode_attempt(&attempt.event_id, &attempt.outcome)
            .map_err(|error| codec_error("attempt", error))?;
        attempts
            .insert(&attempt_key_value, attempt_encoded.as_slice())
            .map_err(persist_err)?;
        let detail = PublishQueueAttemptDetails {
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
    #[cfg(any(test, feature = "test-instrumentation"))]
    if store.failed_lane_start_relays.contains(&key.relay) {
        return Err(PersistenceError::new("injected attempt start failure"));
    }
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
        return Err(PersistenceError::new(
            "transient raw reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.begin_write()?;
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
        let lane_storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&lane_storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::new("delivery lane not found"))?;
        let (lane_event_id, revision, last_ordinal, current_state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        if lane_event_id != key.event_id || revision != expected_revision || last_ordinal != ordinal
        {
            return Err(PersistenceError::new("stale lane handoff"));
        }
        if !matches!(
            current_state,
            PublishQueueLaneState::InFlight {
                ordinal: current,
                phase: PublishQueueInFlightPhase::AwaitingHandoff,
            } if current == ordinal
        ) {
            return Err(PersistenceError::new("lane is not awaiting handoff"));
        }
        let attempt_key_value = attempt_key(key.intent_id, relay_id, ordinal);
        let detail_encoded = details
            .get(&attempt_key_value)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::new("attempt detail row not found"))?;
        let mut recovered_detail =
            decode_attempt_details(&detail_encoded, key.intent_id, key.relay.clone(), ordinal)
                .map_err(|error| codec_error("attempt details", error))?;
        if let Some(existing) = &recovered_detail.handoff {
            if existing != &detail {
                return Err(PersistenceError::new("conflicting handoff evidence"));
            }
        } else {
            recovered_detail.handoff = Some(detail);
        }
        let state = match next {
            PublishQueuePostHandoffState::WaitingConnection => {
                PublishQueueLaneState::WaitingConnection
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
                    return Err(PersistenceError::new("Started is not terminal"));
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
            key,
            relay_id,
            expected_revision,
            state,
        )?;
        if lane.last_ordinal != ordinal {
            return Err(PersistenceError::new("stale lane handoff ordinal"));
        }
        let detail_encoded = encode_attempt_details(&recovered_detail)
            .map_err(|error| codec_error("attempt details", error))?;
        details
            .insert(&attempt_key_value, detail_encoded.as_slice())
            .map_err(persist_err)?;
        lane
    };
    #[cfg(any(test, feature = "test-instrumentation"))]
    if std::mem::take(&mut store.fail_next_lane_handoff) {
        return Err(PersistenceError::new("injected lane handoff failure"));
    }
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
        return Err(PersistenceError::new("Started is not terminal"));
    }
    let lane_outcome = PublishQueueTerminalOutcome::from_attempt(outcome.clone())?;
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.begin_write()?;
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
        let storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::new("delivery lane not found"))?;
        let (lane_event_id, revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        let current = PublishQueueLane {
            key: key.clone(),
            revision,
            last_ordinal,
            state,
        };
        if lane_event_id != key.event_id
            || current.revision != expected_revision
            || current.last_ordinal != ordinal
        {
            return Err(PersistenceError::new("stale terminal attempt"));
        }
        let detail_key = attempt_key(key.intent_id, relay_id, ordinal);
        let detail_encoded = details
            .get(&detail_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::new("attempt detail row not found"))?;
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
                return Err(PersistenceError::new(
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
    #[cfg(any(test, feature = "test-instrumentation"))]
    if std::mem::take(&mut store.fail_next_lane_attempt_finish) {
        return Err(PersistenceError::new("injected attempt finish failure"));
    }
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
        return Err(PersistenceError::new(
            "authentication denial reason exceeds 4096 bytes",
        ));
    }
    let relay_id = store.publish_queue_relay_id(&key.relay)?;
    let write_txn = store.begin_write()?;
    let lane = {
        let mut lanes = write_txn
            .open_table(PUBLISH_QUEUE_LANES)
            .map_err(persist_err)?;
        let mut deadlines = write_txn
            .open_table(PUBLISH_QUEUE_DEADLINES)
            .map_err(persist_err)?;
        let storage_key = lane_key(key.intent_id, relay_id);
        let lane_encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::new("delivery lane not found"))?;
        let (lane_event_id, revision, last_ordinal, state) =
            decode_lane(&lane_encoded).map_err(|error| codec_error("lane", error))?;
        let current = PublishQueueLane {
            key: key.clone(),
            revision,
            last_ordinal,
            state,
        };
        if lane_event_id != key.event_id || current.revision != expected_revision {
            return Err(PersistenceError::new(
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
                return Err(PersistenceError::new(
                    "lane is not waiting for authentication",
                ));
            }
            replace_lane_in_txn(
                &mut lanes,
                &mut deadlines,
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
    let read_txn = store.begin_read()?;
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
            return Err(PersistenceError::new(
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

pub(super) fn terminal_intent_evidence_bytes(
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
            .ok_or_else(|| PersistenceError::new("terminal evidence bytes overflow"))?;
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
            .ok_or_else(|| PersistenceError::new("terminal evidence bytes overflow"))?;
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
            .ok_or_else(|| PersistenceError::new("terminal evidence bytes overflow"))?;
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
            .ok_or_else(|| PersistenceError::new("terminal evidence bytes overflow"))?;
    }
    Ok(total)
}

fn close_intent(
    store: &mut RedbStore,
    intent_id: IntentId,
    shape: LaneShape,
) -> Result<CloseIntentOutcome, PersistenceError> {
    let write_txn = store.begin_write()?;
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
                    return Err(PersistenceError::new(
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
                return Err(PersistenceError::new(match shape {
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
            {
                let lanes_table = write_txn
                    .open_table(PUBLISH_QUEUE_LANES)
                    .map_err(persist_err)?;
                clear_intent_deadlines(&lanes_table, &mut deadlines, intent_id)?;
            }
            intents.remove(&intent_key_value).map_err(persist_err)?;
            drop(deadlines);
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
        return Err(PersistenceError::new(
            "already-expired writes are refused before receipt custody",
        ));
    }
    // Receipt-ONLY and terminal at birth: touches `PUBLISH_QUEUE_RECEIPTS`
    // (+ `PUBLISH_QUEUE_META` for the id allocation) alone — no `EVENTS` row,
    // no `PUBLISH_QUEUE_INTENTS` row, `intent_id: None` (nothing backs it).
    let write_txn = store.begin_write()?;
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
    let read_txn = store.begin_read()?;
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
    let read_txn = store.begin_read()?;
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

pub(crate) fn maintain_terminal_receipts_at(
    store: &mut RedbStore,
    now: Timestamp,
    limits: TerminalRetentionLimits,
) -> Result<Vec<u64>, PersistenceError> {
    if !terminal_retention_due(store, now, limits)? {
        return Ok(Vec::new());
    }
    let write_txn = store.begin_write()?;
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
                    PersistenceError::new("terminal index references missing receipt")
                })?;
            let record =
                decode_receipt(&receipt).map_err(|error| codec_error("terminal receipt", error))?;
            let terminal_bytes = record
                .terminal_bytes
                .ok_or_else(|| PersistenceError::new("terminal receipt has no byte accounting"))?;
            if record.terminal_sequence != Some(sequence) {
                return Err(PersistenceError::new(
                    "terminal receipt and FIFO sequence disagree",
                ));
            }
            let terminal_at = record
                .terminal_at
                .ok_or_else(|| PersistenceError::new("terminal receipt has no completion time"))?;
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
                .ok_or_else(|| PersistenceError::new("terminal receipt count underflow"))?;
            remaining_bytes = remaining_bytes
                .checked_sub(terminal_bytes)
                .ok_or_else(|| PersistenceError::new("terminal receipt bytes underflow"))?;
        }
        if remaining_count > limits.max_count || remaining_bytes > limits.max_bytes {
            return Err(PersistenceError::new(
                "terminal receipt accounting exceeds its FIFO index",
            ));
        }
        candidates
    };

    for receipt_id in &candidates {
        if remove_publish_queue_entry_in_txn(write_txn, *receipt_id)?
            != crate::RemoveQueueEntryOutcome::Removed
        {
            return Err(PersistenceError::new(
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
    let read_txn = store.begin_read()?;
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
        .ok_or_else(|| PersistenceError::new("terminal index references missing receipt"))?;
    let record =
        decode_receipt(&receipt).map_err(|error| codec_error("terminal receipt", error))?;
    if record.terminal_sequence != Some(sequence) {
        return Err(PersistenceError::new(
            "terminal receipt and FIFO sequence disagree",
        ));
    }
    let terminal_at = record
        .terminal_at
        .ok_or_else(|| PersistenceError::new("terminal receipt has no completion time"))?;
    Ok(now.as_secs() >= terminal_at.as_secs().saturating_add(limits.max_age_secs))
}

/// Forget one retained receipt and every piece of evidence keyed to it
/// (#1039). Refuses while the receipt still owns an open intent row.
pub(super) fn remove_publish_queue_entry(
    store: &mut RedbStore,
    receipt_id: u64,
) -> Result<crate::RemoveQueueEntryOutcome, PersistenceError> {
    let write_txn = store.begin_write()?;
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
                return Err(PersistenceError::new(
                    "terminal receipt still owns provisional suppression claims",
                ));
            }
        }
        let mut meta = write_txn
            .open_table(PUBLISH_QUEUE_META)
            .map_err(persist_err)?;
        remove_terminal_receipt_index(&mut meta, receipt_id, &record)?;
        receipts.remove(&key).map_err(persist_err)?;
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
            let mut deadlines = write_txn
                .open_table(PUBLISH_QUEUE_DEADLINES)
                .map_err(persist_err)?;
            // The lane rows are what name this intent's deadlines, so the
            // deadlines go while those rows still stand.
            clear_intent_deadlines(&lanes, &mut deadlines, intent_id)?;
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
        }
        Ok(crate::RemoveQueueEntryOutcome::Removed)
    }
}
