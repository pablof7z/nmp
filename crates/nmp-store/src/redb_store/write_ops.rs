use super::canonical::{
    encode_stored_event_record, record_to_stored_event, stored_event_to_record,
};
use super::ingest_txn::{GovernedIngestTxn, GovernedWrite, RedbIngestTxn};
use super::mutation::{
    fan_out_signed_in_txn, find_any_displaced_key_by_event_id_in_txn,
    find_displaced_key_by_event_id_in_txn, missing_addr_index_target,
    process_kind5_deletions_provisional_in_txn, reinsert_stashed_in_txn, remove_row_in_txn,
    tombstone_refuses,
};
use super::publish_queue::{
    alloc_intent_id_in_txn, alloc_receipt_id_in_txn, is_suppressed_in_txn, mark_terminal_receipt,
    remove_addr_claimant_in_txn, remove_claimant_in_txn, update_publish_queue_receipt,
    PublishQueueIntentRecord, PublishQueueReceiptRecord, SuppressClaimRecord,
};
use super::publish_queue_codec::{
    attempt_range, codec_error, deadline_intent_range, deadline_key, decode_attempt_handoff,
    decode_claims, decode_deadline, decode_displaced, decode_intent, decode_receipt, encode_claims,
    encode_displaced, encode_intent, encode_receipt, id_claim_key, intent_key, lane_range,
    parse_deadline_by_intent_key, parse_lane_key, receipt_key, route_revision_range,
};
use super::query::expiration_key;
use super::schema::{
    persist_err, EventKey, PUBLISH_QUEUE_ATTEMPTS, PUBLISH_QUEUE_ATTEMPT_DETAILS,
    PUBLISH_QUEUE_CORRELATIONS, PUBLISH_QUEUE_DEADLINES, PUBLISH_QUEUE_DEADLINES_BY_INTENT,
    PUBLISH_QUEUE_LANES, PUBLISH_QUEUE_ROUTE_REVISIONS,
};
#[cfg(test)]
use super::store::RedbCrashPoint;
use super::store::RedbStore;
use super::{
    address_key_for, candidate_wins, AcceptOutcome, AcceptWrite, BTreeMap, BTreeSet,
    CompensateOutcome, EventId, HashMap, HashSet, IntentId, IntentSigState, Kind, LocalOrigin,
    PersistenceError, PromoteOutcome, Provenance, ReceiptState, RefuseReason, SigState,
    StoredEvent, Timestamp,
};
use crate::terminal_retention::{wall_clock_now, TerminalRetentionLimits};
use crate::{handoff_may_have_occurred, RetiredIntent, VerifiedSignature};
use redb::ReadableTable;

