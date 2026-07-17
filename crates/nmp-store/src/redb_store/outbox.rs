use super::schema::{
    id_tombstone_key, persist_err, NEXT_INTENT_ID_KEY, NEXT_RECEIPT_ID_KEY,
    PENDING_EPHEMERAL_RECEIPTS_KEY,
};
use super::{
    address_key_for, AttemptOutcome, BTreeSet, DeadlineKind, Deserialize, Event, EventId,
    InFlightPhase, IntentId, IntentSigState, LaneDeadline, LaneKey, LaneState, PersistenceError,
    PublicKey, ReadableTable, ReceiptState, RecoveredAttempt, RecoveredAttemptDetails,
    RecoveredLane, RecoveredRouteRevision, RelayUrl, Serialize, TableDefinition, Timestamp,
    WriteDurability,
};
use nostr::JsonUtil;

pub(super) fn attempt_prefix(intent_id: IntentId, relay: &RelayUrl) -> String {
    // Length-prefixing makes relay-prefix pairs (`wss://x` and
    // `wss://x:443`) disjoint without relying on URL separator rules.
    format!(
        "{:020}:{:020}:{}:",
        intent_id.0,
        relay.as_str().len(),
        relay.as_str()
    )
}

pub(super) fn intent_row_prefix(intent_id: IntentId) -> String {
    format!("{:020}:", intent_id.0)
}

/// Every outbox prefix ends in the `:` delimiter. Replacing that final byte
/// with its immediate ASCII successor yields the smallest exclusive upper
/// bound containing every key beginning with the original prefix.
pub(super) fn prefix_range(prefix: String) -> (String, String) {
    debug_assert!(prefix.ends_with(':'));
    let mut upper = prefix.clone();
    upper.pop();
    upper.push(';');
    (prefix, upper)
}

pub(super) fn attempt_key(intent_id: IntentId, relay: &RelayUrl, ordinal: u64) -> String {
    format!("{}{:020}", attempt_prefix(intent_id, relay), ordinal)
}

pub(super) fn lane_key(key: &LaneKey) -> String {
    let relay: &nostr::Url = (&key.relay).into();
    let relay = relay.as_str();
    format!("{:020}:{:020}:{relay}", key.intent_id.0, relay.len())
}

pub(super) fn relay_order_key(relay: &RelayUrl) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let canonical: &nostr::Url = relay.into();
    let bytes = canonical.as_str().as_bytes();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

pub(super) fn deadline_key(deadline: &LaneDeadline) -> String {
    format!(
        "{:020}:{:020}:{}",
        deadline.at.as_secs(),
        deadline.key.intent_id.0,
        relay_order_key(&deadline.key.relay)
    )
}

pub(super) fn deadline_intent_key(deadline: &LaneDeadline) -> String {
    format!(
        "{:020}:{:020}:{}",
        deadline.key.intent_id.0,
        deadline.at.as_secs(),
        relay_order_key(&deadline.key.relay)
    )
}

pub(super) fn deadline_upper(now: Timestamp) -> String {
    format!("{:020};", now.as_secs())
}

pub(super) fn encode_json(value: &impl Serialize, what: &str) -> Result<String, PersistenceError> {
    serde_json::to_string(value).map_err(|err| PersistenceError(format!("encode {what}: {err}")))
}

pub(super) fn decode_lane(key: &str, json: &str) -> Result<RecoveredLane, PersistenceError> {
    let lane: RecoveredLane = serde_json::from_str(json)
        .map_err(|err| PersistenceError(format!("decode outbox lane: {err}")))?;
    if lane.version != 1 {
        return Err(PersistenceError(format!(
            "unsupported outbox lane version {}",
            lane.version
        )));
    }
    if lane_key(&lane.key) != key {
        return Err(PersistenceError(
            "outbox lane key does not match value".into(),
        ));
    }
    if lane.revision == 0 {
        return Err(PersistenceError(
            "outbox lane revision must be non-zero".into(),
        ));
    }
    let state_ordinal = match &lane.state {
        LaneState::InFlight { ordinal, .. }
        | LaneState::Transient { ordinal, .. }
        | LaneState::LegacyInFlight { ordinal }
        | LaneState::Terminal { ordinal, .. } => Some(*ordinal),
        _ => None,
    };
    if state_ordinal.is_some_and(|ordinal| ordinal != lane.last_ordinal) {
        return Err(PersistenceError(
            "outbox lane state ordinal disagrees with cursor".into(),
        ));
    }
    if matches!(
        lane.state,
        LaneState::Terminal {
            outcome: AttemptOutcome::Started,
            ..
        }
    ) {
        return Err(PersistenceError(
            "terminal lane cannot contain Started".into(),
        ));
    }
    if matches!(
        &lane.state,
        LaneState::Transient {
            raw_reason: Some(reason),
            ..
        } if reason.len() > 4_096
    ) {
        return Err(PersistenceError(
            "transient raw reason exceeds 4096 bytes".into(),
        ));
    }
    Ok(lane)
}

