use super::publish_queue_codec::{
    codec_error, deadline_by_intent_key, deadline_key, decode_addr_claimants, decode_claimants,
    decode_lane, decode_meta_u64, decode_receipt, encode_addr_claimants, encode_claimants,
    encode_deadline, encode_lane, encode_meta_u64, encode_receipt, lane_key, receipt_key,
    PublishQueueRelayId, NEXT_INTENT_ID_KEY, NEXT_RECEIPT_ID_KEY,
};
use super::schema::persist_err;
use super::{
    address_key_for, Deserialize, Event, EventId, IntentId, IntentSigState, PersistenceError,
    PublicKey, PublishQueueDeadline, PublishQueueDeadlineKind, PublishQueueInFlightPhase,
    PublishQueueLane, PublishQueueLaneKey, PublishQueueLaneState, ReadableTable, ReceiptState,
    Serialize, Timestamp,
};

pub(super) fn lane_deadline(lane: &PublishQueueLane) -> Option<PublishQueueDeadline> {
    let (at, kind) = match lane.state {
        PublishQueueLaneState::Transient { eligible_at, .. } => {
            (eligible_at, PublishQueueDeadlineKind::RetryEligible)
        }
        PublishQueueLaneState::InFlight {
            phase: PublishQueueInFlightPhase::AwaitingAck { deadline },
            ..
        } => (deadline, PublishQueueDeadlineKind::AckTimeout),
        _ => return None,
    };
    Some(PublishQueueDeadline {
        at,
        key: lane.key.clone(),
        lane_revision: lane.revision,
        kind,
    })
}

pub(super) fn replace_lane_in_txn(
    lanes: &mut redb::Table<'_, &'static [u8; 12], &'static [u8]>,
    deadlines: &mut redb::Table<'_, &'static [u8; 20], &'static [u8]>,
    deadlines_by_intent: &mut redb::Table<'_, &'static [u8; 20], &'static [u8]>,
    key: &PublishQueueLaneKey,
    relay_id: PublishQueueRelayId,
    expected_revision: u64,
    state: PublishQueueLaneState,
) -> Result<PublishQueueLane, PersistenceError> {
    let storage_key = lane_key(key.intent_id, relay_id);
    let encoded = lanes
        .get(&storage_key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_vec())
        .ok_or_else(|| PersistenceError::invariant("delivery lane not found"))?;
    let (revision, last_ordinal, current_state) =
        decode_lane(&encoded).map_err(|error| codec_error("lane", error))?;
    if revision != expected_revision {
        return Err(PersistenceError::invariant("stale delivery lane revision"));
    }
    let current = PublishQueueLane {
        version: 1,
        key: key.clone(),
        revision,
        last_ordinal,
        state: current_state,
    };
    if let Some(old) = lane_deadline(&current) {
        let ordered_key = deadline_key(old.at, key.intent_id, relay_id);
        let by_intent_key = deadline_by_intent_key(key.intent_id, old.at, relay_id);
        deadlines.remove(&ordered_key).map_err(persist_err)?;
        deadlines_by_intent
            .remove(&by_intent_key)
            .map_err(persist_err)?;
    }
    let lane = PublishQueueLane {
        version: 1,
        key: key.clone(),
        revision: current
            .revision
            .checked_add(1)
            .ok_or_else(|| PersistenceError::invariant("delivery lane revision exhausted"))?,
        last_ordinal: match &state {
            PublishQueueLaneState::InFlight { ordinal, .. }
            | PublishQueueLaneState::Transient { ordinal, .. }
            | PublishQueueLaneState::Terminal { ordinal, .. } => {
                if *ordinal > current.last_ordinal.saturating_add(1) {
                    return Err(PersistenceError::invariant(
                        "delivery lane state skips an attempt ordinal",
                    ));
                }
                *ordinal
            }
            _ => current.last_ordinal,
        },
        state,
    };
    let encoded = encode_lane(lane.revision, lane.last_ordinal, &lane.state)
        .map_err(|error| codec_error("lane", error))?;
    lanes
        .insert(&storage_key, encoded.as_slice())
        .map_err(persist_err)?;
    if let Some(deadline) = lane_deadline(&lane) {
        let encoded = encode_deadline(deadline.lane_revision, deadline.kind);
        let ordered_key = deadline_key(deadline.at, key.intent_id, relay_id);
        let by_intent_key = deadline_by_intent_key(key.intent_id, deadline.at, relay_id);
        deadlines
            .insert(&ordered_key, encoded.as_slice())
            .map_err(persist_err)?;
        deadlines_by_intent
            .insert(&by_intent_key, encoded.as_slice())
            .map_err(persist_err)?;
    }
    Ok(lane)
}
/// One `PUBLISH_QUEUE_INTENTS` row's JSON value — the full acceptance journal
/// payload (Fable checkpoint R7), everything issue #3's "one crash-atomic
/// commit" enumerates besides the pending row itself (which lives in
/// `EVENTS`, not duplicated here).
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PublishQueueIntentRecord {
    pub(super) receipt_id: u64,
    pub(super) frozen: Event,
    pub(super) expected_pubkey: PublicKey,
    pub(super) signing_identity_ref: String,
    pub(super) routing: String,
    pub(super) sig_state: IntentSigState,
    pub(super) accepted_at: Timestamp,
}