/// Redb half of replaceable-delivery coalescing. Everything here runs inside
/// the same governed transaction that installs the newer canonical winner.
fn retire_superseded_owners_in_txn(
    ingest: &mut RedbIngestTxn<'_, '_>,
    write_txn: &redb::WriteTransaction,
    correlations: &mut redb::Table<'_, &[u8], &[u8; 8]>,
    mut replaced: StoredEvent,
) -> Result<(Option<StoredEvent>, Vec<RetiredIntent>), PersistenceError> {
    let mut attempts = write_txn
        .open_table(PUBLISH_QUEUE_ATTEMPTS)
        .map_err(persist_err)?;
    let mut attempt_details = write_txn
        .open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)
        .map_err(persist_err)?;
    let mut lanes = write_txn
        .open_table(PUBLISH_QUEUE_LANES)
        .map_err(persist_err)?;
    let mut route_revisions = write_txn
        .open_table(PUBLISH_QUEUE_ROUTE_REVISIONS)
        .map_err(persist_err)?;
    let mut deadlines = write_txn
        .open_table(PUBLISH_QUEUE_DEADLINES)
        .map_err(persist_err)?;
    let mut deadlines_by_intent = write_txn
        .open_table(PUBLISH_QUEUE_DEADLINES_BY_INTENT)
        .map_err(persist_err)?;

    let owners = replaced
        .provenance
        .local
        .as_ref()
        .map(|local| local.owners.clone())
        .unwrap_or_default();
    let mut eligible = Vec::new();
    for owner in owners {
        let key = intent_key(owner);
        let Some(intent_bytes) = ingest
            .publish_queue_intents
            .get(&key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
        else {
            continue;
        };
        let intent = decode_intent(&intent_bytes)
            .map_err(|error| codec_error(&format!("intent {}", owner.0), error))?;
        let (attempt_lower, attempt_upper) = attempt_range(owner);
        let attempt_keys = attempts
            .range::<&[u8; 20]>(&attempt_lower..=&attempt_upper)
            .map_err(persist_err)?
            .map(|row| row.map(|(key, _)| *key.value()).map_err(persist_err))
            .collect::<Result<Vec<_>, _>>()?;
        let attempt_detail_rows = attempt_details
            .range::<&[u8; 20]>(&attempt_lower..=&attempt_upper)
            .map_err(persist_err)?
            .map(|row| {
                let (key, value) = row.map_err(persist_err)?;
                let handoff = decode_attempt_handoff(value.value())
                    .map_err(|error| codec_error("attempt details", error))?;
                Ok::<_, PersistenceError>((*key.value(), handoff))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut lane_keys = Vec::new();
        let mut lane_started = false;
        let (lane_lower, lane_upper) = lane_range(owner);
        for row in lanes
            .range::<&[u8; 12]>(&lane_lower..=&lane_upper)
            .map_err(persist_err)?
        {
            let (lane_key, lane_bytes) = row.map_err(persist_err)?;
            let (key_intent, _) =
                parse_lane_key(lane_key.value()).map_err(|error| codec_error("lane key", error))?;
            let (_, _, last_ordinal, _) =
                super::publish_queue_codec::decode_lane(lane_bytes.value())
                    .map_err(|error| codec_error("lane", error))?;
            if key_intent != owner {
                return Err(PersistenceError::invariant(
                    "lane retirement range escaped intent prefix",
                ));
            }
            lane_started |= last_ordinal != 0;
            lane_keys.push(*lane_key.value());
        }
        let attempt_evidence_exists = !attempt_keys.is_empty() || !attempt_detail_rows.is_empty();
        let retain_safety_receipt = if attempt_evidence_exists {
            attempt_keys.iter().any(|attempt_key| {
                let details = attempt_detail_rows
                    .iter()
                    .find_map(|(detail_key, details)| {
                        (detail_key == attempt_key).then_some(details)
                    });
                handoff_may_have_occurred(details.and_then(Option::as_ref))
            }) || attempt_detail_rows.iter().any(|(detail_key, handoff)| {
                !attempt_keys.contains(detail_key) && handoff_may_have_occurred(handoff.as_ref())
            })
        } else {
            lane_started
        };
        eligible.push((
            owner,
            intent.receipt_id,
            lane_keys,
            attempt_keys,
            attempt_detail_rows
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            retain_safety_receipt,
        ));
    }

    for (owner, receipt_id, lane_keys, attempt_keys, attempt_detail_keys, retain_safety_receipt) in
        &eligible
    {
        if let Some(local) = replaced.provenance.local.as_mut() {
            local.owners.remove(owner);
        }
        let key = intent_key(*owner);
        ingest
            .publish_queue_displaced
            .remove(&key)
            .map_err(persist_err)?;
        ingest
            .publish_queue_intents
            .remove(&key)
            .map_err(persist_err)?;
        for attempt_key in attempt_keys {
            attempts.remove(attempt_key).map_err(persist_err)?;
        }
        for attempt_key in attempt_detail_keys {
            attempt_details.remove(attempt_key).map_err(persist_err)?;
        }
        for lane_key in lane_keys {
            lanes.remove(lane_key).map_err(persist_err)?;
        }

        let (lower, upper) = route_revision_range(*owner);
        let route_keys = route_revisions
            .range::<&[u8; 16]>(&lower..=&upper)
            .map_err(persist_err)?
            .map(|row| row.map(|(key, _)| *key.value()).map_err(persist_err))
            .collect::<Result<Vec<_>, _>>()?;
        for route_key in route_keys {
            route_revisions.remove(&route_key).map_err(persist_err)?;
        }

        let (lower, upper) = deadline_intent_range(*owner);
        let deadline_rows = deadlines_by_intent
            .range::<&[u8; 20]>(&lower..=&upper)
            .map_err(persist_err)?
            .map(|row| {
                let (by_intent_key, encoded) = row.map_err(persist_err)?;
                let (key_intent, at, relay_id) =
                    parse_deadline_by_intent_key(by_intent_key.value())
                        .map_err(|error| codec_error("deadline-by-intent key", error))?;
                if key_intent != *owner {
                    return Err(PersistenceError::invariant(
                        "deadline retirement range escaped intent prefix",
                    ));
                }
                let value = decode_deadline(encoded.value())
                    .map_err(|error| codec_error("deadline", error))?;
                Ok((*by_intent_key.value(), at, relay_id, value))
            })
            .collect::<Result<Vec<_>, PersistenceError>>()?;
        for (by_intent_key, at, relay_id, _) in deadline_rows {
            deadlines
                .remove(&deadline_key(at, *owner, relay_id))
                .map_err(persist_err)?;
            deadlines_by_intent
                .remove(&by_intent_key)
                .map_err(persist_err)?;
        }
        if *retain_safety_receipt {
            update_publish_queue_receipt(
                &mut ingest.publish_queue_receipts,
                *receipt_id,
                ReceiptState::Superseded,
            )?;
            mark_terminal_receipt(
                &mut ingest.publish_queue_receipts,
                &mut ingest.publish_queue_meta,
                *receipt_id,
                wall_clock_now(),
                0,
            )?;
            continue;
        }
        let removed_receipt = ingest
            .publish_queue_receipts
            .remove(&receipt_key(*receipt_id))
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| {
                PersistenceError::invariant("open delivery intent must retain its receipt")
            })?;
        let removed_record = decode_receipt(&removed_receipt)
            .map_err(|error| codec_error("retired receipt", error))?;
        if let Some(token) = removed_record.correlation {
            let mapped = correlations
                .get(token.as_bytes())
                .map_err(persist_err)?
                .map(|guard| u64::from_be_bytes(*guard.value()));
            if mapped != Some(*receipt_id) {
                return Err(PersistenceError::invariant(
                    "receipt correlation reverse ownership disagrees",
                ));
            }
            correlations.remove(token.as_bytes()).map_err(persist_err)?;
        }
    }

    let retired = eligible
        .into_iter()
        .map(
            |(intent_id, receipt_id, .., handoff_may_have_occurred)| RetiredIntent {
                intent_id,
                receipt_id,
                handoff_may_have_occurred,
            },
        )
        .collect();
    let displaced = (!replaced.provenance.seen.is_empty()).then_some(replaced);
    Ok((displaced, retired))
}

pub(super) fn accept_write(
    store: &mut RedbStore,
    accept: AcceptWrite,
) -> Result<AcceptOutcome, PersistenceError> {
    let AcceptWrite {
        payload,
        expected_pubkey,
        signing_identity_ref,
        accepted_at,
        correlation,
    } = accept;
    if let crate::AcceptWritePayload::ReplaceableOperation(operation) = payload {
        return super::semantic_edit_ops::accept(
            store,
            expected_pubkey,
            signing_identity_ref,
            accepted_at,
            correlation,
            *operation,
        );
    }
    let crate::AcceptWritePayload::Event {
        frozen,
        replaceable_base,
        monotonic_stamp,
        routing,
        mut sig_state,
    } = payload
    else {
        unreachable!("replaceable operation returned above")
    };
    let mut frozen = *frozen;
    // Overridden inside the `Duplicate` branch when the existing row
    // is ALREADY signed (codex-nova ruling) — the shared R7 journal
    // write below uses these instead of the hardcoded `Accepted`/
    // caller-supplied values in that one case.
    let mut receipt_state = ReceiptState::Accepted;

    // Refused at the door FIRST, same as `insert`: never journaled,
    // nothing to recover, and neither an `IntentId` nor a receipt id
    // is ever allocated (R3 + architecture review correction: a
    // refusal can never burn either).
    if frozen.is_expired_at(&accepted_at) {
        return Ok(AcceptOutcome::Refused(RefuseReason::AlreadyExpired));
    }

    let mut write = GovernedWrite::begin(store)?;
    let outcome = write.apply(|ingest, write_txn| {
        let mut publish_queue_correlations = write_txn
            .open_table(PUBLISH_QUEUE_CORRELATIONS)
            .map_err(persist_err)?;

        if let Some(expected) = replaceable_base {
            let Some(address) = address_key_for(&frozen) else {
                return Ok(AcceptOutcome::Refused(
                    RefuseReason::ReplaceableBaseOnRegularEvent,
                ));
            };
            let address_key = address.to_redb_key();
            let winner = match ingest
                .addr_index
                .get(address_key.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value())
            {
                Some(event_key) => ingest.canonical.load_by_key(event_key)?,
                None => None,
            };
            let actual = winner.as_ref().map(|stored| stored.event.id);
            if actual != expected {
                return Ok(AcceptOutcome::Refused(
                    RefuseReason::ReplaceableBaseChanged { expected, actual },
                ));
            }
            // `max(clock, winner.created_at + 1)`, computed against the
            // row the comparison just held — the whole reason the stamp
            // belongs inside this transaction rather than in the caller.
            // A stale base cannot produce a stale stamp because a stale
            // base never gets this far.
            if monotonic_stamp {
                if let Some(winner) = &winner {
                    if frozen.created_at <= winner.event.created_at {
                        // `checked_add` rather than saturating: at
                        // `u64::MAX` there is no greater second to move to,
                        // so the write keeps the caller's clock and loses
                        // through the ordinary stale door rather than
                        // silently tying the winner. This also keeps
                        // "restamped implies strictly greater" true, which
                        // is what makes `Stale` unreachable for a stamped
                        // write (see `AcceptOutcome::accepted_row`).
                        if let Some(next) = winner.event.created_at.as_secs().checked_add(1) {
                            frozen = crate::restamped(&frozen, Timestamp::from_secs(next));
                        }
                    }
                }
            }
        }

        let existing = ingest.canonical.load_by_id(&frozen.id)?;
        let is_deletion = frozen.kind == Kind::EventDeletion;

        // Dedup detection: checked against BOTH the live `EVENTS` row
        // AND every OTHER intent's `PUBLISH_QUEUE_DISPLACED` stash (issue #2
        // P0 correction, codex-nova ruling) — a duplicate accepted
        // while its canonical predecessor is currently sitting
        // displaced (superseded by a later local edit, not yet
        // restored) must ALSO join that stash entry's owner set,
        // otherwise it would be silently treated as a fresh insert and
        // strand its own obligation outside the shared ownership
        // entirely. See `find_any_displaced_key_by_event_id_in_txn`'s
        // doc.
        enum DupLoc {
            Live(EventKey, Box<StoredEvent>),
            Stash([u8; 8]),
        }
        let dup_loc = if let Some((event_key, stored)) = existing {
            Some(DupLoc::Live(event_key, Box::new(stored)))
        } else {
            find_any_displaced_key_by_event_id_in_txn(&ingest.publish_queue_displaced, frozen.id)?
                .map(DupLoc::Stash)
        };

        // Same tombstone-refusal + dedup-by-id + replaceable/addressable
        // supersession rules `insert` runs — see this fn's own doc and
        // `AcceptOutcome`'s. `Refused` is the ONLY branch that skips
        // both the journal write below AND `IntentId`/receipt-id
        // allocation.
        let (result, displaced): (AcceptOutcome, Option<StoredEvent>) = if let Some(dup_loc) =
            dup_loc
        {
            let intent_id = alloc_intent_id_in_txn(&mut ingest.publish_queue_meta)?;
            let receipt_id = alloc_receipt_id_in_txn(&mut ingest.publish_queue_meta)?;
            let mut existing_record = match &dup_loc {
                DupLoc::Live(_event_key, stored) => stored_event_to_record(stored),
                DupLoc::Stash(key) => stored_event_to_record(
                    &decode_displaced(
                        ingest
                            .publish_queue_displaced
                            .get(key)
                            .map_err(persist_err)?
                            .expect("just found this key")
                            .value(),
                    )
                    .map_err(|error| codec_error("displaced event", error))?,
                ),
            };
            // codex-nova ruling: a row with NO local provenance at
            // all is purely relay-observed — its event signature
            // signature is by construction already real (never a
            // sentinel, since `insert` only ever stores what a
            // relay actually delivered), so it counts as "already
            // signed" exactly like a locally-owned row whose own
            // `sig_state` is `Signed`.
            let already_signed = existing_record
                .local
                .as_ref()
                .map(|l| l.sig_state == SigState::Signed)
                .unwrap_or(true);

            // Architecture review correction (issue #2, team-lead
            // decision): this new intent joins the existing row's
            // owner set — an exact `Duplicate` must retain
            // INDEPENDENT ownership rather than being silently
            // coalesced into whichever intent already backs the
            // row (see `LocalOrigin`'s doc for why coalescing was
            // rejected). This now applies even to a PURELY
            // relay-observed row (codex-nova ruling): its `local`
            // becomes `Some` for the first time, tracking this
            // intent's own obligation.
            let mut owners = existing_record
                .local
                .as_ref()
                .map(|l| l.owners.clone())
                .unwrap_or_default();
            owners.insert(intent_id);
            let row_sig_state = existing_record
                .local
                .as_ref()
                .map(|l| l.sig_state)
                .unwrap_or(SigState::Signed);
            existing_record.local = Some(LocalOrigin {
                owners,
                sig_state: row_sig_state,
            });
            match &dup_loc {
                DupLoc::Live(event_key, _stored) => {
                    ingest
                        .canonical
                        .replace_local(*event_key, existing_record.local.clone())?;
                }
                DupLoc::Stash(key) => {
                    let encoded = encode_stored_event_record(&existing_record);
                    ingest
                        .publish_queue_displaced
                        .insert(key, encoded.as_slice())
                        .map_err(persist_err)?;
                }
            }

            // Issue #61 P0 correction: an exact-duplicate kind:5
            // intent must own an INDEPENDENT suppression claim
            // too — otherwise cancelling the canonical original
            // while this duplicate remains pending would
            // incorrectly reveal a target it is still obligated
            // to delete. Only meaningful while still PENDING — an
            // already-signed kind:5's tombstones are already
            // permanent, nothing provisional left to claim.
            if frozen.kind == Kind::EventDeletion && !already_signed {
                let (_hidden, claims) =
                    process_kind5_deletions_provisional_in_txn(ingest, intent_id, &frozen)?;
                let encoded_claims =
                    encode_claims(&claims).map_err(|error| codec_error("claims", error))?;
                ingest
                    .publish_queue_kind5_claims
                    .insert(&intent_key(intent_id), encoded_claims.as_slice())
                    .map_err(persist_err)?;
            }

            let row = record_to_stored_event(&existing_record);

            // codex-nova ruling: a duplicate of an ALREADY-signed
            // row (local or relay) must itself start `Signed`,
            // journaling the CANONICAL bytes (`row.event`, not
            // this call's own sentinel-signed `frozen`) — an
            // offline co-owner signer must never strand a receipt
            // behind an event that's already validly signed, and
            // there is nothing left for THIS intent to sign. The
            // shared R7 journal-write section below picks these
            // overridden values up.
            if already_signed {
                frozen = row.event.clone();
                sig_state = IntentSigState::Signed;
                receipt_state = ReceiptState::Signed;
            }

            (
                AcceptOutcome::Duplicate {
                    intent_id,
                    receipt_id,
                    row,
                },
                None,
            )
        } else if tombstone_refuses(ingest, &frozen)? {
            (AcceptOutcome::Refused(RefuseReason::Tombstoned), None)
        } else {
            let intent_id = alloc_intent_id_in_txn(&mut ingest.publish_queue_meta)?;
            let receipt_id = alloc_receipt_id_in_txn(&mut ingest.publish_queue_meta)?;
            let local = LocalOrigin {
                owners: BTreeSet::from([intent_id]),
                sig_state: SigState::Pending,
            };
            let stored = StoredEvent {
                event: frozen.clone(),
                provenance: Provenance {
                    seen: BTreeMap::new(),
                    local: Some(local),
                },
            };
            match address_key_for(&frozen) {
                None => {
                    let event_key = ingest
                        .canonical
                        .insert_new(&stored.event, &stored.provenance)?;
                    ingest.insert_indexes(&frozen, event_key)?;
                    if let Some(ts) = frozen.tags.expiration().copied() {
                        let exp_key = expiration_key(ts, &frozen.id);
                        ingest
                            .expiration_index
                            .insert(&exp_key, event_key)
                            .map_err(persist_err)?;
                    }
                    // Architecture review correction: a
                    // locally-composed kind:5 draft stages a
                    // REVERSIBLE suppression claim over every
                    // target it names, immediately, in this same
                    // transaction — issue #2's "no app optimistic
                    // mirror" promise extends to local deletions
                    // too. Kind:5 has no replaceable/addressable
                    // address, so this branch is the only one it
                    // can ever reach (mirrors `insert`'s own
                    // kind:5 invariant). See
                    // `SuppressClaimRecord`'s doc for why this
                    // hides rather than removes: `compensate_write`
                    // can then simply drop the claim (nothing to
                    // re-insert, the row never left), and the
                    // target's OWN `promote_signed`/
                    // `compensate_write` keep working on exactly
                    // the row they always did.
                    if is_deletion {
                        let (hidden, claims) =
                            process_kind5_deletions_provisional_in_txn(ingest, intent_id, &frozen)?;
                        let encoded_claims =
                            encode_claims(&claims).map_err(|error| codec_error("claims", error))?;
                        ingest
                            .publish_queue_kind5_claims
                            .insert(&intent_key(intent_id), encoded_claims.as_slice())
                            .map_err(persist_err)?;
                        (
                            AcceptOutcome::Kind5Processed {
                                intent_id,
                                receipt_id,
                                row: stored,
                                hidden,
                            },
                            None,
                        )
                    } else {
                        (
                            AcceptOutcome::Inserted {
                                intent_id,
                                receipt_id,
                                row: stored,
                            },
                            None,
                        )
                    }
                }
                Some(addr_key) => {
                    let addr_key_str = addr_key.to_redb_key();
                    let current_key = ingest
                        .addr_index
                        .get(addr_key_str.as_str())
                        .map_err(persist_err)?
                        .map(|guard| guard.value());

                    match current_key {
                        None => {
                            let event_key = ingest
                                .canonical
                                .insert_new(&stored.event, &stored.provenance)?;
                            ingest
                                .addr_index
                                .insert(addr_key_str.as_str(), event_key)
                                .map_err(persist_err)?;
                            ingest.insert_indexes(&frozen, event_key)?;
                            if let Some(ts) = frozen.tags.expiration().copied() {
                                let exp_key = expiration_key(ts, &frozen.id);
                                ingest
                                    .expiration_index
                                    .insert(&exp_key, event_key)
                                    .map_err(persist_err)?;
                            }
                            (
                                AcceptOutcome::Inserted {
                                    intent_id,
                                    receipt_id,
                                    row: stored,
                                },
                                None,
                            )
                        }
                        Some(current_key) => {
                            let current = ingest
                                .canonical
                                .load_by_key(current_key)?
                                .ok_or_else(|| missing_addr_index_target(current_key))?;
                            let current_event = &current.event;

                            if candidate_wins(&frozen, current_event) {
                                let replaced =
                                    remove_row_in_txn(ingest, current_event.id, |_| true)?
                                        .ok_or_else(|| missing_addr_index_target(current_key))?;

                                let event_key = ingest
                                    .canonical
                                    .insert_new(&stored.event, &stored.provenance)?;
                                ingest
                                    .addr_index
                                    .insert(addr_key_str.as_str(), event_key)
                                    .map_err(persist_err)?;
                                ingest.insert_indexes(&frozen, event_key)?;
                                if let Some(ts) = frozen.tags.expiration().copied() {
                                    let exp_key = expiration_key(ts, &frozen.id);
                                    ingest
                                        .expiration_index
                                        .insert(&exp_key, event_key)
                                        .map_err(persist_err)?;
                                }
                                let (displaced, retired) = retire_superseded_owners_in_txn(
                                    ingest,
                                    write_txn,
                                    &mut publish_queue_correlations,
                                    replaced.clone(),
                                )?;
                                (
                                    AcceptOutcome::Superseded {
                                        intent_id,
                                        receipt_id,
                                        row: stored,
                                        replaced: Box::new(replaced.clone()),
                                        retired,
                                    },
                                    displaced,
                                )
                            } else {
                                (
                                    AcceptOutcome::Stale {
                                        intent_id,
                                        receipt_id,
                                    },
                                    None,
                                )
                            }
                        }
                    }
                }
            }
        };

        #[cfg(test)]
        store.crash_if(RedbCrashPoint::AcceptAfterEventBeforeJournal);

        // R7: the intent's full journal payload AND the retained
        // receipt record commit in this SAME transaction as the
        // event-table mutation (and the `IntentId`/receipt-id
        // allocation) above — a crash here leaves either nothing or a
        // fully `recover_publish_queue`-able `Accepted`. R3: `Refused` is the
        // one outcome that journals nothing at all.
        if let (Some(intent_id), Some(receipt_id)) =
            (result.journaled_intent_id(), result.journaled_receipt_id())
        {
            let key = intent_key(intent_id);
            let intent_record = PublishQueueIntentRecord {
                receipt_id,
                work: super::publish_queue::PublishQueueIntentRecordWork::Event {
                    frozen: frozen.clone(),
                    routing: routing.clone(),
                    sig_state,
                },
                expected_pubkey,
                signing_identity_ref,
                accepted_at,
            };
            let encoded_intent =
                encode_intent(&intent_record).map_err(|error| codec_error("intent", error))?;
            ingest
                .publish_queue_intents
                .insert(&key, encoded_intent.as_slice())
                .map_err(persist_err)?;

            if let Some(displaced) = &displaced {
                let encoded_displaced = encode_displaced(displaced)
                    .map_err(|error| codec_error("displaced event", error))?;
                ingest
                    .publish_queue_displaced
                    .insert(&key, encoded_displaced.as_slice())
                    .map_err(persist_err)?;
            }

            // Architecture review correction: the RETAINED receipt
            // record, independent of `PUBLISH_QUEUE_INTENTS`'s open-work row.
            // `receipt_state` is `Accepted` except for the `Duplicate`-
            // of-an-already-signed-row case above, which overrides it
            // to `Signed` (codex-nova ruling).
            let receipt_record = PublishQueueReceiptRecord {
                intent_id: Some(intent_id),
                expected_pubkey,
                accepted_at: Some(accepted_at),
                payload: crate::PublishQueueReceiptPayload::Event {
                    event_id: frozen.id,
                    state: receipt_state,
                },
                correlation: correlation.as_ref().map(|token| token.as_ref().to_owned()),
                terminal_sequence: None,
                terminal_at: None,
                terminal_bytes: None,
            };
            let encoded_receipt = encode_receipt(&receipt_record);
            ingest
                .publish_queue_receipts
                .insert(&receipt_key(receipt_id), encoded_receipt.as_slice())
                .map_err(persist_err)?;

            // #591: journal the caller's correlation token, in this
            // SAME transaction, alongside the receipt id it now names.
            // Overwrite-safe even on a (contract-violating) reused
            // token: the door that would ever observe a stale mapping
            // is `lookup_correlation`, and the caller's own reuse is
            // documented as their contract violation, not a case this
            // store detects or refuses.
            if let Some(token) = &correlation {
                publish_queue_correlations
                    .insert(token.as_ref().as_bytes(), &receipt_key(receipt_id))
                    .map_err(persist_err)?;
            }
        }

        Ok(result)
    })?;
    if matches!(
        outcome,
        AcceptOutcome::Refused(
            RefuseReason::ReplaceableBaseOnRegularEvent
                | RefuseReason::ReplaceableBaseChanged { .. }
        )
    ) {
        return Ok(outcome);
    }
    super::publish_queue_ops::maintain_terminal_receipts_in_txn(
        write.transaction(),
        wall_clock_now(),
        TerminalRetentionLimits::PRODUCTION,
    )?;
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::AcceptBeforeCommit);
    write.commit_prepared(outcome)
}