pub(super) fn decode_deadline(key: &str, json: &str) -> Result<LaneDeadline, PersistenceError> {
    let deadline: LaneDeadline = serde_json::from_str(json)
        .map_err(|err| PersistenceError(format!("decode outbox deadline: {err}")))?;
    if deadline_key(&deadline) != key {
        return Err(PersistenceError(
            "outbox deadline key does not match value".into(),
        ));
    }
    Ok(deadline)
}

pub(super) fn decode_deadline_by_intent(
    key: &str,
    json: &str,
) -> Result<LaneDeadline, PersistenceError> {
    let deadline: LaneDeadline = serde_json::from_str(json)
        .map_err(|err| PersistenceError(format!("decode outbox deadline: {err}")))?;
    if deadline_intent_key(&deadline) != key {
        return Err(PersistenceError(
            "outbox deadline-by-intent key does not match value".into(),
        ));
    }
    Ok(deadline)
}

pub(super) fn decode_attempt_details(
    key: &str,
    json: &str,
) -> Result<RecoveredAttemptDetails, PersistenceError> {
    let details: RecoveredAttemptDetails = serde_json::from_str(json)
        .map_err(|err| PersistenceError(format!("decode attempt details: {err}")))?;
    if details.version != 1 {
        return Err(PersistenceError(format!(
            "unsupported attempt details version {}",
            details.version
        )));
    }
    if attempt_key(details.intent_id, &details.relay, details.ordinal) != key {
        return Err(PersistenceError(
            "attempt detail key does not match value".into(),
        ));
    }
    if details.terminal == Some(AttemptOutcome::Started) {
        return Err(PersistenceError(
            "attempt details terminal cannot contain Started".into(),
        ));
    }
    if details.finished_at.is_some() && details.terminal.is_none() {
        return Err(PersistenceError(
            "attempt details finish time lacks terminal outcome".into(),
        ));
    }
    if details
        .transient
        .as_ref()
        .and_then(|detail| detail.raw_reason.as_ref())
        .is_some_and(|reason| reason.len() > 4_096)
    {
        return Err(PersistenceError(
            "transient raw reason exceeds 4096 bytes".into(),
        ));
    }
    Ok(details)
}

pub(super) fn lane_deadline(lane: &RecoveredLane) -> Option<LaneDeadline> {
    let (at, kind) = match lane.state {
        LaneState::Transient { eligible_at, .. } => (eligible_at, DeadlineKind::RetryEligible),
        LaneState::InFlight {
            phase: InFlightPhase::AwaitingAck { deadline },
            ..
        } => (deadline, DeadlineKind::AckTimeout),
        _ => return None,
    };
    Some(LaneDeadline {
        at,
        key: lane.key.clone(),
        lane_revision: lane.revision,
        kind,
    })
}

