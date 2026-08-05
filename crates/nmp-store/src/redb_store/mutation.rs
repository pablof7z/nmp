use super::canonical::stored_event_to_record;
use super::ingest_txn::{GovernedIngestTxn, GovernedPublishQueueMap, RedbIngestTxn};
use super::publish_queue::{
    add_addr_claimant_in_txn, add_claimant_in_txn, is_suppressed_in_txn, SuppressClaimRecord,
};
use super::publish_queue_codec::{
    codec_error, decode_addr_claimants, decode_claimants, decode_claims, decode_displaced,
    decode_intent, decode_receipt, encode_addr_claimants, encode_claimants, encode_intent,
    encode_receipt, id_claim_key, intent_key, receipt_key,
};
use super::query::{expiration_key, AddrTombstoneRecord};
use super::schema::{addr_tombstone_key, id_tombstone_key, persist_err, EventKey};
use super::{
    address_key_for, address_key_for_coordinate, candidate_wins, BTreeSet, Event, EventId, HashMap,
    HashSet, IntentId, IntentSigState, Kind, LocalOrigin, PersistenceError, ReceiptState,
    RelayObserved, SigState, StoredEvent,
};
use redb::ReadableTable;

/// The address index naming a canonical row that is gone (#790). This used
/// to be an `.expect()` on every one of the six sites that dereference
/// `ADDR_INDEX`; it is a broken relational invariant between two persisted
/// tables, which is exactly the class the issue says must become a typed
/// refusal. Returning `None`/skipping instead would turn a corrupt index
/// into a silent wrong answer at the address — the false miss this closes.
pub(super) fn missing_addr_index_target(event_key: EventKey) -> PersistenceError {
    PersistenceError::invariant(format!(
        "address index points at missing canonical event {event_key}"
    ))
}