pub(super) fn promote_signed(
    store: &mut RedbStore,
    intent_id: IntentId,
    verified: VerifiedSignature,
) -> Result<PromoteOutcome, PersistenceError> {
    let sig = verified.signature();
    let mut write = GovernedWrite::begin(store)?;
    let outcome = write.apply(|ingest, _write_txn| {
        let key = intent_key(intent_id);
        let intent_bytes = ingest
            .publish_queue_intents
            .get(&key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec());

        let outcome = match intent_bytes {
            None => PromoteOutcome::NotFound,
            Some(intent_bytes) => {
                let intent_record = decode_intent(&intent_bytes)
                    .map_err(|error| codec_error(&format!("intent {}", intent_id.0), error))?;
                let Some((frozen_event, intent_sig_state)) = intent_record.event() else {
                    return Ok(PromoteOutcome::NotFound);
                };
                // Intent binding (#768). `VerifiedSignature` proves one
                // `Event::verify` succeeded; it does NOT prove the event
                // it covered is the one THIS intent froze. Everything
                // below is irreversible — a permanent kind:5 tombstone
                // most of all — so the match runs before this
                // transaction touches a single table, and the
                // `GovernedWrite` is abandoned rather than committed.
                if verified.event_id() != frozen_event.id {
                    return Err(PersistenceError::invariant(format!(
                        "promotion evidence verifies event {} but intent {} froze event {}",
                        verified.event_id(),
                        intent_id.0,
                        frozen_event.id,
                    )));
                }
                // No-second-transition guard (codex-nova finding): a
                // repeat promotion (e.g. a duplicate signer completion)
                // must not overwrite an already-Signed row and re-emit
                // `Promoted` — the trait doc already promised
                // "already-promoted returns NotFound"; this enforces
                // it. Load-bearing for `AtMostOnce`: a second silent
                // transition here could let the caller re-publish.
                if intent_sig_state == IntentSigState::Signed {
                    return Ok(PromoteOutcome::NotFound);
                }
                let frozen_event = frozen_event.clone();
                let frozen_id = frozen_event.id;

                // Architecture review correction (load-bearing): is
                // this intent AMONG the owners of the LIVE row at its
                // own frozen id? A `Duplicate`/`Stale` intent never
                // had one of its own; a once-live row can since have
                // been superseded (locally or by a relay),
                // kind:5-deleted, or expired. Ownership is a SET
                // (issue #2, team-lead decision): an exact `Duplicate`
                // is a CO-OWNER of the SAME canonical row, not a
                // second row of its own — see `LocalOrigin`'s doc.
                let live_record = ingest
                    .canonical
                    .load_by_id(&frozen_id)?
                    .map(|(event_key, stored)| (event_key, stored_event_to_record(&stored)));
                let is_live = live_record.as_ref().is_some_and(|(_key, r)| {
                    r.local
                        .as_ref()
                        .is_some_and(|l| l.owners.contains(&intent_id))
                });

                // Row-level already-signed check: is the shared row/
                // stash entry ALREADY signed by some OTHER co-owner?
                // Structurally this should never actually be reached
                // in a healthy run any more (see below) — the eager
                // cross-owner propagation this call itself performs
                // means the per-intent guard above already catches a
                // co-owner's OWN later call — but it is kept as a
                // defensive fallback: never overwrite a canonical
                // signature that's already there.
                let already_signed = if is_live {
                    live_record
                        .as_ref()
                        .and_then(|(_key, r)| r.local.as_ref())
                        .is_some_and(|l| l.sig_state == SigState::Signed)
                } else if let Some(other_key) = find_displaced_key_by_event_id_in_txn(
                    &ingest.publish_queue_displaced,
                    frozen_id,
                    intent_id,
                )? {
                    let other_bytes = ingest
                        .publish_queue_displaced
                        .get(&other_key)
                        .map_err(persist_err)?
                        .expect("just found this key")
                        .value()
                        .to_vec();
                    let other_record = stored_event_to_record(
                        &decode_displaced(&other_bytes)
                            .map_err(|error| codec_error("displaced event", error))?,
                    );
                    other_record
                        .local
                        .as_ref()
                        .is_some_and(|l| l.sig_state == SigState::Signed)
                } else {
                    false
                };

                let mut signed_frozen_event = frozen_event.clone();
                signed_frozen_event.sig = sig;
                let (row, owners) = if is_live {
                    // Swap the sentinel for the real signature — same
                    // id (a NIP-01 id never depends on `sig`), so this
                    // is purely a value update: no EVENTS/ADDR_INDEX/
                    // BY_AUTHOR/BY_KIND key ever changes. Skipped
                    // entirely if `already_signed`: the canonical
                    // signature some OTHER owner already committed
                    // must never be overwritten.
                    let (event_key, mut record) = live_record.expect("checked is_live above");
                    if !already_signed {
                        let mut local = record.local.expect("checked is_live above");
                        local.sig_state = SigState::Signed;
                        record.local = Some(local);
                        record.event = signed_frozen_event.clone();
                        ingest.canonical.replace_event(event_key, &record.event)?;
                        ingest
                            .canonical
                            .replace_local(event_key, record.local.clone())?;
                    }
                    let owners = record
                        .local
                        .as_ref()
                        .expect("checked is_live above")
                        .owners
                        .clone();
                    (
                        StoredEvent {
                            event: record.event,
                            provenance: Provenance {
                                seen: record.provenance,
                                local: record.local,
                            },
                        },
                        owners,
                    )
                } else if let Some(other_key) = find_displaced_key_by_event_id_in_txn(
                    &ingest.publish_queue_displaced,
                    frozen_id,
                    intent_id,
                )? {
                    // Not live. If this intent's exact frozen bytes
                    // are sitting in some OTHER intent's displaced
                    // stash (it was superseded by a later local edit
                    // before it could sign), sync the real signature
                    // into that stash entry too — otherwise a future
                    // restore of it would resurrect a stale sentinel
                    // copy of an intent that actually did sign. Same
                    // `already_signed` skip as the live case above.
                    let other_bytes = ingest
                        .publish_queue_displaced
                        .get(&other_key)
                        .map_err(persist_err)?
                        .expect("just found this key")
                        .value()
                        .to_vec();
                    let mut other_record = stored_event_to_record(
                        &decode_displaced(&other_bytes)
                            .map_err(|error| codec_error("displaced event", error))?,
                    );
                    if !already_signed {
                        other_record.event = signed_frozen_event.clone();
                        if let Some(local) = other_record.local.as_mut() {
                            local.sig_state = SigState::Signed;
                        }
                        let encoded_other = encode_stored_event_record(&other_record);
                        ingest
                            .publish_queue_displaced
                            .insert(&other_key, encoded_other.as_slice())
                            .map_err(persist_err)?;
                    }
                    let owners = other_record
                        .local
                        .as_ref()
                        .expect("just matched an owned stash entry")
                        .owners
                        .clone();
                    (
                        StoredEvent {
                            event: other_record.event,
                            provenance: Provenance {
                                seen: other_record.provenance,
                                local: other_record.local,
                            },
                        },
                        owners,
                    )
                } else {
                    // Neither live nor in anyone's stash — synthesize
                    // the resulting signed bytes from the journal's
                    // own copy. The engine can still publish these
                    // even though this intent does not (or no longer)
                    // win any local address. Only reachable when
                    // `!already_signed`: `already_signed` requires a
                    // matching live row or stash entry to have been
                    // found above.
                    (
                        StoredEvent {
                            event: signed_frozen_event.clone(),
                            provenance: Provenance {
                                seen: BTreeMap::new(),
                                local: Some(LocalOrigin {
                                    owners: BTreeSet::from([intent_id]),
                                    sig_state: SigState::Signed,
                                }),
                            },
                        },
                        BTreeSet::from([intent_id]),
                    )
                };
                // codex-nova ruling (tightened after review): the
                // FIRST owner to sign atomically transitions EVERY
                // co-owner's OWN journal/receipt to `Signed` against
                // the SAME canonical bytes, in THIS SAME transaction
                // — never lazily deferred until (or unless) each
                // co-owner separately calls `promote_signed` itself.
                // An offline co-owner signer that never calls back
                // must never strand its receipt behind an event
                // that's already validly signed. Shared with
                // `reinsert_stashed_in_txn`'s dedup collision and
                // `insert`'s relay-dedup-onto-pending path.
                let co_signed: Vec<IntentId> = fan_out_signed_in_txn(ingest, &owners, &row.event)?
                    .into_iter()
                    .filter(|owner_id| *owner_id != intent_id)
                    .collect();

                PromoteOutcome::Promoted {
                    row: Box::new(row),
                    co_signed,
                }
            }
        };
        Ok(outcome)
    })?;
    if matches!(outcome, PromoteOutcome::NotFound) {
        return Ok(outcome);
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::PromoteBeforeCommit);
    write.commit_prepared(outcome)
}

