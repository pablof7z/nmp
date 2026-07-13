//! [`RedbStore`] — the persistent, `redb`-backed `EventStore` (M3 step A1).
//!
//! Canonical events use an immutable portable binary note value addressed by
//! a compact monotonic `u64` key. Raw event ids map to that key, optional local
//! state has a dedicated compact value, and relay observations are fixed-width
//! `(event, interned-relay) -> timestamp` rows. Every ordered secondary index
//! points straight at the event key. Queries borrow note fields from redb
//! guards and join provenance only for returned rows. Displaced outbox rows
//! remain self-contained binary snapshots; other outbox/coverage metadata
//! remains typed JSON.
//!
//! `redb`'s own errors (`TableError`/`StorageError`/…) are all invariant
//! violations for this crate's purposes — a healthy embedded DB file does
//! not fail to open a table it created itself, or fail to commit a
//! transaction it started — so they are `.expect()`ed rather than threaded
//! through `EventStore`'s trait signatures (which, matching `MemoryStore`,
//! carry no `Result` at all). A real I/O error here means the on-disk file
//! is corrupt, not a reachable, recoverable condition this crate's callers
//! could usefully branch on today.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use nmp_grammar::{ConcreteFilter, ContextualAtom};
use nostr::secp256k1::schnorr::Signature;
use nostr::{
    Event, EventId, Filter, JsonUtil, Kind, PublicKey, RelayUrl, SingleLetterTag, Timestamp,
};
use redb::{
    Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle,
};
use serde::{Deserialize, Serialize};

use crate::address_key::{address_key_for, address_key_for_coordinate, candidate_wins};
use crate::binary_event::{self, IndexedMatch, StoredEventView};
use crate::coverage::{
    coverage_key as compute_coverage_key, merge_interval, shape_matches, shrink_after_eviction,
    window_erase, ShapeRecord,
};
use crate::{
    AcceptOutcome, AcceptWrite, AttemptHandoffDetail, AttemptOutcome, AttemptTransientDetail,
    ClaimSet, CloseIntentOutcome, CompensateOutcome, CoverageInterval, CoverageKey, DeadlineKind,
    EventStore, FinishAttemptOutcome, GcReport, InFlightPhase, InsertOutcome, IntentId,
    IntentSigState, LaneDeadline, LaneKey, LaneState, LocalOrigin, PersistenceError,
    PostHandoffState, PromoteOutcome, Provenance, ReceiptState, RecoveredAttempt,
    RecoveredAttemptDetails, RecoveredIntent, RecoveredLane, RecoveredReceipt,
    RecoveredRouteRevision, RefuseReason, RelayObserved, RetractReason, SigState, StoredEvent,
    TransientCause, WriteDurability,
};

/// Wrap any `redb` operation error as a [`PersistenceError`] (architecture
/// review correction — see its doc). `accept_write`/`accept_ephemeral`/
/// `promote_signed`/`compensate_write`, and every table-touching helper
/// they call, propagate through this via `?`; the crate's OTHER,
/// pre-existing doors (`insert`/`remove`/`expire_due`/`gc`) still
/// `.expect()` these same `Result`s at their own call sites into the
/// shared helpers below — unchanged behavior for them, just funneled
/// through one typed error type instead of a bespoke panic message each.
fn persist_err(e: impl std::fmt::Display) -> PersistenceError {
    PersistenceError(e.to_string())
}

type EventKey = u64;
type RelayKey = u32;

/// Breaking v4 event schema. Compatibility is intentionally not carried:
/// immutable notes, local state, interned relay observations, raw-id lookup,
/// and compact primary keys are independent tables from the first byte.
const EVENTS: TableDefinition<EventKey, &[u8]> = TableDefinition::new("events_v4");
const EVENT_IDS: TableDefinition<&[u8], EventKey> = TableDefinition::new("event_ids_v4");
const EVENT_LOCAL: TableDefinition<EventKey, &[u8]> = TableDefinition::new("event_local_v4");
const EVENT_STORE_META: TableDefinition<&str, EventKey> =
    TableDefinition::new("event_store_meta_v4");
const NEXT_EVENT_KEY: &str = "next_event_key";
const RELAYS: TableDefinition<RelayKey, &str> = TableDefinition::new("relays_v4");
const RELAY_KEYS: TableDefinition<&str, RelayKey> = TableDefinition::new("relay_keys_v4");
const RELAY_REFS: TableDefinition<RelayKey, u64> = TableDefinition::new("relay_refs_v4");
const RELAY_META: TableDefinition<&str, RelayKey> = TableDefinition::new("relay_meta_v4");
const NEXT_RELAY_KEY: &str = "next_relay_key";
/// Fixed-width key: `event_key:u64-be | relay_key:u32-be`; value is the
/// greatest observation timestamp in seconds.
const EVENT_OBSERVATIONS: TableDefinition<&[u8; 12], u64> =
    TableDefinition::new("event_observations_v4");
const LEGACY_EVENT_TABLES: [&str; 5] = [
    "events",
    "events_v2",
    "events_v3",
    "outbox_displaced_v2",
    "outbox_displaced_v3",
];
const ADDR_INDEX: TableDefinition<&str, EventKey> = TableDefinition::new("addr_index_v4");
const COVERAGE: TableDefinition<&str, &str> = TableDefinition::new("coverage");
/// Permanent kind:5 tombstones for individual event ids
/// (retraction-and-negative-deltas.md §2/§7). Key: `"{id_hex}:{author_hex}"`
/// -- one row PER CLAIMING AUTHOR, never collapsed to one row per id: the
/// target's real author is unknown until it actually arrives, so an
/// unauthorized third party can always name an id someone else has already
/// (or will later) legitimately delete. A single overwritable row per id
/// would let that unauthorized claim silently replace -- and so undo -- the
/// real author's permanent, authorized deletion. Value: the deleting
/// kind:5's own id hex (diagnostics only; the key alone decides refusal).
/// Never GC-claimed.
const TOMBSTONES: TableDefinition<&str, &str> = TableDefinition::new("tombstones");
/// Permanent kind:5 tombstones for replaceable/addressable addresses. Key:
/// [`crate::address_key::AddressKey::to_redb_key`]. Value carries the
/// deletion ceiling (highest deleting-event `created_at` seen for that
/// address) — a candidate with `created_at <= ceiling` is tombstoned.
const ADDR_TOMBSTONES: TableDefinition<&str, &str> = TableDefinition::new("addr_tombstones");
/// The persistent NIP-40 expiration index (retraction-and-negative-
/// deltas.md §3.1). Key: [`expiration_key`] (`"{ts:020}:{id_hex}"`, so
/// byte-lexicographic order matches numeric deadline order); value: the
/// canonical event's compact surrogate key.
const EXPIRATION_INDEX: TableDefinition<&str, EventKey> =
    TableDefinition::new("expiration_index_v4");
/// Binary ordered indexes all end in the same sortable suffix:
/// `created_at:u64-be | !event_id:[u8;32]`. Reverse scans therefore yield
/// `created_at DESC, event_id ASC` and can stop exactly at the visible limit.
const BY_CREATED_AT: TableDefinition<&[u8], EventKey> = TableDefinition::new("by_created_at_v3");
const BY_AUTHOR: TableDefinition<&[u8], EventKey> = TableDefinition::new("by_author_time_v3");
const BY_KIND: TableDefinition<&[u8], EventKey> = TableDefinition::new("by_kind_time_v3");
const BY_AUTHOR_KIND: TableDefinition<&[u8], EventKey> =
    TableDefinition::new("by_author_kind_time_v3");
/// NIP-01 single-letter tag index, borrowing nostrdb's clustered
/// `(tag,value,created_at)` layout. The binary key is:
///
/// `tag:u8 | encoding:u8 | value | created_at:u64-be | !event_id:[u8;32]`
///
/// Big-endian timestamp bytes make redb's ordinary byte ordering usable as a
/// newest-first reverse range scan. The event id suffix both disambiguates
/// equal timestamps. The id bytes are inverted so a reverse scan is
/// `created_at DESC, event_id ASC`, NMP's canonical NIP-01 tie-break, without
/// parsing hex.
/// Values are compact event keys, so a hit dereferences the immutable note
/// directly without rebuilding or hex-encoding its NIP-01 id.
const BY_TAG: TableDefinition<&[u8], EventKey> = TableDefinition::new("by_tag_v3");
/// Exact live-row counts for every ordered-index prefix. Keys are namespaced
/// binary prefixes (global, author, kind, author+kind, or tag/value); values
/// count physical index rows in that bucket. Mutations accumulate deltas in
/// memory and flush each hot prefix once in the same crash-atomic write
/// transaction as the canonical row and indexes.
const INDEX_CARDINALITY: TableDefinition<&[u8], u64> = TableDefinition::new("index_cardinality_v1");
const INDEX_CARDINALITY_META: TableDefinition<&str, u64> =
    TableDefinition::new("index_cardinality_meta_v1");
const INDEX_CARDINALITY_VERSION_KEY: &str = "version";
const INDEX_CARDINALITY_VERSION: u64 = 1;
/// The durable write-outbox journal (crashsafe-accepted-2-3-plan.md §2.2,
/// Fable checkpoint Q2 — APPROVED as co-resident in this same `Database`:
/// redb atomicity is a per-`Database` property, so the one crash-atomic
/// commit #3 requires forces the journal into the store's own transaction
/// boundary). Key: [`intent_key`] (zero-padded decimal `IntentId`, shared
/// verbatim with [`OUTBOX_DISPLACED`]). Value: JSON-encoded
/// `OutboxIntentRecord`. A row exists for exactly as long as its intent is
/// open — `compensate_write` deletes it on pre-signature termination; R8's
/// terminal-deletion-on-full-delivery is a later unit's job (this frame
/// never marks an intent all-lanes-terminal, since dispatch/ack tracking is
/// U3/U4).
const OUTBOX_INTENTS: TableDefinition<&str, &str> = TableDefinition::new("outbox_intents");
/// The predecessor each open intent displaced, if any (retraction doc
/// §4.2's durable stash). Key: the SAME [`intent_key`] as its
/// `OUTBOX_INTENTS` row. Value is a self-contained `NMPC` binary snapshot
/// (immutable event plus provenance), unlike canonical `EVENTS`'s event-only
/// `NMPE` value. See [`encode_stored_event`]/[`decode_stored_event`]. Deleted
/// durably by `promote_signed` (R6) or `compensate_write`; never by
/// `recover_outbox` (read-only).
const OUTBOX_DISPLACED: TableDefinition<&str, &[u8]> = TableDefinition::new("outbox_displaced_v4");
/// Per-`(intent, relay, ordinal)` durable attempt evidence
/// (crashsafe-accepted-2-3-plan.md §5) — schema created here so the table
/// exists for the dispatch-time attempt writer to come (U3/U4: "Persist
/// `AttemptStarted` before dispatch"). This unit does not write rows into
/// it (Fable checkpoint R2: folding attempt eligibility into
/// `next_deadline` here is a busy-loop spin hazard — that fold ships with
/// the retry-owner follow-up, not this frame) and `recover_outbox` does not
/// read it — it is created purely for forward schema compatibility.
#[allow(dead_code)]
const OUTBOX_ATTEMPTS: TableDefinition<&str, &str> = TableDefinition::new("outbox_attempts");
/// Append-only exact resolved-route snapshots, keyed by `(intent, ordinal)`.
const OUTBOX_ROUTE_REVISIONS: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_route_revisions");
const OUTBOX_LANES: TableDefinition<&str, &str> = TableDefinition::new("outbox_lanes");
const OUTBOX_DEADLINES: TableDefinition<&str, &str> = TableDefinition::new("outbox_deadlines");
const OUTBOX_DEADLINES_BY_INTENT: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_deadlines_by_intent");
const OUTBOX_ATTEMPT_DETAILS: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_attempt_details");
/// Store-owned outbox metadata — two rows, `"next_intent_id"` and
/// `"next_receipt_id"`, each the next id of its kind to allocate (decimal
/// `u64`, defaulting to 1 if the row has never been written). Both are
/// bumped inside the SAME transaction as the `OUTBOX_INTENTS`/`EVENTS`
/// writes they accompany, so allocation and the intent/receipt it names
/// commit or roll back together (architecture review correction:
/// allocation of EITHER id is a durable fact the store itself owns — see
/// [`IntentId`]'s doc for the reuse hazard this closes; the identical
/// hazard applies to receipt ids once receipts are durably retained).
const OUTBOX_META: TableDefinition<&str, &str> = TableDefinition::new("outbox_meta");
const NEXT_INTENT_ID_KEY: &str = "next_intent_id";
const NEXT_RECEIPT_ID_KEY: &str = "next_receipt_id";
/// Durably-RETAINED receipt records, keyed by `receipt_id` (zero-padded
/// decimal, mirroring [`intent_key`]'s convention) — independent of
/// `OUTBOX_INTENTS`'s open-work rows (architecture review correction: see
/// [`crate::ReceiptState`]'s doc for why this separation exists). Never
/// pruned by this unit.
const OUTBOX_RECEIPTS: TableDefinition<&str, &str> = TableDefinition::new("outbox_receipts");

fn attempt_prefix(intent_id: IntentId, relay: &RelayUrl) -> String {
    // Length-prefixing makes relay-prefix pairs (`wss://x` and
    // `wss://x:443`) disjoint without relying on URL separator rules.
    format!(
        "{:020}:{:020}:{}:",
        intent_id.0,
        relay.as_str().len(),
        relay.as_str()
    )
}

fn intent_row_prefix(intent_id: IntentId) -> String {
    format!("{:020}:", intent_id.0)
}

/// Every outbox prefix ends in the `:` delimiter. Replacing that final byte
/// with its immediate ASCII successor yields the smallest exclusive upper
/// bound containing every key beginning with the original prefix.
fn prefix_range(prefix: String) -> (String, String) {
    debug_assert!(prefix.ends_with(':'));
    let mut upper = prefix.clone();
    upper.pop();
    upper.push(';');
    (prefix, upper)
}

fn attempt_key(intent_id: IntentId, relay: &RelayUrl, ordinal: u64) -> String {
    format!("{}{:020}", attempt_prefix(intent_id, relay), ordinal)
}

fn lane_key(key: &LaneKey) -> String {
    let relay: &nostr::Url = (&key.relay).into();
    let relay = relay.as_str();
    format!("{:020}:{:020}:{relay}", key.intent_id.0, relay.len())
}