/// Allocate the next [`IntentId`] from [`PUBLISH_QUEUE_META`]'s durable high-water
/// mark, bumping it in the SAME already-open write transaction the caller
/// is about to journal the intent in (architecture review correction — see
/// [`IntentId`]'s doc). Starts at 1 if the row has never been written.
pub(super) fn alloc_intent_id_in_txn(
    publish_queue_meta: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
) -> Result<IntentId, PersistenceError> {
    Ok(IntentId(alloc_counter_in_txn(
        publish_queue_meta,
        NEXT_INTENT_ID_KEY,
    )?))
}

/// Allocate the next receipt id from `PUBLISH_QUEUE_META`'s durable high-water
/// mark, same treatment as [`alloc_intent_id_in_txn`] (architecture review
/// correction: receipt ids have the identical reuse hazard now that
/// receipts are durably retained across restart).
pub(super) fn alloc_receipt_id_in_txn(
    publish_queue_meta: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
) -> Result<u64, PersistenceError> {
    let id = alloc_counter_in_txn(publish_queue_meta, NEXT_RECEIPT_ID_KEY)?;
    if id >= (1u64 << 63) {
        return Err(PersistenceError::invariant(
            "durable receipt id namespace exhausted",
        ));
    }
    Ok(id)
}

/// Shared bump-and-return for one `PUBLISH_QUEUE_META` counter row, keyed by
/// `meta_key` (either [`NEXT_INTENT_ID_KEY`] or [`NEXT_RECEIPT_ID_KEY`]).
/// Starts at 1 if the row has never been written.
pub(super) fn alloc_counter_in_txn(
    publish_queue_meta: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    meta_key: &[u8],
) -> Result<u64, PersistenceError> {
    let current = publish_queue_meta
        .get(meta_key)
        .map_err(persist_err)?
        .map(|guard| decode_meta_u64(guard.value(), "delivery counter"))
        .transpose()
        .map_err(|error| codec_error("counter", error))?
        .unwrap_or(1);
    let next = current
        .checked_add(1)
        .ok_or_else(|| PersistenceError::invariant("delivery id counter exhausted"))?;
    let encoded = encode_meta_u64(next);
    publish_queue_meta
        .insert(meta_key, encoded.as_slice())
        .map_err(persist_err)?;
    Ok(current)
}

/// One `PUBLISH_QUEUE_RECEIPTS` row's JSON value (architecture review correction —
/// see [`crate::ReceiptState`]'s doc). `EventId`/`PublicKey`/`IntentId`/
/// `ReceiptState` all already derive `Serialize`/`Deserialize`, so this
/// mirrors `crate::PublishQueueReceipt` field-for-field with no re-encoding.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PublishQueueReceiptRecord {
    /// `None` for a refused-at-acceptance receipt-only record — see
    /// `crate::PublishQueueReceipt::intent_id`'s doc.
    pub(super) intent_id: Option<IntentId>,
    pub(super) frozen_id: EventId,
    pub(super) expected_pubkey: PublicKey,
    pub(super) state: ReceiptState,
}

/// Update `PUBLISH_QUEUE_RECEIPTS[receipt_id]`'s `state` in place. Absence or corrupt
/// bytes are persistence failures: returning success would let promotion or
/// cancellation fabricate a terminal fact that was never retained.
pub(super) fn update_publish_queue_receipt(
    publish_queue_receipts: &mut redb::Table<'_, &'static [u8; 8], &'static [u8]>,
    receipt_id: u64,
    state: ReceiptState,
) -> Result<(), PersistenceError> {
    let key = receipt_key(receipt_id);
    // Two statements, not one chained expression — see `remove_row_in_txn`'s
    // comment on the same `?`-temporary-lifetime-extension quirk.
    let existing = publish_queue_receipts.get(&key).map_err(persist_err)?;
    let encoded = existing
        .map(|guard| guard.value().to_vec())
        .ok_or_else(|| {
            PersistenceError::invariant(format!("missing delivery receipt {receipt_id}"))
        })?;
    let mut record = decode_receipt(&encoded)
        .map_err(|error| codec_error(&format!("receipt {receipt_id}"), error))?;
    record.state = state;
    let encoded = encode_receipt(&record);
    publish_queue_receipts
        .insert(&key, encoded.as_slice())
        .map_err(persist_err)?;
    Ok(())
}