/// Read-side tombstone check shared by `insert`
/// (retraction-and-negative-deltas.md §2): `true` iff `event` must be
/// `Refused(Tombstoned)`. Mirrors `MemoryStore::tombstone_refuses` exactly,
/// including the deferred NIP-09 author-only check for an id-tombstone
/// written before its target ever arrived: refused iff `event.pubkey`
/// itself claimed this exact id, regardless of any OTHER author's
/// (irrelevant) claim on the same id.
pub(super) fn tombstone_refuses<T: GovernedIngestTxn>(
    txn: &T,
    event: &Event,
) -> Result<bool, PersistenceError> {
    let key = id_tombstone_key(&event.id, &event.pubkey);
    if txn.tombstone_get(&key)?.is_some() {
        return Ok(true);
    }
    if let Some(key) = address_key_for(event) {
        let key_str = key.to_redb_key();
        if let Some(encoded) = txn.tombstone_get(&addr_tombstone_key(&key_str))? {
            let rec: AddrTombstoneRecord = serde_json::from_slice(&encoded).map_err(|error| {
                PersistenceError::invariant(format!("decode address tombstone {key_str}: {error}"))
            })?;
            if event.created_at.as_secs() <= rec.ceiling {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Remove `id`'s row within an already-open write transaction, iff
/// `predicate` accepts the decoded row — clearing the address index (if it
/// still points at `id`), the expiration index (if the row carried a
/// NIP-40 `expiration`), and the packed ordered-postings query indexes in
/// the same pass. Shared by the trait's own `remove` (`predicate` always
/// `true`) and kind:5 processing (`predicate` is the NIP-09 author-only
/// check).
#[allow(clippy::too_many_arguments)]
pub(super) fn remove_row_in_txn<T: GovernedIngestTxn>(
    txn: &mut T,
    id: EventId,
    predicate: impl FnOnce(&StoredEvent) -> bool,
) -> Result<Option<StoredEvent>, PersistenceError> {
    let Some((event_key, se)) = txn.load_by_id(&id)? else {
        return Ok(None);
    };
    if !predicate(&se) {
        return Ok(None);
    }

    txn.remove_canonical(event_key, &id)?;
    txn.remove_indexes(&se.event, event_key)?;

    if let Some(addr_key) = address_key_for(&se.event) {
        let addr_key_str = addr_key.to_redb_key();
        let still_points_here = txn.address_get(addr_key_str.as_str())? == Some(event_key);
        if still_points_here {
            txn.address_remove(addr_key_str.as_str())?;
        }
    }

    if let Some(ts) = se.event.tags.expiration().copied() {
        let exp_key = expiration_key(ts, &id);
        txn.expiration_remove(&exp_key)?;
    }

    Ok(Some(se))
}

/// kind:5 processing (retraction-and-negative-deltas.md §2), run within the
/// same write transaction that just stored the deleting event itself. For
/// each `e`-tag id / `a`-tag coordinate: author-verify (immediately if the
/// target is held or the coordinate carries its own pubkey; deferred via
/// `tombstone_refuses` at the target's own future insert otherwise), write
/// the PERMANENT tombstone, and drop the row if currently held. Returns
/// every row actually dropped.
#[allow(clippy::too_many_arguments)]
pub(super) fn process_kind5_deletions<T: GovernedIngestTxn>(
    txn: &mut T,
    deleting: &Event,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    let mut deleted = Vec::new();
    let deleting_id_hex = deleting.id.to_hex();
    let deleting_author_hex = deleting.pubkey.to_hex();

    let target_ids: Vec<EventId> = deleting.tags.event_ids().copied().collect();
    for target_id in target_ids {
        if let Some(removed) =
            remove_row_in_txn(txn, target_id, |se| se.event.pubkey == deleting.pubkey)?
        {
            deleted.push(removed);
        }
        // Claim recorded regardless of hold state right now -- a target
        // not yet held is checked, deferred, by `tombstone_refuses` at the
        // moment it actually arrives. NEVER collapse another author's
        // existing claim on this same id (composite key -- see
        // `TOMBSTONES`'s doc): each claiming author gets its own row.
        let key = id_tombstone_key(&target_id, &deleting.pubkey);
        txn.tombstone_put(&key, deleting.id.as_bytes())?;
    }

    let coords: Vec<_> = deleting.tags.coordinates().cloned().collect();
    for coord in coords {
        if coord.public_key != deleting.pubkey {
            // NIP-09 author-only: a coordinate naming a pubkey other than
            // this deletion's own author carries no authority at all here
            // -- skip entirely, no tombstone recorded.
            continue;
        }
        let Some(key) = address_key_for_coordinate(&coord) else {
            continue;
        };
        let key_str = key.to_redb_key();

        let tombstone_key = addr_tombstone_key(&key_str);
        let existing_ceiling = txn
            .tombstone_get(&tombstone_key)?
            .map(|encoded| {
                serde_json::from_slice::<AddrTombstoneRecord>(&encoded)
                    .map(|rec| rec.ceiling)
                    .map_err(|error| {
                        PersistenceError::invariant(format!(
                            "decode address tombstone {key_str}: {error}"
                        ))
                    })
            })
            .transpose()?;
        let new_ceiling = deleting.created_at.as_secs();
        if existing_ceiling.is_none_or(|ceiling| new_ceiling > ceiling) {
            let record = AddrTombstoneRecord {
                ceiling: new_ceiling,
                deleting_event_id: deleting_id_hex.clone(),
                deleting_author: deleting_author_hex.clone(),
            };
            let encoded = serde_json::to_vec(&record).expect("redb: encode addr tombstone");
            txn.tombstone_put(&tombstone_key, &encoded)?;
        }

        let current_key = txn.address_get(key_str.as_str())?;
        if let Some(current_key) = current_key {
            let current = txn
                .load_by_key(current_key)?
                .ok_or_else(|| missing_addr_index_target(current_key))?;
            let current_id = current.event.id;
            if let Some(removed) = remove_row_in_txn(txn, current_id, |se| {
                se.event.created_at <= deleting.created_at
            })? {
                deleted.push(removed);
            }
        }
    }

    Ok(deleted)
}

/// Atomically transition every intent in `owners` whose OWN journal is
/// still `Pending` to `Signed`, using `canonical_event` as the frozen
/// bytes each owner's journal now reflects, dropping each owner's own
/// displaced stash too (R6) and closing each owner's own kind:5
/// suppression claims if `canonical_event` is a deletion (running the
/// FULL, permanent [`process_kind5_deletions`] once, not per-owner).
/// Architecture review requirement (issue #2 P0 correction, codex-nova
/// ruling): `promote_signed`, [`reinsert_stashed_in_txn`]'s dedup
/// collision, and `insert`'s relay-dedup onto a pending sentinel must all
/// fan out IDENTICALLY — an offline co-owner signer must never strand a
/// receipt behind an event that's already validly signed, regardless of
/// HOW that signature became canonical. Mirrors
/// `MemoryStore::fan_out_signed` exactly. Returns every intent THIS call
/// actually transitioned (an already-`Signed` owner is left untouched and
/// excluded).
fn update_publish_queue_receipt<T: GovernedIngestTxn>(
    txn: &mut T,
    receipt_id: u64,
    state: ReceiptState,
) -> Result<(), PersistenceError> {
    let key = receipt_key(receipt_id);
    let encoded = txn
        .publish_queue_get(GovernedPublishQueueMap::Receipts, &key)?
        .ok_or_else(|| {
            PersistenceError::invariant(format!("missing delivery receipt {receipt_id}"))
        })?;
    let mut record = decode_receipt(&encoded)
        .map_err(|error| codec_error(&format!("receipt {receipt_id}"), error))?;
    record.state = state;
    txn.publish_queue_put(
        GovernedPublishQueueMap::Receipts,
        &key,
        &encode_receipt(&record),
    )
}

fn remove_claimant<T: GovernedIngestTxn>(
    txn: &mut T,
    key: &[u8; 64],
    intent_id: IntentId,
) -> Result<(), PersistenceError> {
    let map = GovernedPublishQueueMap::SuppressById;
    let Some(encoded) = txn.publish_queue_get(map, key)? else {
        return Ok(());
    };
    let mut claimants =
        decode_claimants(&encoded).map_err(|error| codec_error("claimants", error))?;
    claimants.retain(|id| *id != intent_id.0);
    if claimants.is_empty() {
        txn.publish_queue_remove(map, key)?;
    } else {
        let encoded =
            encode_claimants(&claimants).map_err(|error| codec_error("claimants", error))?;
        txn.publish_queue_put(map, key, &encoded)?;
    }
    Ok(())
}

fn remove_addr_claimant<T: GovernedIngestTxn>(
    txn: &mut T,
    key: &[u8],
    intent_id: IntentId,
) -> Result<(), PersistenceError> {
    let map = GovernedPublishQueueMap::SuppressByAddr;
    let Some(encoded) = txn.publish_queue_get(map, key)? else {
        return Ok(());
    };
    let mut claimants =
        decode_addr_claimants(&encoded).map_err(|error| codec_error("address claimants", error))?;
    claimants.retain(|claimant| claimant.intent_id != intent_id.0);
    if claimants.is_empty() {
        txn.publish_queue_remove(map, key)?;
    } else {
        let encoded = encode_addr_claimants(&claimants)
            .map_err(|error| codec_error("address claimants", error))?;
        txn.publish_queue_put(map, key, &encoded)?;
    }
    Ok(())
}

pub(super) fn fan_out_signed_in_txn<T: GovernedIngestTxn>(
    txn: &mut T,
    owners: &BTreeSet<IntentId>,
    canonical_event: &Event,
) -> Result<Vec<IntentId>, PersistenceError> {
    let mut transitioned = Vec::new();
    let is_deletion = canonical_event.kind == Kind::EventDeletion;
    for owner_id in owners {
        let owner_key = intent_key(*owner_id);
        txn.displaced_remove(&owner_key)?;
        let owner_intent = txn.publish_queue_get(GovernedPublishQueueMap::Intents, &owner_key)?;
        if let Some(owner_intent) = owner_intent {
            let mut owner_record = decode_intent(&owner_intent)
                .map_err(|error| codec_error(&format!("intent {}", owner_id.0), error))?;
            if owner_record.sig_state != IntentSigState::Signed {
                owner_record.sig_state = IntentSigState::Signed;
                owner_record.frozen = canonical_event.clone();
                let encoded_owner =
                    encode_intent(&owner_record).map_err(|error| codec_error("intent", error))?;
                txn.publish_queue_put(
                    GovernedPublishQueueMap::Intents,
                    &owner_key,
                    &encoded_owner,
                )?;
                update_publish_queue_receipt(txn, owner_record.receipt_id, ReceiptState::Signed)?;
                transitioned.push(*owner_id);
            }
        }
        if is_deletion {
            let claims =
                txn.publish_queue_remove(GovernedPublishQueueMap::Kind5Claims, &owner_key)?;
            if let Some(claims) = claims {
                let claims = decode_claims(&claims).map_err(|error| {
                    codec_error(
                        &format!("suppression claims for intent {}", owner_id.0),
                        error,
                    )
                })?;
                for claim in claims {
                    match claim {
                        SuppressClaimRecord::Id {
                            target,
                            claiming_author,
                        } => {
                            remove_claimant(
                                txn,
                                &id_claim_key(&target, &claiming_author),
                                *owner_id,
                            )?;
                        }
                        SuppressClaimRecord::Addr { key: addr_key, .. } => {
                            remove_addr_claimant(txn, &addr_key, *owner_id)?;
                        }
                    }
                }
            }
        }
    }
    if is_deletion {
        process_kind5_deletions(txn, canonical_event)?;
    }
    Ok(transitioned)
}

/// The PENDING half of kind:5 processing (architecture review requirement
/// — see [`SuppressClaimRecord`]'s doc): stages a REVERSIBLE suppression
/// claim over every e-tag id target and a-tag address target `deleting`
/// names, hiding whatever row currently lives there from `query` — via
/// [`is_suppressed_in_txn`], consulted at read time — WITHOUT moving or
/// removing it from `EVENTS`/`ADDR_INDEX`. Called for EVERY accepted
/// pending kind:5 intent, including an exact `Duplicate` (issue #61 P0
/// correction — see this fn's caller in `accept_write`). `promote_signed`
/// later drops these claims and runs the FULL, permanent
/// [`process_kind5_deletions`]; `compensate_write` just drops them
/// (nothing to re-insert — a claim never moved or removed the row it
/// names). Returns the rows that ACTUALLY became newly hidden as a result
/// of THIS call — a true visibility delta (issue #61 P1 correction),
/// computed from before/after suppression state and deduped by event id
/// — and the exact claims staged (for `PUBLISH_QUEUE_KIND5_CLAIMS`). Mirrors
/// `MemoryStore::process_kind5_deletions_provisional` exactly.
pub(super) fn process_kind5_deletions_provisional_in_txn(
    txn: &mut RedbIngestTxn<'_, '_>,
    intent_id: IntentId,
    deleting: &Event,
) -> Result<(Vec<StoredEvent>, Vec<SuppressClaimRecord>), PersistenceError> {
    let target_ids: Vec<EventId> = deleting.tags.event_ids().copied().collect();
    let coords: Vec<_> = deleting.tags.coordinates().cloned().collect();

    let mut candidate_ids: Vec<EventId> = Vec::new();
    let mut seen_candidates: HashSet<EventId> = HashSet::new();
    for target_id in &target_ids {
        if seen_candidates.insert(*target_id) {
            candidate_ids.push(*target_id);
        }
    }
    for coord in &coords {
        if coord.public_key != deleting.pubkey {
            continue;
        }
        if let Some(key) = address_key_for_coordinate(coord) {
            let key_str = key.to_redb_key();
            let current_key = txn
                .addr_index
                .get(key_str.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value());
            if let Some(current_key) = current_key {
                let current_id = txn
                    .canonical
                    .load_by_key(current_key)?
                    .ok_or_else(|| missing_addr_index_target(current_key))?
                    .event
                    .id;
                if seen_candidates.insert(current_id) {
                    candidate_ids.push(current_id);
                }
            }
        }
    }

    let mut visible_before: HashMap<EventId, bool> = HashMap::new();
    for id in &candidate_ids {
        let visible = match txn.canonical.load_by_id(id)? {
            None => false,
            Some((_key, se)) => !is_suppressed_in_txn(
                &txn.publish_queue_suppress_by_id,
                &txn.publish_queue_suppress_by_addr,
                &se.event,
            )?,
        };
        visible_before.insert(*id, visible);
    }

    let mut claims = Vec::new();
    for target_id in target_ids {
        let key = id_claim_key(&target_id, &deleting.pubkey);
        add_claimant_in_txn(&mut txn.publish_queue_suppress_by_id, &key, intent_id)?;
        claims.push(SuppressClaimRecord::Id {
            target: target_id,
            claiming_author: deleting.pubkey,
        });
    }
    for coord in coords {
        if coord.public_key != deleting.pubkey {
            // NIP-09 author-only: a coordinate naming a pubkey other than
            // this deletion's own author carries no authority at all here
            // -- skip entirely, no claim staged.
            continue;
        }
        let Some(key) = address_key_for_coordinate(&coord) else {
            continue;
        };
        let key_bytes = key.to_redb_key().into_bytes();
        add_addr_claimant_in_txn(
            &mut txn.publish_queue_suppress_by_addr,
            &key_bytes,
            intent_id,
            deleting.created_at,
        )?;
        claims.push(SuppressClaimRecord::Addr {
            key: key_bytes,
            ceiling: deleting.created_at.as_secs(),
            deleting_author: deleting.pubkey,
        });
    }

    let mut hidden = Vec::new();
    for id in candidate_ids {
        if !visible_before.get(&id).copied().unwrap_or(false) {
            continue;
        }
        if let Some((_key, se)) = txn.canonical.load_by_id(&id)? {
            if is_suppressed_in_txn(
                &txn.publish_queue_suppress_by_id,
                &txn.publish_queue_suppress_by_addr,
                &se.event,
            )? {
                hidden.push(se);
            }
        }
    }

    Ok((hidden, claims))
}

/// Scan `PUBLISH_QUEUE_DISPLACED` for the row (if any) whose stashed event's id is
/// `frozen_id` AND whose OWN local provenance's owner SET contains
/// `intent_id` — used by `promote_signed`/`compensate_write` for an intent
/// that is not currently the live row at its own id: it may instead be
/// sitting in some OTHER intent's displaced stash, having been superseded
/// by a LATER local edit before it could sign or be cancelled (architecture
/// review correction: a stashed predecessor "can later sign or cancel", so
/// its copy must be kept in sync or invalidated, never left to resurrect
/// stale or cancelled state). The `intent_id` membership check is
/// load-bearing, not redundant with the event-id match (codex-nova
/// finding): two DIFFERENT intents can share the same frozen event id (a
/// real intent and a byte-identical `Duplicate` of it), so matching by
/// event id alone could let one intent's promote/compensate call mutate or
/// delete an UNRELATED intent's stash entry. `owners` is a SET, not a
/// single id (issue #2, team-lead decision): a `Duplicate` accepted
/// BEFORE its predecessor was superseded is a CO-OWNER of the SAME stash
/// slot, not a slot of its own — see `LocalOrigin`'s doc. Returns the
/// OWNING stash's `PUBLISH_QUEUE_DISPLACED` key, if found — at most one, by
/// construction (a `StoredEvent` is only ever the CURRENT displaced stash
/// of the one intent that most recently superseded it).
pub(super) fn find_displaced_key_by_event_id_in_txn(
    publish_queue_displaced: &redb::Table<'_, &'static [u8; 8], &'static [u8]>,
    frozen_id: EventId,
    intent_id: IntentId,
) -> Result<Option<[u8; 8]>, PersistenceError> {
    for entry in publish_queue_displaced.iter().map_err(persist_err)? {
        let (key, value) = entry.map_err(persist_err)?;
        let record = stored_event_to_record(
            &decode_displaced(value.value())
                .map_err(|error| codec_error("displaced event", error))?,
        );
        let owned_by_this_intent = record
            .local
            .as_ref()
            .is_some_and(|l| l.owners.contains(&intent_id));
        if !owned_by_this_intent {
            continue;
        }
        if record.event.id == frozen_id {
            return Ok(Some(*key.value()));
        }
    }
    Ok(None)
}

/// Find ANY displaced-stash entry (regardless of which intent owns it)
/// whose frozen event id matches `frozen_id`. Architecture review
/// requirement (issue #2 P0 correction, codex-nova ruling): `accept_write`'s
/// duplicate detection must search the DISPLACED stash too, not only the
/// live `EVENTS` row — a duplicate accepted while its canonical predecessor
/// is currently sitting displaced (superseded by a later local edit, not
/// yet restored) must ALSO join that stash entry's owner set, or it would
/// be silently treated as a fresh insert and strand its own obligation
/// outside the shared ownership entirely. Unlike
/// [`find_displaced_key_by_event_id_in_txn`] (which only matches an entry a
/// SPECIFIC intent already owns), this is used for a BRAND NEW intent that
/// owns nothing yet, so it must match on event id alone.
pub(super) fn find_any_displaced_key_by_event_id_in_txn(
    publish_queue_displaced: &redb::Table<'_, &'static [u8; 8], &'static [u8]>,
    frozen_id: EventId,
) -> Result<Option<[u8; 8]>, PersistenceError> {
    for entry in publish_queue_displaced.iter().map_err(persist_err)? {
        let (key, value) = entry.map_err(persist_err)?;
        let record = stored_event_to_record(
            &decode_displaced(value.value())
                .map_err(|error| codec_error("displaced event", error))?,
        );
        if record.event.id == frozen_id {
            return Ok(Some(*key.value()));
        }
    }
    Ok(None)
}

/// Re-admit a durably-stashed predecessor `se` through the ordinary
/// dedup/tombstone/supersession rules `insert` runs, preserving its FULL
/// original provenance (both relay `seen` history and any `local` origin)
/// rather than reconstructing it from a single fresh observation —
/// `compensate_write`'s compensating re-insert (retraction-and-negative-
/// deltas.md §4.2: "through the same one door... wins its address back by
/// ordinary supersession rules", never an un-supersede operation). Mirrors
/// `MemoryStore::reinsert_stashed` exactly. Returns the row as it now
/// stands if `se` actually (re)claims a slot; `None` if it is refused,
/// deduped away, or loses the address race (`Stale` — the correct, silent
/// §3.4 outcome for a re-offered grand-predecessor: nothing churns).
pub(super) fn reinsert_stashed_in_txn(
    txn: &mut RedbIngestTxn<'_, '_>,
    se: StoredEvent,
) -> Result<Option<StoredEvent>, PersistenceError> {
    if let Some((event_key, existing)) = txn.canonical.load_by_id(&se.event.id)? {
        // Architecture review requirement (issue #2 P0 correction,
        // codex-nova ruling): union the owner sets and apply Signed
        // dominance — never silently drop the stashed entry's OWN
        // ownership/signature-state fact just because this exact id
        // happens to already be held. If the union newly becomes Signed
        // for previously-Pending owners, fan out to all of them — the
        // SAME invariant `promote_signed` enforces explicitly, since a
        // dedup collision here is functionally no different from a relay
        // independently confirming the signature.
        let mut event = existing.event;
        let mut provenance = existing.provenance;
        for (relay, at) in &se.provenance.seen {
            provenance.merge_observation(&RelayObserved::new(relay.clone(), *at));
        }
        let mut fan_out_owners: Option<BTreeSet<IntentId>> = None;
        if let Some(stashed_local) = &se.provenance.local {
            // codex-nova ruling (cross-door reachability finding): a row
            // with NO local provenance at all is purely relay-observed --
            // its event signature is by construction already
            // real, never a sentinel -- so it counts as "already signed"
            // exactly like a locally-owned row whose own `sig_state` is
            // `Signed` (the SAME rule `accept_write`'s `already_signed`
            // and `insert`'s dedup branch already apply). `unwrap_or(true)`,
            // NOT `is_some_and` defaulting to `false` -- getting this
            // backwards here specifically meant a relay-confirmed row
            // restored from a stash collision never told the stash's own
            // owner it was safe to stop waiting.
            let existing_signed = provenance
                .local
                .as_ref()
                .map(|l| l.sig_state == SigState::Signed)
                .unwrap_or(true);
            let stashed_signed = stashed_local.sig_state == SigState::Signed;
            if !existing_signed && stashed_signed {
                // Adopt the stash's real signature onto the record's OWN
                // event bytes (NIP-01 id never depends on `sig`, so this
                // is a pure value update, no id churn).
                event.sig = se.event.sig;
                txn.canonical.replace_event(event_key, &event)?;
            }
            let mut owners = provenance
                .local
                .as_ref()
                .map(|l| l.owners.clone())
                .unwrap_or_default();
            owners.extend(stashed_local.owners.iter().copied());
            let result_signed = existing_signed || stashed_signed;
            provenance.local = Some(LocalOrigin {
                owners: owners.clone(),
                sig_state: if result_signed {
                    SigState::Signed
                } else {
                    SigState::Pending
                },
            });
            // Fan out whenever the RESULT is Signed, regardless of which
            // side already held the real signature -- `fan_out_signed_in_
            // txn` itself is idempotent per owner (it only transitions an
            // owner whose OWN journal is still `Pending`), so this is
            // always safe, and it is the ONLY way the STASH's own
            // owner(s) ever learn that a row which was ALREADY signed on
            // the live/relay side is done waiting on them.
            if result_signed {
                fan_out_owners = Some(owners);
            }
        }
        txn.canonical.replace_provenance(event_key, &provenance)?;
        if let Some(owners) = &fan_out_owners {
            fan_out_signed_in_txn(txn, owners, &event)?;
        }
        return Ok(Some(StoredEvent { event, provenance }));
    }
    if tombstone_refuses(txn, &se.event)? {
        return Ok(None);
    }

    let result = match address_key_for(&se.event) {
        None => {
            let event_key = txn.canonical.insert_new(&se.event, &se.provenance)?;
            txn.insert_indexes(&se.event, event_key)?;
            if let Some(ts) = se.event.tags.expiration().copied() {
                let exp_key = expiration_key(ts, &se.event.id);
                txn.expiration_index
                    .insert(&exp_key, event_key)
                    .map_err(persist_err)?;
            }
            Some(se)
        }
        Some(addr_key) => {
            let addr_key_str = addr_key.to_redb_key();
            let current_key = txn
                .addr_index
                .get(addr_key_str.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value());

            match current_key {
                None => {
                    let event_key = txn.canonical.insert_new(&se.event, &se.provenance)?;
                    txn.addr_index
                        .insert(addr_key_str.as_str(), event_key)
                        .map_err(persist_err)?;
                    txn.insert_indexes(&se.event, event_key)?;
                    if let Some(ts) = se.event.tags.expiration().copied() {
                        let exp_key = expiration_key(ts, &se.event.id);
                        txn.expiration_index
                            .insert(&exp_key, event_key)
                            .map_err(persist_err)?;
                    }
                    Some(se)
                }
                Some(current_key) => {
                    let current_event = txn
                        .canonical
                        .load_by_key(current_key)?
                        .ok_or_else(|| missing_addr_index_target(current_key))?
                        .event;

                    if candidate_wins(&se.event, &current_event) {
                        let current_id = current_event.id;
                        remove_row_in_txn(txn, current_id, |_| true)?
                            .ok_or_else(|| missing_addr_index_target(current_key))?;

                        let event_key = txn.canonical.insert_new(&se.event, &se.provenance)?;
                        txn.addr_index
                            .insert(addr_key_str.as_str(), event_key)
                            .map_err(persist_err)?;
                        txn.insert_indexes(&se.event, event_key)?;
                        if let Some(ts) = se.event.tags.expiration().copied() {
                            let exp_key = expiration_key(ts, &se.event.id);
                            txn.expiration_index
                                .insert(&exp_key, event_key)
                                .map_err(persist_err)?;
                        }
                        Some(se)
                    } else {
                        // Stale — §3.4: nothing churns.
                        None
                    }
                }
            }
        }
    };
    Ok(result)
}