pub(super) fn compensate_write_with_state(
    store: &mut RedbStore,
    intent_id: IntentId,
    reason: crate::CompensationReason,
) -> Result<CompensateOutcome, PersistenceError> {
    let terminal_state = match reason {
        crate::CompensationReason::Failure => ReceiptState::Compensated,
        crate::CompensationReason::ExplicitCancellation => ReceiptState::Cancelled,
    };
    let mut write = GovernedWrite::begin(store)?;
    let outcome = write.apply(|ingest, _write_txn| {
        let key = intent_key(intent_id);
        let intent_bytes = ingest
            .publish_queue_intents
            .get(&key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec());

        let outcome = match intent_bytes {
            None => CompensateOutcome::NotFound,
            Some(intent_bytes) => {
                let intent_record = decode_intent(&intent_bytes)
                    .map_err(|error| codec_error(&format!("intent {}", intent_id.0), error))?;
                let Some((frozen_event, intent_sig_state)) = intent_record.event() else {
                    return Ok(CompensateOutcome::NotFound);
                };
                if intent_sig_state == IntentSigState::Signed {
                    // Pre-signature only (retraction doc §4.2's
                    // "Promotion correction").
                    CompensateOutcome::AlreadySigned
                } else {
                    let frozen_event = frozen_event.clone();
                    let frozen_id = frozen_event.id;
                    let live = ingest.canonical.load_by_id(&frozen_id)?;
                    let is_live = live.as_ref().is_some_and(|(_event_key, stored)| {
                        let r = stored_event_to_record(stored);
                        r.local
                            .as_ref()
                            .is_some_and(|l| l.owners.contains(&intent_id))
                    });

                    if is_live {
                        // Architecture review correction (issue #2,
                        // team-lead decision): removing THIS intent
                        // from the row's owner set only actually
                        // retracts the canonical row once the set is
                        // EMPTY, `sig_state` is still `Pending`, AND
                        // no relay has independently confirmed it — an
                        // exact-`Duplicate`'s still-open obligation,
                        // an already-`Signed` state some OTHER owner
                        // committed, or independent relay provenance,
                        // must all survive this one intent's
                        // cancellation (see `LocalOrigin`'s doc).
                        // §4.2: `remove(id, Rejected)` writes no
                        // tombstone (only kind:5 processing ever
                        // does).
                        let (event_key, stored) = live.as_ref().expect("checked is_live above");
                        let mut record = stored_event_to_record(stored);
                        let mut local = record.local.clone().expect("checked is_live above");
                        local.owners.remove(&intent_id);
                        let should_retract = local.owners.is_empty()
                            && local.sig_state == SigState::Pending
                            && record.provenance.is_empty();
                        if should_retract {
                            remove_row_in_txn(ingest, frozen_id, |_| true)?;
                        } else {
                            record.local = Some(local);
                            ingest.canonical.replace_local(*event_key, record.local)?;
                        }
                    } else if let Some(other_key) = find_displaced_key_by_event_id_in_txn(
                        &ingest.publish_queue_displaced,
                        frozen_id,
                        intent_id,
                    )? {
                        // Not live, but sitting in someone else's
                        // displaced stash (chained local supersession
                        // before this intent could sign) — remove
                        // THIS intent from THAT stash entry's owner
                        // set, same conditional-retraction rule as the
                        // live case above: an exact-`Duplicate`
                        // co-owner (or a signed/relay-confirmed state)
                        // sitting in the SAME stash slot must survive
                        // this intent's cancellation too.
                        let other_bytes = ingest
                            .publish_queue_displaced
                            .get(&other_key)
                            .map_err(persist_err)?
                            .expect("just found this key")
                            .value()
                            .to_vec();
                        let mut other_record = stored_event_to_record(
                            &decode_displaced(&other_bytes)
                                .map_err(|error| codec_error("displaced event", error))?,
                        );
                        let Some(mut local) = other_record.local.clone() else {
                            return Err(PersistenceError::invariant(format!(
                                "displaced event for intent {} lost local ownership",
                                intent_id.0
                            )));
                        };
                        local.owners.remove(&intent_id);
                        let should_drop = local.owners.is_empty()
                            && local.sig_state == SigState::Pending
                            && other_record.provenance.is_empty();
                        if should_drop {
                            ingest
                                .publish_queue_displaced
                                .remove(&other_key)
                                .map_err(persist_err)?;
                        } else {
                            other_record.local = Some(local);
                            let encoded_other = encode_stored_event_record(&other_record);
                            ingest
                                .publish_queue_displaced
                                .insert(&other_key, encoded_other.as_slice())
                                .map_err(persist_err)?;
                        }
                    }

                    ingest
                        .publish_queue_intents
                        .remove(&key)
                        .map_err(persist_err)?;
                    // THIS intent's OWN displaced predecessor (if any)
                    // is restored through the same one door regardless
                    // of whether its row was live or already gone for
                    // some other reason (kind:5/expiry/relay
                    // supersession) — `reinsert_stashed_in_txn`'s own
                    // tombstone check makes this safe even if the
                    // predecessor was itself since deleted or expired.
                    let displaced_bytes = ingest
                        .publish_queue_displaced
                        .remove(&key)
                        .map_err(persist_err)?
                        .map(|guard| guard.value().to_vec());
                    let restored = match displaced_bytes {
                        Some(bytes) => reinsert_stashed_in_txn(
                            ingest,
                            decode_displaced(&bytes)
                                .map_err(|error| codec_error("displaced event", error))?,
                        )?
                        .map(Box::new),
                        None => None,
                    };

                    // Architecture review requirement (kind:5
                    // suppression-claim reversal, codex-nova's model):
                    // if this was a still-pending kind:5 draft, drop
                    // its OWN claims outright — nothing was ever moved
                    // or removed, so there is nothing to re-insert.
                    // `revealed` is a true visibility DELTA (issue #61
                    // P1 correction), computed from before/after
                    // suppression state and deduped by event id — so
                    // a target still hidden by some OTHER intent's
                    // overlapping claim, one already gone for good
                    // because a different intent already promoted its
                    // own deletion of the same target, or one this
                    // claim's own (author/ceiling) component never
                    // actually covered in the first place (e.g. a
                    // wrong-author e-tag claim on a row some OTHER
                    // author holds), is correctly excluded.
                    let mut revealed = Vec::new();
                    let claims_bytes = ingest
                        .publish_queue_kind5_claims
                        .remove(&key)
                        .map_err(persist_err)?
                        .map(|guard| guard.value().to_vec());
                    if let Some(claims_bytes) = claims_bytes {
                        let claims = decode_claims(&claims_bytes).map_err(|error| {
                            codec_error(
                                &format!("suppression claims for intent {}", intent_id.0),
                                error,
                            )
                        })?;

                        let mut candidate_ids: Vec<EventId> = Vec::new();
                        let mut seen_candidates: HashSet<EventId> = HashSet::new();
                        for claim in &claims {
                            let target_id = match claim {
                                SuppressClaimRecord::Id { target, .. } => Some(*target),
                                SuppressClaimRecord::Addr { key: addr_key, .. } => {
                                    let addr_key = std::str::from_utf8(addr_key).map_err(|_| {
                                        PersistenceError::invariant(
                                            "address suppression key is not UTF-8",
                                        )
                                    })?;
                                    let event_key = ingest
                                        .addr_index
                                        .get(addr_key)
                                        .map_err(persist_err)?
                                        .map(|guard| guard.value());
                                    match event_key {
                                        Some(event_key) => ingest
                                            .canonical
                                            .load_by_key(event_key)?
                                            .map(|stored| stored.event.id),
                                        None => None,
                                    }
                                }
                            };
                            if let Some(target_id) = target_id {
                                if seen_candidates.insert(target_id) {
                                    candidate_ids.push(target_id);
                                }
                            }
                        }

                        let mut visible_before: HashMap<EventId, bool> = HashMap::new();
                        for id in &candidate_ids {
                            let visible = match ingest.canonical.load_by_id(id)? {
                                None => false,
                                Some((_key, se)) => !is_suppressed_in_txn(
                                    &ingest.publish_queue_suppress_by_id,
                                    &ingest.publish_queue_suppress_by_addr,
                                    &se.event,
                                )?,
                            };
                            visible_before.insert(*id, visible);
                        }

                        for claim in claims {
                            match claim {
                                SuppressClaimRecord::Id {
                                    target,
                                    claiming_author,
                                } => {
                                    remove_claimant_in_txn(
                                        &mut ingest.publish_queue_suppress_by_id,
                                        &id_claim_key(&target, &claiming_author),
                                        intent_id,
                                    )?;
                                }
                                SuppressClaimRecord::Addr { key: addr_key, .. } => {
                                    remove_addr_claimant_in_txn(
                                        &mut ingest.publish_queue_suppress_by_addr,
                                        &addr_key,
                                        intent_id,
                                    )?;
                                }
                            }
                        }

                        for id in candidate_ids {
                            if visible_before.get(&id).copied().unwrap_or(false) {
                                continue;
                            }
                            if let Some((_key, se)) = ingest.canonical.load_by_id(&id)? {
                                if !is_suppressed_in_txn(
                                    &ingest.publish_queue_suppress_by_id,
                                    &ingest.publish_queue_suppress_by_addr,
                                    &se.event,
                                )? {
                                    revealed.push(se);
                                }
                            }
                        }
                    }

                    update_publish_queue_receipt(
                        &mut ingest.publish_queue_receipts,
                        intent_record.receipt_id,
                        terminal_state,
                    )?;
                    mark_terminal_receipt(
                        &mut ingest.publish_queue_receipts,
                        &mut ingest.publish_queue_meta,
                        intent_record.receipt_id,
                        wall_clock_now(),
                        0,
                    )?;

                    CompensateOutcome::Compensated { restored, revealed }
                }
            }
        };
        Ok(outcome)
    })?;
    super::publish_queue_ops::maintain_terminal_receipts_in_txn(
        write.transaction(),
        wall_clock_now(),
        TerminalRetentionLimits::PRODUCTION,
    )?;
    #[cfg(any(test, feature = "test-instrumentation"))]
    if std::mem::take(&mut store.fail_next_compensation_with_state) {
        return Err(PersistenceError::invariant("injected compensation failure"));
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::CompensateBeforeCommit);
    write.commit_prepared(outcome)
}