/// One provisional kind:5 suppression claim, as persisted in
/// `PUBLISH_QUEUE_KIND5_CLAIMS` (architecture review requirement — codex-nova's
/// suppression-claim model, replacing a withdrawn design that physically
/// moved a target row into a per-intent stash — see
/// `crate::AcceptOutcome::Kind5Processed`'s doc for why that was unsound).
/// `Id` mirrors [`id_tombstone_key`]'s own composite (target id, claiming
/// author) — a future arrival at that id is only ever suppressed if its
/// real author (fixed by the id's hash) matches. `Addr` names an address
/// (an [`AddressKey::to_redb_key`] string) PLUS the same NIP-09
/// `created_at` ceiling the permanent `ADDR_TOMBSTONES` mechanism uses
/// (issue #61 P0 correction: a claim with no ceiling would hide every
/// future winner at that address forever, including one created AFTER the
/// deletion, which even a PERMANENT tombstone does not do). Authorization
/// is checked immediately at claim-creation time (`coord.public_key ==
/// deleting.pubkey`), so `deleting_author` here is diagnostic parity with
/// `AddrTombstoneRecord`, not load-bearing for the visibility check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum SuppressClaimRecord {
    Id {
        target: EventId,
        claiming_author: PublicKey,
    },
    Addr {
        key: Vec<u8>,
        ceiling: u64,
        deleting_author: PublicKey,
    },
}

/// Append `intent_id` to the JSON-encoded `Vec<u64>` claimant set at
/// `table[key]` (creating it if absent) — shared by `PUBLISH_QUEUE_SUPPRESS_BY_ID`
/// only now (see [`add_addr_claimant_in_txn`] for the ceiling-carrying
/// address counterpart).
pub(super) fn add_claimant_in_txn(
    table: &mut redb::Table<'_, &'static [u8; 64], &'static [u8]>,
    key: &[u8; 64],
    intent_id: IntentId,
) -> Result<(), PersistenceError> {
    let mut claimants: Vec<u64> = table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| decode_claimants(guard.value()))
        .transpose()?
        .unwrap_or_default();
    if !claimants.contains(&intent_id.0) {
        claimants.push(intent_id.0);
    }
    let encoded = encode_claimants(&claimants).map_err(|error| codec_error("claimants", error))?;
    table.insert(key, encoded.as_slice()).map_err(persist_err)?;
    Ok(())
}

/// Remove `intent_id` from the claimant set at `table[key]`, deleting the
/// row outright once it becomes empty (the row's mere existence implies
/// non-empty by construction — [`add_claimant_in_txn`] never inserts an
/// empty set) — the reversal counterpart of [`add_claimant_in_txn`], and
/// [`has_claimants_in_txn`]'s existence check relies on this invariant.
pub(super) fn remove_claimant_in_txn(
    table: &mut redb::Table<'_, &'static [u8; 64], &'static [u8]>,
    key: &[u8; 64],
    intent_id: IntentId,
) -> Result<(), PersistenceError> {
    let Some(json) = table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_vec())
    else {
        return Ok(());
    };
    let mut claimants = decode_claimants(&json).map_err(|error| codec_error("claimants", error))?;
    claimants.retain(|id| *id != intent_id.0);
    if claimants.is_empty() {
        table.remove(key).map_err(persist_err)?;
    } else {
        let encoded =
            encode_claimants(&claimants).map_err(|error| codec_error("claimants", error))?;
        table.insert(key, encoded.as_slice()).map_err(persist_err)?;
    }
    Ok(())
}

/// `true` iff `table[key]` currently names at least one claimant —
/// consulted by [`is_suppressed_in_txn`] for ID claims. Relies on
/// [`remove_claimant_in_txn`]'s "never leave an empty set behind"
/// invariant: mere row existence implies non-empty.
pub(super) fn has_claimants_in_txn(
    table: &impl ReadableTable<&'static [u8; 64], &'static [u8]>,
    key: &[u8; 64],
) -> Result<bool, PersistenceError> {
    Ok(table.get(key).map_err(persist_err)?.is_some())
}

