//! [`RedbStore`] — the persistent, `redb`-backed `EventStore` (M3 step A1).
//!
//! Canonical event and displaced-row values use a portable binary envelope:
//! raw fixed-width identity/signature/time/kind fields plus length-delimited
//! content, tags, and provenance. Queries borrow those fields directly from
//! redb value guards and materialize an owned `nostr::Event` only for rows
//! actually returned. Other outbox/coverage metadata remains typed JSON.
//!
//! `redb`'s own errors (`TableError`/`StorageError`/…) are all invariant
//! violations for this crate's purposes — a healthy embedded DB file does
//! not fail to open a table it created itself, or fail to commit a
//! transaction it started — so they are `.expect()`ed rather than threaded
//! through `EventStore`'s trait signatures (which, matching `MemoryStore`,
//! carry no `Result` at all). A real I/O error here means the on-disk file
//! is corrupt, not a reachable, recoverable condition this crate's callers
//! could usefully branch on today.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Bound::{Excluded, Included};
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use nmp_grammar::{ConcreteFilter, ContextualAtom};
use nostr::secp256k1::schnorr::Signature;
use nostr::{
    Event, EventId, Filter, JsonUtil, Kind, PublicKey, RelayUrl, SingleLetterTag, Timestamp,
};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::address_key::{address_key_for, address_key_for_coordinate, candidate_wins};
use crate::binary_event::{self, StoredEventView};
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

const LEGACY_EVENTS: TableDefinition<&str, &str> = TableDefinition::new("events");
const EVENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("events_v2");
const STORE_META: TableDefinition<&str, &str> = TableDefinition::new("store_meta");
const EVENT_RECORD_VERSION_KEY: &str = "event_record_version";
const EVENT_RECORD_VERSION: &str = "1";
const TAG_INDEX_VERSION_KEY: &str = "tag_index_version";
const TAG_INDEX_VERSION: &str = "1";
const ADDR_INDEX: TableDefinition<&str, &str> = TableDefinition::new("addr_index");
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
/// event id hex (redundant with the key suffix, kept for a cheap decode).
const EXPIRATION_INDEX: TableDefinition<&str, &str> = TableDefinition::new("expiration_index");
const LEGACY_BY_AUTHOR: TableDefinition<&str, &str> = TableDefinition::new("by_author");
const LEGACY_BY_KIND: TableDefinition<&str, &str> = TableDefinition::new("by_kind");
/// Binary ordered indexes all end in the same sortable suffix:
/// `created_at:u64-be | !event_id:[u8;32]`. Reverse scans therefore yield
/// `created_at DESC, event_id ASC` and can stop exactly at the visible limit.
const BY_CREATED_AT: TableDefinition<&[u8], ()> = TableDefinition::new("by_created_at_v1");
const BY_AUTHOR: TableDefinition<&[u8], ()> = TableDefinition::new("by_author_time_v1");
const BY_KIND: TableDefinition<&[u8], ()> = TableDefinition::new("by_kind_time_v1");
const BY_AUTHOR_KIND: TableDefinition<&[u8], ()> = TableDefinition::new("by_author_kind_time_v1");
const ORDERED_INDEX_VERSION_KEY: &str = "ordered_index_version";
const ORDERED_INDEX_VERSION: &str = "1";
/// NIP-01 single-letter tag index, borrowing nostrdb's clustered
/// `(tag,value,created_at)` layout. The binary key is:
///
/// `tag:u8 | value_len:u32-be | value:utf8 | created_at:u64-be | !event_id:[u8;32]`
///
/// Big-endian timestamp bytes make redb's ordinary byte ordering usable as a
/// newest-first reverse range scan. The event id suffix both disambiguates
/// equal timestamps. The id bytes are inverted so a reverse scan is
/// `created_at DESC, event_id ASC`, NMP's canonical NIP-01 tie-break, without
/// parsing hex.
/// Values are unit: every fact needed by the planner is already in the key.
const BY_TAG: TableDefinition<&[u8], ()> = TableDefinition::new("by_tag_v1");
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
/// `OUTBOX_INTENTS` row. Value reuses the exact portable binary `EVENTS`
/// encoding — see [`encode_stored_event`]/[`decode_stored_event`] — so the
/// stash round-trips through the same
/// decode path as any other row. Deleted durably by `promote_signed` (R6)
/// or `compensate_write`; never by `recover_outbox` (read-only).
const LEGACY_OUTBOX_DISPLACED: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_displaced");
const OUTBOX_DISPLACED: TableDefinition<&str, &[u8]> = TableDefinition::new("outbox_displaced_v2");
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

#[derive(Debug, Serialize, Deserialize)]
struct LegacyStoredEventRecord {
    event_json: String,
    provenance: BTreeMap<RelayUrl, Timestamp>,
    #[serde(default)]
    local: Option<LocalOrigin>,
}