pub(super) fn replace_lane_in_txn(
    lanes: &mut redb::Table<'_, &str, &str>,
    deadlines: &mut redb::Table<'_, &str, &str>,
    deadlines_by_intent: &mut redb::Table<'_, &str, &str>,
    key: &LaneKey,
    expected_revision: u64,
    state: LaneState,
) -> Result<RecoveredLane, PersistenceError> {
    let storage_key = lane_key(key);
    let json = lanes
        .get(storage_key.as_str())
        .map_err(persist_err)?
        .map(|guard| guard.value().to_string())
        .ok_or_else(|| PersistenceError("outbox lane not found".into()))?;
    let current = decode_lane(&storage_key, &json)?;
    if current.revision != expected_revision {
        return Err(PersistenceError("stale outbox lane revision".into()));
    }
    if let Some(old) = lane_deadline(&current) {
        deadlines
            .remove(deadline_key(&old).as_str())
            .map_err(persist_err)?;
        deadlines_by_intent
            .remove(deadline_intent_key(&old).as_str())
            .map_err(persist_err)?;
    }
    let lane = RecoveredLane {
        version: 1,
        key: key.clone(),
        revision: current
            .revision
            .checked_add(1)
            .ok_or_else(|| PersistenceError("outbox lane revision exhausted".into()))?,
        last_ordinal: current.last_ordinal,
        state,
    };
    let encoded = encode_json(&lane, "outbox lane")?;
    lanes
        .insert(storage_key.as_str(), encoded.as_str())
        .map_err(persist_err)?;
    if let Some(deadline) = lane_deadline(&lane) {
        let encoded = encode_json(&deadline, "outbox deadline")?;
        deadlines
            .insert(deadline_key(&deadline).as_str(), encoded.as_str())
            .map_err(persist_err)?;
        deadlines_by_intent
            .insert(deadline_intent_key(&deadline).as_str(), encoded.as_str())
            .map_err(persist_err)?;
    }
    Ok(lane)
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct OutboxAttemptRecord {
    pub(super) version: u8,
    pub(super) intent_id: IntentId,
    pub(super) relay: RelayUrl,
    pub(super) ordinal: u64,
    pub(super) event_json: String,
    pub(super) outcome: AttemptOutcome,
}

pub(super) fn route_revision_key(intent_id: IntentId, ordinal: u64) -> String {
    format!("{:020}:{:020}", intent_id.0, ordinal)
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct OutboxRouteRevisionRecord {
    pub(super) version: u8,
    pub(super) intent_id: IntentId,
    pub(super) ordinal: u64,
    pub(super) relays: BTreeSet<RelayUrl>,
}

pub(super) fn decode_route_revision(
    key: &str,
    json: &str,
) -> Result<RecoveredRouteRevision, PersistenceError> {
    let record: OutboxRouteRevisionRecord = serde_json::from_str(json)
        .map_err(|err| PersistenceError(format!("decode route revision: {err}")))?;
    if record.version != 1 {
        return Err(PersistenceError(format!(
            "unsupported route revision version {}",
            record.version
        )));
    }
    if route_revision_key(record.intent_id, record.ordinal) != key {
        return Err(PersistenceError(
            "route revision key does not match its value tuple".into(),
        ));
    }
    Ok(RecoveredRouteRevision {
        version: record.version,
        intent_id: record.intent_id,
        ordinal: record.ordinal,
        relays: record.relays,
    })
}

pub(super) fn decode_attempt(key: &str, json: &str) -> Result<RecoveredAttempt, PersistenceError> {
    let record: OutboxAttemptRecord = serde_json::from_str(json)
        .map_err(|err| PersistenceError(format!("decode outbox attempt: {err}")))?;
    if record.version != 1 {
        return Err(PersistenceError(format!(
            "unsupported outbox attempt record version {}",
            record.version
        )));
    }
    if attempt_key(record.intent_id, &record.relay, record.ordinal) != key {
        return Err(PersistenceError(
            "outbox attempt key does not match its value tuple".into(),
        ));
    }
    let event = Event::from_json(&record.event_json)
        .map_err(|err| PersistenceError(format!("decode attempt event: {err}")))?;
    event
        .verify()
        .map_err(|err| PersistenceError(format!("attempt event is invalid: {err}")))?;
    Ok(RecoveredAttempt {
        version: record.version,
        intent_id: record.intent_id,
        relay: record.relay,
        ordinal: record.ordinal,
        event,
        outcome: record.outcome,
    })
}
/// Every still-open kind:5 intent's OWN suppression claims (architecture
/// review requirement — codex-nova's suppression-claim model; see
/// [`SuppressClaimRecord`]'s doc), keyed by the SAME [`intent_key`] as its
/// `OUTBOX_INTENTS` row. `promote_signed` drops this row (after committing
/// the deletion for real — see [`process_kind5_deletions`]); `compensate_write`
/// just drops it (nothing else to reverse: a claim never moved or removed
/// the row it names).
pub(super) const OUTBOX_KIND5_CLAIMS: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_kind5_claims");
/// Reverse index: `id_tombstone_key(target id, claiming author) ->
/// JSON-encoded `Vec<u64>` of claiming `IntentId`s — consulted by
/// `is_suppressed_in_txn` to decide `query` visibility. More than one
/// intent can claim the SAME target (two independent pending deletes of
/// the same event before either signs or cancels): hidden while ANY claim
/// applies, visible again only once every claim on it is dropped.
pub(super) const OUTBOX_SUPPRESS_BY_ID: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_suppress_by_id");
/// Reverse index for address claims: `AddressKey::to_redb_key() ->
/// JSON-encoded `Vec<u64>``, same treatment as [`OUTBOX_SUPPRESS_BY_ID`].
pub(super) const OUTBOX_SUPPRESS_BY_ADDR: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_suppress_by_addr");

/// One `OUTBOX_INTENTS` row's JSON value — the full acceptance journal
/// payload (Fable checkpoint R7), everything issue #3's "one crash-atomic
/// commit" enumerates besides the pending row itself (which lives in
/// `EVENTS`, not duplicated here).
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct OutboxIntentRecord {
    pub(super) receipt_id: u64,
    pub(super) frozen_json: String,
    pub(super) expected_pubkey: PublicKey,
    pub(super) signing_identity_ref: String,
    pub(super) durability: WriteDurability,
    pub(super) routing: String,
    pub(super) sig_state: IntentSigState,
    pub(super) accepted_at: Timestamp,
}

/// [`OUTBOX_INTENTS`]/[`OUTBOX_DISPLACED`]'s shared key for `id` — a
/// zero-padded decimal so the two tables can never disagree on how to find
/// each other's row for the same intent, and so a future ordered scan sorts
/// by acceptance order (lexicographic == numeric).
pub(super) fn intent_key(id: IntentId) -> String {
    format!("{:020}", id.0)
}

/// Allocate the next [`IntentId`] from [`OUTBOX_META`]'s durable high-water
/// mark, bumping it in the SAME already-open write transaction the caller
/// is about to journal the intent in (architecture review correction — see
/// [`IntentId`]'s doc). Starts at 1 if the row has never been written.
pub(super) fn alloc_intent_id_in_txn(
    outbox_meta: &mut redb::Table<'_, &str, &str>,
) -> Result<IntentId, PersistenceError> {
    Ok(IntentId(alloc_counter_in_txn(
        outbox_meta,
        NEXT_INTENT_ID_KEY,
    )?))
}

/// Allocate the next receipt id from `OUTBOX_META`'s durable high-water
/// mark, same treatment as [`alloc_intent_id_in_txn`] (architecture review
/// correction: receipt ids have the identical reuse hazard now that
/// receipts are durably retained across restart).
pub(super) fn alloc_receipt_id_in_txn(
    outbox_meta: &mut redb::Table<'_, &str, &str>,
) -> Result<u64, PersistenceError> {
    let id = alloc_counter_in_txn(outbox_meta, NEXT_RECEIPT_ID_KEY)?;
    if id >= (1u64 << 63) {
        return Err(PersistenceError(
            "durable receipt id namespace exhausted".into(),
        ));
    }
    Ok(id)
}

/// Shared bump-and-return for one `OUTBOX_META` counter row, keyed by
/// `meta_key` (either [`NEXT_INTENT_ID_KEY`] or [`NEXT_RECEIPT_ID_KEY`]).
/// Starts at 1 if the row has never been written.
pub(super) fn alloc_counter_in_txn(
    outbox_meta: &mut redb::Table<'_, &str, &str>,
    meta_key: &str,
) -> Result<u64, PersistenceError> {
    let current = outbox_meta
        .get(meta_key)
        .map_err(persist_err)?
        .map(|guard| guard.value().parse::<u64>())
        .transpose()
        .map_err(|err| PersistenceError(format!("parse outbox_meta counter: {err}")))?
        .unwrap_or(1);
    let next = current
        .checked_add(1)
        .ok_or_else(|| PersistenceError("outbox id counter exhausted".into()))?;
    let encoded = next.to_string();
    outbox_meta
        .insert(meta_key, encoded.as_str())
        .map_err(persist_err)?;
    Ok(current)
}

pub(super) fn increment_pending_ephemeral_in_txn(
    outbox_meta: &mut redb::Table<'_, &str, &str>,
) -> Result<(), PersistenceError> {
    let current = outbox_meta
        .get(PENDING_EPHEMERAL_RECEIPTS_KEY)
        .map_err(persist_err)?
        .map(|guard| guard.value().parse::<u64>())
        .transpose()
        .map_err(|err| PersistenceError(format!("parse pending ephemeral count: {err}")))?
        .unwrap_or(0);
    let next = current
        .checked_add(1)
        .ok_or_else(|| PersistenceError("pending ephemeral receipt count exhausted".into()))?;
    let encoded = next.to_string();
    outbox_meta
        .insert(PENDING_EPHEMERAL_RECEIPTS_KEY, encoded.as_str())
        .map_err(persist_err)?;
    Ok(())
}

pub(super) fn decrement_pending_ephemeral_in_txn(
    outbox_meta: &mut redb::Table<'_, &str, &str>,
) -> Result<(), PersistenceError> {
    let current = outbox_meta
        .get(PENDING_EPHEMERAL_RECEIPTS_KEY)
        .map_err(persist_err)?
        .map(|guard| guard.value().parse::<u64>())
        .transpose()
        .map_err(|err| PersistenceError(format!("parse pending ephemeral count: {err}")))?
        .unwrap_or(0);
    let next = current
        .checked_sub(1)
        .ok_or_else(|| PersistenceError("pending ephemeral receipt count underflow".into()))?;
    let encoded = next.to_string();
    outbox_meta
        .insert(PENDING_EPHEMERAL_RECEIPTS_KEY, encoded.as_str())
        .map_err(persist_err)?;
    Ok(())
}

/// [`OUTBOX_RECEIPTS`]'s key for `id` — same zero-padding convention as
/// [`intent_key`].
pub(super) fn receipt_key(id: u64) -> String {
    format!("{:020}", id)
}

/// One `OUTBOX_RECEIPTS` row's JSON value (architecture review correction —
/// see [`crate::ReceiptState`]'s doc). `EventId`/`PublicKey`/`IntentId`/
/// `ReceiptState` all already derive `Serialize`/`Deserialize`, so this
/// mirrors `crate::RecoveredReceipt` field-for-field with no re-encoding.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct OutboxReceiptRecord {
    /// `None` for an `Ephemeral` receipt-only record — see
    /// `crate::RecoveredReceipt::intent_id`'s doc.
    pub(super) intent_id: Option<IntentId>,
    pub(super) frozen_id: EventId,
    pub(super) expected_pubkey: PublicKey,
    pub(super) state: ReceiptState,
}

/// Update `OUTBOX_RECEIPTS[receipt_id]`'s `state` in place. Absence or corrupt
/// bytes are persistence failures: returning success would let promotion or
/// cancellation fabricate a terminal fact that was never retained.
pub(super) fn update_outbox_receipt(
    outbox_receipts: &mut redb::Table<'_, &str, &str>,
    receipt_id: u64,
    state: ReceiptState,
) -> Result<(), PersistenceError> {
    let key = receipt_key(receipt_id);
    // Two statements, not one chained expression — see `remove_row_in_txn`'s
    // comment on the same `?`-temporary-lifetime-extension quirk.
    let existing = outbox_receipts.get(key.as_str()).map_err(persist_err)?;
    let json = existing
        .map(|guard| guard.value().to_string())
        .ok_or_else(|| PersistenceError(format!("missing outbox receipt {receipt_id}")))?;
    let mut record: OutboxReceiptRecord = serde_json::from_str(&json).map_err(|error| {
        PersistenceError(format!("decode outbox receipt {receipt_id}: {error}"))
    })?;
    record.state = state;
    let encoded = serde_json::to_string(&record).map_err(|error| {
        PersistenceError(format!("encode outbox receipt {receipt_id}: {error}"))
    })?;
    outbox_receipts
        .insert(key.as_str(), encoded.as_str())
        .map_err(persist_err)?;
    Ok(())
}

/// Boot-time reconciliation: every `Ephemeral` receipt-only record
/// (`intent_id: None`) still `ReceiptState::Accepted` is flipped to
/// `Abandoned` — see `ReceiptState::Abandoned`'s doc for why this is sound
/// without any engine cooperation. `RedbStore::open()` calls this only when
/// exact metadata reports pending ephemeral receipts, inside the conditional
/// recovery transaction. Fresh schema creation never calls it. Two passes
/// (collect then mutate), mirroring `gc`'s victim-collection pattern: `redb`
/// does not allow mutating a table while iterating it.
pub(super) fn reconcile_ephemeral_receipts_in_txn(
    outbox_receipts: &mut redb::Table<'_, &str, &str>,
) -> usize {
    let mut to_abandon: Vec<(String, OutboxReceiptRecord)> = Vec::new();
    for entry in outbox_receipts.iter().expect("redb: iter outbox_receipts") {
        let (key, value) = entry.expect("redb: read outbox_receipts entry");
        let Ok(record) = serde_json::from_str::<OutboxReceiptRecord>(value.value()) else {
            // Preserve corrupt durable evidence verbatim. Reconciliation is
            // only allowed to advance a decodable ephemeral receipt; the
            // checked reattach path will report this retained identity as
            // `RetainedButUnreadable` rather than erasing or publishing it.
            continue;
        };
        if record.intent_id.is_none() && record.state == ReceiptState::Accepted {
            to_abandon.push((key.value().to_string(), record));
        }
    }
    let reconciled = to_abandon.len();
    for (key, mut record) in to_abandon {
        record.state = ReceiptState::Abandoned;
        let encoded = serde_json::to_string(&record).expect("redb: encode outbox receipt");
        outbox_receipts
            .insert(key.as_str(), encoded.as_str())
            .expect("redb: update outbox_receipts (ephemeral abandon)");
    }
    reconciled
}

/// One provisional kind:5 suppression claim, as persisted in
/// `OUTBOX_KIND5_CLAIMS` (architecture review requirement — codex-nova's
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
    Id(String),
    Addr {
        key: String,
        ceiling: u64,
        deleting_author: String,
    },
}