fn relay_order_key(relay: &RelayUrl) -> String {
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

fn deadline_key(deadline: &LaneDeadline) -> String {
    format!(
        "{:020}:{:020}:{}",
        deadline.at.as_secs(),
        deadline.key.intent_id.0,
        relay_order_key(&deadline.key.relay)
    )
}

fn deadline_intent_key(deadline: &LaneDeadline) -> String {
    format!(
        "{:020}:{:020}:{}",
        deadline.key.intent_id.0,
        deadline.at.as_secs(),
        relay_order_key(&deadline.key.relay)
    )
}

fn deadline_upper(now: Timestamp) -> String {
    format!("{:020};", now.as_secs())
}

fn encode_json(value: &impl Serialize, what: &str) -> Result<String, PersistenceError> {
    serde_json::to_string(value).map_err(|err| PersistenceError(format!("encode {what}: {err}")))
}

fn decode_lane(key: &str, json: &str) -> Result<RecoveredLane, PersistenceError> {
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

fn decode_deadline(key: &str, json: &str) -> Result<LaneDeadline, PersistenceError> {
    let deadline: LaneDeadline = serde_json::from_str(json)
        .map_err(|err| PersistenceError(format!("decode outbox deadline: {err}")))?;
    if deadline_key(&deadline) != key {
        return Err(PersistenceError(
            "outbox deadline key does not match value".into(),
        ));
    }
    Ok(deadline)
}

fn decode_deadline_by_intent(key: &str, json: &str) -> Result<LaneDeadline, PersistenceError> {
    let deadline: LaneDeadline = serde_json::from_str(json)
        .map_err(|err| PersistenceError(format!("decode outbox deadline: {err}")))?;
    if deadline_intent_key(&deadline) != key {
        return Err(PersistenceError(
            "outbox deadline-by-intent key does not match value".into(),
        ));
    }
    Ok(deadline)
}

fn decode_attempt_details(
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

fn lane_deadline(lane: &RecoveredLane) -> Option<LaneDeadline> {
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

fn replace_lane_in_txn(
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
struct OutboxAttemptRecord {
    version: u8,
    intent_id: IntentId,
    relay: RelayUrl,
    ordinal: u64,
    event_json: String,
    outcome: AttemptOutcome,
}

fn route_revision_key(intent_id: IntentId, ordinal: u64) -> String {
    format!("{:020}:{:020}", intent_id.0, ordinal)
}

#[derive(Debug, Serialize, Deserialize)]
struct OutboxRouteRevisionRecord {
    version: u8,
    intent_id: IntentId,
    ordinal: u64,
    relays: BTreeSet<RelayUrl>,
}

fn decode_route_revision(
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

fn decode_attempt(key: &str, json: &str) -> Result<RecoveredAttempt, PersistenceError> {
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
const OUTBOX_KIND5_CLAIMS: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_kind5_claims");
/// Reverse index: `id_tombstone_key(target id, claiming author) ->
/// JSON-encoded `Vec<u64>` of claiming `IntentId`s — consulted by
/// `is_suppressed_in_txn` to decide `query` visibility. More than one
/// intent can claim the SAME target (two independent pending deletes of
/// the same event before either signs or cancels): hidden while ANY claim
/// applies, visible again only once every claim on it is dropped.
const OUTBOX_SUPPRESS_BY_ID: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_suppress_by_id");
/// Reverse index for address claims: `AddressKey::to_redb_key() ->
/// JSON-encoded `Vec<u64>``, same treatment as [`OUTBOX_SUPPRESS_BY_ID`].
const OUTBOX_SUPPRESS_BY_ADDR: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_suppress_by_addr");

/// Owned mutation form of one portable binary event row. Query filtering
/// uses [`StoredEventView`] directly and never constructs this form for a
/// rejected candidate.
#[derive(Debug)]
struct StoredEventRecord {
    event: Event,
    provenance: BTreeMap<RelayUrl, Timestamp>,
    local: Option<LocalOrigin>,
}

/// One `OUTBOX_INTENTS` row's JSON value — the full acceptance journal
/// payload (Fable checkpoint R7), everything issue #3's "one crash-atomic
/// commit" enumerates besides the pending row itself (which lives in
/// `EVENTS`, not duplicated here).
#[derive(Debug, Serialize, Deserialize)]
struct OutboxIntentRecord {
    receipt_id: u64,
    frozen_json: String,
    expected_pubkey: PublicKey,
    signing_identity_ref: String,
    durability: WriteDurability,
    routing: String,
    sig_state: IntentSigState,
    accepted_at: Timestamp,
}

/// [`OUTBOX_INTENTS`]/[`OUTBOX_DISPLACED`]'s shared key for `id` — a
/// zero-padded decimal so the two tables can never disagree on how to find
/// each other's row for the same intent, and so a future ordered scan sorts
/// by acceptance order (lexicographic == numeric, matching
/// [`expiration_key`]'s convention).
fn intent_key(id: IntentId) -> String {
    format!("{:020}", id.0)
}

/// Allocate the next [`IntentId`] from [`OUTBOX_META`]'s durable high-water
/// mark, bumping it in the SAME already-open write transaction the caller
/// is about to journal the intent in (architecture review correction — see
/// [`IntentId`]'s doc). Starts at 1 if the row has never been written.
fn alloc_intent_id_in_txn(
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
fn alloc_receipt_id_in_txn(
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
fn alloc_counter_in_txn(
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

/// [`OUTBOX_RECEIPTS`]'s key for `id` — same zero-padding convention as
/// [`intent_key`].
fn receipt_key(id: u64) -> String {
    format!("{:020}", id)
}

/// One `OUTBOX_RECEIPTS` row's JSON value (architecture review correction —
/// see [`crate::ReceiptState`]'s doc). `EventId`/`PublicKey`/`IntentId`/
/// `ReceiptState` all already derive `Serialize`/`Deserialize`, so this
/// mirrors `crate::RecoveredReceipt` field-for-field with no re-encoding.
#[derive(Debug, Serialize, Deserialize)]
struct OutboxReceiptRecord {
    /// `None` for an `Ephemeral` receipt-only record — see
    /// `crate::RecoveredReceipt::intent_id`'s doc.
    intent_id: Option<IntentId>,
    frozen_id: EventId,
    expected_pubkey: PublicKey,
    state: ReceiptState,
}

/// Update `OUTBOX_RECEIPTS[receipt_id]`'s `state` in place, if a row exists
/// (it always should, by construction — every journaled `accept_write`
/// writes one in the same transaction). Shared by `promote_signed` and
/// `compensate_write` (architecture review correction).
fn update_outbox_receipt(
    outbox_receipts: &mut redb::Table<'_, &str, &str>,
    receipt_id: u64,
    state: ReceiptState,
) -> Result<(), PersistenceError> {
    let key = receipt_key(receipt_id);
    // Two statements, not one chained expression — see `remove_row_in_txn`'s
    // comment on the same `?`-temporary-lifetime-extension quirk.
    let existing = outbox_receipts.get(key.as_str()).map_err(persist_err)?;
    if let Some(json) = existing.map(|guard| guard.value().to_string()) {
        let mut record: OutboxReceiptRecord =
            serde_json::from_str(&json).expect("redb: decode outbox receipt");
        record.state = state;
        let encoded = serde_json::to_string(&record).expect("redb: encode outbox receipt");
        outbox_receipts
            .insert(key.as_str(), encoded.as_str())
            .map_err(persist_err)?;
    }
    Ok(())
}

/// Boot-time reconciliation: every `Ephemeral` receipt-only record
/// (`intent_id: None`) still `ReceiptState::Accepted` is flipped to
/// `Abandoned` — see `ReceiptState::Abandoned`'s doc for why this is sound
/// without any engine cooperation. Called from `RedbStore::open()`, inside
/// the SAME write transaction that ensures every table exists (a fresh
/// store's `OUTBOX_RECEIPTS` is empty, so this is a no-op there). Two
/// passes (collect then mutate), mirroring `gc`'s victim-collection
/// pattern: `redb` does not allow mutating a table while iterating it.
fn reconcile_ephemeral_receipts_in_txn(outbox_receipts: &mut redb::Table<'_, &str, &str>) {
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
    for (key, mut record) in to_abandon {
        record.state = ReceiptState::Abandoned;
        let encoded = serde_json::to_string(&record).expect("redb: encode outbox receipt");
        outbox_receipts
            .insert(key.as_str(), encoded.as_str())
            .expect("redb: update outbox_receipts (ephemeral abandon)");
    }
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
enum SuppressClaimRecord {
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
fn add_claimant_in_txn(
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
fn remove_claimant_in_txn(
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
    let mut claimants: Vec<u64> = serde_json::from_str(&json).expect("redb: decode claimant set");
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
fn has_claimants_in_txn(
    table: &impl ReadableTable<&'static str, &'static str>,
    key: &str,
) -> Result<bool, PersistenceError> {
    Ok(table.get(key).map_err(persist_err)?.is_some())
}

/// One `(claiming_intent_id, created_at_ceiling)` pair — `OUTBOX_SUPPRESS_BY_ADDR`'s
/// value shape (issue #61 P0 correction, mirrors `SuppressClaimRecord::Addr`'s
/// doc for why a bare claimant list is not enough).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddrClaimant {
    intent_id: u64,
    ceiling: u64,
}

/// Add (or update) `intent_id`'s ceiling in the JSON-encoded
/// `Vec<AddrClaimant>` claimant list at `table[key]` — the address
/// counterpart of [`add_claimant_in_txn`], carrying a ceiling per
/// claimant instead of a bare id.
fn add_addr_claimant_in_txn(
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
fn remove_addr_claimant_in_txn(
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
    let mut claimants: Vec<AddrClaimant> =
        serde_json::from_str(&json).expect("redb: decode addr claimant set");
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
fn addr_has_covering_claimant_in_txn(
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
    let claimants: Vec<AddrClaimant> =
        serde_json::from_str(&json).expect("redb: decode addr claimant set");
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
fn is_suppressed_in_txn(
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

/// Convert `se` into the record shape used by self-contained displaced rows
/// and governed mutation helpers.
fn stored_event_to_record(se: &StoredEvent) -> StoredEventRecord {
    StoredEventRecord {
        event: se.event.clone(),
        provenance: se.provenance.seen.clone(),
        local: se.provenance.local.clone(),
    }
}

/// The read-side counterpart of [`stored_event_to_record`].
fn record_to_stored_event(record: &StoredEventRecord) -> StoredEvent {
    StoredEvent {
        event: record.event.clone(),
        provenance: Provenance {
            seen: record.provenance.clone(),
            local: record.local.clone(),
        },
    }
}

/// Encode `se` as a self-contained portable `OUTBOX_DISPLACED` snapshot.
fn encode_stored_event(se: &StoredEvent) -> Vec<u8> {
    binary_event::encode(se).expect("redb: encode portable stored event")
}

/// Materialize one self-contained portable `OUTBOX_DISPLACED` value — the
/// read-side counterpart of [`encode_stored_event`].
fn decode_stored_event(bytes: &[u8]) -> StoredEvent {
    binary_event::decode(bytes).expect("redb: decode portable stored event")
}

fn decode_stored_event_record(bytes: &[u8]) -> StoredEventRecord {
    stored_event_to_record(&decode_stored_event(bytes))
}

fn encode_stored_event_record(record: &StoredEventRecord) -> Vec<u8> {
    encode_stored_event(&record_to_stored_event(record))
}

fn observation_key(event_key: EventKey, relay_key: RelayKey) -> [u8; 12] {
    let mut key = [0u8; 12];
    key[..8].copy_from_slice(&event_key.to_be_bytes());
    key[8..].copy_from_slice(&relay_key.to_be_bytes());
    key
}

fn observation_range(event_key: EventKey) -> ([u8; 12], [u8; 12]) {
    (
        observation_key(event_key, RelayKey::MIN),
        observation_key(event_key, RelayKey::MAX),
    )
}

fn observation_relay_key(key: &[u8]) -> RelayKey {
    RelayKey::from_be_bytes(
        key[8..12]
            .try_into()
            .expect("validated observation key is twelve bytes"),
    )
}

#[cfg(test)]
fn observation_event_key(key: &[u8]) -> EventKey {
    EventKey::from_be_bytes(
        key[..8]
            .try_into()
            .expect("validated observation key is twelve bytes"),
    )
}

/// Tables that jointly own one canonical event row. Keeping them behind one
/// value makes it hard for a write path to mutate the immutable note without
/// also considering its raw-id mapping, local state, and relay observations.
struct CanonicalWriteTables<'txn> {
    events: redb::Table<'txn, EventKey, &'static [u8]>,
    event_ids: redb::Table<'txn, &'static [u8], EventKey>,
    local: redb::Table<'txn, EventKey, &'static [u8]>,
    store_meta: redb::Table<'txn, &'static str, EventKey>,
    observations: redb::Table<'txn, &'static [u8; 12], u64>,
    relays: redb::Table<'txn, RelayKey, &'static str>,
    relay_keys: redb::Table<'txn, &'static str, RelayKey>,
    relay_refs: redb::Table<'txn, RelayKey, u64>,
    relay_meta: redb::Table<'txn, &'static str, RelayKey>,
    cardinality: redb::Table<'txn, &'static [u8], u64>,
    /// Effective counts touched by this transaction. Busy batches commonly
    /// share one relay, so the durable hot row is read and written once.
    relay_ref_counts: HashMap<RelayKey, u64>,
    /// Net live-row changes by ordered-index prefix. A governed batch can
    /// touch the same busy room/kind hundreds of times; persisting once per
    /// prefix keeps the single-writer transaction cheap.
    cardinality_deltas: HashMap<Vec<u8>, i64>,
}

impl<'txn> CanonicalWriteTables<'txn> {
    fn open(write_txn: &'txn redb::WriteTransaction) -> Result<Self, PersistenceError> {
        Ok(Self {
            events: write_txn.open_table(EVENTS).map_err(persist_err)?,
            event_ids: write_txn.open_table(EVENT_IDS).map_err(persist_err)?,
            local: write_txn.open_table(EVENT_LOCAL).map_err(persist_err)?,
            store_meta: write_txn
                .open_table(EVENT_STORE_META)
                .map_err(persist_err)?,
            observations: write_txn
                .open_table(EVENT_OBSERVATIONS)
                .map_err(persist_err)?,
            relays: write_txn.open_table(RELAYS).map_err(persist_err)?,
            relay_keys: write_txn.open_table(RELAY_KEYS).map_err(persist_err)?,
            relay_refs: write_txn.open_table(RELAY_REFS).map_err(persist_err)?,
            relay_meta: write_txn.open_table(RELAY_META).map_err(persist_err)?,
            cardinality: write_txn
                .open_table(INDEX_CARDINALITY)
                .map_err(persist_err)?,
            relay_ref_counts: HashMap::new(),
            cardinality_deltas: HashMap::new(),
        })
    }

    fn key_for_id(&self, id: &EventId) -> Result<Option<EventKey>, PersistenceError> {
        Ok(self
            .event_ids
            .get(id.as_bytes().as_slice())
            .map_err(persist_err)?
            .map(|guard| guard.value()))
    }

    fn load_by_key(&self, key: EventKey) -> Result<Option<StoredEvent>, PersistenceError> {
        let Some(event_bytes) = self.events.get(key).map_err(persist_err)? else {
            return Ok(None);
        };
        let local_bytes = self.local.get(key).map_err(persist_err)?;
        let event = StoredEventView::from_trusted(event_bytes.value())
            .expect("redb: decode canonical event view")
            .materialize_event()
            .expect("redb: materialize canonical event");
        let local = local_bytes.map(|bytes| {
            binary_event::decode_local(bytes.value()).expect("redb: decode canonical local state")
        });
        let provenance = Provenance {
            seen: self.load_seen(key)?,
            local,
        };
        Ok(Some(StoredEvent { event, provenance }))
    }

    fn load_local(&self, key: EventKey) -> Result<Option<LocalOrigin>, PersistenceError> {
        Ok(self.local.get(key).map_err(persist_err)?.map(|bytes| {
            binary_event::decode_local(bytes.value()).expect("redb: decode canonical local state")
        }))
    }

    fn load_seen(
        &self,
        event_key: EventKey,
    ) -> Result<BTreeMap<RelayUrl, Timestamp>, PersistenceError> {
        let (lower, upper) = observation_range(event_key);
        let mut seen = BTreeMap::new();
        for entry in self
            .observations
            .range::<&[u8; 12]>(&lower..=&upper)
            .map_err(persist_err)?
        {
            let (encoded_key, at) = entry.map_err(persist_err)?;
            let relay_key = observation_relay_key(encoded_key.value());
            let relay = self
                .relays
                .get(relay_key)
                .map_err(persist_err)?
                .expect("redb: observation relay key exists");
            let relay =
                RelayUrl::parse(relay.value()).expect("redb: interned relay URL remains canonical");
            assert!(seen.insert(relay, Timestamp::from(at.value())).is_none());
        }
        Ok(seen)
    }

    fn load_by_id(
        &self,
        id: &EventId,
    ) -> Result<Option<(EventKey, StoredEvent)>, PersistenceError> {
        let Some(key) = self.key_for_id(id)? else {
            return Ok(None);
        };
        Ok(self.load_by_key(key)?.map(|stored| (key, stored)))
    }

    fn allocate_key(&mut self) -> Result<EventKey, PersistenceError> {
        let next = self
            .store_meta
            .get(NEXT_EVENT_KEY)
            .map_err(persist_err)?
            .map(|guard| guard.value())
            .unwrap_or(1);
        let following = next
            .checked_add(1)
            .ok_or_else(|| PersistenceError("canonical event key space exhausted".to_owned()))?;
        self.store_meta
            .insert(NEXT_EVENT_KEY, following)
            .map_err(persist_err)?;
        Ok(next)
    }

    fn allocate_relay_key(&mut self) -> Result<RelayKey, PersistenceError> {
        let next = self
            .relay_meta
            .get(NEXT_RELAY_KEY)
            .map_err(persist_err)?
            .map(|guard| guard.value())
            .unwrap_or(1);
        let following = next
            .checked_add(1)
            .ok_or_else(|| PersistenceError("relay key space exhausted".to_owned()))?;
        self.relay_meta
            .insert(NEXT_RELAY_KEY, following)
            .map_err(persist_err)?;
        Ok(next)
    }

    fn intern_relay(&mut self, relay: &RelayUrl) -> Result<RelayKey, PersistenceError> {
        if let Some(existing) = self.relay_keys.get(relay.as_str()).map_err(persist_err)? {
            return Ok(existing.value());
        }
        let key = self.allocate_relay_key()?;
        self.relays
            .insert(key, relay.as_str())
            .map_err(persist_err)?;
        self.relay_keys
            .insert(relay.as_str(), key)
            .map_err(persist_err)?;
        self.relay_refs.insert(key, 0).map_err(persist_err)?;
        Ok(key)
    }

    fn effective_relay_ref(&mut self, relay_key: RelayKey) -> Result<u64, PersistenceError> {
        if let Some(current) = self.relay_ref_counts.get(&relay_key) {
            return Ok(*current);
        }
        let current = self
            .relay_refs
            .get(relay_key)
            .map_err(persist_err)?
            .expect("redb: interned relay has refcount")
            .value();
        self.relay_ref_counts.insert(relay_key, current);
        Ok(current)
    }

    fn increment_relay_ref(&mut self, relay_key: RelayKey) -> Result<(), PersistenceError> {
        let current = self.effective_relay_ref(relay_key)?;
        let next = current
            .checked_add(1)
            .ok_or_else(|| PersistenceError("relay reference count exhausted".to_owned()))?;
        self.relay_ref_counts.insert(relay_key, next);
        Ok(())
    }

    fn decrement_relay_ref(&mut self, relay_key: RelayKey) -> Result<(), PersistenceError> {
        let current = self.effective_relay_ref(relay_key)?;
        let next = current
            .checked_sub(1)
            .ok_or_else(|| PersistenceError("relay reference count underflow".to_owned()))?;
        self.relay_ref_counts.insert(relay_key, next);
        Ok(())
    }

    fn adjust_cardinality(&mut self, key: Vec<u8>, delta: i64) -> Result<(), PersistenceError> {
        let current = self.cardinality_deltas.entry(key).or_default();
        *current = current
            .checked_add(delta)
            .ok_or_else(|| PersistenceError("index cardinality delta overflow".to_owned()))?;
        Ok(())
    }

    fn flush_counts(&mut self) -> Result<(), PersistenceError> {
        for (relay_key, effective) in std::mem::take(&mut self.relay_ref_counts) {
            let persisted = self
                .relay_refs
                .get(relay_key)
                .map_err(persist_err)?
                .expect("redb: interned relay has refcount")
                .value();
            if effective > 0 {
                if effective == persisted {
                    continue;
                }
                self.relay_refs
                    .insert(relay_key, effective)
                    .map_err(persist_err)?;
                continue;
            }
            let relay = self
                .relays
                .get(relay_key)
                .map_err(persist_err)?
                .expect("redb: interned relay exists")
                .value()
                .to_owned();
            self.relay_refs.remove(relay_key).map_err(persist_err)?;
            self.relays.remove(relay_key).map_err(persist_err)?;
            self.relay_keys
                .remove(relay.as_str())
                .map_err(persist_err)?;
        }
        for (key, delta) in std::mem::take(&mut self.cardinality_deltas) {
            if delta == 0 {
                continue;
            }
            let persisted = self
                .cardinality
                .get(key.as_slice())
                .map_err(persist_err)?
                .map(|guard| guard.value())
                .unwrap_or(0);
            let effective = if delta > 0 {
                persisted.checked_add(delta as u64)
            } else {
                persisted.checked_sub(delta.unsigned_abs())
            }
            .ok_or_else(|| {
                PersistenceError(format!(
                    "index cardinality underflow/overflow for prefix {key:?}"
                ))
            })?;
            if effective == 0 {
                self.cardinality
                    .remove(key.as_slice())
                    .map_err(persist_err)?;
            } else {
                self.cardinality
                    .insert(key.as_slice(), effective)
                    .map_err(persist_err)?;
            }
        }
        Ok(())
    }

    fn merge_observation(
        &mut self,
        event_key: EventKey,
        relay: &RelayUrl,
        at: Timestamp,
    ) -> Result<bool, PersistenceError> {
        let relay_key = self.intern_relay(relay)?;
        let encoded_key = observation_key(event_key, relay_key);
        let existing = self
            .observations
            .get(&encoded_key)
            .map_err(persist_err)?
            .map(|guard| guard.value());
        if existing.is_some_and(|existing| existing >= at.as_secs()) {
            return Ok(false);
        }
        self.observations
            .insert(&encoded_key, at.as_secs())
            .map_err(persist_err)?;
        if existing.is_none() {
            self.increment_relay_ref(relay_key)?;
        }
        Ok(true)
    }

    fn remove_observation(
        &mut self,
        event_key: EventKey,
        relay_key: RelayKey,
    ) -> Result<(), PersistenceError> {
        let encoded_key = observation_key(event_key, relay_key);
        if self
            .observations
            .remove(&encoded_key)
            .map_err(persist_err)?
            .is_some()
        {
            self.decrement_relay_ref(relay_key)?;
        }
        Ok(())
    }

    fn remove_all_observations(&mut self, event_key: EventKey) -> Result<(), PersistenceError> {
        let (lower, upper) = observation_range(event_key);
        let relay_keys = self
            .observations
            .range::<&[u8; 12]>(&lower..=&upper)
            .map_err(persist_err)?
            .map(|entry| {
                entry
                    .map(|(key, _)| observation_relay_key(key.value()))
                    .map_err(persist_err)
            })
            .collect::<Result<Vec<_>, _>>()?;
        for relay_key in relay_keys {
            self.remove_observation(event_key, relay_key)?;
        }
        Ok(())
    }

    fn insert_new(
        &mut self,
        event: &Event,
        provenance: &Provenance,
    ) -> Result<EventKey, PersistenceError> {
        debug_assert!(self.key_for_id(&event.id)?.is_none());
        let key = self.allocate_key()?;
        let event_bytes =
            binary_event::encode_event(event).expect("redb: encode immutable canonical event");
        self.events
            .insert(key, event_bytes.as_slice())
            .map_err(persist_err)?;
        self.event_ids
            .insert(event.id.as_bytes().as_slice(), key)
            .map_err(persist_err)?;
        if let Some(local) = &provenance.local {
            let encoded =
                binary_event::encode_local(local).expect("redb: encode canonical local state");
            self.local
                .insert(key, encoded.as_slice())
                .map_err(persist_err)?;
        }
        for (relay, at) in &provenance.seen {
            self.merge_observation(key, relay, *at)?;
        }
        Ok(key)
    }

    fn replace_event(&mut self, key: EventKey, event: &Event) -> Result<(), PersistenceError> {
        let encoded =
            binary_event::encode_event(event).expect("redb: encode immutable canonical event");
        self.events
            .insert(key, encoded.as_slice())
            .map_err(persist_err)?;
        Ok(())
    }

    fn replace_provenance(
        &mut self,
        key: EventKey,
        provenance: &Provenance,
    ) -> Result<(), PersistenceError> {
        let existing = self.load_seen(key)?;
        for relay in existing.keys() {
            if !provenance.seen.contains_key(relay) {
                let relay_key = self
                    .relay_keys
                    .get(relay.as_str())
                    .map_err(persist_err)?
                    .expect("redb: observed relay remains interned")
                    .value();
                self.remove_observation(key, relay_key)?;
            }
        }
        for (relay, at) in &provenance.seen {
            if existing.get(relay) != Some(at) {
                let relay_key = self.intern_relay(relay)?;
                let encoded_key = observation_key(key, relay_key);
                let was_absent = self
                    .observations
                    .get(&encoded_key)
                    .map_err(persist_err)?
                    .is_none();
                self.observations
                    .insert(&encoded_key, at.as_secs())
                    .map_err(persist_err)?;
                if was_absent {
                    self.increment_relay_ref(relay_key)?;
                }
            }
        }
        self.replace_local(key, provenance.local.clone())
    }

    fn replace_local(
        &mut self,
        key: EventKey,
        local: Option<LocalOrigin>,
    ) -> Result<(), PersistenceError> {
        if let Some(local) = local {
            let encoded =
                binary_event::encode_local(&local).expect("redb: encode canonical local state");
            self.local
                .insert(key, encoded.as_slice())
                .map_err(persist_err)?;
        } else {
            self.local.remove(key).map_err(persist_err)?;
        }
        Ok(())
    }

    fn remove_by_key(&mut self, key: EventKey, id: &EventId) -> Result<(), PersistenceError> {
        self.events.remove(key).map_err(persist_err)?;
        self.event_ids
            .remove(id.as_bytes().as_slice())
            .map_err(persist_err)?;
        self.local.remove(key).map_err(persist_err)?;
        self.remove_all_observations(key)?;
        Ok(())
    }
}

/// The `addr_tombstones` table's JSON value.
#[derive(Debug, Serialize, Deserialize)]
struct AddrTombstoneRecord {
    ceiling: u64,
    deleting_event_id: String,
    deleting_author: String,
}

/// The `expiration_index` table's key: zero-padded decimal seconds so
/// byte-lexicographic order (what `redb`'s `range` uses) matches numeric
/// timestamp order, `:`-joined with the event id hex to disambiguate
/// multiple events sharing one deadline.
fn expiration_key(ts: Timestamp, id: &EventId) -> String {
    format!("{:020}:{}", ts.as_secs(), id.to_hex())
}

/// The inclusive upper bound of every `expiration_key` at or before `ts`:
/// `'f'` is the greatest ASCII hex-digit character, so 64 of them sorts
/// after every real 32-byte id hex sharing that same timestamp prefix.
fn expiration_key_upper_bound(ts: Timestamp) -> String {
    format!("{:020}:{}", ts.as_secs(), "f".repeat(64))
}

/// The `tombstones` table's key for one (target id, claiming author) pair —
/// see [`TOMBSTONES`]'s doc for why this is composite, not just the id.
fn id_tombstone_key(id: &EventId, author: &PublicKey) -> String {
    format!("{}:{}", id.to_hex(), author.to_hex())
}

fn ordered_key(prefix: &[u8], created_at: Timestamp, id: &EventId) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + 8 + 32);
    key.extend_from_slice(prefix);
    key.extend_from_slice(&created_at.as_secs().to_be_bytes());
    key.extend(id.as_bytes().iter().map(|byte| !byte));
    key
}

fn ordered_range(prefix: &[u8], since: u64, until: u64) -> (Vec<u8>, Vec<u8>) {
    let mut lower = Vec::with_capacity(prefix.len() + 8 + 32);
    lower.extend_from_slice(prefix);
    lower.extend_from_slice(&since.to_be_bytes());
    lower.extend_from_slice(&[0u8; 32]);
    let mut upper = Vec::with_capacity(prefix.len() + 8 + 32);
    upper.extend_from_slice(prefix);
    upper.extend_from_slice(&until.to_be_bytes());
    upper.extend_from_slice(&[u8::MAX; 32]);
    (lower, upper)
}

fn created_at_key(event: &Event) -> Vec<u8> {
    ordered_key(&[], event.created_at, &event.id)
}

fn by_author_key(event: &Event) -> Vec<u8> {
    ordered_key(event.pubkey.as_bytes(), event.created_at, &event.id)
}

fn by_author_prefix(author: &PublicKey) -> Vec<u8> {
    author.as_bytes().to_vec()
}

fn by_kind_key(event: &Event) -> Vec<u8> {
    ordered_key(
        &event.kind.as_u16().to_be_bytes(),
        event.created_at,
        &event.id,
    )
}

fn by_kind_prefix(kind: Kind) -> Vec<u8> {
    kind.as_u16().to_be_bytes().to_vec()
}

fn by_author_kind_key(event: &Event) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(34);
    prefix.extend_from_slice(event.pubkey.as_bytes());
    prefix.extend_from_slice(&event.kind.as_u16().to_be_bytes());
    ordered_key(&prefix, event.created_at, &event.id)
}

fn by_author_kind_prefix(author: &PublicKey, kind: Kind) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(34);
    prefix.extend_from_slice(author.as_bytes());
    prefix.extend_from_slice(&kind.as_u16().to_be_bytes());
    prefix
}

const CARDINALITY_GLOBAL: u8 = 0;
const CARDINALITY_AUTHOR: u8 = 1;
const CARDINALITY_KIND: u8 = 2;
const CARDINALITY_AUTHOR_KIND: u8 = 3;
const CARDINALITY_TAG: u8 = 4;

fn cardinality_key(namespace: u8, prefix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + prefix.len());
    key.push(namespace);
    key.extend_from_slice(prefix);
    key
}

fn global_cardinality_key() -> Vec<u8> {
    cardinality_key(CARDINALITY_GLOBAL, &[])
}

fn author_cardinality_key(author: &PublicKey) -> Vec<u8> {
    cardinality_key(CARDINALITY_AUTHOR, author.as_bytes())
}

fn kind_cardinality_key(kind: Kind) -> Vec<u8> {
    cardinality_key(CARDINALITY_KIND, &kind.as_u16().to_be_bytes())
}

fn author_kind_cardinality_key(author: &PublicKey, kind: Kind) -> Vec<u8> {
    cardinality_key(
        CARDINALITY_AUTHOR_KIND,
        &by_author_kind_prefix(author, kind),
    )
}

fn tag_cardinality_key(tag: SingleLetterTag, value: &str) -> Vec<u8> {
    cardinality_key(CARDINALITY_TAG, &tag_index_prefix(tag, value))
}

fn tag_index_prefix(tag: SingleLetterTag, value: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(2 + 4 + value.len());
    key.push(tag.as_char() as u8);
    if let Some(raw_id) = decode_hex_32(value) {
        // nostrdb's packed-id win, kept portable and explicit: e/p/a-like
        // values that are exactly one 32-byte hex identity occupy raw bytes
        // in the index instead of repeating 64 ASCII bytes.
        key.push(1);
        key.extend_from_slice(&raw_id);
    } else {
        key.push(0);
        let value = value.as_bytes();
        let value_len = u32::try_from(value.len()).expect("a Nostr tag value fits in u32");
        key.extend_from_slice(&value_len.to_be_bytes());
        key.extend_from_slice(value);
    }
    key
}

fn decode_hex_32(value: &str) -> Option<[u8; 32]> {
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }

    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut raw = [0u8; 32];
    for (index, byte) in raw.iter_mut().enumerate() {
        let pair = index * 2;
        *byte = (nibble(bytes[pair])? << 4) | nibble(bytes[pair + 1])?;
    }
    Some(raw)
}

fn tag_index_key(
    tag: SingleLetterTag,
    value: &str,
    created_at: Timestamp,
    id: &EventId,
) -> Vec<u8> {
    ordered_key(&tag_index_prefix(tag, value), created_at, id)
}

#[cfg(test)]
fn add_event_cardinalities(counts: &mut BTreeMap<Vec<u8>, u64>, event: &Event) {
    let mut increment = |key: Vec<u8>| {
        let count = counts.entry(key).or_default();
        *count = count.checked_add(1).expect("event cardinality fits in u64");
    };
    increment(global_cardinality_key());
    increment(author_cardinality_key(&event.pubkey));
    increment(kind_cardinality_key(event.kind));
    increment(author_kind_cardinality_key(&event.pubkey, event.kind));
    let mut tags = BTreeSet::new();
    for tag in event.tags.iter() {
        let (Some(single_letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
            continue;
        };
        tags.insert(tag_cardinality_key(single_letter, value));
    }
    for key in tags {
        increment(key);
    }
}

fn count_ordered_index_prefixes(
    counts: &mut BTreeMap<Vec<u8>, u64>,
    index: &redb::Table<'_, &[u8], EventKey>,
    namespace: u8,
) -> Result<(), redb::StorageError> {
    for entry in index.iter()? {
        let (key, _event_key) = entry?;
        let key = key.value();
        let prefix_len = key
            .len()
            .checked_sub(40)
            .expect("redb: ordered index key carries created_at and id");
        let count = counts
            .entry(cardinality_key(namespace, &key[..prefix_len]))
            .or_default();
        *count = count
            .checked_add(1)
            .expect("ordered index cardinality fits in u64");
    }
    Ok(())
}

/// Bootstrap the independently versioned cardinality sidecar by counting
/// ordered index keys only. No canonical event value is dereferenced or
/// materialized during the upgrade. The caller writes the version marker in
/// the same redb transaction, so a crash exposes either the previous complete
/// sidecar or no upgrade.
fn rebuild_index_cardinality(
    by_created_at: &redb::Table<'_, &[u8], EventKey>,
    by_author: &redb::Table<'_, &[u8], EventKey>,
    by_kind: &redb::Table<'_, &[u8], EventKey>,
    by_author_kind: &redb::Table<'_, &[u8], EventKey>,
    by_tag: &redb::Table<'_, &[u8], EventKey>,
    cardinality: &mut redb::Table<'_, &[u8], u64>,
) -> Result<(), redb::StorageError> {
    let old_keys = cardinality
        .iter()?
        .map(|entry| entry.map(|(key, _value)| key.value().to_vec()))
        .collect::<Result<Vec<_>, _>>()?;
    for key in old_keys {
        cardinality.remove(key.as_slice())?;
    }

    let mut counts = BTreeMap::new();
    count_ordered_index_prefixes(&mut counts, by_created_at, CARDINALITY_GLOBAL)?;
    count_ordered_index_prefixes(&mut counts, by_author, CARDINALITY_AUTHOR)?;
    count_ordered_index_prefixes(&mut counts, by_kind, CARDINALITY_KIND)?;
    count_ordered_index_prefixes(&mut counts, by_author_kind, CARDINALITY_AUTHOR_KIND)?;
    count_ordered_index_prefixes(&mut counts, by_tag, CARDINALITY_TAG)?;
    for (key, count) in counts {
        cardinality.insert(key.as_slice(), count)?;
    }
    Ok(())
}

fn ordered_index_event_id(key: &[u8]) -> EventId {
    let id_start = key
        .len()
        .checked_sub(32)
        .expect("redb: tag index key is at least 32 bytes");
    let mut id = [0u8; 32];
    for (dst, encoded) in id.iter_mut().zip(&key[id_start..]) {
        *dst = !encoded;
    }
    EventId::from_byte_array(id)
}

fn ordered_index_created_at(key: &[u8]) -> u64 {
    let timestamp_start = key
        .len()
        .checked_sub(40)
        .expect("redb: ordered index key is at least 40 bytes");
    u64::from_be_bytes(
        key[timestamp_start..timestamp_start + 8]
            .try_into()
            .expect("timestamp slice is eight bytes"),
    )
}

/// Test-only raw-table audit for v4 event/relay surrogate integrity. Every
/// governed crash/reopen proof calls this directly, without going through
/// query paths that could hide a missing or orphan pointer.
#[cfg(test)]
fn assert_canonical_integrity(db: &Database) {
    let read_txn = db.begin_read().expect("begin canonical integrity audit");
    let events = read_txn.open_table(EVENTS).expect("audit events");
    let event_ids = read_txn.open_table(EVENT_IDS).expect("audit event ids");
    let local = read_txn
        .open_table(EVENT_LOCAL)
        .expect("audit event local metadata");
    let store_meta = read_txn
        .open_table(EVENT_STORE_META)
        .expect("audit event store meta");
    let observations = read_txn
        .open_table(EVENT_OBSERVATIONS)
        .expect("audit event observations");
    let relays = read_txn.open_table(RELAYS).expect("audit relays");
    let relay_keys = read_txn.open_table(RELAY_KEYS).expect("audit relay keys");
    let relay_refs = read_txn.open_table(RELAY_REFS).expect("audit relay refs");
    let relay_meta = read_txn.open_table(RELAY_META).expect("audit relay meta");
    let cardinality = read_txn
        .open_table(INDEX_CARDINALITY)
        .expect("audit index cardinality");
    let cardinality_meta = read_txn
        .open_table(INDEX_CARDINALITY_META)
        .expect("audit index cardinality meta");
    assert_eq!(
        cardinality_meta
            .get(INDEX_CARDINALITY_VERSION_KEY)
            .expect("audit cardinality version")
            .expect("cardinality version exists")
            .value(),
        INDEX_CARDINALITY_VERSION
    );

    let mut canonical = BTreeMap::new();
    for entry in events.iter().expect("iterate audit events") {
        let (key, bytes) = entry.expect("read audit event");
        let key = key.value();
        let view = StoredEventView::parse(bytes.value()).expect("audit event binary value");
        let event = view.materialize_event().expect("audit materialized event");
        assert_eq!(
            event_ids
                .get(event.id.as_bytes().as_slice())
                .expect("audit id lookup")
                .expect("every event has a raw-id mapping")
                .value(),
            key
        );
        assert!(canonical.insert(key, event).is_none());
    }

    assert_eq!(
        event_ids.len().expect("count audit event ids"),
        canonical.len() as u64
    );
    for entry in event_ids.iter().expect("iterate audit event ids") {
        let (raw_id, event_key) = entry.expect("read audit event id");
        let event_key = event_key.value();
        let event = canonical
            .get(&event_key)
            .expect("raw id mapping points at a live event");
        assert_eq!(raw_id.value(), event.id.as_bytes().as_slice());
    }

    for entry in local.iter().expect("iterate audit local metadata") {
        let (event_key, value) = entry.expect("read audit local metadata");
        assert!(canonical.contains_key(&event_key.value()));
        binary_event::decode_local(value.value()).expect("audit local metadata sidecar");
    }

    if let Some(max_key) = canonical.keys().next_back() {
        let next = store_meta
            .get(NEXT_EVENT_KEY)
            .expect("audit next event key")
            .expect("nonempty canonical store has next event key")
            .value();
        assert!(next > *max_key, "surrogate allocator must not reuse keys");
    }

    let mut expected_relay_refs = BTreeMap::<RelayKey, u64>::new();
    for entry in observations.iter().expect("iterate audit observations") {
        let (encoded_key, _at) = entry.expect("read audit observation");
        let encoded_key = encoded_key.value();
        assert_eq!(encoded_key.len(), 12);
        let event_key = observation_event_key(encoded_key);
        let relay_key = observation_relay_key(encoded_key);
        assert!(
            canonical.contains_key(&event_key),
            "observation points at live event"
        );
        assert!(
            relays.get(relay_key).expect("audit relay lookup").is_some(),
            "observation points at interned relay"
        );
        *expected_relay_refs.entry(relay_key).or_default() += 1;
    }
    assert_eq!(
        relays.len().expect("count audit relays"),
        expected_relay_refs.len() as u64
    );
    assert_eq!(
        relay_keys.len().expect("count audit relay keys"),
        expected_relay_refs.len() as u64
    );
    assert_eq!(
        relay_refs.len().expect("count audit relay refs"),
        expected_relay_refs.len() as u64
    );
    for entry in relays.iter().expect("iterate audit relays") {
        let (relay_key, encoded_url) = entry.expect("read audit relay");
        let relay_key = relay_key.value();
        RelayUrl::parse(encoded_url.value()).expect("interned relay is canonical");
        assert_eq!(
            relay_keys
                .get(encoded_url.value())
                .expect("audit reverse relay lookup")
                .expect("relay has reverse key")
                .value(),
            relay_key
        );
        assert_eq!(
            relay_refs
                .get(relay_key)
                .expect("audit relay ref lookup")
                .expect("relay has refcount")
                .value(),
            expected_relay_refs[&relay_key]
        );
    }
    for entry in relay_keys.iter().expect("iterate audit reverse relays") {
        let (encoded_url, relay_key) = entry.expect("read audit reverse relay");
        assert_eq!(
            relays
                .get(relay_key.value())
                .expect("audit forward relay lookup")
                .expect("reverse relay has forward row")
                .value(),
            encoded_url.value()
        );
    }
    if let Some(max_key) = expected_relay_refs.keys().next_back() {
        let next = relay_meta
            .get(NEXT_RELAY_KEY)
            .expect("audit next relay key")
            .expect("nonempty relay dictionary has next key")
            .value();
        assert!(next > *max_key, "relay allocator must not reuse keys");
    }

    let actual_ordered = |definition: TableDefinition<&[u8], EventKey>| {
        let index = read_txn
            .open_table(definition)
            .expect("audit ordered index");
        index
            .iter()
            .expect("iterate audit ordered index")
            .map(|entry| {
                let (encoded_key, event_key) = entry.expect("read audit ordered index");
                (encoded_key.value().to_vec(), event_key.value())
            })
            .collect::<BTreeSet<_>>()
    };
    let mut expected_created = BTreeSet::new();
    let mut expected_author = BTreeSet::new();
    let mut expected_kind = BTreeSet::new();
    let mut expected_author_kind = BTreeSet::new();
    let mut expected_tag = BTreeSet::new();
    let mut expected_address = BTreeSet::new();
    let mut expected_expiration = BTreeSet::new();
    let mut expected_cardinality = BTreeMap::new();
    for (&event_key, event) in &canonical {
        add_event_cardinalities(&mut expected_cardinality, event);
        expected_created.insert((created_at_key(event), event_key));
        expected_author.insert((by_author_key(event), event_key));
        expected_kind.insert((by_kind_key(event), event_key));
        expected_author_kind.insert((by_author_kind_key(event), event_key));
        for tag in event.tags.iter() {
            let (Some(single_letter), Some(value)) = (tag.single_letter_tag(), tag.content())
            else {
                continue;
            };
            expected_tag.insert((
                tag_index_key(single_letter, value, event.created_at, &event.id),
                event_key,
            ));
        }
        if let Some(address) = address_key_for(event) {
            expected_address.insert((address.to_redb_key(), event_key));
        }
        if let Some(timestamp) = event.tags.expiration().copied() {
            expected_expiration.insert((expiration_key(timestamp, &event.id), event_key));
        }
    }
    assert_eq!(actual_ordered(BY_CREATED_AT), expected_created);
    assert_eq!(actual_ordered(BY_AUTHOR), expected_author);
    assert_eq!(actual_ordered(BY_KIND), expected_kind);
    assert_eq!(actual_ordered(BY_AUTHOR_KIND), expected_author_kind);
    assert_eq!(actual_ordered(BY_TAG), expected_tag);
    let actual_cardinality = cardinality
        .iter()
        .expect("iterate audit cardinality")
        .map(|entry| {
            let (key, count) = entry.expect("read audit cardinality");
            (key.value().to_vec(), count.value())
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(actual_cardinality, expected_cardinality);

    let address = read_txn
        .open_table(ADDR_INDEX)
        .expect("audit address index");
    let actual_address = address
        .iter()
        .expect("iterate audit address index")
        .map(|entry| {
            let (encoded_address, event_key) = entry.expect("read audit address index");
            (encoded_address.value().to_owned(), event_key.value())
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_address, expected_address);

    let expiration = read_txn
        .open_table(EXPIRATION_INDEX)
        .expect("audit expiration index");
    let actual_expiration = expiration
        .iter()
        .expect("iterate audit expiration index")
        .map(|entry| {
            let (encoded_expiration, event_key) = entry.expect("read audit expiration index");
            (encoded_expiration.value().to_owned(), event_key.value())
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_expiration, expected_expiration);
}

struct OrderedCursor {
    entries: std::iter::Rev<redb::Range<'static, &'static [u8], EventKey>>,
}

impl OrderedCursor {
    fn new(
        table: &redb::ReadOnlyTable<&[u8], EventKey>,
        prefix: &[u8],
        since: u64,
        until: u64,
    ) -> Result<Self, PersistenceError> {
        let (lower, upper) = ordered_range(prefix, since, until);
        Ok(Self {
            entries: table
                .range(lower.as_slice()..=upper.as_slice())
                .map_err(persist_err)?
                .rev(),
        })
    }

    fn next_head(&mut self, cursor: usize) -> Result<Option<OrderedHead>, PersistenceError> {
        Ok(match self.entries.next() {
            Some(entry) => {
                let (key, value) = entry.map_err(persist_err)?;
                let key = key.value();
                Some(OrderedHead {
                    created_at: ordered_index_created_at(key),
                    id: ordered_index_event_id(key),
                    event_key: value.value(),
                    cursor,
                })
            }
            None => None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OrderedHead {
    created_at: u64,
    id: EventId,
    event_key: EventKey,
    cursor: usize,
}

impl Ord for OrderedHead {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.created_at
            .cmp(&other.created_at)
            // Canonical ordering is id ascending at equal timestamps; a
            // BinaryHeap pops the greatest item, so invert only this tie.
            .then_with(|| other.id.cmp(&self.id))
            .then_with(|| self.cursor.cmp(&other.cursor))
    }
}

impl PartialOrd for OrderedHead {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderedIndex {
    Global,
    Author,
    Kind,
    AuthorKind,
    Tag(SingleLetterTag),
}

impl OrderedIndex {
    fn table(self) -> TableDefinition<'static, &'static [u8], EventKey> {
        match self {
            Self::Global => BY_CREATED_AT,
            Self::Author => BY_AUTHOR,
            Self::Kind => BY_KIND,
            Self::AuthorKind => BY_AUTHOR_KIND,
            Self::Tag(_) => BY_TAG,
        }
    }

    fn matched(self) -> IndexedMatch {
        match self {
            Self::Global => IndexedMatch::None,
            Self::Author => IndexedMatch::Author,
            Self::Kind => IndexedMatch::Kind,
            Self::AuthorKind => IndexedMatch::AuthorKind,
            Self::Tag(tag) => IndexedMatch::Tag(tag),
        }
    }

    fn tie_rank(self) -> u8 {
        match self {
            Self::AuthorKind => 0,
            Self::Author => 1,
            Self::Tag(_) => 2,
            Self::Kind => 3,
            Self::Global => 4,
        }
    }
}

#[derive(Debug)]
struct OrderedPlan {
    index: OrderedIndex,
    prefixes: Vec<Vec<u8>>,
    estimated_rows: u64,
}

// A composite author×kind index is useful only while its OR-range fan-out
// remains bounded. Public `Filter` values are not trusted to respect relay
// subscription limits, so never allocate an unbounded Cartesian product.
const MAX_COMPOSITE_QUERY_RANGES: usize = 4_096;

fn cardinality_of(
    table: &redb::ReadOnlyTable<&[u8], u64>,
    key: &[u8],
) -> Result<u64, PersistenceError> {
    Ok(table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| guard.value())
        .unwrap_or(0))
}

fn sum_cardinalities<'a>(
    table: &redb::ReadOnlyTable<&[u8], u64>,
    keys: impl IntoIterator<Item = &'a [u8]>,
) -> Result<u64, PersistenceError> {
    let mut total = 0u64;
    for key in keys {
        total = total.saturating_add(cardinality_of(table, key)?);
    }
    Ok(total)
}

fn plan_ordered_query(
    read_txn: &redb::ReadTransaction,
    filter: &Filter,
) -> Result<OrderedPlan, PersistenceError> {
    let cardinality = read_txn
        .open_table(INDEX_CARDINALITY)
        .map_err(persist_err)?;
    let global_key = global_cardinality_key();
    let mut plans = vec![OrderedPlan {
        index: OrderedIndex::Global,
        prefixes: vec![Vec::new()],
        estimated_rows: cardinality_of(&cardinality, &global_key)?,
    }];

    let authors = filter.authors.as_ref().filter(|values| !values.is_empty());
    let kinds = filter.kinds.as_ref().filter(|values| !values.is_empty());
    if let Some(authors) = authors {
        let prefixes: Vec<_> = authors.iter().map(by_author_prefix).collect();
        let keys: Vec<_> = authors.iter().map(author_cardinality_key).collect();
        plans.push(OrderedPlan {
            index: OrderedIndex::Author,
            prefixes,
            estimated_rows: sum_cardinalities(&cardinality, keys.iter().map(Vec::as_slice))?,
        });
    }
    if let Some(kinds) = kinds {
        let prefixes: Vec<_> = kinds.iter().map(|kind| by_kind_prefix(*kind)).collect();
        let keys: Vec<_> = kinds
            .iter()
            .map(|kind| kind_cardinality_key(*kind))
            .collect();
        plans.push(OrderedPlan {
            index: OrderedIndex::Kind,
            prefixes,
            estimated_rows: sum_cardinalities(&cardinality, keys.iter().map(Vec::as_slice))?,
        });
    }
    for (tag, values) in &filter.generic_tags {
        let prefixes: Vec<_> = values
            .iter()
            .map(|value| tag_index_prefix(*tag, value))
            .collect();
        let keys: Vec<_> = values
            .iter()
            .map(|value| tag_cardinality_key(*tag, value))
            .collect();
        plans.push(OrderedPlan {
            index: OrderedIndex::Tag(*tag),
            prefixes,
            estimated_rows: sum_cardinalities(&cardinality, keys.iter().map(Vec::as_slice))?,
        });
    }

    if let (Some(authors), Some(kinds)) = (authors, kinds) {
        let range_count = authors.len().checked_mul(kinds.len());
        if range_count.is_some_and(|count| count <= MAX_COMPOSITE_QUERY_RANGES) {
            let best_rows = plans
                .iter()
                .map(|plan| plan.estimated_rows)
                .min()
                .expect("global ordered query plan always exists");
            let mut estimated_rows = 0u64;
            let mut can_win = true;
            'authors: for author in authors {
                for kind in kinds {
                    estimated_rows = estimated_rows.saturating_add(cardinality_of(
                        &cardinality,
                        &author_kind_cardinality_key(author, *kind),
                    )?);
                    // Estimates are non-negative. Once the composite exceeds
                    // the best already-materialized plan it cannot recover;
                    // abandon before allocating its Cartesian prefixes.
                    if estimated_rows > best_rows {
                        can_win = false;
                        break 'authors;
                    }
                }
            }
            if can_win {
                let mut prefixes = Vec::with_capacity(range_count.expect("bounded above"));
                for author in authors {
                    for kind in kinds {
                        prefixes.push(by_author_kind_prefix(author, *kind));
                    }
                }
                plans.push(OrderedPlan {
                    index: OrderedIndex::AuthorKind,
                    prefixes,
                    estimated_rows,
                });
            }
        }
    }

    Ok(plans
        .into_iter()
        .min_by_key(|plan| (plan.estimated_rows, plan.index.tie_rank()))
        .expect("global ordered query plan always exists"))
}

fn insert_tag_index_rows(
    by_tag: &mut redb::Table<'_, &[u8], EventKey>,
    event: &Event,
    event_key: EventKey,
) -> Result<(), redb::StorageError> {
    for tag in event.tags.iter() {
        let (Some(single_letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
            continue;
        };
        let key = tag_index_key(single_letter, value, event.created_at, &event.id);
        by_tag.insert(key.as_slice(), event_key)?;
    }
    Ok(())
}

fn remove_tag_index_rows(
    by_tag: &mut redb::Table<'_, &[u8], EventKey>,
    event: &Event,
) -> Result<(), redb::StorageError> {
    for tag in event.tags.iter() {
        let (Some(single_letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
            continue;
        };
        let key = tag_index_key(single_letter, value, event.created_at, &event.id);
        by_tag.remove(key.as_slice())?;
    }
    Ok(())
}

/// The five physical query indexes are one mutation unit. Keeping their
/// tables bundled makes every governed writer go through the same
/// insert/remove doors that also maintain prefix cardinalities.
struct QueryIndexWriteTables<'txn> {
    by_created_at: redb::Table<'txn, &'static [u8], EventKey>,
    by_author: redb::Table<'txn, &'static [u8], EventKey>,
    by_kind: redb::Table<'txn, &'static [u8], EventKey>,
    by_author_kind: redb::Table<'txn, &'static [u8], EventKey>,
    by_tag: redb::Table<'txn, &'static [u8], EventKey>,
}

impl<'txn> QueryIndexWriteTables<'txn> {
    fn open(write_txn: &'txn redb::WriteTransaction) -> Result<Self, PersistenceError> {
        Ok(Self {
            by_created_at: write_txn.open_table(BY_CREATED_AT).map_err(persist_err)?,
            by_author: write_txn.open_table(BY_AUTHOR).map_err(persist_err)?,
            by_kind: write_txn.open_table(BY_KIND).map_err(persist_err)?,
            by_author_kind: write_txn.open_table(BY_AUTHOR_KIND).map_err(persist_err)?,
            by_tag: write_txn.open_table(BY_TAG).map_err(persist_err)?,
        })
    }
}

fn insert_query_index_rows(
    canonical: &mut CanonicalWriteTables<'_>,
    indexes: &mut QueryIndexWriteTables<'_>,
    event: &Event,
    event_key: EventKey,
) -> Result<(), PersistenceError> {
    let created = created_at_key(event);
    let author = by_author_key(event);
    let kind = by_kind_key(event);
    let author_kind = by_author_kind_key(event);
    indexes
        .by_created_at
        .insert(created.as_slice(), event_key)
        .map_err(persist_err)?;
    indexes
        .by_author
        .insert(author.as_slice(), event_key)
        .map_err(persist_err)?;
    indexes
        .by_kind
        .insert(kind.as_slice(), event_key)
        .map_err(persist_err)?;
    indexes
        .by_author_kind
        .insert(author_kind.as_slice(), event_key)
        .map_err(persist_err)?;
    insert_tag_index_rows(&mut indexes.by_tag, event, event_key).map_err(persist_err)?;
    canonical.adjust_cardinality(global_cardinality_key(), 1)?;
    canonical.adjust_cardinality(author_cardinality_key(&event.pubkey), 1)?;
    canonical.adjust_cardinality(kind_cardinality_key(event.kind), 1)?;
    canonical.adjust_cardinality(author_kind_cardinality_key(&event.pubkey, event.kind), 1)?;
    let mut tags = BTreeSet::new();
    for tag in event.tags.iter() {
        let (Some(single_letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
            continue;
        };
        tags.insert(tag_cardinality_key(single_letter, value));
    }
    for key in tags {
        canonical.adjust_cardinality(key, 1)?;
    }
    Ok(())
}

fn remove_query_index_rows(
    canonical: &mut CanonicalWriteTables<'_>,
    indexes: &mut QueryIndexWriteTables<'_>,
    event: &Event,
) -> Result<(), PersistenceError> {
    let created = created_at_key(event);
    let author = by_author_key(event);
    let kind = by_kind_key(event);
    let author_kind = by_author_kind_key(event);
    indexes
        .by_created_at
        .remove(created.as_slice())
        .map_err(persist_err)?;
    indexes
        .by_author
        .remove(author.as_slice())
        .map_err(persist_err)?;
    indexes
        .by_kind
        .remove(kind.as_slice())
        .map_err(persist_err)?;
    indexes
        .by_author_kind
        .remove(author_kind.as_slice())
        .map_err(persist_err)?;
    remove_tag_index_rows(&mut indexes.by_tag, event).map_err(persist_err)?;
    canonical.adjust_cardinality(global_cardinality_key(), -1)?;
    canonical.adjust_cardinality(author_cardinality_key(&event.pubkey), -1)?;
    canonical.adjust_cardinality(kind_cardinality_key(event.kind), -1)?;
    canonical.adjust_cardinality(author_kind_cardinality_key(&event.pubkey, event.kind), -1)?;
    let mut tags = BTreeSet::new();
    for tag in event.tags.iter() {
        let (Some(single_letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
            continue;
        };
        tags.insert(tag_cardinality_key(single_letter, value));
    }
    for key in tags {
        canonical.adjust_cardinality(key, -1)?;
    }
    Ok(())
}

/// Read-side tombstone check shared by `insert`
/// (retraction-and-negative-deltas.md §2): `true` iff `event` must be
/// `Refused(Tombstoned)`. Mirrors `MemoryStore::tombstone_refuses` exactly,
/// including the deferred NIP-09 author-only check for an id-tombstone
/// written before its target ever arrived: refused iff `event.pubkey`
/// itself claimed this exact id, regardless of any OTHER author's
/// (irrelevant) claim on the same id.
fn tombstone_refuses(
    tombstones: &redb::Table<'_, &str, &str>,
    addr_tombstones: &redb::Table<'_, &str, &str>,
    event: &Event,
) -> Result<bool, PersistenceError> {
    let key = id_tombstone_key(&event.id, &event.pubkey);
    if tombstones.get(key.as_str()).map_err(persist_err)?.is_some() {
        return Ok(true);
    }
    if let Some(key) = address_key_for(event) {
        let key_str = key.to_redb_key();
        if let Some(guard) = addr_tombstones.get(key_str.as_str()).map_err(persist_err)? {
            let rec: AddrTombstoneRecord =
                serde_json::from_str(guard.value()).expect("redb: decode addr tombstone");
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
/// NIP-40 `expiration`), and the [`BY_AUTHOR`]/[`BY_KIND`]/[`BY_TAG`] query indexes in
/// the same pass. Shared by the trait's own `remove` (`predicate` always
/// `true`) and kind:5 processing (`predicate` is the NIP-09 author-only
/// check).
#[allow(clippy::too_many_arguments)]
fn remove_row_in_txn(
    canonical: &mut CanonicalWriteTables<'_>,
    addr_index: &mut redb::Table<'_, &str, EventKey>,
    expiration_index: &mut redb::Table<'_, &str, EventKey>,
    indexes: &mut QueryIndexWriteTables<'_>,
    id: EventId,
    predicate: impl FnOnce(&StoredEvent) -> bool,
) -> Result<Option<StoredEvent>, PersistenceError> {
    let Some((event_key, se)) = canonical.load_by_id(&id)? else {
        return Ok(None);
    };
    if !predicate(&se) {
        return Ok(None);
    }

    canonical.remove_by_key(event_key, &id)?;
    remove_query_index_rows(canonical, indexes, &se.event)?;

    if let Some(addr_key) = address_key_for(&se.event) {
        let addr_key_str = addr_key.to_redb_key();
        let still_points_here = addr_index
            .get(addr_key_str.as_str())
            .map_err(persist_err)?
            .map(|guard| guard.value())
            == Some(event_key);
        if still_points_here {
            addr_index
                .remove(addr_key_str.as_str())
                .map_err(persist_err)?;
        }
    }

    if let Some(ts) = se.event.tags.expiration().copied() {
        let exp_key = expiration_key(ts, &id);
        expiration_index
            .remove(exp_key.as_str())
            .map_err(persist_err)?;
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
fn process_kind5_deletions(
    canonical: &mut CanonicalWriteTables<'_>,
    addr_index: &mut redb::Table<'_, &str, EventKey>,
    tombstones: &mut redb::Table<'_, &str, &str>,
    addr_tombstones: &mut redb::Table<'_, &str, &str>,
    expiration_index: &mut redb::Table<'_, &str, EventKey>,
    indexes: &mut QueryIndexWriteTables<'_>,
    deleting: &Event,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    let mut deleted = Vec::new();
    let deleting_id_hex = deleting.id.to_hex();
    let deleting_author_hex = deleting.pubkey.to_hex();

    let target_ids: Vec<EventId> = deleting.tags.event_ids().copied().collect();
    for target_id in target_ids {
        if let Some(removed) = remove_row_in_txn(
            canonical,
            addr_index,
            expiration_index,
            indexes,
            target_id,
            |se| se.event.pubkey == deleting.pubkey,
        )? {
            deleted.push(removed);
        }
        // Claim recorded regardless of hold state right now -- a target
        // not yet held is checked, deferred, by `tombstone_refuses` at the
        // moment it actually arrives. NEVER collapse another author's
        // existing claim on this same id (composite key -- see
        // `TOMBSTONES`'s doc): each claiming author gets its own row.
        let key = id_tombstone_key(&target_id, &deleting.pubkey);
        tombstones
            .insert(key.as_str(), deleting_id_hex.as_str())
            .map_err(persist_err)?;
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

        let existing_ceiling = addr_tombstones
            .get(key_str.as_str())
            .map_err(persist_err)?
            .map(|guard| {
                let rec: AddrTombstoneRecord =
                    serde_json::from_str(guard.value()).expect("redb: decode addr tombstone");
                rec.ceiling
            });
        let new_ceiling = deleting.created_at.as_secs();
        if existing_ceiling.is_none_or(|ceiling| new_ceiling > ceiling) {
            let record = AddrTombstoneRecord {
                ceiling: new_ceiling,
                deleting_event_id: deleting_id_hex.clone(),
                deleting_author: deleting_author_hex.clone(),
            };
            let encoded = serde_json::to_string(&record).expect("redb: encode addr tombstone");
            addr_tombstones
                .insert(key_str.as_str(), encoded.as_str())
                .map_err(persist_err)?;
        }

        let current_key = addr_index
            .get(key_str.as_str())
            .map_err(persist_err)?
            .map(|guard| guard.value());
        if let Some(current_key) = current_key {
            let current = canonical
                .load_by_key(current_key)?
                .expect("addr_index must always point at a stored event");
            let current_id = current.event.id;
            if let Some(removed) = remove_row_in_txn(
                canonical,
                addr_index,
                expiration_index,
                indexes,
                current_id,
                |se| se.event.created_at <= deleting.created_at,
            )? {
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
#[allow(clippy::too_many_arguments)]
fn fan_out_signed_in_txn(
    canonical: &mut CanonicalWriteTables<'_>,
    addr_index: &mut redb::Table<'_, &str, EventKey>,
    tombstones: &mut redb::Table<'_, &str, &str>,
    addr_tombstones: &mut redb::Table<'_, &str, &str>,
    expiration_index: &mut redb::Table<'_, &str, EventKey>,
    indexes: &mut QueryIndexWriteTables<'_>,
    outbox_intents: &mut redb::Table<'_, &str, &str>,
    outbox_receipts: &mut redb::Table<'_, &str, &str>,
    outbox_displaced: &mut redb::Table<'_, &str, &[u8]>,
    outbox_kind5_claims: &mut redb::Table<'_, &str, &str>,
    outbox_suppress_by_id: &mut redb::Table<'_, &str, &str>,
    outbox_suppress_by_addr: &mut redb::Table<'_, &str, &str>,
    owners: &BTreeSet<IntentId>,
    canonical_event: &Event,
) -> Result<Vec<IntentId>, PersistenceError> {
    let mut transitioned = Vec::new();
    let is_deletion = canonical_event.kind == Kind::EventDeletion;
    let canonical_json = canonical_event.as_json();
    for owner_id in owners {
        let owner_key = intent_key(*owner_id);
        outbox_displaced
            .remove(owner_key.as_str())
            .map_err(persist_err)?;
        let owner_intent_json = outbox_intents
            .get(owner_key.as_str())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_string());
        if let Some(owner_intent_json) = owner_intent_json {
            let mut owner_record: OutboxIntentRecord =
                serde_json::from_str(&owner_intent_json).expect("redb: decode outbox intent");
            if owner_record.sig_state != IntentSigState::Signed {
                owner_record.sig_state = IntentSigState::Signed;
                owner_record.frozen_json = canonical_json.clone();
                let encoded_owner =
                    serde_json::to_string(&owner_record).expect("redb: encode outbox intent");
                outbox_intents
                    .insert(owner_key.as_str(), encoded_owner.as_str())
                    .map_err(persist_err)?;
                update_outbox_receipt(
                    outbox_receipts,
                    owner_record.receipt_id,
                    ReceiptState::Signed,
                )?;
                transitioned.push(*owner_id);
            }
        }
        if is_deletion {
            let claims_json = outbox_kind5_claims
                .remove(owner_key.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value().to_string());
            if let Some(claims_json) = claims_json {
                let claims: Vec<SuppressClaimRecord> =
                    serde_json::from_str(&claims_json).expect("redb: decode claims");
                for claim in claims {
                    match claim {
                        SuppressClaimRecord::Id(id_key) => {
                            remove_claimant_in_txn(outbox_suppress_by_id, &id_key, *owner_id)?;
                        }
                        SuppressClaimRecord::Addr { key: addr_key, .. } => {
                            remove_addr_claimant_in_txn(
                                outbox_suppress_by_addr,
                                &addr_key,
                                *owner_id,
                            )?;
                        }
                    }
                }
            }
        }
    }
    if is_deletion {
        process_kind5_deletions(
            canonical,
            addr_index,
            tombstones,
            addr_tombstones,
            expiration_index,
            indexes,
            canonical_event,
        )?;
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
/// — and the exact claims staged (for `OUTBOX_KIND5_CLAIMS`). Mirrors
/// `MemoryStore::process_kind5_deletions_provisional` exactly.
fn process_kind5_deletions_provisional_in_txn(
    canonical: &CanonicalWriteTables<'_>,
    addr_index: &redb::Table<'_, &str, EventKey>,
    outbox_suppress_by_id: &mut redb::Table<'_, &str, &str>,
    outbox_suppress_by_addr: &mut redb::Table<'_, &str, &str>,
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
            let current_key = addr_index
                .get(key_str.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value());
            if let Some(current_key) = current_key {
                let current_id = canonical
                    .load_by_key(current_key)?
                    .expect("addr_index must always point at a stored event")
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
        let visible = match canonical.load_by_id(id)? {
            None => false,
            Some((_key, se)) => {
                !is_suppressed_in_txn(outbox_suppress_by_id, outbox_suppress_by_addr, &se.event)?
            }
        };
        visible_before.insert(*id, visible);
    }

    let mut claims = Vec::new();
    for target_id in target_ids {
        let key = id_tombstone_key(&target_id, &deleting.pubkey);
        add_claimant_in_txn(outbox_suppress_by_id, &key, intent_id)?;
        claims.push(SuppressClaimRecord::Id(key));
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
        let key_str = key.to_redb_key();
        add_addr_claimant_in_txn(
            outbox_suppress_by_addr,
            &key_str,
            intent_id,
            deleting.created_at,
        )?;
        claims.push(SuppressClaimRecord::Addr {
            key: key_str,
            ceiling: deleting.created_at.as_secs(),
            deleting_author: deleting.pubkey.to_hex(),
        });
    }

    let mut hidden = Vec::new();
    for id in candidate_ids {
        if !visible_before.get(&id).copied().unwrap_or(false) {
            continue;
        }
        if let Some((_key, se)) = canonical.load_by_id(&id)? {
            if is_suppressed_in_txn(outbox_suppress_by_id, outbox_suppress_by_addr, &se.event)? {
                hidden.push(se);
            }
        }
    }

    Ok((hidden, claims))
}

/// Scan `OUTBOX_DISPLACED` for the row (if any) whose stashed event's id is
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
/// OWNING stash's `OUTBOX_DISPLACED` key, if found — at most one, by
/// construction (a `StoredEvent` is only ever the CURRENT displaced stash
/// of the one intent that most recently superseded it).
fn find_displaced_key_by_event_id_in_txn(
    outbox_displaced: &redb::Table<'_, &str, &[u8]>,
    frozen_id: EventId,
    intent_id: IntentId,
) -> Result<Option<String>, PersistenceError> {
    for entry in outbox_displaced.iter().map_err(persist_err)? {
        let (key, value) = entry.map_err(persist_err)?;
        let record = decode_stored_event_record(value.value());
        let owned_by_this_intent = record
            .local
            .as_ref()
            .is_some_and(|l| l.owners.contains(&intent_id));
        if !owned_by_this_intent {
            continue;
        }
        if record.event.id == frozen_id {
            return Ok(Some(key.value().to_string()));
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
fn find_any_displaced_key_by_event_id_in_txn(
    outbox_displaced: &redb::Table<'_, &str, &[u8]>,
    frozen_id: EventId,
) -> Result<Option<String>, PersistenceError> {
    for entry in outbox_displaced.iter().map_err(persist_err)? {
        let (key, value) = entry.map_err(persist_err)?;
        let record = decode_stored_event_record(value.value());
        if record.event.id == frozen_id {
            return Ok(Some(key.value().to_string()));
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
#[allow(clippy::too_many_arguments)]
fn reinsert_stashed_in_txn(
    canonical: &mut CanonicalWriteTables<'_>,
    addr_index: &mut redb::Table<'_, &str, EventKey>,
    tombstones: &mut redb::Table<'_, &str, &str>,
    addr_tombstones: &mut redb::Table<'_, &str, &str>,
    expiration_index: &mut redb::Table<'_, &str, EventKey>,
    indexes: &mut QueryIndexWriteTables<'_>,
    outbox_intents: &mut redb::Table<'_, &str, &str>,
    outbox_receipts: &mut redb::Table<'_, &str, &str>,
    outbox_displaced: &mut redb::Table<'_, &str, &[u8]>,
    outbox_kind5_claims: &mut redb::Table<'_, &str, &str>,
    outbox_suppress_by_id: &mut redb::Table<'_, &str, &str>,
    outbox_suppress_by_addr: &mut redb::Table<'_, &str, &str>,
    se: StoredEvent,
) -> Result<Option<StoredEvent>, PersistenceError> {
    if let Some((event_key, existing)) = canonical.load_by_id(&se.event.id)? {
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
                canonical.replace_event(event_key, &event)?;
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
        canonical.replace_provenance(event_key, &provenance)?;
        if let Some(owners) = &fan_out_owners {
            fan_out_signed_in_txn(
                canonical,
                addr_index,
                tombstones,
                addr_tombstones,
                expiration_index,
                indexes,
                outbox_intents,
                outbox_receipts,
                outbox_displaced,
                outbox_kind5_claims,
                outbox_suppress_by_id,
                outbox_suppress_by_addr,
                owners,
                &event,
            )?;
        }
        return Ok(Some(StoredEvent { event, provenance }));
    }
    if tombstone_refuses(tombstones, addr_tombstones, &se.event)? {
        return Ok(None);
    }

    let result = match address_key_for(&se.event) {
        None => {
            let event_key = canonical.insert_new(&se.event, &se.provenance)?;
            insert_query_index_rows(canonical, indexes, &se.event, event_key)
                .map_err(persist_err)?;
            if let Some(ts) = se.event.tags.expiration().copied() {
                let exp_key = expiration_key(ts, &se.event.id);
                expiration_index
                    .insert(exp_key.as_str(), event_key)
                    .map_err(persist_err)?;
            }
            Some(se)
        }
        Some(addr_key) => {
            let addr_key_str = addr_key.to_redb_key();
            let current_key = addr_index
                .get(addr_key_str.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value());

            match current_key {
                None => {
                    let event_key = canonical.insert_new(&se.event, &se.provenance)?;
                    addr_index
                        .insert(addr_key_str.as_str(), event_key)
                        .map_err(persist_err)?;
                    insert_query_index_rows(canonical, indexes, &se.event, event_key)
                        .map_err(persist_err)?;
                    if let Some(ts) = se.event.tags.expiration().copied() {
                        let exp_key = expiration_key(ts, &se.event.id);
                        expiration_index
                            .insert(exp_key.as_str(), event_key)
                            .map_err(persist_err)?;
                    }
                    Some(se)
                }
                Some(current_key) => {
                    let current_event = canonical
                        .load_by_key(current_key)?
                        .expect("addr_index must always point at a stored event")
                        .event;

                    if candidate_wins(&se.event, &current_event) {
                        let current_id = current_event.id;
                        remove_row_in_txn(
                            canonical,
                            addr_index,
                            expiration_index,
                            indexes,
                            current_id,
                            |_| true,
                        )?
                        .expect("addr_index must always point at a stored event");

                        let event_key = canonical.insert_new(&se.event, &se.provenance)?;
                        addr_index
                            .insert(addr_key_str.as_str(), event_key)
                            .map_err(persist_err)?;
                        insert_query_index_rows(canonical, indexes, &se.event, event_key)
                            .map_err(persist_err)?;
                        if let Some(ts) = se.event.tags.expiration().copied() {
                            let exp_key = expiration_key(ts, &se.event.id);
                            expiration_index
                                .insert(exp_key.as_str(), event_key)
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

/// The `coverage` table's JSON value: the window-erased shape the row was
/// recorded against (needed so `gc` can test event-shape matches — see
/// `ShapeRecord`'s doc comment) plus the proven interval, stored as raw
/// `u64` seconds (round-tripped through `Timestamp::from`/`as_secs`).
#[derive(Debug, Serialize, Deserialize)]
struct CoverageRowRecord {
    shape: ShapeRecord,
    from: u64,
    through: u64,
}

/// A persistent, `redb`-backed `EventStore`. Single file, MVCC, ACID; the
/// same insert door and coverage/GC contract as [`crate::MemoryStore`], the
/// oracle it is diffed against in `nmp-store/tests/store_contract.rs`.
#[cfg(test)]
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedbCrashPoint {
    AcceptAfterEventBeforeJournal = 1,
    AcceptBeforeCommit,
    PromoteBeforeCommit,
    CompensateBeforeCommit,
    RouteRevisionBeforeCommit,
    StartAttemptBeforeCommit,
    FinishAttemptBeforeCommit,
    LaneBootstrapBeforeCommit,
    LaneTransitionBeforeCommit,
    LaneStartBeforeCommit,
    LaneHandoffBeforeCommit,
    LaneCloseBeforeCommit,
    ObservationBeforeCommit,
}

pub struct RedbStore {
    db: Database,
    #[cfg(test)]
    crash_point: AtomicU8,
    /// Owned rows materialized after borrowed filtering.
    #[cfg(test)]
    examined_rows: AtomicU64,
    /// Ordered index entries consumed, including one prefetched head per OR
    /// range needed to establish global ordering.
    #[cfg(test)]
    query_index_rows: AtomicU64,
    /// Canonical binary event values dereferenced for borrowed post-filtering.
    #[cfg(test)]
    query_event_values: AtomicU64,
    /// Number of rows yielded by bounded attempt-table ranges. Tests reset
    /// this to prove work follows the target lane count, not total history.
    #[cfg(test)]
    attempt_range_rows: AtomicU64,
    /// Equivalent instrumentation for resolved-route revision ranges.
    #[cfg(test)]
    route_revision_range_rows: AtomicU64,
}

impl RedbStore {
    fn persist_lane_state(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        state: LaneState,
    ) -> Result<RecoveredLane, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let lane = {
            let mut lanes = write_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
            let mut deadlines = write_txn
                .open_table(OUTBOX_DEADLINES)
                .map_err(persist_err)?;
            let mut deadlines_by_intent = write_txn
                .open_table(OUTBOX_DEADLINES_BY_INTENT)
                .map_err(persist_err)?;
            replace_lane_in_txn(
                &mut lanes,
                &mut deadlines,
                &mut deadlines_by_intent,
                key,
                expected_revision,
                state,
            )?
        };
        #[cfg(test)]
        self.crash_if(RedbCrashPoint::LaneTransitionBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(lane)
    }

    /// Open (creating if absent) a `redb` database file at `path`, ensuring
    /// all tables exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        // Schema v4 deliberately carries no event-row migration. Refuse any
        // older NMP event epoch before creating a single v4 table: otherwise
        // canonical events would appear empty while unversioned durable
        // outbox/coverage/tombstone facts from the old epoch remained live.
        // A caller opting into this breaking release must recreate the whole
        // database, never unknowingly run a split-brain mixture.
        {
            let read_txn = db.begin_read()?;
            let legacy_epoch = read_txn
                .list_tables()?
                .any(|table| LEGACY_EVENT_TABLES.contains(&table.name()));
            if legacy_epoch {
                return Err(redb::Error::UpgradeRequired(4));
            }
        }
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(EVENTS)?;
            write_txn.open_table(EVENT_IDS)?;
            write_txn.open_table(EVENT_LOCAL)?;
            write_txn.open_table(EVENT_STORE_META)?;
            write_txn.open_table(EVENT_OBSERVATIONS)?;
            write_txn.open_table(RELAYS)?;
            write_txn.open_table(RELAY_KEYS)?;
            write_txn.open_table(RELAY_REFS)?;
            write_txn.open_table(RELAY_META)?;
            write_txn.open_table(ADDR_INDEX)?;
            write_txn.open_table(COVERAGE)?;
            write_txn.open_table(TOMBSTONES)?;
            write_txn.open_table(ADDR_TOMBSTONES)?;
            write_txn.open_table(EXPIRATION_INDEX)?;
            let by_created_at = write_txn.open_table(BY_CREATED_AT)?;
            let by_author = write_txn.open_table(BY_AUTHOR)?;
            let by_kind = write_txn.open_table(BY_KIND)?;
            let by_author_kind = write_txn.open_table(BY_AUTHOR_KIND)?;
            let by_tag = write_txn.open_table(BY_TAG)?;
            let mut cardinality = write_txn.open_table(INDEX_CARDINALITY)?;
            let mut cardinality_meta = write_txn.open_table(INDEX_CARDINALITY_META)?;
            let cardinality_version = cardinality_meta
                .get(INDEX_CARDINALITY_VERSION_KEY)?
                .map(|guard| guard.value());
            if cardinality_version != Some(INDEX_CARDINALITY_VERSION) {
                rebuild_index_cardinality(
                    &by_created_at,
                    &by_author,
                    &by_kind,
                    &by_author_kind,
                    &by_tag,
                    &mut cardinality,
                )?;
                cardinality_meta
                    .insert(INDEX_CARDINALITY_VERSION_KEY, INDEX_CARDINALITY_VERSION)?;
            }
            write_txn.open_table(OUTBOX_INTENTS)?;
            write_txn.open_table(OUTBOX_DISPLACED)?;
            write_txn.open_table(OUTBOX_ATTEMPTS)?;
            write_txn.open_table(OUTBOX_ROUTE_REVISIONS)?;
            write_txn.open_table(OUTBOX_LANES)?;
            write_txn.open_table(OUTBOX_DEADLINES)?;
            write_txn.open_table(OUTBOX_DEADLINES_BY_INTENT)?;
            write_txn.open_table(OUTBOX_ATTEMPT_DETAILS)?;
            write_txn.open_table(OUTBOX_META)?;
            write_txn.open_table(OUTBOX_KIND5_CLAIMS)?;
            write_txn.open_table(OUTBOX_SUPPRESS_BY_ID)?;
            write_txn.open_table(OUTBOX_SUPPRESS_BY_ADDR)?;
            let mut outbox_receipts = write_txn.open_table(OUTBOX_RECEIPTS)?;
            // Boot-time reconciliation (VISION-ratified receipt contract,
            // team-lead correction): any `Ephemeral` receipt-only record
            // still `Accepted` at this point can only mean the process
            // died before any further transition was ever recorded — see
            // `ReceiptState::Abandoned`'s doc. A no-op on a fresh store
            // (the table is empty) or a store with no ephemeral receipts.
            reconcile_ephemeral_receipts_in_txn(&mut outbox_receipts);
        }
        write_txn.commit()?;
        Ok(Self {
            db,
            #[cfg(test)]
            crash_point: AtomicU8::new(0),
            #[cfg(test)]
            examined_rows: AtomicU64::new(0),
            #[cfg(test)]
            query_index_rows: AtomicU64::new(0),
            #[cfg(test)]
            query_event_values: AtomicU64::new(0),
            #[cfg(test)]
            attempt_range_rows: AtomicU64::new(0),
            #[cfg(test)]
            route_revision_range_rows: AtomicU64::new(0),
        })
    }

    #[cfg(test)]
    fn open_with_crash_point(
        path: impl AsRef<Path>,
        crash_point: RedbCrashPoint,
    ) -> Result<Self, redb::Error> {
        let store = Self::open(path)?;
        store
            .crash_point
            .store(crash_point as u8, Ordering::Relaxed);
        Ok(store)
    }

    #[cfg(test)]
    fn crash_if(&self, point: RedbCrashPoint) {
        if self
            .crash_point
            .compare_exchange(point as u8, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            std::process::abort();
        }
    }

    #[cfg(test)]
    fn reset_outbox_range_rows(&self) {
        self.attempt_range_rows.store(0, Ordering::Relaxed);
        self.route_revision_range_rows.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn outbox_range_rows(&self) -> (u64, u64) {
        (
            self.attempt_range_rows.load(Ordering::Relaxed),
            self.route_revision_range_rows.load(Ordering::Relaxed),
        )
    }

    /// Current value of [`Self::examined_rows`] — the `query`-indexing
    /// falsifier's read side.
    #[cfg(test)]
    fn examined_rows(&self) -> u64 {
        self.examined_rows.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn reset_query_work(&self) {
        self.examined_rows.store(0, Ordering::Relaxed);
        self.query_index_rows.store(0, Ordering::Relaxed);
        self.query_event_values.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn query_work(&self) -> (u64, u64, u64) {
        (
            self.query_index_rows.load(Ordering::Relaxed),
            self.query_event_values.load(Ordering::Relaxed),
            self.examined_rows.load(Ordering::Relaxed),
        )
    }

    /// The current schema-version row-key PREFIX (#106, Fable's C
    /// refinement): distinguishes a v2 (context-aware `ContextualAtom`)
    /// row from a legacy v1 (bare `ConcreteFilter`, pre-#106) row by a
    /// cheap string check, independent of `CoverageKey`'s own hash-level
    /// version tag (`nmp-store::coverage::COVERAGE_KEY_VERSION`) -- `gc`'s
    /// legacy-purge pass greps for the ABSENCE of this exact prefix.
    const COVERAGE_ROW_KEY_PREFIX: &'static str = "d2:";

    fn coverage_row_key(key: CoverageKey, relay: &RelayUrl) -> String {
        use std::fmt::Write as _;

        // Full 32-byte BLAKE3 digest, hex-encoded -- NOT truncated to 64
        // bits (see `CoverageKey::as_bytes`'s doc): this is the durable
        // redb watermark key, so the full collision-resistant width must
        // survive into the key, not just exist in memory.
        let mut hex = String::with_capacity(64);
        for byte in key.as_bytes() {
            let _ = write!(hex, "{byte:02x}");
        }
        format!("{}{hex}:{}", Self::COVERAGE_ROW_KEY_PREFIX, relay.as_str())
    }

    /// Materialize one portable `EVENTS` value into a [`StoredEvent`] —
    /// `query`'s one decode point, so [`Self::examined_rows`] (test-only)
    /// counts every row `query` actually pays the owned-event cost for,
    /// regardless of which of `query`'s three paths (id/indexed/full-scan)
    /// reached it.
    fn read_provenance(
        &self,
        event_key: EventKey,
        local_bytes: Option<&[u8]>,
        observations: &redb::ReadOnlyTable<&'static [u8; 12], u64>,
        relays: &redb::ReadOnlyTable<RelayKey, &'static str>,
        relay_cache: &mut HashMap<RelayKey, RelayUrl>,
    ) -> Result<Provenance, PersistenceError> {
        let local = local_bytes.map(|bytes| {
            binary_event::decode_local(bytes).expect("redb: decode canonical local state")
        });
        let (lower, upper) = observation_range(event_key);
        let mut seen = BTreeMap::new();
        for entry in observations
            .range::<&[u8; 12]>(&lower..=&upper)
            .map_err(persist_err)?
        {
            let (encoded_key, at) = entry.map_err(persist_err)?;
            let relay_key = observation_relay_key(encoded_key.value());
            let relay = if let Some(relay) = relay_cache.get(&relay_key) {
                relay.clone()
            } else {
                let encoded_relay =
                    relays.get(relay_key).map_err(persist_err)?.ok_or_else(|| {
                        PersistenceError(format!("observation points at missing relay {relay_key}"))
                    })?;
                let relay = RelayUrl::parse(encoded_relay.value())
                    .expect("redb: interned relay URL remains canonical");
                relay_cache.insert(relay_key, relay.clone());
                relay
            };
            assert!(seen.insert(relay, Timestamp::from(at.value())).is_none());
        }
        Ok(Provenance { seen, local })
    }

    fn decode_row(
        &self,
        event_key: EventKey,
        view: StoredEventView<'_>,
        local_bytes: Option<&[u8]>,
        observations: &redb::ReadOnlyTable<&'static [u8; 12], u64>,
        relays: &redb::ReadOnlyTable<RelayKey, &'static str>,
        relay_cache: &mut HashMap<RelayKey, RelayUrl>,
    ) -> Result<StoredEvent, PersistenceError> {
        #[cfg(test)]
        self.examined_rows.fetch_add(1, Ordering::Relaxed);
        Ok(StoredEvent {
            event: view
                .materialize_event()
                .expect("redb: materialize validated portable event"),
            provenance: self.read_provenance(
                event_key,
                local_bytes,
                observations,
                relays,
                relay_cache,
            )?,
        })
    }

    /// Reverse-merge one or more ranges from the planner's chosen index.
    /// Each cursor asks redb for exactly its next key; once `limit` visible
    /// rows have survived the borrowed binary post-filter, no older key or
    /// event value is touched.
    fn query_ordered(
        &self,
        read_txn: &redb::ReadTransaction,
        plan: &OrderedPlan,
        filter: &Filter,
        limit: Option<usize>,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        let events = read_txn.open_table(EVENTS).map_err(persist_err)?;
        let local = read_txn.open_table(EVENT_LOCAL).map_err(persist_err)?;
        let observations = read_txn
            .open_table(EVENT_OBSERVATIONS)
            .map_err(persist_err)?;
        let relays = read_txn.open_table(RELAYS).map_err(persist_err)?;
        let index = read_txn
            .open_table(plan.index.table())
            .map_err(persist_err)?;
        let outbox_suppress_by_id = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ID)
            .map_err(persist_err)?;
        let outbox_suppress_by_addr = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ADDR)
            .map_err(persist_err)?;
        let since = filter.since.map(|ts| ts.as_secs()).unwrap_or(0);
        let until = filter.until.map(|ts| ts.as_secs()).unwrap_or(u64::MAX);
        let mut relay_cache = HashMap::new();
        let mut materialize_if_visible =
            |event_key: EventKey| -> Result<Option<StoredEvent>, PersistenceError> {
                #[cfg(test)]
                self.query_event_values.fetch_add(1, Ordering::Relaxed);
                let Some(value) = events.get(event_key).map_err(persist_err)? else {
                    return Err(PersistenceError(format!(
                        "ordered index points at missing canonical event {event_key}"
                    )));
                };
                let view = StoredEventView::from_trusted(value.value())
                    .expect("redb: decode portable stored event view");
                if !view.matches_filter_after_index(filter, plan.index.matched()) {
                    return Ok(None);
                }
                let local_value = local.get(event_key).map_err(persist_err)?;
                let stored = self.decode_row(
                    event_key,
                    view,
                    local_value.as_ref().map(|value| value.value()),
                    &observations,
                    &relays,
                    &mut relay_cache,
                )?;
                if is_suppressed_in_txn(
                    &outbox_suppress_by_id,
                    &outbox_suppress_by_addr,
                    &stored.event,
                )? {
                    return Ok(None);
                }
                Ok(Some(stored))
            };

        // The dominant room/author/kind case is one contiguous range. Keep
        // redb's iterator alive and walk it once; the cursor-based k-way
        // merge below is reserved for genuine OR sets.
        if let [prefix] = plan.prefixes.as_slice() {
            let (lower, upper) = ordered_range(prefix, since, until);
            let mut out = limit.map_or_else(Vec::new, Vec::with_capacity);
            for entry in index
                .range(lower.as_slice()..=upper.as_slice())
                .map_err(persist_err)?
                .rev()
            {
                let (_key, value) = entry.map_err(persist_err)?;
                #[cfg(test)]
                self.query_index_rows.fetch_add(1, Ordering::Relaxed);
                if let Some(stored) = materialize_if_visible(value.value())? {
                    out.push(stored);
                    if limit.is_some_and(|limit| out.len() == limit) {
                        break;
                    }
                }
            }
            return Ok(out);
        }

        let mut cursors: Vec<_> = plan
            .prefixes
            .iter()
            .map(|prefix| OrderedCursor::new(&index, prefix, since, until))
            .collect::<Result<_, _>>()?;
        let mut heap = BinaryHeap::new();
        for (cursor_index, cursor) in cursors.iter_mut().enumerate() {
            if let Some(head) = cursor.next_head(cursor_index)? {
                #[cfg(test)]
                self.query_index_rows.fetch_add(1, Ordering::Relaxed);
                heap.push(head);
            }
        }

        let mut out = limit.map_or_else(Vec::new, Vec::with_capacity);
        let mut last_event_key = None;
        while let Some(head) = heap.pop() {
            let is_new = last_event_key.replace(head.event_key) != Some(head.event_key);
            if is_new {
                if let Some(stored) = materialize_if_visible(head.event_key)? {
                    out.push(stored);
                    if limit.is_some_and(|limit| out.len() == limit) {
                        // Do not even touch the next ordered index key after
                        // the visible limit is satisfied.
                        break;
                    }
                }
            }
            if let Some(next) = cursors[head.cursor].next_head(head.cursor)? {
                #[cfg(test)]
                self.query_index_rows.fetch_add(1, Ordering::Relaxed);
                heap.push(next);
            }
        }
        Ok(out)
    }
}

struct InsertWriteTables<'txn> {
    canonical: CanonicalWriteTables<'txn>,
    addr_index: redb::Table<'txn, &'static str, EventKey>,
    tombstones: redb::Table<'txn, &'static str, &'static str>,
    addr_tombstones: redb::Table<'txn, &'static str, &'static str>,
    expiration_index: redb::Table<'txn, &'static str, EventKey>,
    indexes: QueryIndexWriteTables<'txn>,
    outbox_intents: redb::Table<'txn, &'static str, &'static str>,
    outbox_receipts: redb::Table<'txn, &'static str, &'static str>,
    outbox_displaced: redb::Table<'txn, &'static str, &'static [u8]>,
    outbox_kind5_claims: redb::Table<'txn, &'static str, &'static str>,
    outbox_suppress_by_id: redb::Table<'txn, &'static str, &'static str>,
    outbox_suppress_by_addr: redb::Table<'txn, &'static str, &'static str>,
}

impl<'txn> InsertWriteTables<'txn> {
    fn open(write_txn: &'txn redb::WriteTransaction) -> Result<Self, PersistenceError> {
        Ok(Self {
            canonical: CanonicalWriteTables::open(write_txn)?,
            addr_index: write_txn.open_table(ADDR_INDEX).map_err(persist_err)?,
            tombstones: write_txn.open_table(TOMBSTONES).map_err(persist_err)?,
            addr_tombstones: write_txn.open_table(ADDR_TOMBSTONES).map_err(persist_err)?,
            expiration_index: write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?,
            indexes: QueryIndexWriteTables::open(write_txn)?,
            outbox_intents: write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?,
            outbox_receipts: write_txn.open_table(OUTBOX_RECEIPTS).map_err(persist_err)?,
            outbox_displaced: write_txn
                .open_table(OUTBOX_DISPLACED)
                .map_err(persist_err)?,
            outbox_kind5_claims: write_txn
                .open_table(OUTBOX_KIND5_CLAIMS)
                .map_err(persist_err)?,
            outbox_suppress_by_id: write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ID)
                .map_err(persist_err)?,
            outbox_suppress_by_addr: write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ADDR)
                .map_err(persist_err)?,
        })
    }
}

#[allow(clippy::too_many_lines)]
fn insert_with_tables(
    tables: &mut InsertWriteTables<'_>,
    event: Event,
    from: RelayObserved,
) -> Result<InsertOutcome, PersistenceError> {
    // Refused at the door FIRST: an already-expired event is never
    // stored, so it never touches dedup or supersession at all.
    if event.is_expired_at(&from.at) {
        return Ok(InsertOutcome::Refused(RefuseReason::AlreadyExpired));
    }

    let InsertWriteTables {
        canonical,
        addr_index,
        tombstones,
        addr_tombstones,
        expiration_index,
        indexes,
        outbox_intents,
        outbox_receipts,
        outbox_displaced,
        outbox_kind5_claims,
        outbox_suppress_by_id,
        outbox_suppress_by_addr,
    } = tables;
    let outcome = {
        if let Some(event_key) = canonical.key_for_id(&event.id)? {
            // Dedup-by-id FIRST: merge provenance, no index churn. Goes
            // through `Provenance::merge_observation` (not a re-derived
            // copy) so the persisted backend can never diverge from
            // `MemoryStore`'s merge semantics.
            let mut local = canonical.load_local(event_key)?;
            let grew = canonical.merge_observation(event_key, &from.relay, from.at)?;
            // Architecture review requirement (issue #2 P0 correction,
            // codex-nova ruling): a relay delivering the real signed
            // event for a still-Pending local draft is functionally the
            // SAME signature-adoption/fan-out invariant `promote_signed`
            // performs explicitly — adopt it, mark every co-owner
            // `Signed`, and fan out, rather than silently keeping our
            // own sentinel forever (`event` here is, by this door's own
            // contract, always a genuine relay delivery, never our OWN
            // sentinel, so its signature is always safe to adopt).
            let needs_adoption = local
                .as_ref()
                .is_some_and(|l| l.sig_state == SigState::Pending);
            let mut fan_out_owners: Option<BTreeSet<IntentId>> = None;
            if needs_adoption {
                let mut adopted = local
                    .clone()
                    .expect("just checked this row carries local provenance");
                adopted.sig_state = SigState::Signed;
                fan_out_owners = Some(adopted.owners.clone());
                local = Some(adopted);
            }
            // `merge_observation` never touches `local` (a relay echo
            // of an already-local row keeps its local provenance,
            // retraction doc §4.1) — `provenance.local` is otherwise
            // unchanged, written straight back.
            if fan_out_owners.is_some() {
                canonical.replace_event(event_key, &event)?;
                canonical.replace_local(event_key, local)?;
            }
            let satisfied_intents = if let Some(owners) = &fan_out_owners {
                fan_out_signed_in_txn(
                    canonical,
                    addr_index,
                    tombstones,
                    addr_tombstones,
                    expiration_index,
                    indexes,
                    outbox_intents,
                    outbox_receipts,
                    outbox_displaced,
                    outbox_kind5_claims,
                    outbox_suppress_by_id,
                    outbox_suppress_by_addr,
                    owners,
                    &event,
                )?
            } else {
                Vec::new()
            };
            InsertOutcome::Duplicate {
                provenance_grew: grew,
                satisfied_intents,
            }
        } else if tombstone_refuses(tombstones, addr_tombstones, &event)? {
            // Tombstone check, AFTER dedup-by-id, BEFORE storage
            // (retraction-and-negative-deltas.md §2).
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        } else {
            let is_deletion = event.kind == Kind::EventDeletion;
            let provenance = Provenance {
                seen: BTreeMap::from([(from.relay.clone(), from.at)]),
                local: None,
            };

            let outcome = match address_key_for(&event) {
                None => {
                    let event_key = canonical.insert_new(&event, &provenance)?;
                    insert_query_index_rows(canonical, indexes, &event, event_key)
                        .map_err(persist_err)?;
                    if let Some(ts) = event.tags.expiration().copied() {
                        let exp_key = expiration_key(ts, &event.id);
                        expiration_index
                            .insert(exp_key.as_str(), event_key)
                            .map_err(persist_err)?;
                    }
                    InsertOutcome::Inserted
                }
                Some(addr_key) => {
                    let addr_key_str = addr_key.to_redb_key();
                    let current_key = addr_index
                        .get(addr_key_str.as_str())
                        .map_err(persist_err)?
                        .map(|guard| guard.value());

                    match current_key {
                        None => {
                            let event_key = canonical.insert_new(&event, &provenance)?;
                            addr_index
                                .insert(addr_key_str.as_str(), event_key)
                                .map_err(persist_err)?;
                            insert_query_index_rows(canonical, indexes, &event, event_key)
                                .map_err(persist_err)?;
                            if let Some(ts) = event.tags.expiration().copied() {
                                let exp_key = expiration_key(ts, &event.id);
                                expiration_index
                                    .insert(exp_key.as_str(), event_key)
                                    .map_err(persist_err)?;
                            }
                            InsertOutcome::Inserted
                        }
                        Some(current_key) => {
                            let replaced = canonical
                                .load_by_key(current_key)?
                                .expect("addr_index must always point at a stored event");
                            let current_event = &replaced.event;

                            if candidate_wins(&event, current_event) {
                                remove_row_in_txn(
                                    canonical,
                                    addr_index,
                                    expiration_index,
                                    indexes,
                                    current_event.id,
                                    |_| true,
                                )?
                                .expect("addr_index must always point at a stored event");
                                let event_key = canonical.insert_new(&event, &provenance)?;
                                addr_index
                                    .insert(addr_key_str.as_str(), event_key)
                                    .map_err(persist_err)?;
                                insert_query_index_rows(canonical, indexes, &event, event_key)
                                    .map_err(persist_err)?;
                                if let Some(ts) = event.tags.expiration().copied() {
                                    let exp_key = expiration_key(ts, &event.id);
                                    expiration_index
                                        .insert(exp_key.as_str(), event_key)
                                        .map_err(persist_err)?;
                                }
                                InsertOutcome::Superseded {
                                    replaced: Box::new(replaced),
                                }
                            } else {
                                InsertOutcome::Stale
                            }
                        }
                    }
                }
            };

            // kind:5 has no replaceable/addressable address (M1's set
            // excludes it), so `outcome` above is always `Inserted`
            // here, by construction -- process its deletions now that
            // the event itself is durably stored (re-servable, §2).
            if is_deletion {
                if let InsertOutcome::Inserted = outcome {
                    let deleted = process_kind5_deletions(
                        canonical,
                        addr_index,
                        tombstones,
                        addr_tombstones,
                        expiration_index,
                        indexes,
                        &event,
                    )?;
                    InsertOutcome::Kind5Processed { deleted }
                } else {
                    outcome
                }
            } else {
                outcome
            }
        }
    };
    Ok(outcome)
}

impl EventStore for RedbStore {
    fn insert(
        &mut self,
        event: Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let outcome = {
            let mut tables = InsertWriteTables::open(&write_txn)?;
            let outcome = insert_with_tables(&mut tables, event, from)?;
            tables.canonical.flush_counts()?;
            outcome
        };
        #[cfg(test)]
        self.crash_if(RedbCrashPoint::ObservationBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(outcome)
    }

    fn insert_batch(
        &mut self,
        events: Vec<(Event, RelayObserved)>,
    ) -> Result<Vec<InsertOutcome>, PersistenceError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let mut outcomes = Vec::with_capacity(events.len());
        {
            let mut tables = InsertWriteTables::open(&write_txn)?;
            for (event, from) in events {
                outcomes.push(insert_with_tables(&mut tables, event, from)?);
            }
            tables.canonical.flush_counts()?;
        }
        #[cfg(test)]
        self.crash_if(RedbCrashPoint::ObservationBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(outcomes)
    }

    fn query(&self, filter: &Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        if filter
            .since
            .zip(filter.until)
            .is_some_and(|(since, until)| since > until)
            || filter.generic_tags.values().any(BTreeSet::is_empty)
        {
            return Ok(Vec::new());
        }
        let read_txn = self.db.begin_read().map_err(persist_err)?;
        // Fast path: exact ids resolve through the raw-id -> surrogate-key
        // table, bounded by `|ids|` regardless of table size (issue #17).
        if let Some(ids) = filter.ids.as_ref().filter(|ids| !ids.is_empty()) {
            let events = read_txn.open_table(EVENTS).map_err(persist_err)?;
            let event_ids = read_txn.open_table(EVENT_IDS).map_err(persist_err)?;
            let local = read_txn.open_table(EVENT_LOCAL).map_err(persist_err)?;
            let observations = read_txn
                .open_table(EVENT_OBSERVATIONS)
                .map_err(persist_err)?;
            let relays = read_txn.open_table(RELAYS).map_err(persist_err)?;
            let mut relay_cache = HashMap::new();
            let outbox_suppress_by_id = read_txn
                .open_table(OUTBOX_SUPPRESS_BY_ID)
                .map_err(persist_err)?;
            let outbox_suppress_by_addr = read_txn
                .open_table(OUTBOX_SUPPRESS_BY_ADDR)
                .map_err(persist_err)?;
            let mut out = Vec::new();
            for id in ids {
                let Some(event_key) = event_ids
                    .get(id.as_bytes().as_slice())
                    .map_err(persist_err)?
                    .map(|guard| guard.value())
                else {
                    continue;
                };
                let value = events
                    .get(event_key)
                    .map_err(persist_err)?
                    .expect("event_ids must always point at a stored event");
                let view = StoredEventView::from_trusted(value.value())
                    .expect("redb: decode portable stored event view");
                if !view.matches_filter(filter) {
                    continue;
                }
                let local_value = local.get(event_key).map_err(persist_err)?;
                let se = self.decode_row(
                    event_key,
                    view,
                    local_value.as_ref().map(|value| value.value()),
                    &observations,
                    &relays,
                    &mut relay_cache,
                )?;
                if !is_suppressed_in_txn(
                    &outbox_suppress_by_id,
                    &outbox_suppress_by_addr,
                    &se.event,
                )? {
                    out.push(se);
                }
            }
            return Ok(out);
        }

        let plan = plan_ordered_query(&read_txn, filter)?;
        self.query_ordered(&read_txn, &plan, filter, None)
    }

    fn query_newest(
        &self,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        if limit == 0
            || filter
                .since
                .zip(filter.until)
                .is_some_and(|(since, until)| since > until)
            || filter.generic_tags.values().any(BTreeSet::is_empty)
        {
            return Ok(Vec::new());
        }
        // Exact ids are already the narrowest possible lookup. They do not
        // form a time-ordered range, so preserve correctness by sorting this
        // caller-bounded set only; no unrelated row is touched.
        if filter.ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
            let mut rows = self.query(filter)?;
            rows.sort_by(|a, b| {
                b.event
                    .created_at
                    .cmp(&a.event.created_at)
                    .then_with(|| a.event.id.cmp(&b.event.id))
            });
            rows.truncate(limit);
            return Ok(rows);
        }

        let read_txn = self.db.begin_read().map_err(persist_err)?;

        let plan = plan_ordered_query(&read_txn, filter)?;
        self.query_ordered(&read_txn, &plan, filter, Some(limit))
    }

    fn remove(
        &mut self,
        id: EventId,
        _reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let removed = {
            let mut canonical = CanonicalWriteTables::open(&write_txn)?;
            let mut addr_index = write_txn.open_table(ADDR_INDEX).map_err(persist_err)?;
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?;
            let mut indexes = QueryIndexWriteTables::open(&write_txn)?;
            let removed = remove_row_in_txn(
                &mut canonical,
                &mut addr_index,
                &mut expiration_index,
                &mut indexes,
                id,
                |_| true,
            )?;
            canonical.flush_counts()?;
            removed
        };
        write_txn.commit().map_err(persist_err)?;
        Ok(removed)
    }

    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let removed = {
            let mut canonical = CanonicalWriteTables::open(&write_txn)?;
            let mut addr_index = write_txn.open_table(ADDR_INDEX).map_err(persist_err)?;
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?;
            let mut indexes = QueryIndexWriteTables::open(&write_txn)?;

            let upper = expiration_key_upper_bound(now);
            // Collect due ids first, propagating any redb read error out of
            // the iterator (a plain `for` accumulate rather than a `.map()`
            // closure so `?` reaches this fn, not the closure).
            let mut due_keys: Vec<EventKey> = Vec::new();
            for entry in expiration_index
                .range::<&str>(..=upper.as_str())
                .map_err(persist_err)?
            {
                let (_key, value) = entry.map_err(persist_err)?;
                due_keys.push(value.value());
            }

            let mut removed = Vec::new();
            for event_key in due_keys {
                let Some(stored) = canonical.load_by_key(event_key)? else {
                    continue;
                };
                if let Some(row) = remove_row_in_txn(
                    &mut canonical,
                    &mut addr_index,
                    &mut expiration_index,
                    &mut indexes,
                    stored.event.id,
                    |_| true,
                )? {
                    removed.push(row);
                }
            }
            canonical.flush_counts()?;
            removed
        };
        write_txn.commit().map_err(persist_err)?;
        Ok(removed)
    }

    fn next_expiration(&self) -> Option<Timestamp> {
        let read_txn = self.db.begin_read().expect("redb: begin_read");
        let expiration_index = read_txn
            .open_table(EXPIRATION_INDEX)
            .expect("redb: open expiration_index");
        let (key, _value) = expiration_index
            .first()
            .expect("redb: first expiration_index")?;
        let ts_str = key
            .value()
            .split(':')
            .next()
            .expect("expiration_index key always has a ts prefix");
        Some(Timestamp::from(
            ts_str
                .parse::<u64>()
                .expect("redb: parse expiration_index ts"),
        ))
    }

    fn record_coverage(
        &mut self,
        atom: &ContextualAtom,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) -> Result<(), PersistenceError> {
        let key = compute_coverage_key(atom);
        let shape = window_erase(&atom.filter);
        let row_key = Self::coverage_row_key(key, relay);

        let write_txn = self.db.begin_write().map_err(persist_err)?;
        {
            let mut coverage = write_txn.open_table(COVERAGE).map_err(persist_err)?;
            let existing = coverage
                .get(row_key.as_str())
                .map_err(persist_err)?
                .map(|guard| decode_interval(guard.value()));

            let merged = merge_interval(existing, proven);
            let record = CoverageRowRecord {
                shape: ShapeRecord::from(&shape),
                from: merged.from.as_secs(),
                through: merged.through.as_secs(),
            };
            let encoded = serde_json::to_string(&record).expect("redb: encode coverage row");
            coverage
                .insert(row_key.as_str(), encoded.as_str())
                .map_err(persist_err)?;
        }
        write_txn.commit().map_err(persist_err)?;
        Ok(())
    }

    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        let row_key = Self::coverage_row_key(key, relay);
        let read_txn = self.db.begin_read().expect("redb: begin_read");
        let coverage = read_txn.open_table(COVERAGE).expect("redb: open coverage");
        coverage
            .get(row_key.as_str())
            .expect("redb: get coverage row")
            .map(|guard| decode_interval(guard.value()))
    }

    fn gc(&mut self, claims: &ClaimSet) -> Result<GcReport, PersistenceError> {
        let mut report = GcReport::default();

        let write_txn = self.db.begin_write().map_err(persist_err)?;
        {
            let mut canonical = CanonicalWriteTables::open(&write_txn)?;
            let mut addr_index = write_txn.open_table(ADDR_INDEX).map_err(persist_err)?;
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?;
            let mut coverage = write_txn.open_table(COVERAGE).map_err(persist_err)?;
            let mut indexes = QueryIndexWriteTables::open(&write_txn)?;
            let outbox_suppress_by_id = write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ID)
                .map_err(persist_err)?;
            let outbox_suppress_by_addr = write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ADDR)
                .map_err(persist_err)?;

            // Pass 1: find victims (regular events matched by no claim, and
            // not an open — unsigned — local intent: Fable checkpoint R5,
            // mirrors `MemoryStore::gc`'s exclusion exactly). A row
            // currently hidden by a still-open kind:5 suppression claim is
            // pinned the same way (architecture review requirement — GC
            // must never evict a target a pending cancel/promote can still
            // act on; NIP-40 expiry may still remove it separately).
            // Collected up front into owned values so the removal pass
            // below never holds a borrow across a mutation.
            let mut victims: Vec<Event> = Vec::new();
            for entry in canonical.events.iter().map_err(persist_err)? {
                let (key, value) = entry.map_err(persist_err)?;
                let event = StoredEventView::from_trusted(value.value())
                    .expect("redb: decode canonical event view")
                    .materialize_event()
                    .expect("redb: materialize canonical event");
                let local = canonical
                    .local
                    .get(key.value())
                    .map_err(persist_err)?
                    .map(|value| {
                        binary_event::decode_local(value.value())
                            .expect("redb: decode canonical local state")
                    });
                if address_key_for(&event).is_none()
                    && !matches!(
                        local,
                        Some(LocalOrigin {
                            sig_state: SigState::Pending,
                            ..
                        })
                    )
                    && !is_suppressed_in_txn(
                        &outbox_suppress_by_id,
                        &outbox_suppress_by_addr,
                        &event,
                    )?
                    && !claims.is_claimed(&event)
                {
                    victims.push(event);
                }
            }

            for event in &victims {
                remove_row_in_txn(
                    &mut canonical,
                    &mut addr_index,
                    &mut expiration_index,
                    &mut indexes,
                    event.id,
                    |_| true,
                )?
                .expect("gc victim must remain present until removal");
                report.events_evicted += 1;
            }

            // Pass 2: shrink/delete every coverage row an evicted event
            // falls inside AND whose retained shape matches it. Same write
            // transaction as the event removals above — the shrink/delete
            // and the event delete commit atomically together (ruling §5:
            // never leave a watermark claiming coverage of evicted data).
            let mut row_updates: Vec<(String, Option<CoverageRowRecord>)> = Vec::new();
            let mut legacy_row_keys: Vec<String> = Vec::new();
            for entry in coverage.iter().map_err(persist_err)? {
                let (row_key, value) = entry.map_err(persist_err)?;

                // Legacy-row purge (#106, Fable's C refinement): a row
                // whose key predates the current schema version (no
                // `COVERAGE_ROW_KEY_PREFIX`) is permanently orphaned --
                // nothing will ever compute a matching key for it again
                // (v2 keys fold context + a version tag into the hash
                // itself, so no v1 key can ever collide forward into v2).
                // Delete it outright rather than let it linger forever,
                // tracked separately from `report.coverage_rows_deleted`
                // (which is specifically shrink-emptied current-schema
                // rows).
                if !row_key.value().starts_with(Self::COVERAGE_ROW_KEY_PREFIX) {
                    legacy_row_keys.push(row_key.value().to_string());
                    continue;
                }

                let mut record: CoverageRowRecord =
                    serde_json::from_str(value.value()).expect("redb: decode coverage row");
                let shape: ConcreteFilter = (&record.shape).into();
                let mut interval = CoverageInterval::new(
                    Timestamp::from(record.from),
                    Timestamp::from(record.through),
                );

                let mut deleted = false;
                let mut shrunk = false;
                for event in &victims {
                    let evicted_at = event.created_at;
                    if interval.from <= evicted_at
                        && evicted_at <= interval.through
                        && shape_matches(&shape, event)
                    {
                        match shrink_after_eviction(interval, evicted_at) {
                            Some(next) => {
                                interval = next;
                                shrunk = true;
                            }
                            None => {
                                deleted = true;
                                break;
                            }
                        }
                    }
                }

                if deleted {
                    row_updates.push((row_key.value().to_string(), None));
                } else if shrunk {
                    record.from = interval.from.as_secs();
                    record.through = interval.through.as_secs();
                    row_updates.push((row_key.value().to_string(), Some(record)));
                }
            }

            for (row_key, update) in row_updates {
                match update {
                    None => {
                        coverage.remove(row_key.as_str()).map_err(persist_err)?;
                        report.coverage_rows_deleted += 1;
                    }
                    Some(record) => {
                        let encoded =
                            serde_json::to_string(&record).expect("redb: encode coverage row");
                        coverage
                            .insert(row_key.as_str(), encoded.as_str())
                            .map_err(persist_err)?;
                        report.coverage_rows_shrunk += 1;
                    }
                }
            }

            for row_key in legacy_row_keys {
                coverage.remove(row_key.as_str()).map_err(persist_err)?;
                report.legacy_coverage_rows_purged += 1;
            }
            canonical.flush_counts()?;
        }
        write_txn.commit().map_err(persist_err)?;

        Ok(report)
    }

    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        let AcceptWrite {
            mut frozen,
            expected_pubkey,
            signing_identity_ref,
            durability,
            routing,
            mut sig_state,
            accepted_at,
        } = accept;
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

        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let outcome = {
            let mut canonical = CanonicalWriteTables::open(&write_txn)?;
            let mut addr_index = write_txn.open_table(ADDR_INDEX).map_err(persist_err)?;
            let tombstones = write_txn.open_table(TOMBSTONES).map_err(persist_err)?;
            let addr_tombstones = write_txn.open_table(ADDR_TOMBSTONES).map_err(persist_err)?;
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?;
            let mut indexes = QueryIndexWriteTables::open(&write_txn)?;
            let mut outbox_intents = write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
            let mut outbox_displaced = write_txn
                .open_table(OUTBOX_DISPLACED)
                .map_err(persist_err)?;
            let mut outbox_meta = write_txn.open_table(OUTBOX_META).map_err(persist_err)?;
            let mut outbox_receipts = write_txn.open_table(OUTBOX_RECEIPTS).map_err(persist_err)?;
            let mut outbox_kind5_claims = write_txn
                .open_table(OUTBOX_KIND5_CLAIMS)
                .map_err(persist_err)?;
            let mut outbox_suppress_by_id = write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ID)
                .map_err(persist_err)?;
            let mut outbox_suppress_by_addr = write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ADDR)
                .map_err(persist_err)?;

            let existing = canonical.load_by_id(&frozen.id)?;
            let is_deletion = frozen.kind == Kind::EventDeletion;

            // Dedup detection: checked against BOTH the live `EVENTS` row
            // AND every OTHER intent's `OUTBOX_DISPLACED` stash (issue #2
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
                Stash(String),
            }
            let dup_loc = if let Some((event_key, stored)) = existing {
                Some(DupLoc::Live(event_key, Box::new(stored)))
            } else {
                find_any_displaced_key_by_event_id_in_txn(&outbox_displaced, frozen.id)?
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
                let intent_id = alloc_intent_id_in_txn(&mut outbox_meta)?;
                let receipt_id = alloc_receipt_id_in_txn(&mut outbox_meta)?;
                let mut existing_record = match &dup_loc {
                    DupLoc::Live(_event_key, stored) => stored_event_to_record(stored),
                    DupLoc::Stash(key) => decode_stored_event_record(
                        outbox_displaced
                            .get(key.as_str())
                            .map_err(persist_err)?
                            .expect("just found this key")
                            .value(),
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
                        canonical.replace_local(*event_key, existing_record.local.clone())?;
                    }
                    DupLoc::Stash(key) => {
                        let encoded = encode_stored_event_record(&existing_record);
                        outbox_displaced
                            .insert(key.as_str(), encoded.as_slice())
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
                    let (_hidden, claims) = process_kind5_deletions_provisional_in_txn(
                        &canonical,
                        &addr_index,
                        &mut outbox_suppress_by_id,
                        &mut outbox_suppress_by_addr,
                        intent_id,
                        &frozen,
                    )?;
                    let encoded_claims =
                        serde_json::to_string(&claims).expect("redb: encode claims");
                    outbox_kind5_claims
                        .insert(intent_key(intent_id).as_str(), encoded_claims.as_str())
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
            } else if tombstone_refuses(&tombstones, &addr_tombstones, &frozen)? {
                (AcceptOutcome::Refused(RefuseReason::Tombstoned), None)
            } else {
                let intent_id = alloc_intent_id_in_txn(&mut outbox_meta)?;
                let receipt_id = alloc_receipt_id_in_txn(&mut outbox_meta)?;
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
                        let event_key = canonical.insert_new(&stored.event, &stored.provenance)?;
                        insert_query_index_rows(&mut canonical, &mut indexes, &frozen, event_key)
                            .map_err(persist_err)?;
                        if let Some(ts) = frozen.tags.expiration().copied() {
                            let exp_key = expiration_key(ts, &frozen.id);
                            expiration_index
                                .insert(exp_key.as_str(), event_key)
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
                            let (hidden, claims) = process_kind5_deletions_provisional_in_txn(
                                &canonical,
                                &addr_index,
                                &mut outbox_suppress_by_id,
                                &mut outbox_suppress_by_addr,
                                intent_id,
                                &frozen,
                            )?;
                            let encoded_claims =
                                serde_json::to_string(&claims).expect("redb: encode claims");
                            outbox_kind5_claims
                                .insert(intent_key(intent_id).as_str(), encoded_claims.as_str())
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
                        let current_key = addr_index
                            .get(addr_key_str.as_str())
                            .map_err(persist_err)?
                            .map(|guard| guard.value());

                        match current_key {
                            None => {
                                let event_key =
                                    canonical.insert_new(&stored.event, &stored.provenance)?;
                                addr_index
                                    .insert(addr_key_str.as_str(), event_key)
                                    .map_err(persist_err)?;
                                insert_query_index_rows(
                                    &mut canonical,
                                    &mut indexes,
                                    &frozen,
                                    event_key,
                                )
                                .map_err(persist_err)?;
                                if let Some(ts) = frozen.tags.expiration().copied() {
                                    let exp_key = expiration_key(ts, &frozen.id);
                                    expiration_index
                                        .insert(exp_key.as_str(), event_key)
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
                                let current = canonical
                                    .load_by_key(current_key)?
                                    .expect("addr_index must always point at a stored event");
                                let current_event = &current.event;

                                if candidate_wins(&frozen, current_event) {
                                    let replaced = remove_row_in_txn(
                                        &mut canonical,
                                        &mut addr_index,
                                        &mut expiration_index,
                                        &mut indexes,
                                        current_event.id,
                                        |_| true,
                                    )?
                                    .expect("addr_index must always point at a stored event");

                                    let event_key =
                                        canonical.insert_new(&stored.event, &stored.provenance)?;
                                    addr_index
                                        .insert(addr_key_str.as_str(), event_key)
                                        .map_err(persist_err)?;
                                    insert_query_index_rows(
                                        &mut canonical,
                                        &mut indexes,
                                        &frozen,
                                        event_key,
                                    )
                                    .map_err(persist_err)?;
                                    if let Some(ts) = frozen.tags.expiration().copied() {
                                        let exp_key = expiration_key(ts, &frozen.id);
                                        expiration_index
                                            .insert(exp_key.as_str(), event_key)
                                            .map_err(persist_err)?;
                                    }
                                    (
                                        AcceptOutcome::Superseded {
                                            intent_id,
                                            receipt_id,
                                            row: stored,
                                            replaced: Box::new(replaced.clone()),
                                        },
                                        Some(replaced),
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
            self.crash_if(RedbCrashPoint::AcceptAfterEventBeforeJournal);

            // R7: the intent's full journal payload AND the retained
            // receipt record commit in this SAME transaction as the
            // event-table mutation (and the `IntentId`/receipt-id
            // allocation) above — a crash here leaves either nothing or a
            // fully `recover_outbox`-able `Accepted`. R3: `Refused` is the
            // one outcome that journals nothing at all.
            if let (Some(intent_id), Some(receipt_id)) =
                (result.journaled_intent_id(), result.journaled_receipt_id())
            {
                let key = intent_key(intent_id);
                let intent_record = OutboxIntentRecord {
                    receipt_id,
                    frozen_json: frozen.as_json(),
                    expected_pubkey,
                    signing_identity_ref,
                    durability,
                    routing,
                    sig_state,
                    accepted_at,
                };
                let encoded_intent =
                    serde_json::to_string(&intent_record).expect("redb: encode outbox intent");
                outbox_intents
                    .insert(key.as_str(), encoded_intent.as_str())
                    .map_err(persist_err)?;

                if let Some(displaced) = &displaced {
                    let encoded_displaced = encode_stored_event(displaced);
                    outbox_displaced
                        .insert(key.as_str(), encoded_displaced.as_slice())
                        .map_err(persist_err)?;
                }

                // Architecture review correction: the RETAINED receipt
                // record, independent of `OUTBOX_INTENTS`'s open-work row.
                // `receipt_state` is `Accepted` except for the `Duplicate`-
                // of-an-already-signed-row case above, which overrides it
                // to `Signed` (codex-nova ruling).
                let receipt_record = OutboxReceiptRecord {
                    intent_id: Some(intent_id),
                    frozen_id: frozen.id,
                    expected_pubkey,
                    state: receipt_state,
                };
                let encoded_receipt =
                    serde_json::to_string(&receipt_record).expect("redb: encode outbox receipt");
                outbox_receipts
                    .insert(receipt_key(receipt_id).as_str(), encoded_receipt.as_str())
                    .map_err(persist_err)?;
            }

            canonical.flush_counts()?;
            result
        };
        #[cfg(test)]
        self.crash_if(RedbCrashPoint::AcceptBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(outcome)
    }

    fn promote_signed(
        &mut self,
        intent_id: IntentId,
        sig: Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let outcome = {
            let mut canonical = CanonicalWriteTables::open(&write_txn)?;
            let mut outbox_intents = write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
            let mut outbox_displaced = write_txn
                .open_table(OUTBOX_DISPLACED)
                .map_err(persist_err)?;
            let mut outbox_receipts = write_txn.open_table(OUTBOX_RECEIPTS).map_err(persist_err)?;
            let mut outbox_kind5_claims = write_txn
                .open_table(OUTBOX_KIND5_CLAIMS)
                .map_err(persist_err)?;
            let mut outbox_suppress_by_id = write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ID)
                .map_err(persist_err)?;
            let mut outbox_suppress_by_addr = write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ADDR)
                .map_err(persist_err)?;
            let mut addr_index = write_txn.open_table(ADDR_INDEX).map_err(persist_err)?;
            let mut tombstones = write_txn.open_table(TOMBSTONES).map_err(persist_err)?;
            let mut addr_tombstones = write_txn.open_table(ADDR_TOMBSTONES).map_err(persist_err)?;
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?;
            let mut indexes = QueryIndexWriteTables::open(&write_txn)?;

            let key = intent_key(intent_id);
            let intent_json = outbox_intents
                .get(key.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value().to_string());

            let outcome = match intent_json {
                None => PromoteOutcome::NotFound,
                Some(intent_json) => {
                    let intent_record: OutboxIntentRecord =
                        serde_json::from_str(&intent_json).expect("redb: decode outbox intent");
                    // No-second-transition guard (codex-nova finding): a
                    // repeat promotion (e.g. a duplicate signer completion)
                    // must not overwrite an already-Signed row and re-emit
                    // `Promoted` — the trait doc already promised
                    // "already-promoted returns NotFound"; this enforces
                    // it. Load-bearing for `AtMostOnce`: a second silent
                    // transition here could let the caller re-publish.
                    if intent_record.sig_state == IntentSigState::Signed {
                        return Ok(PromoteOutcome::NotFound);
                    }
                    let frozen_event = Event::from_json(&intent_record.frozen_json)
                        .expect("redb: decode frozen event json");
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
                    let live_record = canonical
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
                        &outbox_displaced,
                        frozen_id,
                        intent_id,
                    )? {
                        let other_bytes = outbox_displaced
                            .get(other_key.as_str())
                            .map_err(persist_err)?
                            .expect("just found this key")
                            .value()
                            .to_vec();
                        let other_record = decode_stored_event_record(&other_bytes);
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
                            canonical.replace_event(event_key, &record.event)?;
                            canonical.replace_local(event_key, record.local.clone())?;
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
                        &outbox_displaced,
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
                        let other_bytes = outbox_displaced
                            .get(other_key.as_str())
                            .map_err(persist_err)?
                            .expect("just found this key")
                            .value()
                            .to_vec();
                        let mut other_record = decode_stored_event_record(&other_bytes);
                        if !already_signed {
                            other_record.event = signed_frozen_event.clone();
                            if let Some(local) = other_record.local.as_mut() {
                                local.sig_state = SigState::Signed;
                            }
                            let encoded_other = encode_stored_event_record(&other_record);
                            outbox_displaced
                                .insert(other_key.as_str(), encoded_other.as_slice())
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
                    let co_signed: Vec<IntentId> = fan_out_signed_in_txn(
                        &mut canonical,
                        &mut addr_index,
                        &mut tombstones,
                        &mut addr_tombstones,
                        &mut expiration_index,
                        &mut indexes,
                        &mut outbox_intents,
                        &mut outbox_receipts,
                        &mut outbox_displaced,
                        &mut outbox_kind5_claims,
                        &mut outbox_suppress_by_id,
                        &mut outbox_suppress_by_addr,
                        &owners,
                        &row.event,
                    )?
                    .into_iter()
                    .filter(|owner_id| *owner_id != intent_id)
                    .collect();

                    PromoteOutcome::Promoted {
                        row: Box::new(row),
                        co_signed,
                    }
                }
            };
            canonical.flush_counts()?;
            outcome
        };
        #[cfg(test)]
        self.crash_if(RedbCrashPoint::PromoteBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(outcome)
    }

    fn compensate_write(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let outcome = {
            let mut canonical = CanonicalWriteTables::open(&write_txn)?;
            let mut addr_index = write_txn.open_table(ADDR_INDEX).map_err(persist_err)?;
            let mut tombstones = write_txn.open_table(TOMBSTONES).map_err(persist_err)?;
            let mut addr_tombstones = write_txn.open_table(ADDR_TOMBSTONES).map_err(persist_err)?;
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?;
            let mut indexes = QueryIndexWriteTables::open(&write_txn)?;
            let mut outbox_intents = write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
            let mut outbox_displaced = write_txn
                .open_table(OUTBOX_DISPLACED)
                .map_err(persist_err)?;
            let mut outbox_receipts = write_txn.open_table(OUTBOX_RECEIPTS).map_err(persist_err)?;
            let mut outbox_kind5_claims = write_txn
                .open_table(OUTBOX_KIND5_CLAIMS)
                .map_err(persist_err)?;
            let mut outbox_suppress_by_id = write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ID)
                .map_err(persist_err)?;
            let mut outbox_suppress_by_addr = write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ADDR)
                .map_err(persist_err)?;

            let key = intent_key(intent_id);
            let intent_json = outbox_intents
                .get(key.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value().to_string());

            let outcome = match intent_json {
                None => CompensateOutcome::NotFound,
                Some(intent_json) => {
                    let intent_record: OutboxIntentRecord =
                        serde_json::from_str(&intent_json).expect("redb: decode outbox intent");
                    if intent_record.sig_state == IntentSigState::Signed {
                        // Pre-signature only (retraction doc §4.2's
                        // "Promotion correction").
                        CompensateOutcome::NotFound
                    } else {
                        let frozen_event = Event::from_json(&intent_record.frozen_json)
                            .expect("redb: decode frozen event json");
                        let frozen_id = frozen_event.id;
                        let live = canonical.load_by_id(&frozen_id)?;
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
                                remove_row_in_txn(
                                    &mut canonical,
                                    &mut addr_index,
                                    &mut expiration_index,
                                    &mut indexes,
                                    frozen_id,
                                    |_| true,
                                )?;
                            } else {
                                record.local = Some(local);
                                canonical.replace_local(*event_key, record.local)?;
                            }
                        } else if let Some(other_key) = find_displaced_key_by_event_id_in_txn(
                            &outbox_displaced,
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
                            let other_bytes = outbox_displaced
                                .get(other_key.as_str())
                                .map_err(persist_err)?
                                .expect("just found this key")
                                .value()
                                .to_vec();
                            let mut other_record = decode_stored_event_record(&other_bytes);
                            let mut local = other_record.local.clone().expect(
                                "find_displaced_key_by_event_id_in_txn only matches owned entries",
                            );
                            local.owners.remove(&intent_id);
                            let should_drop = local.owners.is_empty()
                                && local.sig_state == SigState::Pending
                                && other_record.provenance.is_empty();
                            if should_drop {
                                outbox_displaced
                                    .remove(other_key.as_str())
                                    .map_err(persist_err)?;
                            } else {
                                other_record.local = Some(local);
                                let encoded_other = encode_stored_event_record(&other_record);
                                outbox_displaced
                                    .insert(other_key.as_str(), encoded_other.as_slice())
                                    .map_err(persist_err)?;
                            }
                        }

                        outbox_intents.remove(key.as_str()).map_err(persist_err)?;
                        // THIS intent's OWN displaced predecessor (if any)
                        // is restored through the same one door regardless
                        // of whether its row was live or already gone for
                        // some other reason (kind:5/expiry/relay
                        // supersession) — `reinsert_stashed_in_txn`'s own
                        // tombstone check makes this safe even if the
                        // predecessor was itself since deleted or expired.
                        let displaced_bytes = outbox_displaced
                            .remove(key.as_str())
                            .map_err(persist_err)?
                            .map(|guard| guard.value().to_vec());
                        let restored = match displaced_bytes {
                            Some(bytes) => reinsert_stashed_in_txn(
                                &mut canonical,
                                &mut addr_index,
                                &mut tombstones,
                                &mut addr_tombstones,
                                &mut expiration_index,
                                &mut indexes,
                                &mut outbox_intents,
                                &mut outbox_receipts,
                                &mut outbox_displaced,
                                &mut outbox_kind5_claims,
                                &mut outbox_suppress_by_id,
                                &mut outbox_suppress_by_addr,
                                decode_stored_event(&bytes),
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
                        let claims_json = outbox_kind5_claims
                            .remove(key.as_str())
                            .map_err(persist_err)?
                            .map(|guard| guard.value().to_string());
                        if let Some(claims_json) = claims_json {
                            let claims: Vec<SuppressClaimRecord> =
                                serde_json::from_str(&claims_json).expect("redb: decode claims");

                            let mut candidate_ids: Vec<EventId> = Vec::new();
                            let mut seen_candidates: HashSet<EventId> = HashSet::new();
                            for claim in &claims {
                                let target_id = match claim {
                                    SuppressClaimRecord::Id(id_key) => {
                                        // `id_tombstone_key` is
                                        // `"{id_hex}:{author_hex}"` — the
                                        // target's own id is everything
                                        // before the first `:`.
                                        let hex = id_key
                                            .split(':')
                                            .next()
                                            .expect("id_tombstone_key always has a ':'");
                                        Some(
                                            EventId::from_hex(hex)
                                                .expect("redb: decode id claim target"),
                                        )
                                    }
                                    SuppressClaimRecord::Addr { key: addr_key, .. } => {
                                        let event_key = addr_index
                                            .get(addr_key.as_str())
                                            .map_err(persist_err)?
                                            .map(|guard| guard.value());
                                        match event_key {
                                            Some(event_key) => canonical
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
                                let visible = match canonical.load_by_id(id)? {
                                    None => false,
                                    Some((_key, se)) => !is_suppressed_in_txn(
                                        &outbox_suppress_by_id,
                                        &outbox_suppress_by_addr,
                                        &se.event,
                                    )?,
                                };
                                visible_before.insert(*id, visible);
                            }

                            for claim in claims {
                                match claim {
                                    SuppressClaimRecord::Id(id_key) => {
                                        remove_claimant_in_txn(
                                            &mut outbox_suppress_by_id,
                                            &id_key,
                                            intent_id,
                                        )?;
                                    }
                                    SuppressClaimRecord::Addr { key: addr_key, .. } => {
                                        remove_addr_claimant_in_txn(
                                            &mut outbox_suppress_by_addr,
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
                                if let Some((_key, se)) = canonical.load_by_id(&id)? {
                                    if !is_suppressed_in_txn(
                                        &outbox_suppress_by_id,
                                        &outbox_suppress_by_addr,
                                        &se.event,
                                    )? {
                                        revealed.push(se);
                                    }
                                }
                            }
                        }

                        update_outbox_receipt(
                            &mut outbox_receipts,
                            intent_record.receipt_id,
                            ReceiptState::Compensated,
                        )?;

                        CompensateOutcome::Compensated { restored, revealed }
                    }
                }
            };
            canonical.flush_counts()?;
            outcome
        };
        #[cfg(test)]
        self.crash_if(RedbCrashPoint::CompensateBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(outcome)
    }

    fn recover_outbox(&self) -> Vec<RecoveredIntent> {
        let read_txn = self.db.begin_read().expect("redb: begin_read");
        let outbox_intents = read_txn
            .open_table(OUTBOX_INTENTS)
            .expect("redb: open outbox_intents");
        let outbox_displaced = read_txn
            .open_table(OUTBOX_DISPLACED)
            .expect("redb: open outbox_displaced");

        let mut out = Vec::new();
        for entry in outbox_intents.iter().expect("redb: iter outbox_intents") {
            let (key, value) = entry.expect("redb: read outbox_intents entry");
            let intent_id = IntentId(
                key.value()
                    .parse::<u64>()
                    .expect("redb: parse outbox_intents key"),
            );
            let record: OutboxIntentRecord =
                serde_json::from_str(value.value()).expect("redb: decode outbox intent");
            let frozen =
                Event::from_json(&record.frozen_json).expect("redb: decode frozen event json");

            let displaced = outbox_displaced
                .get(key.value())
                .expect("redb: get outbox_displaced")
                .map(|guard| decode_stored_event(guard.value()));

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
        out
    }

    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError> {
        // NOT a Q4 "always empty" door: retention (not crash-survival) is
        // the contract — `OUTBOX_RECEIPTS` rows are never deleted by this
        // unit, so this is an ordinary durable read.
        let read_txn = self.db.begin_read().map_err(persist_err)?;
        let outbox_receipts = read_txn.open_table(OUTBOX_RECEIPTS).map_err(persist_err)?;
        let Some(json) = outbox_receipts
            .get(receipt_key(receipt_id).as_str())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_string())
        else {
            return Ok(None);
        };
        let record: OutboxReceiptRecord = serde_json::from_str(&json)
            .map_err(|err| PersistenceError(format!("decode retained receipt: {err}")))?;
        Ok(Some(RecoveredReceipt {
            receipt_id,
            intent_id: record.intent_id,
            frozen_id: record.frozen_id,
            expected_pubkey: record.expected_pubkey,
            state: record.state,
        }))
    }

    fn record_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let revision = {
            let intents = write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
            let intent_key = intent_key(intent_id);
            if intents
                .get(intent_key.as_str())
                .map_err(persist_err)?
                .is_none()
            {
                return Err(PersistenceError("route revision intent is not open".into()));
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
                self.route_revision_range_rows
                    .fetch_add(1, Ordering::Relaxed);
                let (key, value) = entry.map_err(persist_err)?;
                let recovered = decode_route_revision(key.value(), value.value())?;
                if recovered.intent_id != intent_id {
                    return Err(PersistenceError(
                        "route revision range does not match its value intent".into(),
                    ));
                }
                last = last.max(recovered.ordinal);
            }
            let ordinal = last
                .checked_add(1)
                .ok_or_else(|| PersistenceError("route revision ordinal exhausted".into()))?;
            let record = OutboxRouteRevisionRecord {
                version: 1,
                intent_id,
                ordinal,
                relays: relays.clone(),
            };
            let encoded = serde_json::to_string(&record)
                .map_err(|err| PersistenceError(format!("encode route revision: {err}")))?;
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
        self.crash_if(RedbCrashPoint::RouteRevisionBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(revision)
    }

    fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
        let read_txn = self.db.begin_read().map_err(persist_err)?;
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
            self.route_revision_range_rows
                .fetch_add(1, Ordering::Relaxed);
            let (key, value) = entry.map_err(persist_err)?;
            let revision = decode_route_revision(key.value(), value.value())?;
            if revision.intent_id != intent_id {
                return Err(PersistenceError(
                    "route revision range does not match its value intent".into(),
                ));
            }
            recovered.push(revision);
        }
        recovered.sort_by_key(|revision| revision.ordinal);
        Ok(recovered)
    }

    fn start_attempt(
        &mut self,
        intent_id: IntentId,
        relay: RelayUrl,
        event: Event,
    ) -> Result<RecoveredAttempt, PersistenceError> {
        let read_txn = self.db.begin_read().map_err(persist_err)?;
        let intents = read_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
        let key = intent_key(intent_id);
        let json = intents
            .get(key.as_str())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_string())
            .ok_or_else(|| PersistenceError("attempt intent is not open".into()))?;
        let intent: OutboxIntentRecord = serde_json::from_str(&json)
            .map_err(|err| PersistenceError(format!("decode attempt intent: {err}")))?;
        let frozen = Event::from_json(&intent.frozen_json)
            .map_err(|err| PersistenceError(format!("decode attempt intent event: {err}")))?;
        if intent.sig_state != IntentSigState::Signed || frozen != event {
            return Err(PersistenceError(
                "attempt bytes are not the intent's promoted signed bytes".into(),
            ));
        }
        event
            .verify()
            .map_err(|err| PersistenceError(format!("attempt event is invalid: {err}")))?;
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let attempt = {
            let mut attempts = write_txn.open_table(OUTBOX_ATTEMPTS).map_err(persist_err)?;
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
            let key = LaneKey {
                intent_id,
                relay: relay.clone(),
            };
            let lane_storage_key = lane_key(&key);
            let existing_lane = lanes
                .get(lane_storage_key.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value().to_string());
            let lane = match existing_lane {
                Some(json) => decode_lane(&lane_storage_key, &json)?,
                None => {
                    // Temporary compatibility door for pre-lane callers:
                    // seed from only the lexicographically-last exact-relay
                    // row, never rescan retained history.
                    let prefix = attempt_prefix(intent_id, &relay);
                    let (lower, upper) = prefix_range(prefix.clone());
                    let last_ordinal = match attempts
                        .range(lower.as_str()..upper.as_str())
                        .map_err(persist_err)?
                        .next_back()
                    {
                        None => 0,
                        Some(row) => {
                            #[cfg(test)]
                            self.attempt_range_rows.fetch_add(1, Ordering::Relaxed);
                            let (raw_key, raw_value) = row.map_err(persist_err)?;
                            let suffix =
                                raw_key.value().strip_prefix(&prefix).ok_or_else(|| {
                                    PersistenceError(
                                        "outbox attempt range escaped its prefix".into(),
                                    )
                                })?;
                            let ordinal = suffix.parse::<u64>().map_err(|err| {
                                PersistenceError(format!("parse outbox attempt ordinal: {err}"))
                            })?;
                            if ordinal == u64::MAX {
                                return Err(PersistenceError("attempt ordinal exhausted".into()));
                            }
                            decode_attempt(raw_key.value(), raw_value.value())?;
                            ordinal
                        }
                    };
                    let lane = RecoveredLane {
                        version: 1,
                        key: key.clone(),
                        revision: 1,
                        last_ordinal,
                        state: if last_ordinal == 0 {
                            LaneState::WaitingConnection
                        } else {
                            LaneState::LegacyInFlight {
                                ordinal: last_ordinal,
                            }
                        },
                    };
                    lanes
                        .insert(
                            lane_storage_key.as_str(),
                            encode_json(&lane, "outbox lane")?.as_str(),
                        )
                        .map_err(persist_err)?;
                    lane
                }
            };
            if lane.last_ordinal > 0 {
                let previous_key = attempt_key(intent_id, &relay, lane.last_ordinal);
                let previous_json = attempts
                    .get(previous_key.as_str())
                    .map_err(persist_err)?
                    .map(|guard| guard.value().to_string())
                    .ok_or_else(|| PersistenceError("lane cursor attempt row not found".into()))?;
                let previous = decode_attempt(&previous_key, &previous_json)?;
                let previous_details = details
                    .get(previous_key.as_str())
                    .map_err(persist_err)?
                    .map(|guard| guard.value().to_string())
                    .map(|json| decode_attempt_details(&previous_key, &json))
                    .transpose()?;
                if crate::attempt_is_live(&previous, previous_details.as_ref()) {
                    return Err(PersistenceError(
                        "cannot start a new ordinal while the current attempt is live".into(),
                    ));
                }
            }
            let ordinal = lane
                .last_ordinal
                .checked_add(1)
                .ok_or_else(|| PersistenceError("attempt ordinal exhausted".into()))?;
            let record = OutboxAttemptRecord {
                version: 1,
                intent_id,
                relay: relay.clone(),
                ordinal,
                event_json: event.as_json(),
                outcome: AttemptOutcome::Started,
            };
            let encoded = serde_json::to_string(&record)
                .map_err(|err| PersistenceError(format!("encode outbox attempt: {err}")))?;
            attempts
                .insert(
                    attempt_key(intent_id, &relay, ordinal).as_str(),
                    encoded.as_str(),
                )
                .map_err(persist_err)?;
            let detail = RecoveredAttemptDetails {
                version: 1,
                intent_id,
                relay: relay.clone(),
                ordinal,
                started_at: None,
                handoff: None,
                transient: None,
                finished_at: None,
                terminal: None,
            };
            let encoded_detail = encode_json(&detail, "attempt details")?;
            details
                .insert(
                    attempt_key(intent_id, &relay, ordinal).as_str(),
                    encoded_detail.as_str(),
                )
                .map_err(persist_err)?;
            let mut advanced = replace_lane_in_txn(
                &mut lanes,
                &mut deadlines,
                &mut deadlines_by_intent,
                &key,
                lane.revision,
                LaneState::InFlight {
                    ordinal,
                    phase: InFlightPhase::AwaitingHandoff,
                },
            )?;
            advanced.last_ordinal = ordinal;
            let encoded_lane = encode_json(&advanced, "outbox lane")?;
            lanes
                .insert(lane_storage_key.as_str(), encoded_lane.as_str())
                .map_err(persist_err)?;
            RecoveredAttempt {
                version: 1,
                intent_id,
                relay,
                ordinal,
                event,
                outcome: AttemptOutcome::Started,
            }
        };
        #[cfg(test)]
        self.crash_if(RedbCrashPoint::StartAttemptBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(attempt)
    }

    fn finish_attempt(
        &mut self,
        intent_id: IntentId,
        relay: &RelayUrl,
        ordinal: u64,
        outcome: AttemptOutcome,
    ) -> Result<FinishAttemptOutcome, PersistenceError> {
        if outcome == AttemptOutcome::Started {
            return Err(PersistenceError("Started is not a terminal outcome".into()));
        }
        let target_key = attempt_key(intent_id, relay, ordinal);
        {
            let read_txn = self.db.begin_read().map_err(persist_err)?;
            let attempts = read_txn.open_table(OUTBOX_ATTEMPTS).map_err(persist_err)?;
            let raw = attempts
                .get(target_key.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value().to_string())
                .ok_or_else(|| PersistenceError("attempt row not found".into()))?;
            decode_attempt(&target_key, &raw)?;
        }
        self.bootstrap_outbox_lanes(intent_id)?;
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let result = {
            let attempts = write_txn.open_table(OUTBOX_ATTEMPTS).map_err(persist_err)?;
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
            let key = attempt_key(intent_id, relay, ordinal);
            let existing = attempts.get(key.as_str()).map_err(persist_err)?;
            let json = existing
                .map(|guard| guard.value().to_string())
                .ok_or_else(|| PersistenceError("attempt row not found".into()))?;
            let recovered = decode_attempt(&key, &json)?;
            if recovered.outcome != AttemptOutcome::Started {
                if recovered.outcome == outcome {
                    return Ok(FinishAttemptOutcome::AlreadySame);
                }
                return Err(PersistenceError(
                    "legacy attempt row has a conflicting terminal outcome".into(),
                ));
            }
            let lane_key_value = LaneKey {
                intent_id,
                relay: relay.clone(),
            };
            let lane_storage_key = lane_key(&lane_key_value);
            let lane_json = lanes
                .get(lane_storage_key.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value().to_string())
                .ok_or_else(|| PersistenceError("attempt lane not found".into()))?;
            let lane = decode_lane(&lane_storage_key, &lane_json)?;
            if lane.last_ordinal != ordinal {
                return Err(PersistenceError("stale attempt ordinal".into()));
            }
            let detail_json = details
                .get(key.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value().to_string())
                .ok_or_else(|| PersistenceError("attempt detail row not found".into()))?;
            let mut detail = decode_attempt_details(&key, &detail_json)?;
            if detail.terminal.as_ref() == Some(&outcome) {
                FinishAttemptOutcome::AlreadySame
            } else if detail.terminal.is_none() {
                detail.terminal = Some(outcome.clone());
                let encoded = encode_json(&detail, "attempt details")?;
                details
                    .insert(key.as_str(), encoded.as_str())
                    .map_err(persist_err)?;
                replace_lane_in_txn(
                    &mut lanes,
                    &mut deadlines,
                    &mut deadlines_by_intent,
                    &lane_key_value,
                    lane.revision,
                    LaneState::Terminal { ordinal, outcome },
                )?;
                FinishAttemptOutcome::Committed
            } else {
                return Err(PersistenceError(
                    "attempt already has a conflicting terminal outcome".into(),
                ));
            }
        };
        #[cfg(test)]
        self.crash_if(RedbCrashPoint::FinishAttemptBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(result)
    }

    fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        let read_txn = self.db.begin_read().map_err(persist_err)?;
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
            self.attempt_range_rows.fetch_add(1, Ordering::Relaxed);
            let (key, value) = entry.map_err(persist_err)?;
            let mut attempt = decode_attempt(key.value(), value.value())?;
            if attempt.intent_id != intent_id {
                return Err(PersistenceError(
                    "outbox attempt range does not match its value intent".into(),
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

    fn bootstrap_outbox_lanes(
        &mut self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        {
            let intents = write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
            if intents
                .get(intent_key(intent_id).as_str())
                .map_err(persist_err)?
                .is_none()
            {
                return Err(PersistenceError("lane bootstrap intent is not open".into()));
            }
            let route_revisions = write_txn
                .open_table(OUTBOX_ROUTE_REVISIONS)
                .map_err(persist_err)?;
            let attempts_table = write_txn.open_table(OUTBOX_ATTEMPTS).map_err(persist_err)?;
            let mut lanes = write_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
            let mut details = write_txn
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
                self.attempt_range_rows.fetch_add(1, Ordering::Relaxed);
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
                self.route_revision_range_rows
                    .fetch_add(1, Ordering::Relaxed);
                let (key, value) = row.map_err(persist_err)?;
                let revision = decode_route_revision(key.value(), value.value())?;
                relays.extend(revision.relays);
            }
            for attempt in &attempts {
                relays.insert(attempt.relay.clone());
            }
            for attempt in &attempts {
                if !details_by_key.contains_key(&(attempt.relay.clone(), attempt.ordinal)) {
                    let shell = RecoveredAttemptDetails {
                        version: 1,
                        intent_id,
                        relay: attempt.relay.clone(),
                        ordinal: attempt.ordinal,
                        started_at: None,
                        handoff: None,
                        transient: None,
                        finished_at: None,
                        terminal: None,
                    };
                    details
                        .insert(
                            attempt_key(intent_id, &attempt.relay, attempt.ordinal).as_str(),
                            encode_json(&shell, "attempt details")?.as_str(),
                        )
                        .map_err(persist_err)?;
                }
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
                    return Err(PersistenceError(
                        "contradictory live v1 Started attempt history".into(),
                    ));
                }
                if let Some(existing) = lanes.get(storage_key.as_str()).map_err(persist_err)? {
                    let lane = decode_lane(&storage_key, existing.value())?;
                    let max = lane_attempts.last().map_or(0, |attempt| attempt.ordinal);
                    if lane.last_ordinal != max {
                        return Err(PersistenceError(
                            "outbox lane cursor disagrees with retained attempt history".into(),
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
                                return Err(PersistenceError(
                                    "terminal attempt and lane state disagree".into(),
                                ));
                            }
                        }
                        _ if matches!(lane.state, LaneState::Terminal { .. }) => {
                            return Err(PersistenceError(
                                "terminal lane lacks matching terminal attempt".into(),
                            ));
                        }
                        _ => {}
                    }
                    continue;
                }
                let last_ordinal = lane_attempts.last().map_or(0, |attempt| attempt.ordinal);
                let state = match lane_attempts.last() {
                    None => LaneState::WaitingConnection,
                    Some(attempt) if attempt.outcome == AttemptOutcome::Started => {
                        LaneState::LegacyInFlight {
                            ordinal: attempt.ordinal,
                        }
                    }
                    Some(attempt) => LaneState::Terminal {
                        ordinal: attempt.ordinal,
                        outcome: attempt.outcome.clone(),
                    },
                };
                let lane = RecoveredLane {
                    version: 1,
                    key,
                    revision: 1,
                    last_ordinal,
                    state,
                };
                let encoded = encode_json(&lane, "outbox lane")?;
                lanes
                    .insert(storage_key.as_str(), encoded.as_str())
                    .map_err(persist_err)?;
            }
        }
        #[cfg(test)]
        self.crash_if(RedbCrashPoint::LaneBootstrapBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        self.recover_outbox_lanes(intent_id)
    }

    fn recover_outbox_lanes(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        let read_txn = self.db.begin_read().map_err(persist_err)?;
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
                return Err(PersistenceError("lane range escaped intent prefix".into()));
            }
            recovered.push(lane);
        }
        recovered.sort_by(|a, b| a.key.relay.cmp(&b.key.relay));
        Ok(recovered)
    }

    fn due_outbox_deadlines(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<LaneDeadline>, PersistenceError> {
        if limit > 1_024 {
            return Err(PersistenceError("deadline read limit exceeds 1024".into()));
        }
        let read_txn = self.db.begin_read().map_err(persist_err)?;
        let deadlines = read_txn.open_table(OUTBOX_DEADLINES).map_err(persist_err)?;
        let deadlines_by_intent = read_txn
            .open_table(OUTBOX_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        if deadlines.len().map_err(persist_err)?
            != deadlines_by_intent.len().map_err(persist_err)?
        {
            return Err(PersistenceError(
                "deadline index cardinalities disagree".into(),
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
                .ok_or_else(|| PersistenceError("deadline is missing by-intent index".into()))?;
            if decode_deadline_by_intent(&secondary_key, &secondary)? != deadline {
                return Err(PersistenceError("deadline indexes disagree".into()));
            }
            let lane_storage_key = lane_key(&deadline.key);
            let lane_json = lanes
                .get(lane_storage_key.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value().to_string())
                .ok_or_else(|| PersistenceError("deadline references missing lane".into()))?;
            let lane = decode_lane(&lane_storage_key, &lane_json)?;
            if lane_deadline(&lane).as_ref() != Some(&deadline) {
                return Err(PersistenceError("deadline and lane disagree".into()));
            }
            recovered.push(deadline);
        }
        Ok(recovered)
    }

    fn next_outbox_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        let read_txn = self.db.begin_read().map_err(persist_err)?;
        let deadlines = read_txn.open_table(OUTBOX_DEADLINES).map_err(persist_err)?;
        let deadlines_by_intent = read_txn
            .open_table(OUTBOX_DEADLINES_BY_INTENT)
            .map_err(persist_err)?;
        if deadlines.len().map_err(persist_err)?
            != deadlines_by_intent.len().map_err(persist_err)?
        {
            return Err(PersistenceError(
                "deadline index cardinalities disagree".into(),
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
            .ok_or_else(|| PersistenceError("deadline is missing by-intent index".into()))?;
        if decode_deadline_by_intent(&secondary_key, &secondary)? != deadline {
            return Err(PersistenceError("deadline indexes disagree".into()));
        }
        let lane_storage_key = lane_key(&deadline.key);
        let lane = lanes
            .get(lane_storage_key.as_str())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_string())
            .ok_or_else(|| PersistenceError("deadline references missing lane".into()))?;
        if lane_deadline(&decode_lane(&lane_storage_key, &lane)?).as_ref() != Some(&deadline) {
            return Err(PersistenceError("deadline and lane disagree".into()));
        }
        Ok(Some(deadline.at))
    }

    fn set_lane_waiting(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.persist_lane_state(
            key,
            expected_revision,
            if auth {
                LaneState::WaitingAuth
            } else {
                LaneState::WaitingConnection
            },
        )
    }

    fn set_lane_eligible(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        since: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.persist_lane_state(key, expected_revision, LaneState::Eligible { since })
    }

    fn set_lane_transient(
        &mut self,
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
            return Err(PersistenceError(
                "transient raw reason exceeds 4096 bytes".into(),
            ));
        }
        let write_txn = self.db.begin_write().map_err(persist_err)?;
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
                .ok_or_else(|| PersistenceError("outbox lane not found".into()))?;
            let current = decode_lane(&storage_key, &json)?;
            if current.last_ordinal != ordinal {
                return Err(PersistenceError("stale attempt ordinal".into()));
            }
            if ordinal > 0 {
                let detail_key = attempt_key(key.intent_id, &key.relay, ordinal);
                let detail_json = details
                    .get(detail_key.as_str())
                    .map_err(persist_err)?
                    .map(|g| g.value().to_string())
                    .ok_or_else(|| PersistenceError("attempt detail row not found".into()))?;
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
        self.crash_if(RedbCrashPoint::LaneTransitionBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(lane)
    }

    fn start_lane_attempt(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        event: Event,
        started_at: Timestamp,
    ) -> Result<(RecoveredAttempt, RecoveredLane), PersistenceError> {
        let read_txn = self.db.begin_read().map_err(persist_err)?;
        let intents = read_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
        let intent_json = intents
            .get(intent_key(key.intent_id).as_str())
            .map_err(persist_err)?
            .map(|g| g.value().to_string())
            .ok_or_else(|| PersistenceError("attempt intent is not open".into()))?;
        let intent: OutboxIntentRecord = serde_json::from_str(&intent_json)
            .map_err(|e| PersistenceError(format!("decode attempt intent: {e}")))?;
        let frozen = Event::from_json(&intent.frozen_json)
            .map_err(|e| PersistenceError(format!("decode attempt intent event: {e}")))?;
        if intent.sig_state != IntentSigState::Signed || frozen != event {
            return Err(PersistenceError(
                "attempt bytes are not the intent's promoted signed bytes".into(),
            ));
        }
        event
            .verify()
            .map_err(|e| PersistenceError(format!("attempt event is invalid: {e}")))?;
        drop(intents);
        drop(read_txn);
        let write_txn = self.db.begin_write().map_err(persist_err)?;
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
                .ok_or_else(|| PersistenceError("outbox lane not found".into()))?;
            let current = decode_lane(&storage_key, &lane_json)?;
            if current.revision != expected_revision
                || !matches!(current.state, LaneState::Eligible { .. })
            {
                return Err(PersistenceError(
                    "lane is not expected eligible cursor".into(),
                ));
            }
            let ordinal = current
                .last_ordinal
                .checked_add(1)
                .ok_or_else(|| PersistenceError("attempt ordinal exhausted".into()))?;
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
        self.crash_if(RedbCrashPoint::LaneStartBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok((attempt, lane))
    }

    fn record_lane_handoff(
        &mut self,
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
            return Err(PersistenceError(
                "transient raw reason exceeds 4096 bytes".into(),
            ));
        }
        let write_txn = self.db.begin_write().map_err(persist_err)?;
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
                .ok_or_else(|| PersistenceError("outbox lane not found".into()))?;
            let current_lane = decode_lane(&lane_storage_key, &lane_json)?;
            if current_lane.revision != expected_revision || current_lane.last_ordinal != ordinal {
                return Err(PersistenceError("stale lane handoff".into()));
            }
            if !matches!(
                current_lane.state,
                LaneState::InFlight {
                    ordinal: current,
                    phase: InFlightPhase::AwaitingHandoff,
                } if current == ordinal
            ) {
                return Err(PersistenceError("lane is not awaiting handoff".into()));
            }
            let attempt_key_value = attempt_key(key.intent_id, &key.relay, ordinal);
            let detail_json = details
                .get(attempt_key_value.as_str())
                .map_err(persist_err)?
                .map(|g| g.value().to_string())
                .ok_or_else(|| PersistenceError("attempt detail row not found".into()))?;
            let mut recovered_detail = decode_attempt_details(&attempt_key_value, &detail_json)?;
            if let Some(existing) = &recovered_detail.handoff {
                if existing != &detail {
                    return Err(PersistenceError("conflicting handoff evidence".into()));
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
                        return Err(PersistenceError("Started is not terminal".into()));
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
                return Err(PersistenceError("stale lane handoff ordinal".into()));
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
        self.crash_if(RedbCrashPoint::LaneHandoffBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(lane)
    }

    fn finish_lane_attempt(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        outcome: AttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        if outcome == AttemptOutcome::Started {
            return Err(PersistenceError("Started is not terminal".into()));
        }
        let write_txn = self.db.begin_write().map_err(persist_err)?;
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
                .ok_or_else(|| PersistenceError("outbox lane not found".into()))?;
            let current = decode_lane(&storage_key, &lane_json)?;
            if current.revision != expected_revision || current.last_ordinal != ordinal {
                return Err(PersistenceError("stale terminal attempt".into()));
            }
            let detail_key = attempt_key(key.intent_id, &key.relay, ordinal);
            let detail_json = details
                .get(detail_key.as_str())
                .map_err(persist_err)?
                .map(|g| g.value().to_string())
                .ok_or_else(|| PersistenceError("attempt detail row not found".into()))?;
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
                    return Err(PersistenceError(
                        "attempt already has conflicting terminal evidence".into(),
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
        self.crash_if(RedbCrashPoint::FinishAttemptBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(lane)
    }

    fn recover_attempt_details(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredAttemptDetails>, PersistenceError> {
        let read_txn = self.db.begin_read().map_err(persist_err)?;
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

    fn close_terminal_intent(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
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
                        return Err(PersistenceError(
                            "lane close range escaped intent prefix".into(),
                        ));
                    }
                    lanes_snapshot.push(lane);
                }
                if lanes_snapshot.is_empty()
                    || lanes_snapshot
                        .iter()
                        .any(|lane| !matches!(lane.state, LaneState::Terminal { .. }))
                {
                    return Err(PersistenceError(
                        "intent lanes are not non-empty and terminal".into(),
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
                    return Err(PersistenceError(
                        "deadline index cardinalities disagree".into(),
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
                        return Err(PersistenceError(
                            "deadline close range escaped intent prefix".into(),
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
                            PersistenceError("by-intent deadline is missing ordered index".into())
                        })?;
                    if decode_deadline(&ordered_key, &ordered)? != deadline {
                        return Err(PersistenceError("deadline indexes disagree".into()));
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
        self.crash_if(RedbCrashPoint::LaneCloseBeforeCommit);
        write_txn.commit().map_err(persist_err)?;
        Ok(result)
    }

    fn accept_ephemeral(
        &mut self,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
    ) -> Result<u64, PersistenceError> {
        // Receipt-ONLY: touches `OUTBOX_RECEIPTS` (+ `OUTBOX_META` for the
        // id allocation) alone — no `EVENTS` row, no `OUTBOX_INTENTS` row,
        // `intent_id: None` (nothing backs it).
        let write_txn = self.db.begin_write().map_err(persist_err)?;
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
            receipt_id
        };
        write_txn.commit().map_err(persist_err)?;
        Ok(receipt_id)
    }
}

fn decode_interval(json: &str) -> CoverageInterval {
    let record: CoverageRowRecord = serde_json::from_str(json).expect("redb: decode coverage row");
    CoverageInterval::new(
        Timestamp::from(record.from),
        Timestamp::from(record.through),
    )
}

#[cfg(test)]
mod crash_atomicity_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_event_epoch_is_rejected_before_any_v4_table_is_created() {
        const LEGACY_EVENTS_V2: TableDefinition<&str, &[u8]> = TableDefinition::new("events_v2");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-epoch.redb");
        let db = Database::create(&path).unwrap();
        let write_txn = db.begin_write().unwrap();
        write_txn.open_table(LEGACY_EVENTS_V2).unwrap();
        write_txn.commit().unwrap();
        drop(db);

        let error = match RedbStore::open(&path) {
            Ok(_) => panic!("legacy event epoch must not open as an empty v4 store"),
            Err(error) => error,
        };
        assert!(matches!(error, redb::Error::UpgradeRequired(4)));

        let db = Database::create(&path).unwrap();
        let read_txn = db.begin_read().unwrap();
        let table_names: BTreeSet<_> = read_txn
            .list_tables()
            .unwrap()
            .map(|table| table.name().to_owned())
            .collect();
        assert_eq!(table_names, BTreeSet::from(["events_v2".to_owned()]));
        assert!(!table_names.contains(EVENTS.name()));
    }

    fn accepted_signed(
        store: &mut RedbStore,
        keys: &nostr::Keys,
        content: &str,
        created_at: u64,
    ) -> (IntentId, Event) {
        use nostr::EventBuilder;

        let signed = EventBuilder::new(Kind::TextNote, content)
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .expect("sign fixture event");
        let frozen = Event::new(
            signed.id,
            signed.pubkey,
            signed.created_at,
            signed.kind,
            signed.tags.clone(),
            signed.content.clone(),
            crate::sentinel_signature(),
        );
        let outcome = store
            .accept_write(AcceptWrite {
                frozen,
                expected_pubkey: keys.public_key(),
                signing_identity_ref: "range-proof".into(),
                durability: WriteDurability::Durable,
                routing: "range-proof".into(),
                sig_state: IntentSigState::Pending,
                accepted_at: Timestamp::from(created_at),
            })
            .expect("accept fixture intent");
        let intent = outcome.journaled_intent_id().expect("intent id");
        store
            .promote_signed(intent, signed.sig)
            .expect("promote fixture intent");
        (intent, signed)
    }

    /// Issue #87's measurable bound: 128 unrelated intents must add zero
    /// visited rows to target-intent recovery, route revision allocation, or
    /// exact-relay attempt allocation. Relay URLs deliberately share textual
    /// prefixes, and intent 1 coexists with prefix-adversarial ids 10/100.
    #[test]
    fn outbox_ranges_visit_only_target_intent_and_exact_relay_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("outbox-ranges.redb");
        let mut store = RedbStore::open(&path).expect("open redb store");
        let keys = nostr::Keys::generate();
        let short = RelayUrl::parse("wss://prefix.example/x").unwrap();
        let extended = RelayUrl::parse("wss://prefix.example/x:443").unwrap();

        let (target, target_event) = accepted_signed(&mut store, &keys, "target", 1_000);
        assert_eq!(target, IntentId(1));
        store
            .record_route_revision(target, BTreeSet::from([short.clone(), extended.clone()]))
            .unwrap();
        store
            .record_route_revision(target, BTreeSet::from([short.clone()]))
            .unwrap();
        store
            .start_attempt(target, short.clone(), target_event.clone())
            .unwrap();
        store
            .start_attempt(target, extended.clone(), target_event.clone())
            .unwrap();

        for index in 0..128u64 {
            let (intent, event) =
                accepted_signed(&mut store, &keys, &format!("noise-{index}"), 2_000 + index);
            let relay = RelayUrl::parse(&format!("wss://noise-{index}.example")).unwrap();
            store
                .record_route_revision(intent, BTreeSet::from([relay.clone()]))
                .unwrap();
            store.start_attempt(intent, relay, event).unwrap();
        }
        store
            .finish_attempt(target, &short, 1, AttemptOutcome::GaveUp)
            .unwrap();

        store.reset_outbox_range_rows();
        let attempts = store.recover_attempts(target).unwrap();
        let revisions = store.recover_route_revisions(target).unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(revisions.len(), 2);
        assert_eq!(store.outbox_range_rows(), (2, 2));

        store.reset_outbox_range_rows();
        let next = store
            .start_attempt(target, short.clone(), target_event)
            .unwrap();
        assert_eq!(next.ordinal, 2);
        store
            .record_route_revision(target, BTreeSet::from([extended]))
            .unwrap();
        assert_eq!(
            store.outbox_range_rows(),
            (0, 2),
            "cursor allocation must not rescan retained attempt history"
        );
    }

    /// The durable-key falsifier for this fix: `coverage_row_key` must
    /// carry the FULL 32-byte BLAKE3 digest (64 hex chars), not a
    /// truncated 8-byte (16 hex char) prefix -- truncating back down to
    /// 64 bits in the on-disk key would silently undo the whole point of
    /// widening `DescriptorHash`/`CoverageKey` (a forged collision only
    /// needs to defeat whatever width actually reaches the durable key).
    #[test]
    fn coverage_row_key_carries_the_full_256_bit_digest() {
        let filter = ConcreteFilter {
            kinds: Some(std::collections::BTreeSet::from([1u16])),
            authors: Some(std::collections::BTreeSet::from(["aa".to_string()])),
            ..ConcreteFilter::default()
        };
        let atom = ContextualAtom {
            filter,
            source: nmp_grammar::SourceAuthority::AuthorOutboxes,
            access: nmp_grammar::AccessContext::Public,
        };
        let key = compute_coverage_key(&atom);
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let row_key = RedbStore::coverage_row_key(key, &relay);

        // Row key shape is now `<version-prefix><hex>:<relay>` (#106) --
        // skip the version prefix before taking the hex segment.
        let without_prefix = row_key
            .strip_prefix(RedbStore::COVERAGE_ROW_KEY_PREFIX)
            .expect("row key must carry the current schema-version prefix");
        let hex_part = without_prefix
            .split(':')
            .next()
            .expect("row key always has a hex-prefix:relay-url shape");
        assert_eq!(
            hex_part.len(),
            64,
            "expected 64 hex chars (32 bytes) in the durable key, got {} in {row_key:?}",
            hex_part.len()
        );
    }

    /// #106's legacy-purge falsifier: a coverage row written under the OLD
    /// (pre-#106, unversioned) key format is silently unreachable via
    /// `get_coverage` (its key never matches anything `record_coverage`
    /// computes anymore) and `gc` deletes it outright, tracked via
    /// `GcReport::legacy_coverage_rows_purged` (disjoint from the ordinary
    /// shrink/delete counters).
    #[test]
    fn gc_purges_legacy_unversioned_coverage_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let mut store = RedbStore::open(&db_path).unwrap();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();

        // Write a legacy-shaped row directly (bypassing `record_coverage`,
        // which always writes under the CURRENT version prefix) -- the
        // exact shape a pre-#106 row would have on disk.
        let legacy_shape = ConcreteFilter {
            kinds: Some(std::collections::BTreeSet::from([1u16])),
            authors: Some(std::collections::BTreeSet::from(["aa".to_string()])),
            ..ConcreteFilter::default()
        };
        let legacy_key = compute_coverage_key(&ContextualAtom {
            filter: legacy_shape.clone(),
            source: nmp_grammar::SourceAuthority::AuthorOutboxes,
            access: nmp_grammar::AccessContext::Public,
        });
        let mut legacy_hex = String::new();
        {
            use std::fmt::Write as _;
            for byte in legacy_key.as_bytes() {
                let _ = write!(legacy_hex, "{byte:02x}");
            }
        }
        let legacy_row_key = format!("{legacy_hex}:{}", relay.as_str());
        let legacy_record = CoverageRowRecord {
            shape: ShapeRecord::from(&legacy_shape),
            from: 0,
            through: 100,
        };
        {
            let write_txn = store.db.begin_write().unwrap();
            {
                let mut coverage = write_txn.open_table(COVERAGE).unwrap();
                coverage
                    .insert(
                        legacy_row_key.as_str(),
                        serde_json::to_string(&legacy_record).unwrap().as_str(),
                    )
                    .unwrap();
            }
            write_txn.commit().unwrap();
        }

        let report = store.gc(&ClaimSet::new(Vec::new())).unwrap();
        assert_eq!(
            report.legacy_coverage_rows_purged, 1,
            "the unversioned legacy row must be purged"
        );

        let read_txn = store.db.begin_read().unwrap();
        let coverage = read_txn.open_table(COVERAGE).unwrap();
        assert!(
            coverage.get(legacy_row_key.as_str()).unwrap().is_none(),
            "the legacy row must be gone after gc"
        );
    }

    /// The row-count falsifier for issue #17: an author-filtered `query`
    /// must decode (JSON-parse) only that author's own rows via
    /// `BY_AUTHOR`, never the whole `EVENTS` table -- the documented M5
    /// replay jank was `RedbStore::query` doing exactly that unbounded
    /// scan+decode on every refresh.
    #[test]
    fn query_by_author_does_not_scan_all_rows() {
        use nostr::EventBuilder;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("store.redb");
        let mut store = RedbStore::open(&path).expect("open redb store");
        let r1 = RelayUrl::parse("wss://r1").expect("relay url");

        let target = nostr::Keys::generate();
        let target_event = EventBuilder::new(Kind::TextNote, "hi")
            .sign_with_keys(&target)
            .expect("sign target event");
        let target_id = target_event.id;
        store
            .insert(
                target_event,
                RelayObserved::new(r1.clone(), Timestamp::from(1u64)),
            )
            .unwrap();

        // A pile of OTHER authors' rows -- large enough that a full-table
        // scan would dwarf the one-row match set below.
        for i in 0..200u64 {
            let noise_author = nostr::Keys::generate();
            let noise = EventBuilder::new(Kind::TextNote, "noise")
                .custom_created_at(Timestamp::from(100 + i))
                .sign_with_keys(&noise_author)
                .expect("sign noise event");
            store
                .insert(
                    noise,
                    RelayObserved::new(r1.clone(), Timestamp::from(100 + i)),
                )
                .unwrap();
        }

        let before = store.examined_rows();
        let results = store
            .query(&Filter::new().author(target.public_key()))
            .unwrap();
        let examined = store.examined_rows() - before;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event.id, target_id);
        assert_eq!(
            examined, 1,
            "author-filtered query decoded {examined} row(s) on a 201-row table; \
             expected exactly 1 (the match), not a full-table scan"
        );
    }

    fn room_event(keys: &nostr::Keys, room: &str, created_at: u64, content: &str) -> Event {
        use nostr::{EventBuilder, Tag};

        EventBuilder::new(Kind::from(9u16), content)
            .tag(Tag::parse(["h", room]).expect("valid h tag"))
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .expect("sign room event")
    }

    fn raw_canonical_row(store: &RedbStore, id: EventId) -> (EventKey, Vec<u8>, Option<Vec<u8>>) {
        let read_txn = store.db.begin_read().unwrap();
        let event_ids = read_txn.open_table(EVENT_IDS).unwrap();
        let events = read_txn.open_table(EVENTS).unwrap();
        let local = read_txn.open_table(EVENT_LOCAL).unwrap();
        let event_key = event_ids
            .get(id.as_bytes().as_slice())
            .unwrap()
            .expect("raw id mapping")
            .value();
        let event_bytes = events
            .get(event_key)
            .unwrap()
            .expect("raw event row")
            .value()
            .to_vec();
        let local_bytes = local
            .get(event_key)
            .unwrap()
            .map(|value| value.value().to_vec());
        (event_key, event_bytes, local_bytes)
    }

    fn raw_observation_rows(store: &RedbStore, event_key: EventKey) -> Vec<(Vec<u8>, u64)> {
        let read_txn = store.db.begin_read().unwrap();
        let observations = read_txn.open_table(EVENT_OBSERVATIONS).unwrap();
        let (lower, upper) = observation_range(event_key);
        observations
            .range::<&[u8; 12]>(&lower..=&upper)
            .unwrap()
            .map(|entry| {
                let (key, at) = entry.unwrap();
                (key.value().to_vec(), at.value())
            })
            .collect()
    }

    #[test]
    fn tag_index_packs_canonical_hex_ids_without_aliasing_other_strings() {
        let tag = SingleLetterTag::lowercase(nostr::Alphabet::P);
        let canonical = "ab".repeat(32);
        let packed = tag_index_prefix(tag, &canonical);
        assert_eq!(packed.len(), 1 + 1 + 32);
        assert_eq!(packed[1], 1);

        let uppercase = canonical.to_uppercase();
        let ordinary = tag_index_prefix(tag, &uppercase);
        assert_eq!(ordinary[1], 0);
        assert_ne!(ordinary, packed);
    }

    #[test]
    fn duplicate_observation_adds_one_fixed_row_without_rewriting_event_or_local_state() {
        use nostr::EventBuilder;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata-sidecar.redb");
        let mut store = RedbStore::open(&path).unwrap();
        let keys = nostr::Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "immutable body")
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&keys)
            .unwrap();
        let first = RelayUrl::parse("wss://first.example").unwrap();
        let second = RelayUrl::parse("wss://second.example").unwrap();
        store
            .insert(
                event.clone(),
                RelayObserved::new(first, Timestamp::from(20u64)),
            )
            .unwrap();
        let (event_key, before_event, before_local) = raw_canonical_row(&store, event.id);
        let before_observations = raw_observation_rows(&store, event_key);

        let outcome = store
            .insert(
                event.clone(),
                RelayObserved::new(second.clone(), Timestamp::from(30u64)),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            InsertOutcome::Duplicate {
                provenance_grew: true,
                ..
            }
        ));
        let (after_key, after_event, after_local) = raw_canonical_row(&store, event.id);
        assert_eq!(after_key, event_key, "surrogate identity is stable");
        assert_eq!(
            after_event, before_event,
            "immutable event bytes were rewritten"
        );
        assert_eq!(after_local, before_local, "local state was rewritten");
        let after_observations = raw_observation_rows(&store, event_key);
        assert_eq!(before_observations.len(), 1);
        assert_eq!(after_observations.len(), 2);
        assert_eq!(
            store.query(&Filter::new().id(event.id)).unwrap()[0]
                .provenance
                .seen
                .get(&second),
            Some(&Timestamp::from(30u64))
        );
    }

    #[test]
    fn equal_or_earlier_redelivery_is_a_true_physical_noop() {
        use nostr::EventBuilder;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata-noop.redb");
        let mut store = RedbStore::open(&path).unwrap();
        let keys = nostr::Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "no cow churn")
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&keys)
            .unwrap();
        let relay = RelayUrl::parse("wss://same.example").unwrap();
        store
            .insert(
                event.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(30u64)),
            )
            .unwrap();
        let before = raw_canonical_row(&store, event.id);
        let before_observations = raw_observation_rows(&store, before.0);

        let outcome = store
            .insert(
                event.clone(),
                RelayObserved::new(relay, Timestamp::from(20u64)),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            InsertOutcome::Duplicate {
                provenance_grew: false,
                ..
            }
        ));
        assert_eq!(raw_canonical_row(&store, event.id), before);
        assert_eq!(raw_observation_rows(&store, before.0), before_observations);
    }

    #[test]
    fn relay_dictionary_is_shared_refcounted_reclaimed_and_never_reuses_keys() {
        use nostr::EventBuilder;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay-refcounts.redb");
        let mut store = RedbStore::open(&path).unwrap();
        let keys = nostr::Keys::generate();
        let relay = RelayUrl::parse("wss://shared-relay.example").unwrap();
        let make_event = |created_at| {
            EventBuilder::new(Kind::TextNote, format!("event-{created_at}"))
                .custom_created_at(Timestamp::from(created_at))
                .sign_with_keys(&keys)
                .unwrap()
        };
        let first = make_event(1);
        let second = make_event(2);
        store
            .insert(
                first.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(10u64)),
            )
            .unwrap();
        store
            .insert(
                second.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(20u64)),
            )
            .unwrap();

        let first_relay_key = {
            let read_txn = store.db.begin_read().unwrap();
            let relay_keys = read_txn.open_table(RELAY_KEYS).unwrap();
            let relay_refs = read_txn.open_table(RELAY_REFS).unwrap();
            let relay_key = relay_keys.get(relay.as_str()).unwrap().unwrap().value();
            assert_eq!(relay_refs.get(relay_key).unwrap().unwrap().value(), 2);
            relay_key
        };
        assert_canonical_integrity(&store.db);

        store.remove(first.id, RetractReason::Deleted).unwrap();
        {
            let read_txn = store.db.begin_read().unwrap();
            let relay_refs = read_txn.open_table(RELAY_REFS).unwrap();
            assert_eq!(relay_refs.get(first_relay_key).unwrap().unwrap().value(), 1);
        }
        assert_canonical_integrity(&store.db);

        store.remove(second.id, RetractReason::Deleted).unwrap();
        {
            let read_txn = store.db.begin_read().unwrap();
            assert!(read_txn
                .open_table(RELAY_KEYS)
                .unwrap()
                .get(relay.as_str())
                .unwrap()
                .is_none());
            assert_eq!(read_txn.open_table(RELAYS).unwrap().len().unwrap(), 0);
            assert_eq!(read_txn.open_table(RELAY_REFS).unwrap().len().unwrap(), 0);
            assert_eq!(
                read_txn
                    .open_table(EVENT_OBSERVATIONS)
                    .unwrap()
                    .len()
                    .unwrap(),
                0
            );
        }
        assert_canonical_integrity(&store.db);

        let third = make_event(3);
        store
            .insert(
                third,
                RelayObserved::new(relay.clone(), Timestamp::from(30u64)),
            )
            .unwrap();
        let read_txn = store.db.begin_read().unwrap();
        let new_relay_key = read_txn
            .open_table(RELAY_KEYS)
            .unwrap()
            .get(relay.as_str())
            .unwrap()
            .unwrap()
            .value();
        assert!(new_relay_key > first_relay_key);
    }

    #[test]
    fn batch_relay_refcounts_flush_once_per_distinct_relay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay-refcount-batch.redb");
        let store = RedbStore::open(&path).unwrap();
        let relay = RelayUrl::parse("wss://one-hot-refcount.example").unwrap();
        let write_txn = store.db.begin_write().unwrap();
        {
            let mut canonical = CanonicalWriteTables::open(&write_txn).unwrap();
            let relay_key = canonical.intern_relay(&relay).unwrap();
            for _ in 0..1_114 {
                canonical.increment_relay_ref(relay_key).unwrap();
            }
            assert_eq!(canonical.relay_ref_counts.len(), 1);
            assert_eq!(canonical.relay_ref_counts[&relay_key], 1_114);
            assert_eq!(
                canonical
                    .relay_refs
                    .get(relay_key)
                    .unwrap()
                    .unwrap()
                    .value(),
                0,
                "the durable hot row stays untouched until the batch flush"
            );
            canonical.flush_counts().unwrap();
            assert!(canonical.relay_ref_counts.is_empty());
            assert_eq!(
                canonical
                    .relay_refs
                    .get(relay_key)
                    .unwrap()
                    .unwrap()
                    .value(),
                1_114
            );
        }
        // This is a white-box write-coalescing proof, not a valid canonical
        // store state, so abort rather than committing the synthetic count.
        write_txn.abort().unwrap();
    }

    #[test]
    fn batch_net_zero_observation_reclaims_new_relay_dictionary_row() {
        use nostr::EventBuilder;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay-refcount-net-zero.redb");
        let mut store = RedbStore::open(&path).unwrap();
        let keys = nostr::Keys::generate();
        let old = EventBuilder::new(Kind::ContactList, "old")
            .custom_created_at(Timestamp::from(1u64))
            .sign_with_keys(&keys)
            .unwrap();
        let new = EventBuilder::new(Kind::ContactList, "new")
            .custom_created_at(Timestamp::from(2u64))
            .sign_with_keys(&keys)
            .unwrap();
        let old_relay = RelayUrl::parse("wss://superseded-in-batch.example").unwrap();
        let new_relay = RelayUrl::parse("wss://winner-in-batch.example").unwrap();

        let outcomes = store
            .insert_batch(vec![
                (
                    old,
                    RelayObserved::new(old_relay.clone(), Timestamp::from(1u64)),
                ),
                (
                    new,
                    RelayObserved::new(new_relay.clone(), Timestamp::from(2u64)),
                ),
            ])
            .unwrap();
        assert!(matches!(outcomes[0], InsertOutcome::Inserted));
        assert!(matches!(outcomes[1], InsertOutcome::Superseded { .. }));
        assert_canonical_integrity(&store.db);

        let read_txn = store.db.begin_read().unwrap();
        let relay_keys = read_txn.open_table(RELAY_KEYS).unwrap();
        assert!(relay_keys.get(old_relay.as_str()).unwrap().is_none());
        let winner_key = relay_keys.get(new_relay.as_str()).unwrap().unwrap().value();
        assert_eq!(
            read_txn
                .open_table(RELAY_REFS)
                .unwrap()
                .get(winner_key)
                .unwrap()
                .unwrap()
                .value(),
            1
        );
    }

    #[test]
    fn later_same_relay_updates_only_one_timestamp_value() {
        use nostr::EventBuilder;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay-timestamp.redb");
        let mut store = RedbStore::open(&path).unwrap();
        let keys = nostr::Keys::generate();
        let relay = RelayUrl::parse("wss://timestamp-relay.example").unwrap();
        let event = EventBuilder::new(Kind::TextNote, "timestamp")
            .custom_created_at(Timestamp::from(1u64))
            .sign_with_keys(&keys)
            .unwrap();
        store
            .insert(
                event.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(10u64)),
            )
            .unwrap();
        let canonical_before = raw_canonical_row(&store, event.id);
        let before = raw_observation_rows(&store, canonical_before.0);

        let outcome = store
            .insert(
                event.clone(),
                RelayObserved::new(relay, Timestamp::from(20u64)),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            InsertOutcome::Duplicate {
                provenance_grew: true,
                ..
            }
        ));
        assert_eq!(raw_canonical_row(&store, event.id), canonical_before);
        let after = raw_observation_rows(&store, canonical_before.0);
        assert_eq!(before.len(), 1);
        assert_eq!(after.len(), 1);
        assert_eq!(before[0].0, after[0].0);
        assert_eq!(before[0].1, 10);
        assert_eq!(after[0].1, 20);
        let read_txn = store.db.begin_read().unwrap();
        let relay_refs = read_txn.open_table(RELAY_REFS).unwrap();
        assert_eq!(
            relay_refs
                .iter()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .1
                .value(),
            1
        );
    }

    #[test]
    fn surrogate_keys_are_monotonic_and_never_reused_after_remove_or_reopen() {
        use nostr::EventBuilder;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("surrogate-keys.redb");
        let keys = nostr::Keys::generate();
        let relay = RelayUrl::parse("wss://surrogates.example").unwrap();
        let make_event = |created_at| {
            EventBuilder::new(Kind::TextNote, format!("event-{created_at}"))
                .custom_created_at(Timestamp::from(created_at))
                .sign_with_keys(&keys)
                .unwrap()
        };

        let first = make_event(1);
        let second = make_event(2);
        let third = make_event(3);
        let mut store = RedbStore::open(&path).unwrap();
        store
            .insert(
                first.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(10u64)),
            )
            .unwrap();
        let first_key = raw_canonical_row(&store, first.id).0;
        store.remove(first.id, RetractReason::Expired).unwrap();
        store
            .insert(
                second.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(20u64)),
            )
            .unwrap();
        let second_key = raw_canonical_row(&store, second.id).0;
        assert!(second_key > first_key);

        drop(store);
        let mut reopened = RedbStore::open(&path).unwrap();
        reopened
            .insert(
                third.clone(),
                RelayObserved::new(relay, Timestamp::from(30u64)),
            )
            .unwrap();
        let third_key = raw_canonical_row(&reopened, third.id).0;
        assert!(third_key > second_key);
        assert_canonical_integrity(&reopened.db);
    }

    #[test]
    fn canonical_integrity_survives_every_governed_event_mutation_class() {
        use nostr::{EventBuilder, Tag};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("governed-integrity.redb");
        let mut store = RedbStore::open(&path).unwrap();
        let keys = nostr::Keys::generate();
        let relay1 = RelayUrl::parse("wss://integrity-one.example").unwrap();
        let relay2 = RelayUrl::parse("wss://integrity-two.example").unwrap();
        let observed = |relay: RelayUrl, at| RelayObserved::new(relay, Timestamp::from(at));

        let target = EventBuilder::new(Kind::TextNote, "target")
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&keys)
            .unwrap();
        store
            .insert(target.clone(), observed(relay1.clone(), 10))
            .unwrap();
        store
            .insert(target.clone(), observed(relay2.clone(), 11))
            .unwrap();
        assert_canonical_integrity(&store.db);

        let replaceable_old = EventBuilder::new(Kind::ContactList, "old")
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&keys)
            .unwrap();
        let replaceable_new = EventBuilder::new(Kind::ContactList, "new")
            .custom_created_at(Timestamp::from(30u64))
            .sign_with_keys(&keys)
            .unwrap();
        store
            .insert(replaceable_old, observed(relay1.clone(), 20))
            .unwrap();
        store
            .insert(replaceable_new, observed(relay1.clone(), 30))
            .unwrap();
        assert_canonical_integrity(&store.db);

        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target.id))
            .custom_created_at(Timestamp::from(40u64))
            .sign_with_keys(&keys)
            .unwrap();
        store
            .insert(deletion, observed(relay1.clone(), 40))
            .unwrap();
        assert_canonical_integrity(&store.db);

        let expiring = EventBuilder::new(Kind::TextNote, "expiring")
            .tag(Tag::expiration(Timestamp::from(60u64)))
            .custom_created_at(Timestamp::from(50u64))
            .sign_with_keys(&keys)
            .unwrap();
        store
            .insert(expiring, observed(relay1.clone(), 50))
            .unwrap();
        store.expire_due(Timestamp::from(60u64)).unwrap();
        assert_canonical_integrity(&store.db);

        let gc_candidate = EventBuilder::new(Kind::TextNote, "gc")
            .custom_created_at(Timestamp::from(70u64))
            .sign_with_keys(&keys)
            .unwrap();
        store
            .insert(gc_candidate, observed(relay1.clone(), 70))
            .unwrap();
        store.gc(&ClaimSet::new(Vec::new())).unwrap();
        assert_canonical_integrity(&store.db);

        let signed = EventBuilder::new(Kind::TextNote, "pending")
            .custom_created_at(Timestamp::from(80u64))
            .sign_with_keys(&keys)
            .unwrap();
        let frozen = Event::new(
            signed.id,
            signed.pubkey,
            signed.created_at,
            signed.kind,
            signed.tags.clone(),
            signed.content.clone(),
            crate::sentinel_signature(),
        );
        let accepted = store
            .accept_write(AcceptWrite {
                frozen,
                expected_pubkey: keys.public_key(),
                signing_identity_ref: "integrity".into(),
                durability: WriteDurability::Durable,
                routing: "integrity".into(),
                sig_state: IntentSigState::Pending,
                accepted_at: Timestamp::from(80u64),
            })
            .unwrap();
        assert_canonical_integrity(&store.db);
        store
            .compensate_write(accepted.journaled_intent_id().unwrap())
            .unwrap();
        assert_canonical_integrity(&store.db);
    }

    #[test]
    fn query_by_single_letter_tag_decodes_only_that_tag_bucket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tag-index.redb");
        let mut store = RedbStore::open(&path).expect("open redb store");
        let keys = nostr::Keys::generate();
        let relay = RelayUrl::parse("wss://groups.example").unwrap();

        for i in 0..12u64 {
            store
                .insert(
                    room_event(&keys, "target", 1_000 + i, &format!("target-{i}")),
                    RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
                )
                .unwrap();
        }
        for i in 0..200u64 {
            store
                .insert(
                    room_event(&keys, "noise", 3_000 + i, &format!("noise-{i}")),
                    RelayObserved::new(relay.clone(), Timestamp::from(4_000 + i)),
                )
                .unwrap();
        }

        let filter = Filter::new()
            .kind(Kind::from(9u16))
            .custom_tag(SingleLetterTag::lowercase(nostr::Alphabet::H), "target");
        let before = store.examined_rows();
        let rows = store.query(&filter).unwrap();
        let examined = store.examined_rows() - before;
        assert_eq!(rows.len(), 12);
        assert_eq!(examined, 12, "noise-room rows must never be decoded");
    }

    #[test]
    fn query_newest_tag_scan_stops_before_decoding_past_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tag-limit.redb");
        let mut store = RedbStore::open(&path).expect("open redb store");
        let keys = nostr::Keys::generate();
        let relay = RelayUrl::parse("wss://groups.example").unwrap();

        for i in 0..240u64 {
            store
                .insert(
                    room_event(&keys, "target", 1_000 + i, &format!("target-{i}")),
                    RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
                )
                .unwrap();
        }

        let filter = Filter::new()
            .kind(Kind::from(9u16))
            .custom_tag(SingleLetterTag::lowercase(nostr::Alphabet::H), "target");
        let before = store.examined_rows();
        let rows = store.query_newest(&filter, 25).unwrap();
        let examined = store.examined_rows() - before;

        assert_eq!(rows.len(), 25);
        assert_eq!(examined, 25, "rows past the top-N must not be decoded");
        assert!(rows
            .windows(2)
            .all(|pair| pair[0].event.created_at >= pair[1].event.created_at));
        assert_eq!(rows[0].event.created_at, Timestamp::from(1_239u64));
        assert_eq!(rows[24].event.created_at, Timestamp::from(1_215u64));
    }

    #[test]
    fn query_newest_postfilters_binary_views_before_event_materialization() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("binary-postfilter.redb");
        let mut store = RedbStore::open(&path).expect("open redb store");
        let wanted = nostr::Keys::generate();
        let noise = nostr::Keys::generate();
        let relay = RelayUrl::parse("wss://groups.example").unwrap();

        store
            .insert(
                room_event(&wanted, "target", 1_000, "wanted"),
                RelayObserved::new(relay.clone(), Timestamp::from(2_000u64)),
            )
            .unwrap();
        for i in 0..200u64 {
            store
                .insert(
                    room_event(&noise, "target", 2_000 + i, &format!("noise-{i}")),
                    RelayObserved::new(relay.clone(), Timestamp::from(3_000 + i)),
                )
                .unwrap();
        }

        let filter = Filter::new().kind(Kind::from(9u16)).search("wanted");
        let before = store.examined_rows();
        let rows = store.query_newest(&filter, 1).unwrap();
        let materialized = store.examined_rows() - before;

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event.pubkey, wanted.public_key());
        assert_eq!(
            materialized, 1,
            "200 newer kind-index candidates rejected by search must stay borrowed binary views; only the returned row becomes an owned Event"
        );
    }

    #[test]
    fn query_newest_kind_and_global_scans_stop_at_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ordered-limit.redb");
        let mut store = RedbStore::open(&path).expect("open redb store");
        let keys = nostr::Keys::generate();
        let relay = RelayUrl::parse("wss://groups.example").unwrap();

        for i in 0..240u64 {
            store
                .insert(
                    room_event(&keys, "target", 1_000 + i, &format!("event-{i}")),
                    RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
                )
                .unwrap();
        }

        let before = store.examined_rows();
        let kind_rows = store
            .query_newest(&Filter::new().kind(Kind::from(9u16)), 25)
            .unwrap();
        assert_eq!(kind_rows.len(), 25);
        assert_eq!(store.examined_rows() - before, 25);
        assert_eq!(kind_rows[0].event.created_at, Timestamp::from(1_239u64));

        let before = store.examined_rows();
        let global_rows = store.query_newest(&Filter::new(), 17).unwrap();
        assert_eq!(global_rows.len(), 17);
        assert_eq!(store.examined_rows() - before, 17);
        assert_eq!(global_rows[0].event.created_at, Timestamp::from(1_239u64));
    }

    #[test]
    fn query_newest_merges_multiple_tag_values_in_global_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tag-merge.redb");
        let mut store = RedbStore::open(&path).expect("open redb store");
        let keys = nostr::Keys::generate();
        let relay = RelayUrl::parse("wss://groups.example").unwrap();

        for (room, created_at) in [("a", 100), ("b", 104), ("a", 103), ("b", 101)] {
            store
                .insert(
                    room_event(&keys, room, created_at, room),
                    RelayObserved::new(relay.clone(), Timestamp::from(created_at + 1)),
                )
                .unwrap();
        }

        let filter =
            Filter::new().custom_tags(SingleLetterTag::lowercase(nostr::Alphabet::H), ["a", "b"]);
        let rows = store.query_newest(&filter, 3).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.event.created_at.as_secs())
                .collect::<Vec<_>>(),
            vec![104, 103, 101]
        );
    }

    #[test]
    fn query_newest_tag_scan_uses_id_ascending_tie_break() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tag-tie-break.redb");
        let mut store = RedbStore::open(&path).expect("open redb store");
        let keys = nostr::Keys::generate();
        let relay = RelayUrl::parse("wss://groups.example").unwrap();
        let mut expected = Vec::new();

        for i in 0..8u64 {
            let event = room_event(&keys, "target", 1_000, &format!("target-{i}"));
            expected.push(event.id);
            store
                .insert(
                    event,
                    RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
                )
                .unwrap();
        }
        expected.sort();

        let filter =
            Filter::new().custom_tag(SingleLetterTag::lowercase(nostr::Alphabet::H), "target");
        let rows = store.query_newest(&filter, 3).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.event.id).collect::<Vec<_>>(),
            expected[..3]
        );
    }

    #[test]
    fn cardinality_planner_selects_smallest_real_tag_bucket_for_complete_query() {
        use nostr::{Alphabet, EventBuilder, Tag};

        let dir = tempfile::tempdir().unwrap();
        let mut store = RedbStore::open(dir.path().join("cardinality-plan.redb")).unwrap();
        let keys = nostr::Keys::generate();
        let member = nostr::Keys::generate().public_key().to_hex();
        let relay = RelayUrl::parse("wss://cardinality.example").unwrap();
        let h = SingleLetterTag::lowercase(Alphabet::H);
        let p = SingleLetterTag::lowercase(Alphabet::P);

        for i in 0..100u64 {
            let mut builder = EventBuilder::new(Kind::from(9u16), format!("room-{i}"))
                .tag(Tag::parse(["h", "busy-room"]).unwrap());
            if i < 5 {
                builder = builder.tag(Tag::parse(["p", member.as_str()]).unwrap());
            }
            let event = builder
                .custom_created_at(Timestamp::from(1_000 + i))
                .sign_with_keys(&keys)
                .unwrap();
            store
                .insert(
                    event,
                    RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
                )
                .unwrap();
        }
        // Same rare #p but the wrong #h: proves the chosen-tag matched mask
        // skips only #p, not every tag predicate.
        let wrong_room = EventBuilder::new(Kind::from(9u16), "wrong-room")
            .tags([
                Tag::parse(["h", "other-room"]).unwrap(),
                Tag::parse(["p", member.as_str()]).unwrap(),
            ])
            .custom_created_at(Timestamp::from(2_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        store
            .insert(
                wrong_room,
                RelayObserved::new(relay, Timestamp::from(3_000u64)),
            )
            .unwrap();

        let filter = Filter::new()
            .kind(Kind::from(9u16))
            .custom_tag(h, "busy-room")
            .custom_tag(p, member);
        let read_txn = store.db.begin_read().unwrap();
        let plan = plan_ordered_query(&read_txn, &filter).unwrap();
        assert_eq!(plan.index, OrderedIndex::Tag(p));
        assert_eq!(plan.estimated_rows, 6);
        drop(read_txn);

        store.reset_query_work();
        let rows = store.query(&filter).unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(store.query_work(), (6, 6, 5));
        assert_canonical_integrity(&store.db);
    }

    #[test]
    fn cardinality_planner_never_materializes_unbounded_author_kind_products() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path().join("bounded-composite-plan.redb")).unwrap();
        let authors: BTreeSet<_> = (0..65)
            .map(|_| nostr::Keys::generate().public_key())
            .collect();
        let kinds: BTreeSet<_> = (0..65u16).map(Kind::from).collect();
        assert!(authors.len() * kinds.len() > MAX_COMPOSITE_QUERY_RANGES);

        let filter = Filter::new().authors(authors).kinds(kinds);
        let read_txn = store.db.begin_read().unwrap();
        let plan = plan_ordered_query(&read_txn, &filter).unwrap();
        assert_eq!(plan.index, OrderedIndex::Author);
        assert_eq!(plan.prefixes.len(), 65);
    }

    #[test]
    fn empty_filter_sets_and_reversed_windows_match_nostr_semantics() {
        use nostr::{Alphabet, EventBuilder};

        let dir = tempfile::tempdir().unwrap();
        let mut store = RedbStore::open(dir.path().join("empty-filter-sets.redb")).unwrap();
        let keys = nostr::Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "one")
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&keys)
            .unwrap();
        store
            .insert(
                event,
                RelayObserved::new(
                    RelayUrl::parse("wss://empty-sets.example").unwrap(),
                    Timestamp::from(10u64),
                ),
            )
            .unwrap();

        for filter in [
            Filter {
                ids: Some(BTreeSet::new()),
                ..Filter::new()
            },
            Filter {
                authors: Some(BTreeSet::new()),
                ..Filter::new()
            },
            Filter {
                kinds: Some(BTreeSet::new()),
                ..Filter::new()
            },
        ] {
            assert_eq!(store.query(&filter).unwrap().len(), 1);
            assert_eq!(store.query_newest(&filter, 10).unwrap().len(), 1);
        }

        let mut impossible_tag = Filter::new();
        impossible_tag
            .generic_tags
            .insert(SingleLetterTag::lowercase(Alphabet::H), BTreeSet::new());
        assert!(store.query(&impossible_tag).unwrap().is_empty());
        assert!(store.query_newest(&impossible_tag, 10).unwrap().is_empty());

        let reversed = Filter::new()
            .since(Timestamp::from(11u64))
            .until(Timestamp::from(10u64));
        assert!(store.query(&reversed).unwrap().is_empty());
        assert!(store.query_newest(&reversed, 10).unwrap().is_empty());
    }

    #[test]
    fn missing_cardinality_epoch_rebuilds_atomically_from_ordered_indexes() {
        use nostr::EventBuilder;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cardinality-rebuild.redb");
        let keys = nostr::Keys::generate();
        let relay = RelayUrl::parse("wss://cardinality-rebuild.example").unwrap();
        let mut store = RedbStore::open(&path).unwrap();
        for i in 0..7u64 {
            let event = EventBuilder::new(Kind::TextNote, format!("row-{i}"))
                .custom_created_at(Timestamp::from(i + 1))
                .sign_with_keys(&keys)
                .unwrap();
            store
                .insert(
                    event,
                    RelayObserved::new(relay.clone(), Timestamp::from(i + 1)),
                )
                .unwrap();
        }
        drop(store);

        let db = Database::create(&path).unwrap();
        let write_txn = db.begin_write().unwrap();
        {
            let mut meta = write_txn.open_table(INDEX_CARDINALITY_META).unwrap();
            meta.remove(INDEX_CARDINALITY_VERSION_KEY).unwrap();
            let mut cardinality = write_txn.open_table(INDEX_CARDINALITY).unwrap();
            cardinality
                .insert(global_cardinality_key().as_slice(), 999)
                .unwrap();
        }
        write_txn.commit().unwrap();
        drop(db);

        let reopened = RedbStore::open(&path).unwrap();
        assert_eq!(reopened.query(&Filter::new()).unwrap().len(), 7);
        assert_canonical_integrity(&reopened.db);
    }

    #[test]
    fn multi_value_tag_merge_deduplicates_one_event_without_candidate_set() {
        use nostr::{Alphabet, EventBuilder, Tag};

        let dir = tempfile::tempdir().unwrap();
        let mut store = RedbStore::open(dir.path().join("tag-overlap.redb")).unwrap();
        let keys = nostr::Keys::generate();
        let event = EventBuilder::new(Kind::from(9u16), "both")
            .tags([
                Tag::parse(["h", "a"]).unwrap(),
                Tag::parse(["h", "b"]).unwrap(),
            ])
            .custom_created_at(Timestamp::from(100u64))
            .sign_with_keys(&keys)
            .unwrap();
        store
            .insert(
                event.clone(),
                RelayObserved::new(
                    RelayUrl::parse("wss://tag-overlap.example").unwrap(),
                    Timestamp::from(100u64),
                ),
            )
            .unwrap();
        let filter = Filter::new().custom_tags(SingleLetterTag::lowercase(Alphabet::H), ["a", "b"]);
        store.reset_query_work();
        let rows = store.query(&filter).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event.id, event.id);
        let (_index_rows, event_values, materialized) = store.query_work();
        assert_eq!(event_values, 1);
        assert_eq!(materialized, 1);
        assert_canonical_integrity(&store.db);
    }

    #[test]
    fn cardinality_planner_is_differentially_equivalent_over_mixed_filters() {
        use nostr::{Alphabet, EventBuilder, Tag};

        fn next(state: &mut u64) -> u64 {
            *state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *state
        }

        let dir = tempfile::tempdir().unwrap();
        let mut redb = RedbStore::open(dir.path().join("planner-differential.redb")).unwrap();
        let mut memory = crate::MemoryStore::new();
        let authors: Vec<_> = (0..8).map(|_| nostr::Keys::generate()).collect();
        let relay = RelayUrl::parse("wss://planner-differential.example").unwrap();
        let mut events = Vec::new();
        for i in 0..120u64 {
            let kind = Kind::from([1u16, 9, 42][(i as usize) % 3]);
            let content = if i % 9 == 0 {
                format!("needle-{i}")
            } else {
                format!("ordinary-{i}")
            };
            let mut tags = vec![
                Tag::parse(vec!["h".to_owned(), format!("room-{}", i % 7)]).unwrap(),
                Tag::parse(vec!["p".to_owned(), format!("member-{}", i % 11)]).unwrap(),
            ];
            if i % 10 == 0 {
                tags.push(
                    Tag::parse(vec!["h".to_owned(), format!("room-{}", (i + 1) % 7)]).unwrap(),
                );
            }
            let event = EventBuilder::new(kind, content)
                .tags(tags)
                .custom_created_at(Timestamp::from(1_000 + (i * 17) % 97))
                .sign_with_keys(&authors[(i as usize) % authors.len()])
                .unwrap();
            let observed = RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i));
            redb.insert(event.clone(), observed.clone()).unwrap();
            memory.insert(event.clone(), observed).unwrap();
            events.push(event);
        }

        let h = SingleLetterTag::lowercase(Alphabet::H);
        let p = SingleLetterTag::lowercase(Alphabet::P);
        let mut state = 0x169_cafe_f00d_u64;
        for round in 0..100u64 {
            let random = next(&mut state);
            let mut filter = Filter::new();
            if round % 5 == 0 {
                filter.ids = Some(if round % 20 == 0 {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([
                        events[(random as usize) % events.len()].id,
                        events[((random >> 8) as usize) % events.len()].id,
                    ])
                });
            }
            if round % 3 == 0 {
                filter.authors = Some(if round % 21 == 0 {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([
                        authors[(random as usize) % authors.len()].public_key(),
                        authors[((random >> 5) as usize) % authors.len()].public_key(),
                    ])
                });
            }
            if round % 4 == 0 {
                filter.kinds = Some(if round % 28 == 0 {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([Kind::from([1u16, 9, 42][((random >> 11) as usize) % 3])])
                });
            }
            if round % 2 == 0 {
                filter.generic_tags.insert(
                    h,
                    if round % 22 == 0 {
                        BTreeSet::new()
                    } else {
                        BTreeSet::from([
                            format!("room-{}", (random >> 17) % 7),
                            format!("room-{}", (random >> 23) % 7),
                        ])
                    },
                );
            }
            if round % 6 == 0 {
                filter.generic_tags.insert(
                    p,
                    BTreeSet::from([format!("member-{}", (random >> 29) % 11)]),
                );
            }
            if round % 7 == 0 {
                filter.search = Some("needle".to_owned());
            }
            if round % 8 == 0 {
                filter.since = Some(Timestamp::from(1_020 + (random % 30)));
                filter.until = Some(Timestamp::from(1_050 + ((random >> 7) % 30)));
            }
            if round % 31 == 0 {
                filter.since = Some(Timestamp::from(1_100u64));
                filter.until = Some(Timestamp::from(1_000u64));
            }

            let redb_complete: BTreeSet<_> = redb
                .query(&filter)
                .unwrap()
                .into_iter()
                .map(|row| row.event.id)
                .collect();
            let memory_complete: BTreeSet<_> = memory
                .query(&filter)
                .unwrap()
                .into_iter()
                .map(|row| row.event.id)
                .collect();
            assert_eq!(redb_complete, memory_complete, "complete round {round}");

            let limit = 1 + (random as usize % 12);
            let redb_newest: Vec<_> = redb
                .query_newest(&filter, limit)
                .unwrap()
                .into_iter()
                .map(|row| row.event.id)
                .collect();
            let memory_newest: Vec<_> = memory
                .query_newest(&filter, limit)
                .unwrap()
                .into_iter()
                .map(|row| row.event.id)
                .collect();
            assert_eq!(redb_newest, memory_newest, "bounded round {round}");
        }
        assert_canonical_integrity(&redb.db);
    }
}