fn decode_legacy_stored_event(json: &str) -> StoredEvent {
    let record: LegacyStoredEventRecord =
        serde_json::from_str(json).expect("redb: decode legacy stored event");
    StoredEvent {
        event: Event::from_json(record.event_json).expect("redb: decode legacy event json"),
        provenance: Provenance {
            seen: record.provenance,
            local: record.local,
        },
    }
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

/// Convert `se` into the `StoredEventRecord` any `EVENTS`/`OUTBOX_DISPLACED`
/// row encodes — shared by [`encode_stored_event`] so the two never drift
/// on field mapping.
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

/// Encode `se` exactly as the `EVENTS` table stores a row — shared by every
/// door that writes a full [`StoredEvent`] back out (the durable
/// `OUTBOX_DISPLACED` stash, `compensate_write`'s restore path).
fn encode_stored_event(se: &StoredEvent) -> Vec<u8> {
    binary_event::encode(se).expect("redb: encode portable stored event")
}

/// Materialize one portable `EVENTS`/`OUTBOX_DISPLACED` value — the
/// read-side counterpart of [`encode_stored_event`].
fn decode_stored_event(bytes: &[u8]) -> StoredEvent {
    StoredEventView::parse(bytes)
        .expect("redb: decode portable stored event")
        .materialize()
        .expect("redb: materialize portable stored event")
}

fn decode_stored_event_record(bytes: &[u8]) -> StoredEventRecord {
    stored_event_to_record(&decode_stored_event(bytes))
}

fn encode_stored_event_record(record: &StoredEventRecord) -> Vec<u8> {
    encode_stored_event(&record_to_stored_event(record))
}

/// True iff `record` is a locally-authored row still awaiting a signature
/// — the GC-exclusion predicate (Fable checkpoint R5), mirrors
/// `MemoryStore`'s `is_open_local_intent` exactly so the two backends can
/// never diverge on which rows GC may evict.
fn is_open_local_intent(record: &StoredEventRecord) -> bool {
    matches!(
        record.local,
        Some(LocalOrigin {
            sig_state: SigState::Pending,
            ..
        })
    )
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

fn tag_index_prefix(tag: SingleLetterTag, value: &str) -> Vec<u8> {
    let value = value.as_bytes();
    let value_len = u32::try_from(value.len()).expect("a Nostr tag value fits in u32");
    let mut key = Vec::with_capacity(1 + 4 + value.len());
    key.push(tag.as_char() as u8);
    key.extend_from_slice(&value_len.to_be_bytes());
    key.extend_from_slice(value);
    key
}

fn tag_index_key(
    tag: SingleLetterTag,
    value: &str,
    created_at: Timestamp,
    id: &EventId,
) -> Vec<u8> {
    ordered_key(&tag_index_prefix(tag, value), created_at, id)
}

fn tag_index_range(
    tag: SingleLetterTag,
    value: &str,
    since: u64,
    until: u64,
) -> (Vec<u8>, Vec<u8>) {
    ordered_range(&tag_index_prefix(tag, value), since, until)
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

#[derive(Debug)]
struct OrderedCursor {
    lower: Vec<u8>,
    upper: Vec<u8>,
    upper_inclusive: bool,
    head: Option<(u64, EventId)>,
}

impl OrderedCursor {
    fn new(prefix: &[u8], since: u64, until: u64) -> Self {
        let (lower, upper) = ordered_range(prefix, since, until);
        Self {
            lower,
            upper,
            upper_inclusive: true,
            head: None,
        }
    }

    fn advance(&mut self, table: &redb::ReadOnlyTable<&[u8], ()>) -> Result<(), PersistenceError> {
        let next = if self.upper_inclusive {
            table
                .range(self.lower.as_slice()..=self.upper.as_slice())
                .map_err(persist_err)?
                .next_back()
        } else {
            table
                .range::<&[u8]>((
                    Included(self.lower.as_slice()),
                    Excluded(self.upper.as_slice()),
                ))
                .map_err(persist_err)?
                .next_back()
        };
        self.head = match next {
            Some(entry) => {
                let (key, _value) = entry.map_err(persist_err)?;
                let key = key.value();
                self.upper = key.to_vec();
                self.upper_inclusive = false;
                Some((ordered_index_created_at(key), ordered_index_event_id(key)))
            }
            None => None,
        };
        Ok(())
    }
}

fn insert_tag_index_rows(
    by_tag: &mut redb::Table<'_, &[u8], ()>,
    event: &Event,
) -> Result<(), redb::StorageError> {
    for tag in event.tags.iter() {
        let (Some(single_letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
            continue;
        };
        let key = tag_index_key(single_letter, value, event.created_at, &event.id);
        by_tag.insert(key.as_slice(), ())?;
    }
    Ok(())
}

fn remove_tag_index_rows(
    by_tag: &mut redb::Table<'_, &[u8], ()>,
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

fn insert_query_index_rows(
    by_created_at: &mut redb::Table<'_, &[u8], ()>,
    by_author: &mut redb::Table<'_, &[u8], ()>,
    by_kind: &mut redb::Table<'_, &[u8], ()>,
    by_author_kind: &mut redb::Table<'_, &[u8], ()>,
    by_tag: &mut redb::Table<'_, &[u8], ()>,
    event: &Event,
) -> Result<(), redb::StorageError> {
    let created = created_at_key(event);
    let author = by_author_key(event);
    let kind = by_kind_key(event);
    let author_kind = by_author_kind_key(event);
    by_created_at.insert(created.as_slice(), ())?;
    by_author.insert(author.as_slice(), ())?;
    by_kind.insert(kind.as_slice(), ())?;
    by_author_kind.insert(author_kind.as_slice(), ())?;
    insert_tag_index_rows(by_tag, event)
}

fn remove_query_index_rows(
    by_created_at: &mut redb::Table<'_, &[u8], ()>,
    by_author: &mut redb::Table<'_, &[u8], ()>,
    by_kind: &mut redb::Table<'_, &[u8], ()>,
    by_author_kind: &mut redb::Table<'_, &[u8], ()>,
    by_tag: &mut redb::Table<'_, &[u8], ()>,
    event: &Event,
) -> Result<(), redb::StorageError> {
    let created = created_at_key(event);
    let author = by_author_key(event);
    let kind = by_kind_key(event);
    let author_kind = by_author_kind_key(event);
    by_created_at.remove(created.as_slice())?;
    by_author.remove(author.as_slice())?;
    by_kind.remove(kind.as_slice())?;
    by_author_kind.remove(author_kind.as_slice())?;
    remove_tag_index_rows(by_tag, event)
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
    events: &mut redb::Table<'_, &str, &[u8]>,
    addr_index: &mut redb::Table<'_, &str, &str>,
    expiration_index: &mut redb::Table<'_, &str, &str>,
    by_created_at: &mut redb::Table<'_, &[u8], ()>,
    by_author: &mut redb::Table<'_, &[u8], ()>,
    by_kind: &mut redb::Table<'_, &[u8], ()>,
    by_author_kind: &mut redb::Table<'_, &[u8], ()>,
    by_tag: &mut redb::Table<'_, &[u8], ()>,
    id: EventId,
    predicate: impl FnOnce(&StoredEvent) -> bool,
) -> Result<Option<StoredEvent>, PersistenceError> {
    let id_hex = id.to_hex();
    // A nested block, not a bare chained expression: the `AccessGuard`'s
    // borrow of `events` must end at the closing `}`, strictly before the
    // `events.remove(..)` mutation below — `?`'s hidden `ControlFlow`
    // temporary otherwise extends it (a known rustc temporary-lifetime-
    // extension quirk that a plain sequence of `let` statements does not
    // reliably avoid here).
    let bytes = {
        let Some(guard) = events.get(id_hex.as_str()).map_err(persist_err)? else {
            return Ok(None);
        };
        guard.value().to_vec()
    };
    let se = decode_stored_event(&bytes);
    if !predicate(&se) {
        return Ok(None);
    }

    events.remove(id_hex.as_str()).map_err(persist_err)?;
    remove_query_index_rows(
        by_created_at,
        by_author,
        by_kind,
        by_author_kind,
        by_tag,
        &se.event,
    )
    .map_err(persist_err)?;

    if let Some(addr_key) = address_key_for(&se.event) {
        let addr_key_str = addr_key.to_redb_key();
        let still_points_here = addr_index
            .get(addr_key_str.as_str())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_string())
            == Some(id_hex.clone());
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
    events: &mut redb::Table<'_, &str, &[u8]>,
    addr_index: &mut redb::Table<'_, &str, &str>,
    tombstones: &mut redb::Table<'_, &str, &str>,
    addr_tombstones: &mut redb::Table<'_, &str, &str>,
    expiration_index: &mut redb::Table<'_, &str, &str>,
    by_created_at: &mut redb::Table<'_, &[u8], ()>,
    by_author: &mut redb::Table<'_, &[u8], ()>,
    by_kind: &mut redb::Table<'_, &[u8], ()>,
    by_author_kind: &mut redb::Table<'_, &[u8], ()>,
    by_tag: &mut redb::Table<'_, &[u8], ()>,
    deleting: &Event,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    let mut deleted = Vec::new();
    let deleting_id_hex = deleting.id.to_hex();
    let deleting_author_hex = deleting.pubkey.to_hex();

    let target_ids: Vec<EventId> = deleting.tags.event_ids().copied().collect();
    for target_id in target_ids {
        if let Some(removed) = remove_row_in_txn(
            events,
            addr_index,
            expiration_index,
            by_created_at,
            by_author,
            by_kind,
            by_author_kind,
            by_tag,
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

        let current_id_hex = addr_index
            .get(key_str.as_str())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_string());
        if let Some(current_id_hex) = current_id_hex {
            let current_id =
                EventId::from_hex(&current_id_hex).expect("redb: decode addr_index id");
            if let Some(removed) = remove_row_in_txn(
                events,
                addr_index,
                expiration_index,
                by_created_at,
                by_author,
                by_kind,
                by_author_kind,
                by_tag,
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
    events: &mut redb::Table<'_, &str, &[u8]>,
    addr_index: &mut redb::Table<'_, &str, &str>,
    tombstones: &mut redb::Table<'_, &str, &str>,
    addr_tombstones: &mut redb::Table<'_, &str, &str>,
    expiration_index: &mut redb::Table<'_, &str, &str>,
    by_created_at: &mut redb::Table<'_, &[u8], ()>,
    by_author: &mut redb::Table<'_, &[u8], ()>,
    by_kind: &mut redb::Table<'_, &[u8], ()>,
    by_author_kind: &mut redb::Table<'_, &[u8], ()>,
    by_tag: &mut redb::Table<'_, &[u8], ()>,
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
            events,
            addr_index,
            tombstones,
            addr_tombstones,
            expiration_index,
            by_created_at,
            by_author,
            by_kind,
            by_author_kind,
            by_tag,
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
    events: &redb::Table<'_, &str, &[u8]>,
    addr_index: &redb::Table<'_, &str, &str>,
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
            let current_id_hex = addr_index
                .get(key_str.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value().to_string());
            if let Some(current_id_hex) = current_id_hex {
                let current_id =
                    EventId::from_hex(&current_id_hex).expect("redb: decode addr_index id");
                if seen_candidates.insert(current_id) {
                    candidate_ids.push(current_id);
                }
            }
        }
    }

    let mut visible_before: HashMap<EventId, bool> = HashMap::new();
    for id in &candidate_ids {
        let id_hex = id.to_hex();
        let visible = match events.get(id_hex.as_str()).map_err(persist_err)? {
            None => false,
            Some(guard) => {
                let se = decode_stored_event(guard.value());
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
        let id_hex = id.to_hex();
        if let Some(guard) = events.get(id_hex.as_str()).map_err(persist_err)? {
            let se = decode_stored_event(guard.value());
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
    events: &mut redb::Table<'_, &str, &[u8]>,
    addr_index: &mut redb::Table<'_, &str, &str>,
    tombstones: &mut redb::Table<'_, &str, &str>,
    addr_tombstones: &mut redb::Table<'_, &str, &str>,
    expiration_index: &mut redb::Table<'_, &str, &str>,
    by_created_at: &mut redb::Table<'_, &[u8], ()>,
    by_author: &mut redb::Table<'_, &[u8], ()>,
    by_kind: &mut redb::Table<'_, &[u8], ()>,
    by_author_kind: &mut redb::Table<'_, &[u8], ()>,
    by_tag: &mut redb::Table<'_, &[u8], ()>,
    outbox_intents: &mut redb::Table<'_, &str, &str>,
    outbox_receipts: &mut redb::Table<'_, &str, &str>,
    outbox_displaced: &mut redb::Table<'_, &str, &[u8]>,
    outbox_kind5_claims: &mut redb::Table<'_, &str, &str>,
    outbox_suppress_by_id: &mut redb::Table<'_, &str, &str>,
    outbox_suppress_by_addr: &mut redb::Table<'_, &str, &str>,
    se: StoredEvent,
) -> Result<Option<StoredEvent>, PersistenceError> {
    let id_hex = se.event.id.to_hex();

    let existing_bytes = events
        .get(id_hex.as_str())
        .map_err(persist_err)?
        .map(|guard| guard.value().to_vec());
    if let Some(existing_bytes) = existing_bytes {
        // Architecture review requirement (issue #2 P0 correction,
        // codex-nova ruling): union the owner sets and apply Signed
        // dominance — never silently drop the stashed entry's OWN
        // ownership/signature-state fact just because this exact id
        // happens to already be held. If the union newly becomes Signed
        // for previously-Pending owners, fan out to all of them — the
        // SAME invariant `promote_signed` enforces explicitly, since a
        // dedup collision here is functionally no different from a relay
        // independently confirming the signature.
        let mut record = decode_stored_event_record(&existing_bytes);
        let mut provenance = Provenance {
            seen: record.provenance,
            local: record.local,
        };
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
                record.event.sig = se.event.sig;
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
        record.provenance = provenance.seen.clone();
        record.local = provenance.local.clone();
        let encoded = encode_stored_event_record(&record);
        events
            .insert(id_hex.as_str(), encoded.as_slice())
            .map_err(persist_err)?;
        if let Some(owners) = &fan_out_owners {
            fan_out_signed_in_txn(
                events,
                addr_index,
                tombstones,
                addr_tombstones,
                expiration_index,
                by_created_at,
                by_author,
                by_kind,
                by_author_kind,
                by_tag,
                outbox_intents,
                outbox_receipts,
                outbox_displaced,
                outbox_kind5_claims,
                outbox_suppress_by_id,
                outbox_suppress_by_addr,
                owners,
                &record.event,
            )?;
        }
        return Ok(Some(StoredEvent {
            event: record.event,
            provenance,
        }));
    }
    if tombstone_refuses(tombstones, addr_tombstones, &se.event)? {
        return Ok(None);
    }

    let encoded = encode_stored_event(&se);

    let result = match address_key_for(&se.event) {
        None => {
            events
                .insert(id_hex.as_str(), encoded.as_slice())
                .map_err(persist_err)?;
            insert_query_index_rows(
                by_created_at,
                by_author,
                by_kind,
                by_author_kind,
                by_tag,
                &se.event,
            )
            .map_err(persist_err)?;
            if let Some(ts) = se.event.tags.expiration().copied() {
                let exp_key = expiration_key(ts, &se.event.id);
                expiration_index
                    .insert(exp_key.as_str(), id_hex.as_str())
                    .map_err(persist_err)?;
            }
            Some(se)
        }
        Some(addr_key) => {
            let addr_key_str = addr_key.to_redb_key();
            let current_id_hex = addr_index
                .get(addr_key_str.as_str())
                .map_err(persist_err)?
                .map(|guard| guard.value().to_string());

            match current_id_hex {
                None => {
                    events
                        .insert(id_hex.as_str(), encoded.as_slice())
                        .map_err(persist_err)?;
                    addr_index
                        .insert(addr_key_str.as_str(), id_hex.as_str())
                        .map_err(persist_err)?;
                    insert_query_index_rows(
                        by_created_at,
                        by_author,
                        by_kind,
                        by_author_kind,
                        by_tag,
                        &se.event,
                    )
                    .map_err(persist_err)?;
                    if let Some(ts) = se.event.tags.expiration().copied() {
                        let exp_key = expiration_key(ts, &se.event.id);
                        expiration_index
                            .insert(exp_key.as_str(), id_hex.as_str())
                            .map_err(persist_err)?;
                    }
                    Some(se)
                }
                Some(current_id_hex) => {
                    let current_bytes = events
                        .get(current_id_hex.as_str())
                        .map_err(persist_err)?
                        .expect("addr_index must always point at a stored event")
                        .value()
                        .to_vec();
                    let current_event = decode_stored_event(&current_bytes).event;

                    if candidate_wins(&se.event, &current_event) {
                        let current_id =
                            EventId::from_hex(&current_id_hex).expect("redb: decode addr_index id");
                        remove_row_in_txn(
                            events,
                            addr_index,
                            expiration_index,
                            by_created_at,
                            by_author,
                            by_kind,
                            by_author_kind,
                            by_tag,
                            current_id,
                            |_| true,
                        )?
                        .expect("addr_index must always point at a stored event");

                        events
                            .insert(id_hex.as_str(), encoded.as_slice())
                            .map_err(persist_err)?;
                        addr_index
                            .insert(addr_key_str.as_str(), id_hex.as_str())
                            .map_err(persist_err)?;
                        insert_query_index_rows(
                            by_created_at,
                            by_author,
                            by_kind,
                            by_author_kind,
                            by_tag,
                            &se.event,
                        )
                        .map_err(persist_err)?;
                        if let Some(ts) = se.event.tags.expiration().copied() {
                            let exp_key = expiration_key(ts, &se.event.id);
                            expiration_index
                                .insert(exp_key.as_str(), id_hex.as_str())
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
}

pub struct RedbStore {
    db: Database,
    #[cfg(test)]
    crash_point: AtomicU8,
    /// Test-only instrumentation for the `query`-indexing falsifier
    /// (`query_by_author_does_not_scan_all_rows`, issue #17): counts every
    /// row `query` actually JSON-decodes across a run, so a test can assert
    /// an author/kind-narrowed query decodes only its match set, never
    /// every row in `EVENTS`. Absent from the struct entirely outside
    /// `cfg(test)` — zero cost in a normal build.
    #[cfg(test)]
    examined_rows: AtomicU64,
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
        let write_txn = db.begin_write()?;
        {
            let mut legacy_events = write_txn.open_table(LEGACY_EVENTS)?;
            let mut events = write_txn.open_table(EVENTS)?;
            let mut store_meta = write_txn.open_table(STORE_META)?;
            write_txn.open_table(ADDR_INDEX)?;
            write_txn.open_table(COVERAGE)?;
            write_txn.open_table(TOMBSTONES)?;
            write_txn.open_table(ADDR_TOMBSTONES)?;
            write_txn.open_table(EXPIRATION_INDEX)?;
            let mut legacy_by_author = write_txn.open_table(LEGACY_BY_AUTHOR)?;
            let mut legacy_by_kind = write_txn.open_table(LEGACY_BY_KIND)?;
            let mut by_created_at = write_txn.open_table(BY_CREATED_AT)?;
            let mut by_author = write_txn.open_table(BY_AUTHOR)?;
            let mut by_kind = write_txn.open_table(BY_KIND)?;
            let mut by_author_kind = write_txn.open_table(BY_AUTHOR_KIND)?;
            let mut by_tag = write_txn.open_table(BY_TAG)?;
            write_txn.open_table(OUTBOX_INTENTS)?;
            let mut legacy_outbox_displaced = write_txn.open_table(LEGACY_OUTBOX_DISPLACED)?;
            let mut outbox_displaced = write_txn.open_table(OUTBOX_DISPLACED)?;
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

            let event_record_version = store_meta
                .get(EVENT_RECORD_VERSION_KEY)?
                .map(|guard| guard.value().to_owned());
            if event_record_version.as_deref() != Some(EVENT_RECORD_VERSION) {
                events.retain(|_, _| false)?;
                for entry in legacy_events.iter()? {
                    let (id, value) = entry?;
                    let stored = decode_legacy_stored_event(value.value());
                    let encoded = encode_stored_event(&stored);
                    events.insert(id.value(), encoded.as_slice())?;
                }
                legacy_events.retain(|_, _| false)?;
                outbox_displaced.retain(|_, _| false)?;
                for entry in legacy_outbox_displaced.iter()? {
                    let (intent, value) = entry?;
                    let stored = decode_legacy_stored_event(value.value());
                    let encoded = encode_stored_event(&stored);
                    outbox_displaced.insert(intent.value(), encoded.as_slice())?;
                }
                legacy_outbox_displaced.retain(|_, _| false)?;
                store_meta.insert(EVENT_RECORD_VERSION_KEY, EVENT_RECORD_VERSION)?;
            }

            let ordered_index_version = store_meta
                .get(ORDERED_INDEX_VERSION_KEY)?
                .map(|guard| guard.value().to_owned());
            if ordered_index_version.as_deref() != Some(ORDERED_INDEX_VERSION) {
                // Rebuild every ordered field index from canonical binary
                // rows, then publish the version in this same transaction.
                // A crash leaves the old marker in place, so the next open
                // repeats the whole rebuild rather than exposing a partial
                // planner view.
                by_created_at.retain(|_, _| false)?;
                by_author.retain(|_, _| false)?;
                by_kind.retain(|_, _| false)?;
                by_author_kind.retain(|_, _| false)?;
                for entry in events.iter()? {
                    let (_id, value) = entry?;
                    let stored = decode_stored_event(value.value());
                    let event = &stored.event;
                    let created = created_at_key(event);
                    let author = by_author_key(event);
                    let kind = by_kind_key(event);
                    let author_kind = by_author_kind_key(event);
                    by_created_at.insert(created.as_slice(), ())?;
                    by_author.insert(author.as_slice(), ())?;
                    by_kind.insert(kind.as_slice(), ())?;
                    by_author_kind.insert(author_kind.as_slice(), ())?;
                }
                legacy_by_author.retain(|_, _| false)?;
                legacy_by_kind.retain(|_, _| false)?;
                store_meta.insert(ORDERED_INDEX_VERSION_KEY, ORDERED_INDEX_VERSION)?;
            }

            let tag_index_version = store_meta
                .get(TAG_INDEX_VERSION_KEY)?
                .map(|guard| guard.value().to_owned());
            if tag_index_version.as_deref() != Some(TAG_INDEX_VERSION) {
                // A missing/different version can be a legacy database or a
                // prior process that died during migration. Rebuild from the
                // canonical EVENTS rows and publish the version in this same
                // transaction, so readers can never observe a partial index.
                by_tag.retain(|_, _| false)?;
                for entry in events.iter()? {
                    let (_id, value) = entry?;
                    let stored = decode_stored_event(value.value());
                    insert_tag_index_rows(&mut by_tag, &stored.event)?;
                }
                store_meta.insert(TAG_INDEX_VERSION_KEY, TAG_INDEX_VERSION)?;
            }
        }
        write_txn.commit()?;
        Ok(Self {
            db,
            #[cfg(test)]
            crash_point: AtomicU8::new(0),
            #[cfg(test)]
            examined_rows: AtomicU64::new(0),
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
    fn decode_row(&self, view: StoredEventView<'_>) -> StoredEvent {
        #[cfg(test)]
        self.examined_rows.fetch_add(1, Ordering::Relaxed);
        view.materialize()
            .expect("redb: materialize validated portable event")
    }

    /// Reverse-merge one or more ranges from the planner's chosen index.
    /// Each cursor asks redb for exactly its next key; once `limit` visible
    /// rows have survived the borrowed binary post-filter, no older key or
    /// event value is touched.
    fn query_newest_ordered(
        &self,
        read_txn: &redb::ReadTransaction,
        index: TableDefinition<&[u8], ()>,
        prefixes: Vec<Vec<u8>>,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        let events = read_txn.open_table(EVENTS).map_err(persist_err)?;
        let index = read_txn.open_table(index).map_err(persist_err)?;
        let outbox_suppress_by_id = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ID)
            .map_err(persist_err)?;
        let outbox_suppress_by_addr = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ADDR)
            .map_err(persist_err)?;
        let since = filter.since.map(|ts| ts.as_secs()).unwrap_or(0);
        let until = filter.until.map(|ts| ts.as_secs()).unwrap_or(u64::MAX);
        let materialize_if_visible =
            |id: EventId| -> Result<Option<StoredEvent>, PersistenceError> {
                let id_hex = id.to_hex();
                let Some(value) = events.get(id_hex.as_str()).map_err(persist_err)? else {
                    return Ok(None);
                };
                let view = StoredEventView::from_trusted(value.value())
                    .expect("redb: decode portable stored event view");
                if !view.matches_filter(filter) {
                    return Ok(None);
                }
                let stored = self.decode_row(view);
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
        if let [prefix] = prefixes.as_slice() {
            let (lower, upper) = ordered_range(prefix, since, until);
            let mut out = Vec::with_capacity(limit);
            for entry in index
                .range(lower.as_slice()..=upper.as_slice())
                .map_err(persist_err)?
                .rev()
            {
                let (key, _value) = entry.map_err(persist_err)?;
                if let Some(stored) = materialize_if_visible(ordered_index_event_id(key.value()))? {
                    out.push(stored);
                    if out.len() == limit {
                        break;
                    }
                }
            }
            return Ok(out);
        }

        let mut cursors: Vec<_> = prefixes
            .iter()
            .map(|prefix| OrderedCursor::new(prefix, since, until))
            .collect();
        for cursor in &mut cursors {
            cursor.advance(&index)?;
        }

        let mut out = Vec::with_capacity(limit);
        let mut seen = HashSet::new();
        while out.len() < limit {
            let Some((cursor_index, (_created_at, id))) = cursors
                .iter()
                .enumerate()
                .filter_map(|(index, cursor)| cursor.head.map(|head| (index, head)))
                .max_by(|(_, (left_ts, left_id)), (_, (right_ts, right_id))| {
                    left_ts.cmp(right_ts).then_with(|| right_id.cmp(left_id))
                })
            else {
                break;
            };
            cursors[cursor_index].advance(&index)?;
            if !seen.insert(id) {
                continue;
            }
            if let Some(stored) = materialize_if_visible(id)? {
                out.push(stored);
            }
        }
        Ok(out)
    }

    /// `query`'s index-narrowing step (issue #17): `Some(ids)` -- the
    /// bounded candidate set gathered from `BY_AUTHOR`/`BY_KIND`/`BY_TAG`
    /// for whichever indexed constraints are present. Values within one
    /// field are unioned; different fields are intersected. Returns `None`
    /// only when no indexed constraint can narrow the query, in which case
    /// `query` must fall back to a full scan.
    /// Does not touch `filter.ids`; that is `query`'s own, even cheaper,
    /// fast path.
    fn candidate_ids(
        &self,
        read_txn: &redb::ReadTransaction,
        filter: &Filter,
    ) -> Result<Option<HashSet<EventId>>, PersistenceError> {
        // `?` reaches this fn (returning `Result`), so redb read failures
        // surface as `PersistenceError` on `query`'s index-narrowing path
        // rather than panicking (issue #122); `EventId::from_hex` on an index
        // value stays `.expect()`-on-invariant (a corrupt persisted key).
        let by_authors = match filter.authors.as_ref() {
            None => None,
            Some(authors) => {
                let by_author = read_txn.open_table(BY_AUTHOR).map_err(persist_err)?;
                let mut ids = HashSet::new();
                for author in authors {
                    let prefix = by_author_prefix(author);
                    let (lower, upper) = ordered_range(&prefix, 0, u64::MAX);
                    for entry in by_author
                        .range(lower.as_slice()..=upper.as_slice())
                        .map_err(persist_err)?
                    {
                        let (key, _value) = entry.map_err(persist_err)?;
                        ids.insert(ordered_index_event_id(key.value()));
                    }
                }
                Some(ids)
            }
        };

        let by_kinds = match filter.kinds.as_ref() {
            None => None,
            Some(kinds) => {
                let by_kind = read_txn.open_table(BY_KIND).map_err(persist_err)?;
                let mut ids = HashSet::new();
                for kind in kinds {
                    let prefix = by_kind_prefix(*kind);
                    let (lower, upper) = ordered_range(&prefix, 0, u64::MAX);
                    for entry in by_kind
                        .range(lower.as_slice()..=upper.as_slice())
                        .map_err(persist_err)?
                    {
                        let (key, _value) = entry.map_err(persist_err)?;
                        ids.insert(ordered_index_event_id(key.value()));
                    }
                }
                Some(ids)
            }
        };

        let by_tags = if filter.generic_tags.is_empty() {
            None
        } else {
            let by_tag = read_txn.open_table(BY_TAG).map_err(persist_err)?;
            let mut all_fields: Option<HashSet<EventId>> = None;
            for (tag, values) in &filter.generic_tags {
                // NIP-01 is OR within one `#x` value set, AND across
                // different single-letter tag fields.
                let mut field_ids = HashSet::new();
                for value in values {
                    let (lower, upper) = tag_index_range(*tag, value, 0, u64::MAX);
                    for entry in by_tag
                        .range(lower.as_slice()..=upper.as_slice())
                        .map_err(persist_err)?
                    {
                        let (key, _value) = entry.map_err(persist_err)?;
                        field_ids.insert(ordered_index_event_id(key.value()));
                    }
                }
                all_fields = Some(match all_fields {
                    None => field_ids,
                    Some(ids) => ids.intersection(&field_ids).copied().collect(),
                });
            }
            all_fields
        };

        let mut indexed_sets = [by_authors, by_kinds, by_tags].into_iter().flatten();
        let Some(mut candidates) = indexed_sets.next() else {
            return Ok(None);
        };
        for ids in indexed_sets {
            candidates.retain(|id| ids.contains(id));
        }
        Ok(Some(candidates))
    }
}

#[allow(clippy::too_many_lines)]
fn insert_in_txn(
    write_txn: &redb::WriteTransaction,
    event: Event,
    from: RelayObserved,
) -> Result<InsertOutcome, PersistenceError> {
    // Refused at the door FIRST: an already-expired event is never
    // stored, so it never touches dedup or supersession at all.
    if event.is_expired_at(&from.at) {
        return Ok(InsertOutcome::Refused(RefuseReason::AlreadyExpired));
    }

    let outcome = {
        let mut events = write_txn.open_table(EVENTS).map_err(persist_err)?;
        let mut addr_index = write_txn.open_table(ADDR_INDEX).map_err(persist_err)?;
        let mut tombstones = write_txn.open_table(TOMBSTONES).map_err(persist_err)?;
        let mut addr_tombstones = write_txn.open_table(ADDR_TOMBSTONES).map_err(persist_err)?;
        let mut expiration_index = write_txn
            .open_table(EXPIRATION_INDEX)
            .map_err(persist_err)?;
        let mut by_created_at = write_txn.open_table(BY_CREATED_AT).map_err(persist_err)?;
        let mut by_author = write_txn.open_table(BY_AUTHOR).map_err(persist_err)?;
        let mut by_kind = write_txn.open_table(BY_KIND).map_err(persist_err)?;
        let mut by_author_kind = write_txn.open_table(BY_AUTHOR_KIND).map_err(persist_err)?;
        let mut by_tag = write_txn.open_table(BY_TAG).map_err(persist_err)?;
        let mut outbox_intents = write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?;
        let mut outbox_receipts = write_txn.open_table(OUTBOX_RECEIPTS).map_err(persist_err)?;
        let mut outbox_displaced = write_txn
            .open_table(OUTBOX_DISPLACED)
            .map_err(persist_err)?;
        let mut outbox_kind5_claims = write_txn
            .open_table(OUTBOX_KIND5_CLAIMS)
            .map_err(persist_err)?;
        let mut outbox_suppress_by_id = write_txn
            .open_table(OUTBOX_SUPPRESS_BY_ID)
            .map_err(persist_err)?;
        let mut outbox_suppress_by_addr = write_txn
            .open_table(OUTBOX_SUPPRESS_BY_ADDR)
            .map_err(persist_err)?;
        let id_hex = event.id.to_hex();

        let existing_bytes = events
            .get(id_hex.as_str())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec());

        if let Some(existing_bytes) = existing_bytes {
            // Dedup-by-id FIRST: merge provenance, no index churn. Goes
            // through `Provenance::merge_observation` (not a re-derived
            // copy) so the persisted backend can never diverge from
            // `MemoryStore`'s merge semantics.
            let mut record = decode_stored_event_record(&existing_bytes);
            let mut provenance = Provenance {
                seen: record.provenance,
                local: record.local,
            };
            let grew = provenance.merge_observation(&from);
            // Architecture review requirement (issue #2 P0 correction,
            // codex-nova ruling): a relay delivering the real signed
            // event for a still-Pending local draft is functionally the
            // SAME signature-adoption/fan-out invariant `promote_signed`
            // performs explicitly — adopt it, mark every co-owner
            // `Signed`, and fan out, rather than silently keeping our
            // own sentinel forever (`event` here is, by this door's own
            // contract, always a genuine relay delivery, never our OWN
            // sentinel, so its signature is always safe to adopt).
            let needs_adoption = provenance
                .local
                .as_ref()
                .is_some_and(|l| l.sig_state == SigState::Pending);
            let mut fan_out_owners: Option<BTreeSet<IntentId>> = None;
            if needs_adoption {
                let mut local = provenance
                    .local
                    .clone()
                    .expect("just checked this row carries local provenance");
                local.sig_state = SigState::Signed;
                fan_out_owners = Some(local.owners.clone());
                provenance.local = Some(local);
            }
            // `merge_observation` never touches `local` (a relay echo
            // of an already-local row keeps its local provenance,
            // retraction doc §4.1) — `provenance.local` is otherwise
            // unchanged, written straight back.
            record.provenance = provenance.seen;
            record.local = provenance.local;
            if fan_out_owners.is_some() {
                record.event = event.clone();
            }
            let encoded = encode_stored_event_record(&record);
            events
                .insert(id_hex.as_str(), encoded.as_slice())
                .map_err(persist_err)?;
            let satisfied_intents = if let Some(owners) = &fan_out_owners {
                fan_out_signed_in_txn(
                    &mut events,
                    &mut addr_index,
                    &mut tombstones,
                    &mut addr_tombstones,
                    &mut expiration_index,
                    &mut by_created_at,
                    &mut by_author,
                    &mut by_kind,
                    &mut by_author_kind,
                    &mut by_tag,
                    &mut outbox_intents,
                    &mut outbox_receipts,
                    &mut outbox_displaced,
                    &mut outbox_kind5_claims,
                    &mut outbox_suppress_by_id,
                    &mut outbox_suppress_by_addr,
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
        } else if tombstone_refuses(&tombstones, &addr_tombstones, &event)? {
            // Tombstone check, AFTER dedup-by-id, BEFORE storage
            // (retraction-and-negative-deltas.md §2).
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        } else {
            let is_deletion = event.kind == Kind::EventDeletion;
            let record = StoredEventRecord {
                event: event.clone(),
                provenance: {
                    let mut m = BTreeMap::new();
                    m.insert(from.relay.clone(), from.at);
                    m
                },
                local: None,
            };
            let encoded = encode_stored_event_record(&record);

            let outcome = match address_key_for(&event) {
                None => {
                    events
                        .insert(id_hex.as_str(), encoded.as_slice())
                        .map_err(persist_err)?;
                    insert_query_index_rows(
                        &mut by_created_at,
                        &mut by_author,
                        &mut by_kind,
                        &mut by_author_kind,
                        &mut by_tag,
                        &event,
                    )
                    .map_err(persist_err)?;
                    if let Some(ts) = event.tags.expiration().copied() {
                        let exp_key = expiration_key(ts, &event.id);
                        expiration_index
                            .insert(exp_key.as_str(), id_hex.as_str())
                            .map_err(persist_err)?;
                    }
                    InsertOutcome::Inserted
                }
                Some(addr_key) => {
                    let addr_key_str = addr_key.to_redb_key();
                    let current_id_hex = addr_index
                        .get(addr_key_str.as_str())
                        .map_err(persist_err)?
                        .map(|guard| guard.value().to_string());

                    match current_id_hex {
                        None => {
                            events
                                .insert(id_hex.as_str(), encoded.as_slice())
                                .map_err(persist_err)?;
                            addr_index
                                .insert(addr_key_str.as_str(), id_hex.as_str())
                                .map_err(persist_err)?;
                            insert_query_index_rows(
                                &mut by_created_at,
                                &mut by_author,
                                &mut by_kind,
                                &mut by_author_kind,
                                &mut by_tag,
                                &event,
                            )
                            .map_err(persist_err)?;
                            if let Some(ts) = event.tags.expiration().copied() {
                                let exp_key = expiration_key(ts, &event.id);
                                expiration_index
                                    .insert(exp_key.as_str(), id_hex.as_str())
                                    .map_err(persist_err)?;
                            }
                            InsertOutcome::Inserted
                        }
                        Some(current_id_hex) => {
                            let current_bytes = events
                                .get(current_id_hex.as_str())
                                .map_err(persist_err)?
                                .expect("addr_index must always point at a stored event")
                                .value()
                                .to_vec();
                            let current_record = decode_stored_event_record(&current_bytes);
                            let current_event = current_record.event.clone();

                            if candidate_wins(&event, &current_event) {
                                let replaced = StoredEvent {
                                    event: current_event,
                                    provenance: Provenance {
                                        seen: current_record.provenance,
                                        local: current_record.local,
                                    },
                                };
                                events
                                    .remove(current_id_hex.as_str())
                                    .map_err(persist_err)?;
                                remove_query_index_rows(
                                    &mut by_created_at,
                                    &mut by_author,
                                    &mut by_kind,
                                    &mut by_author_kind,
                                    &mut by_tag,
                                    &replaced.event,
                                )
                                .map_err(persist_err)?;
                                if let Some(ts) = replaced.event.tags.expiration().copied() {
                                    let exp_key = expiration_key(ts, &replaced.event.id);
                                    expiration_index
                                        .remove(exp_key.as_str())
                                        .map_err(persist_err)?;
                                }
                                events
                                    .insert(id_hex.as_str(), encoded.as_slice())
                                    .map_err(persist_err)?;
                                addr_index
                                    .insert(addr_key_str.as_str(), id_hex.as_str())
                                    .map_err(persist_err)?;
                                insert_query_index_rows(
                                    &mut by_created_at,
                                    &mut by_author,
                                    &mut by_kind,
                                    &mut by_author_kind,
                                    &mut by_tag,
                                    &event,
                                )
                                .map_err(persist_err)?;
                                if let Some(ts) = event.tags.expiration().copied() {
                                    let exp_key = expiration_key(ts, &event.id);
                                    expiration_index
                                        .insert(exp_key.as_str(), id_hex.as_str())
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
                        &mut events,
                        &mut addr_index,
                        &mut tombstones,
                        &mut addr_tombstones,
                        &mut expiration_index,
                        &mut by_created_at,
                        &mut by_author,
                        &mut by_kind,
                        &mut by_author_kind,
                        &mut by_tag,
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
        let outcome = insert_in_txn(&write_txn, event, from)?;
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
        for (event, from) in events {
            outcomes.push(insert_in_txn(&write_txn, event, from)?);
        }
        write_txn.commit().map_err(persist_err)?;
        Ok(outcomes)
    }

    fn query(&self, filter: &Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        let read_txn = self.db.begin_read().map_err(persist_err)?;
        let events = read_txn.open_table(EVENTS).map_err(persist_err)?;
        let outbox_suppress_by_id = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ID)
            .map_err(persist_err)?;
        let outbox_suppress_by_addr = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ADDR)
            .map_err(persist_err)?;
        // A still-open kind:5 intent's provisional suppression claim
        // (architecture review requirement — see `SuppressClaimRecord`'s
        // doc) hides a row from every one of `query`'s three paths WITHOUT
        // ever removing it from `EVENTS` — the row is fully present, only
        // filtered out here.
        let visible = |event: &Event| -> Result<bool, PersistenceError> {
            Ok(!is_suppressed_in_txn(
                &outbox_suppress_by_id,
                &outbox_suppress_by_addr,
                event,
            )?)
        };

        // Fast path: `filter.ids` narrows directly through `EVENTS`'s own
        // id-keyed rows -- no secondary index needed, bounded by `|ids|`
        // regardless of table size (issue #17).
        if let Some(ids) = &filter.ids {
            let mut out = Vec::new();
            for id in ids {
                let id_hex = id.to_hex();
                let Some(value) = events.get(id_hex.as_str()).map_err(persist_err)? else {
                    continue;
                };
                let view = StoredEventView::from_trusted(value.value())
                    .expect("redb: decode portable stored event view");
                if !view.matches_filter(filter) {
                    continue;
                }
                let se = self.decode_row(view);
                if visible(&se.event)? {
                    out.push(se);
                }
            }
            return Ok(out);
        }

        // Otherwise, narrow via `BY_AUTHOR`/`BY_KIND`/`BY_TAG` whenever the
        // filter carries an indexed field. `candidate_ids` returns `None`
        // only when nothing can narrow it (for example a search-only
        // filter), in which case the full scan is the correct fallback.
        match self.candidate_ids(&read_txn, filter)? {
            Some(candidates) => {
                let mut out = Vec::with_capacity(candidates.len());
                for id in candidates {
                    let id_hex = id.to_hex();
                    let Some(value) = events.get(id_hex.as_str()).map_err(persist_err)? else {
                        // A stale index entry outliving its row (e.g. a GC'd
                        // event whose derived index rows haven't been
                        // touched by that path) — harmless, just skip.
                        continue;
                    };
                    let view = StoredEventView::from_trusted(value.value())
                        .expect("redb: decode portable stored event view");
                    if !view.matches_filter(filter) {
                        continue;
                    }
                    let se = self.decode_row(view);
                    if visible(&se.event)? {
                        out.push(se);
                    }
                }
                Ok(out)
            }
            None => {
                let mut out = Vec::new();
                for entry in events.iter().map_err(persist_err)? {
                    let (_key, value) = entry.map_err(persist_err)?;
                    let view = StoredEventView::from_trusted(value.value())
                        .expect("redb: decode portable stored event view");
                    if !view.matches_filter(filter) {
                        continue;
                    }
                    let se = self.decode_row(view);
                    if visible(&se.event)? {
                        out.push(se);
                    }
                }
                Ok(out)
            }
        }
    }

    fn query_newest(
        &self,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // Exact ids are already the narrowest possible lookup. They do not
        // form a time-ordered range, so preserve correctness by sorting this
        // caller-bounded set only; no unrelated row is touched.
        if filter.ids.is_some() {
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

        // nostrdb's one-best-index planner, adapted to redb. OR values in
        // the chosen field become independently ordered ranges and are
        // k-way merged; every remaining constraint is a cheap borrowed
        // binary post-filter. Prefer the composite index when both fields
        // exist, then authors, one selective tag field, kinds, and finally
        // the global time index.
        if let (Some(authors), Some(kinds)) = (&filter.authors, &filter.kinds) {
            let prefixes = authors
                .iter()
                .flat_map(|author| {
                    kinds
                        .iter()
                        .map(move |kind| by_author_kind_prefix(author, *kind))
                })
                .collect();
            return self.query_newest_ordered(&read_txn, BY_AUTHOR_KIND, prefixes, filter, limit);
        }
        if let Some(authors) = &filter.authors {
            let prefixes = authors.iter().map(by_author_prefix).collect();
            return self.query_newest_ordered(&read_txn, BY_AUTHOR, prefixes, filter, limit);
        }
        if let Some((tag, values)) = filter
            .generic_tags
            .iter()
            .min_by_key(|(_tag, values)| values.len())
        {
            let prefixes = values
                .iter()
                .map(|value| tag_index_prefix(*tag, value))
                .collect();
            return self.query_newest_ordered(&read_txn, BY_TAG, prefixes, filter, limit);
        }
        if let Some(kinds) = &filter.kinds {
            let prefixes = kinds.iter().map(|kind| by_kind_prefix(*kind)).collect();
            return self.query_newest_ordered(&read_txn, BY_KIND, prefixes, filter, limit);
        }
        self.query_newest_ordered(&read_txn, BY_CREATED_AT, vec![Vec::new()], filter, limit)
    }

    fn remove(
        &mut self,
        id: EventId,
        _reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let removed = {
            let mut events = write_txn.open_table(EVENTS).map_err(persist_err)?;
            let mut addr_index = write_txn.open_table(ADDR_INDEX).map_err(persist_err)?;
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?;
            let mut by_created_at = write_txn.open_table(BY_CREATED_AT).map_err(persist_err)?;
            let mut by_author = write_txn.open_table(BY_AUTHOR).map_err(persist_err)?;
            let mut by_kind = write_txn.open_table(BY_KIND).map_err(persist_err)?;
            let mut by_author_kind = write_txn.open_table(BY_AUTHOR_KIND).map_err(persist_err)?;
            let mut by_tag = write_txn.open_table(BY_TAG).map_err(persist_err)?;
            remove_row_in_txn(
                &mut events,
                &mut addr_index,
                &mut expiration_index,
                &mut by_created_at,
                &mut by_author,
                &mut by_kind,
                &mut by_author_kind,
                &mut by_tag,
                id,
                |_| true,
            )?
        };
        write_txn.commit().map_err(persist_err)?;
        Ok(removed)
    }

    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let removed = {
            let mut events = write_txn.open_table(EVENTS).map_err(persist_err)?;
            let mut addr_index = write_txn.open_table(ADDR_INDEX).map_err(persist_err)?;
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?;
            let mut by_created_at = write_txn.open_table(BY_CREATED_AT).map_err(persist_err)?;
            let mut by_author = write_txn.open_table(BY_AUTHOR).map_err(persist_err)?;
            let mut by_kind = write_txn.open_table(BY_KIND).map_err(persist_err)?;
            let mut by_author_kind = write_txn.open_table(BY_AUTHOR_KIND).map_err(persist_err)?;
            let mut by_tag = write_txn.open_table(BY_TAG).map_err(persist_err)?;

            let upper = expiration_key_upper_bound(now);
            // Collect due ids first, propagating any redb read error out of
            // the iterator (a plain `for` accumulate rather than a `.map()`
            // closure so `?` reaches this fn, not the closure).
            let mut due_ids: Vec<EventId> = Vec::new();
            for entry in expiration_index
                .range::<&str>(..=upper.as_str())
                .map_err(persist_err)?
            {
                let (_key, value) = entry.map_err(persist_err)?;
                due_ids.push(
                    EventId::from_hex(value.value()).expect("redb: decode expiration_index id"),
                );
            }

            let mut removed = Vec::new();
            for id in due_ids {
                if let Some(row) = remove_row_in_txn(
                    &mut events,
                    &mut addr_index,
                    &mut expiration_index,
                    &mut by_created_at,
                    &mut by_author,
                    &mut by_kind,
                    &mut by_author_kind,
                    &mut by_tag,
                    id,
                    |_| true,
                )? {
                    removed.push(row);
                }
            }
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
            let mut events = write_txn.open_table(EVENTS).map_err(persist_err)?;
            let mut coverage = write_txn.open_table(COVERAGE).map_err(persist_err)?;
            let mut by_created_at = write_txn.open_table(BY_CREATED_AT).map_err(persist_err)?;
            let mut by_author = write_txn.open_table(BY_AUTHOR).map_err(persist_err)?;
            let mut by_kind = write_txn.open_table(BY_KIND).map_err(persist_err)?;
            let mut by_author_kind = write_txn.open_table(BY_AUTHOR_KIND).map_err(persist_err)?;
            let mut by_tag = write_txn.open_table(BY_TAG).map_err(persist_err)?;
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
            let mut victims: Vec<(String, Event)> = Vec::new();
            for entry in events.iter().map_err(persist_err)? {
                let (key, value) = entry.map_err(persist_err)?;
                let record = decode_stored_event_record(value.value());
                let event = record.event.clone();
                if address_key_for(&event).is_none()
                    && !is_open_local_intent(&record)
                    && !is_suppressed_in_txn(
                        &outbox_suppress_by_id,
                        &outbox_suppress_by_addr,
                        &event,
                    )?
                    && !claims.is_claimed(&event)
                {
                    victims.push((key.value().to_string(), event));
                }
            }

            for (id_hex, event) in &victims {
                events.remove(id_hex.as_str()).map_err(persist_err)?;
                // Keep BY_AUTHOR/BY_KIND in lockstep with EVENTS -- a stale
                // index row surviving a gc'd event would keep costing
                // `query` a wasted `events.get` miss on every future hit
                // (harmless, see `query`'s `None` skip, but unbounded
                // growth otherwise).
                remove_query_index_rows(
                    &mut by_created_at,
                    &mut by_author,
                    &mut by_kind,
                    &mut by_author_kind,
                    &mut by_tag,
                    event,
                )
                .map_err(persist_err)?;
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
                for (_, event) in &victims {
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
            let mut events = write_txn.open_table(EVENTS).map_err(persist_err)?;
            let mut addr_index = write_txn.open_table(ADDR_INDEX).map_err(persist_err)?;
            let tombstones = write_txn.open_table(TOMBSTONES).map_err(persist_err)?;
            let addr_tombstones = write_txn.open_table(ADDR_TOMBSTONES).map_err(persist_err)?;
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?;
            let mut by_created_at = write_txn.open_table(BY_CREATED_AT).map_err(persist_err)?;
            let mut by_author = write_txn.open_table(BY_AUTHOR).map_err(persist_err)?;
            let mut by_kind = write_txn.open_table(BY_KIND).map_err(persist_err)?;
            let mut by_author_kind = write_txn.open_table(BY_AUTHOR_KIND).map_err(persist_err)?;
            let mut by_tag = write_txn.open_table(BY_TAG).map_err(persist_err)?;
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

            let id_hex = frozen.id.to_hex();
            let existing = events.get(id_hex.as_str()).map_err(persist_err)?;
            let existing_bytes = existing.map(|guard| guard.value().to_vec());
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
                Live,
                Stash(String),
            }
            let dup_loc = if existing_bytes.is_some() {
                Some(DupLoc::Live)
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
                let existing_bytes_for_dup = match &dup_loc {
                    DupLoc::Live => existing_bytes.clone().expect("checked DupLoc::Live above"),
                    DupLoc::Stash(key) => outbox_displaced
                        .get(key.as_str())
                        .map_err(persist_err)?
                        .expect("just found this key")
                        .value()
                        .to_vec(),
                };
                let mut existing_record = decode_stored_event_record(&existing_bytes_for_dup);
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
                let encoded = encode_stored_event_record(&existing_record);
                match &dup_loc {
                    DupLoc::Live => {
                        events
                            .insert(id_hex.as_str(), encoded.as_slice())
                            .map_err(persist_err)?;
                    }
                    DupLoc::Stash(key) => {
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
                        &events,
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
                let encoded = encode_stored_event(&stored);

                match address_key_for(&frozen) {
                    None => {
                        events
                            .insert(id_hex.as_str(), encoded.as_slice())
                            .map_err(persist_err)?;
                        insert_query_index_rows(
                            &mut by_created_at,
                            &mut by_author,
                            &mut by_kind,
                            &mut by_author_kind,
                            &mut by_tag,
                            &frozen,
                        )
                        .map_err(persist_err)?;
                        if let Some(ts) = frozen.tags.expiration().copied() {
                            let exp_key = expiration_key(ts, &frozen.id);
                            expiration_index
                                .insert(exp_key.as_str(), id_hex.as_str())
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
                                &events,
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
                        let current = addr_index.get(addr_key_str.as_str()).map_err(persist_err)?;
                        let current_id_hex = current.map(|guard| guard.value().to_string());

                        match current_id_hex {
                            None => {
                                events
                                    .insert(id_hex.as_str(), encoded.as_slice())
                                    .map_err(persist_err)?;
                                addr_index
                                    .insert(addr_key_str.as_str(), id_hex.as_str())
                                    .map_err(persist_err)?;
                                insert_query_index_rows(
                                    &mut by_created_at,
                                    &mut by_author,
                                    &mut by_kind,
                                    &mut by_author_kind,
                                    &mut by_tag,
                                    &frozen,
                                )
                                .map_err(persist_err)?;
                                if let Some(ts) = frozen.tags.expiration().copied() {
                                    let exp_key = expiration_key(ts, &frozen.id);
                                    expiration_index
                                        .insert(exp_key.as_str(), id_hex.as_str())
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
                            Some(current_id_hex) => {
                                let current_guard = events
                                    .get(current_id_hex.as_str())
                                    .map_err(persist_err)?
                                    .expect("addr_index must always point at a stored event");
                                let current_bytes = current_guard.value().to_vec();
                                drop(current_guard);
                                let current_event = decode_stored_event(&current_bytes).event;

                                if candidate_wins(&frozen, &current_event) {
                                    let current_id = EventId::from_hex(&current_id_hex)
                                        .expect("redb: decode addr_index id");
                                    let replaced = remove_row_in_txn(
                                        &mut events,
                                        &mut addr_index,
                                        &mut expiration_index,
                                        &mut by_created_at,
                                        &mut by_author,
                                        &mut by_kind,
                                        &mut by_author_kind,
                                        &mut by_tag,
                                        current_id,
                                        |_| true,
                                    )?
                                    .expect("addr_index must always point at a stored event");

                                    events
                                        .insert(id_hex.as_str(), encoded.as_slice())
                                        .map_err(persist_err)?;
                                    addr_index
                                        .insert(addr_key_str.as_str(), id_hex.as_str())
                                        .map_err(persist_err)?;
                                    insert_query_index_rows(
                                        &mut by_created_at,
                                        &mut by_author,
                                        &mut by_kind,
                                        &mut by_author_kind,
                                        &mut by_tag,
                                        &frozen,
                                    )
                                    .map_err(persist_err)?;
                                    if let Some(ts) = frozen.tags.expiration().copied() {
                                        let exp_key = expiration_key(ts, &frozen.id);
                                        expiration_index
                                            .insert(exp_key.as_str(), id_hex.as_str())
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
            let mut events = write_txn.open_table(EVENTS).map_err(persist_err)?;
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
            let mut by_created_at = write_txn.open_table(BY_CREATED_AT).map_err(persist_err)?;
            let mut by_author = write_txn.open_table(BY_AUTHOR).map_err(persist_err)?;
            let mut by_kind = write_txn.open_table(BY_KIND).map_err(persist_err)?;
            let mut by_author_kind = write_txn.open_table(BY_AUTHOR_KIND).map_err(persist_err)?;
            let mut by_tag = write_txn.open_table(BY_TAG).map_err(persist_err)?;

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
                    let frozen_id_hex = frozen_id.to_hex();

                    // Architecture review correction (load-bearing): is
                    // this intent AMONG the owners of the LIVE row at its
                    // own frozen id? A `Duplicate`/`Stale` intent never
                    // had one of its own; a once-live row can since have
                    // been superseded (locally or by a relay),
                    // kind:5-deleted, or expired. Ownership is a SET
                    // (issue #2, team-lead decision): an exact `Duplicate`
                    // is a CO-OWNER of the SAME canonical row, not a
                    // second row of its own — see `LocalOrigin`'s doc.
                    let live_bytes = events
                        .get(frozen_id_hex.as_str())
                        .map_err(persist_err)?
                        .map(|guard| guard.value().to_vec());
                    let live_record: Option<StoredEventRecord> = live_bytes
                        .as_ref()
                        .map(|bytes| decode_stored_event_record(bytes));
                    let is_live = live_record.as_ref().is_some_and(|r| {
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
                            .and_then(|r| r.local.as_ref())
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
                        let mut record = live_record.expect("checked is_live above");
                        if !already_signed {
                            let mut local = record.local.expect("checked is_live above");
                            local.sig_state = SigState::Signed;
                            record.local = Some(local);
                            record.event = signed_frozen_event.clone();
                            let encoded = encode_stored_event_record(&record);
                            events
                                .insert(frozen_id_hex.as_str(), encoded.as_slice())
                                .map_err(persist_err)?;
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
                        &mut events,
                        &mut addr_index,
                        &mut tombstones,
                        &mut addr_tombstones,
                        &mut expiration_index,
                        &mut by_created_at,
                        &mut by_author,
                        &mut by_kind,
                        &mut by_author_kind,
                        &mut by_tag,
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
            let mut events = write_txn.open_table(EVENTS).map_err(persist_err)?;
            let mut addr_index = write_txn.open_table(ADDR_INDEX).map_err(persist_err)?;
            let mut tombstones = write_txn.open_table(TOMBSTONES).map_err(persist_err)?;
            let mut addr_tombstones = write_txn.open_table(ADDR_TOMBSTONES).map_err(persist_err)?;
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?;
            let mut by_created_at = write_txn.open_table(BY_CREATED_AT).map_err(persist_err)?;
            let mut by_author = write_txn.open_table(BY_AUTHOR).map_err(persist_err)?;
            let mut by_kind = write_txn.open_table(BY_KIND).map_err(persist_err)?;
            let mut by_author_kind = write_txn.open_table(BY_AUTHOR_KIND).map_err(persist_err)?;
            let mut by_tag = write_txn.open_table(BY_TAG).map_err(persist_err)?;
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
                        let frozen_id_hex = frozen_id.to_hex();

                        let live_bytes = events
                            .get(frozen_id_hex.as_str())
                            .map_err(persist_err)?
                            .map(|guard| guard.value().to_vec());
                        let is_live = live_bytes.as_ref().is_some_and(|bytes| {
                            let r = decode_stored_event_record(bytes);
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
                            let mut record = decode_stored_event_record(
                                live_bytes.as_deref().expect("checked is_live above"),
                            );
                            let mut local = record.local.clone().expect("checked is_live above");
                            local.owners.remove(&intent_id);
                            let should_retract = local.owners.is_empty()
                                && local.sig_state == SigState::Pending
                                && record.provenance.is_empty();
                            if should_retract {
                                remove_row_in_txn(
                                    &mut events,
                                    &mut addr_index,
                                    &mut expiration_index,
                                    &mut by_created_at,
                                    &mut by_author,
                                    &mut by_kind,
                                    &mut by_author_kind,
                                    &mut by_tag,
                                    frozen_id,
                                    |_| true,
                                )?;
                            } else {
                                record.local = Some(local);
                                let encoded = encode_stored_event_record(&record);
                                events
                                    .insert(frozen_id_hex.as_str(), encoded.as_slice())
                                    .map_err(persist_err)?;
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
                                &mut events,
                                &mut addr_index,
                                &mut tombstones,
                                &mut addr_tombstones,
                                &mut expiration_index,
                                &mut by_created_at,
                                &mut by_author,
                                &mut by_kind,
                                &mut by_author_kind,
                                &mut by_tag,
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
                                    SuppressClaimRecord::Addr { key: addr_key, .. } => addr_index
                                        .get(addr_key.as_str())
                                        .map_err(persist_err)?
                                        .map(|guard| {
                                            EventId::from_hex(guard.value())
                                                .expect("redb: decode addr_index id")
                                        }),
                                };
                                if let Some(target_id) = target_id {
                                    if seen_candidates.insert(target_id) {
                                        candidate_ids.push(target_id);
                                    }
                                }
                            }

                            let mut visible_before: HashMap<EventId, bool> = HashMap::new();
                            for id in &candidate_ids {
                                let id_hex = id.to_hex();
                                let visible =
                                    match events.get(id_hex.as_str()).map_err(persist_err)? {
                                        None => false,
                                        Some(guard) => {
                                            let se = decode_stored_event(guard.value());
                                            !is_suppressed_in_txn(
                                                &outbox_suppress_by_id,
                                                &outbox_suppress_by_addr,
                                                &se.event,
                                            )?
                                        }
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
                                let id_hex = id.to_hex();
                                if let Some(guard) =
                                    events.get(id_hex.as_str()).map_err(persist_err)?
                                {
                                    let se = decode_stored_event(guard.value());
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
    fn open_backfills_tag_index_for_legacy_event_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("legacy-tag-index.redb");
        let keys = nostr::Keys::generate();
        let event = room_event(&keys, "legacy-room", 42, "legacy");
        let stored = StoredEvent {
            event: event.clone(),
            provenance: Provenance::first_observation(RelayObserved::new(
                RelayUrl::parse("wss://legacy.example").unwrap(),
                Timestamp::from(43u64),
            )),
        };

        {
            let db = Database::create(&path).unwrap();
            let write_txn = db.begin_write().unwrap();
            {
                let mut events = write_txn.open_table(LEGACY_EVENTS).unwrap();
                let legacy = LegacyStoredEventRecord {
                    event_json: stored.event.as_json(),
                    provenance: stored.provenance.seen.clone(),
                    local: stored.provenance.local.clone(),
                };
                let encoded = serde_json::to_string(&legacy).unwrap();
                events
                    .insert(event.id.to_hex().as_str(), encoded.as_str())
                    .unwrap();
            }
            write_txn.commit().unwrap();
        }

        let store = RedbStore::open(&path).expect("open migrates tag index");
        let rows = store
            .query(&Filter::new().custom_tag(
                SingleLetterTag::lowercase(nostr::Alphabet::H),
                "legacy-room",
            ))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event.id, event.id);
        assert_eq!(
            store
                .query_newest(&Filter::new().author(event.pubkey), 1)
                .unwrap()[0]
                .event
                .id,
            event.id,
            "open must atomically backfill the ordered author index too"
        );

        let read_txn = store.db.begin_read().unwrap();
        let meta = read_txn.open_table(STORE_META).unwrap();
        assert_eq!(
            meta.get(TAG_INDEX_VERSION_KEY).unwrap().unwrap().value(),
            TAG_INDEX_VERSION
        );
        assert_eq!(
            meta.get(ORDERED_INDEX_VERSION_KEY)
                .unwrap()
                .unwrap()
                .value(),
            ORDERED_INDEX_VERSION
        );
        assert_eq!(
            meta.get(EVENT_RECORD_VERSION_KEY).unwrap().unwrap().value(),
            EVENT_RECORD_VERSION
        );
        let legacy = read_txn.open_table(LEGACY_EVENTS).unwrap();
        assert_eq!(
            legacy.len().unwrap(),
            0,
            "JSON rows are removed after migration"
        );
        let events = read_txn.open_table(EVENTS).unwrap();
        let encoded = events
            .get(event.id.to_hex().as_str())
            .unwrap()
            .expect("migrated binary row");
        assert_eq!(&encoded.value()[..4], b"NMPE");
    }
}