/// Append `intent_id` to the JSON-encoded `Vec<u64>` claimant set at
/// `table[key]` (creating it if absent) — shared by `OUTBOX_SUPPRESS_BY_ID`
/// only now (see [`add_addr_claimant_in_txn`] for the ceiling-carrying
/// address counterpart).
pub(super) fn add_claimant_in_txn(
    table: &mut redb::Table<'_, &str, &str>,
    key: &str,
    intent_id: IntentId,
) -> Result<(), PersistenceError> {
    let mut claimants: Vec<u64> = table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| serde_json::from_str(guard.value()).expect("redb: decode claimant set"))
        .unwrap_or_default();
    if !claimants.contains(&intent_id.0) {
        claimants.push(intent_id.0);
    }
    let encoded = serde_json::to_string(&claimants).expect("redb: encode claimant set");
    table.insert(key, encoded.as_str()).map_err(persist_err)?;
    Ok(())
}

/// Remove `intent_id` from the claimant set at `table[key]`, deleting the
/// row outright once it becomes empty (the row's mere existence implies
/// non-empty by construction — [`add_claimant_in_txn`] never inserts an
/// empty set) — the reversal counterpart of [`add_claimant_in_txn`], and
/// [`has_claimants_in_txn`]'s existence check relies on this invariant.
pub(super) fn remove_claimant_in_txn(
    table: &mut redb::Table<'_, &str, &str>,
    key: &str,
    intent_id: IntentId,
) -> Result<(), PersistenceError> {
    let Some(json) = table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_string())
    else {
        return Ok(());
    };
    let mut claimants: Vec<u64> = serde_json::from_str(&json)
        .map_err(|error| PersistenceError(format!("decode claimant set: {error}")))?;
    claimants.retain(|id| *id != intent_id.0);
    if claimants.is_empty() {
        table.remove(key).map_err(persist_err)?;
    } else {
        let encoded = serde_json::to_string(&claimants).expect("redb: encode claimant set");
        table.insert(key, encoded.as_str()).map_err(persist_err)?;
    }
    Ok(())
}