/// One `(claiming_intent_id, created_at_ceiling)` pair — `PUBLISH_QUEUE_SUPPRESS_BY_ADDR`'s
/// value shape (issue #61 P0 correction, mirrors `SuppressClaimRecord::Addr`'s
/// doc for why a bare claimant list is not enough).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct AddrClaimant {
    pub(super) intent_id: u64,
    pub(super) ceiling: u64,
}

/// Add (or update) `intent_id`'s ceiling in the JSON-encoded
/// `Vec<AddrClaimant>` claimant list at `table[key]` — the address
/// counterpart of [`add_claimant_in_txn`], carrying a ceiling per
/// claimant instead of a bare id.
pub(super) fn add_addr_claimant_in_txn(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    key: &[u8],
    intent_id: IntentId,
    ceiling: Timestamp,
) -> Result<(), PersistenceError> {
    let mut claimants: Vec<AddrClaimant> = table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| decode_addr_claimants(guard.value()))
        .transpose()?
        .unwrap_or_default();
    claimants.retain(|c| c.intent_id != intent_id.0);
    claimants.push(AddrClaimant {
        intent_id: intent_id.0,
        ceiling: ceiling.as_secs(),
    });
    let encoded = encode_addr_claimants(&claimants)
        .map_err(|error| codec_error("address claimants", error))?;
    table.insert(key, encoded.as_slice()).map_err(persist_err)?;
    Ok(())
}

/// Remove `intent_id`'s ceiling entry from `table[key]`, deleting the row
/// outright once empty — the address counterpart of
/// [`remove_claimant_in_txn`].
pub(super) fn remove_addr_claimant_in_txn(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    key: &[u8],
    intent_id: IntentId,
) -> Result<(), PersistenceError> {
    let Some(json) = table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_vec())
    else {
        return Ok(());
    };
    let mut claimants =
        decode_addr_claimants(&json).map_err(|error| codec_error("address claimants", error))?;
    claimants.retain(|c| c.intent_id != intent_id.0);
    if claimants.is_empty() {
        table.remove(key).map_err(persist_err)?;
    } else {
        let encoded = encode_addr_claimants(&claimants)
            .map_err(|error| codec_error("address claimants", error))?;
        table.insert(key, encoded.as_slice()).map_err(persist_err)?;
    }
    Ok(())
}

/// `true` iff ANY claimant at `table[key]` currently covers
/// `candidate_created_at` (its ceiling is at-or-after it) — the
/// provisional counterpart of the permanent `ADDR_TOMBSTONES` ceiling
/// check, consulted by [`is_suppressed_in_txn`].
pub(super) fn addr_has_covering_claimant_in_txn(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    key: &[u8],
    candidate_created_at: Timestamp,
) -> Result<bool, PersistenceError> {
    let Some(json) = table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_vec())
    else {
        return Ok(false);
    };
    let claimants =
        decode_addr_claimants(&json).map_err(|error| codec_error("address claimants", error))?;
    Ok(claimants
        .iter()
        .any(|c| candidate_created_at.as_secs() <= c.ceiling))
}

/// `true` iff `event` is currently hidden by ANY still-open kind:5
/// suppression claim — consulted by `query` and `gc`. Never affects
/// `EVENTS`/`ADDR_INDEX` themselves: a suppressed row is fully present,
/// just filtered out of read results (see [`SuppressClaimRecord`]'s doc).
/// Mirrors `MemoryStore::is_suppressed` exactly, including the
/// per-claimant ceiling check for address claims (issue #61 P0
/// correction). Generic over `ReadableTable` (not the concrete
/// `Table`/`ReadOnlyTable` types) so it works from BOTH `gc`'s write
/// transaction and `query`'s read-only one — every other helper in this
/// file only ever runs inside a write transaction; this is the first
/// read-only caller.
pub(super) fn is_suppressed_in_txn(
    publish_queue_suppress_by_id: &impl ReadableTable<&'static [u8; 64], &'static [u8]>,
    publish_queue_suppress_by_addr: &impl ReadableTable<&'static [u8], &'static [u8]>,
    event: &Event,
) -> Result<bool, PersistenceError> {
    let id_key = super::publish_queue_codec::id_claim_key(&event.id, &event.pubkey);
    if has_claimants_in_txn(publish_queue_suppress_by_id, &id_key)? {
        return Ok(true);
    }
    if let Some(key) = address_key_for(event) {
        let key_bytes = key.to_redb_key().into_bytes();
        if addr_has_covering_claimant_in_txn(
            publish_queue_suppress_by_addr,
            &key_bytes,
            event.created_at,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}