/// `true` iff `table[key]` currently names at least one claimant —
/// consulted by [`is_suppressed_in_txn`] for ID claims. Relies on
/// [`remove_claimant_in_txn`]'s "never leave an empty set behind"
/// invariant: mere row existence implies non-empty.
pub(super) fn has_claimants_in_txn(
    table: &impl ReadableTable<&'static str, &'static str>,
    key: &str,
) -> Result<bool, PersistenceError> {
    Ok(table.get(key).map_err(persist_err)?.is_some())
}

/// One `(claiming_intent_id, created_at_ceiling)` pair — `OUTBOX_SUPPRESS_BY_ADDR`'s
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
    table: &mut redb::Table<'_, &str, &str>,
    key: &str,
    intent_id: IntentId,
    ceiling: Timestamp,
) -> Result<(), PersistenceError> {
    let mut claimants: Vec<AddrClaimant> = table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| serde_json::from_str(guard.value()).expect("redb: decode addr claimant set"))
        .unwrap_or_default();
    claimants.retain(|c| c.intent_id != intent_id.0);
    claimants.push(AddrClaimant {
        intent_id: intent_id.0,
        ceiling: ceiling.as_secs(),
    });
    let encoded = serde_json::to_string(&claimants).expect("redb: encode addr claimant set");
    table.insert(key, encoded.as_str()).map_err(persist_err)?;
    Ok(())
}

/// Remove `intent_id`'s ceiling entry from `table[key]`, deleting the row
/// outright once empty — the address counterpart of
/// [`remove_claimant_in_txn`].
pub(super) fn remove_addr_claimant_in_txn(
    table: &mut redb::Table<'_, &str, &str>,
    key: &str,
    intent_id: IntentId,
) -> Result<(), PersistenceError> {
    let Some(json) = table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_string())
    else {
        return Ok(());
    };
    let mut claimants: Vec<AddrClaimant> = serde_json::from_str(&json)
        .map_err(|error| PersistenceError(format!("decode address claimant set: {error}")))?;
    claimants.retain(|c| c.intent_id != intent_id.0);
    if claimants.is_empty() {
        table.remove(key).map_err(persist_err)?;
    } else {
        let encoded = serde_json::to_string(&claimants).expect("redb: encode addr claimant set");
        table.insert(key, encoded.as_str()).map_err(persist_err)?;
    }
    Ok(())
}

/// `true` iff ANY claimant at `table[key]` currently covers
/// `candidate_created_at` (its ceiling is at-or-after it) — the
/// provisional counterpart of the permanent `ADDR_TOMBSTONES` ceiling
/// check, consulted by [`is_suppressed_in_txn`].
pub(super) fn addr_has_covering_claimant_in_txn(
    table: &impl ReadableTable<&'static str, &'static str>,
    key: &str,
    candidate_created_at: Timestamp,
) -> Result<bool, PersistenceError> {
    let Some(json) = table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| guard.value().to_string())
    else {
        return Ok(false);
    };
    let claimants: Vec<AddrClaimant> = serde_json::from_str(&json)
        .map_err(|error| PersistenceError(format!("decode address claimant set: {error}")))?;
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
    outbox_suppress_by_id: &impl ReadableTable<&'static str, &'static str>,
    outbox_suppress_by_addr: &impl ReadableTable<&'static str, &'static str>,
    event: &Event,
) -> Result<bool, PersistenceError> {
    let id_key = id_tombstone_key(&event.id, &event.pubkey);
    if has_claimants_in_txn(outbox_suppress_by_id, &id_key)? {
        return Ok(true);
    }
    if let Some(key) = address_key_for(event) {
        let key_str = key.to_redb_key();
        if addr_has_covering_claimant_in_txn(outbox_suppress_by_addr, &key_str, event.created_at)? {
            return Ok(true);
        }
    }
    Ok(false)
}
