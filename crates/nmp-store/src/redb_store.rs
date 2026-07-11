//! [`RedbStore`] — the persistent, `redb`-backed `EventStore` (M3 step A1).
//!
//! Every table is `TableDefinition<&str, &str>`: keys are plain strings
//! (event-id hex, an [`crate::address_key::AddressKey`] canonical encoding,
//! or a `coverage-hash:relay-url` composite), values are JSON. `nostr::Event`
//! already has a canonical NIP-01 JSON form (`JsonUtil::as_json`/
//! `from_json`) — reused as-is rather than inventing a second wire format;
//! `Provenance` and coverage rows are plain-old-data JSON-encoded the same
//! way, so the whole store has ONE consistent on-disk encoding.
//!
//! `redb`'s own errors (`TableError`/`StorageError`/…) are all invariant
//! violations for this crate's purposes — a healthy embedded DB file does
//! not fail to open a table it created itself, or fail to commit a
//! transaction it started — so they are `.expect()`ed rather than threaded
//! through `EventStore`'s trait signatures (which, matching `MemoryStore`,
//! carry no `Result` at all). A real I/O error here means the on-disk file
//! is corrupt, not a reachable, recoverable condition this crate's callers
//! could usefully branch on today.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use nmp_grammar::ConcreteFilter;
use nostr::filter::MatchEventOptions;
use nostr::secp256k1::schnorr::Signature;
use nostr::{Event, EventId, Filter, JsonUtil, Kind, PublicKey, RelayUrl, Timestamp};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::address_key::{address_key_for, address_key_for_coordinate, candidate_wins};
use crate::coverage::{
    coverage_key as compute_coverage_key, merge_interval, shape_matches, shrink_after_eviction,
    window_erase, ShapeRecord,
};
use crate::{
    AcceptOutcome, AcceptWrite, ClaimSet, CompensateOutcome, CoverageInterval, CoverageKey,
    EventStore, GcReport, InsertOutcome, IntentId, IntentSigState, LocalOrigin, PromoteOutcome,
    Provenance, RecoveredIntent, RefuseReason, RelayObserved, RetractReason, SigState, StoredEvent,
    WriteDurability,
};

const EVENTS: TableDefinition<&str, &str> = TableDefinition::new("events");
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
/// Secondary index for `query` (issue #17): `"{author_hex}:{id_hex}" -> id_hex`,
/// one row per currently-held event keyed by its author. Mirrors the index
/// discipline `EXPIRATION_INDEX` established (issue #31) — a persistent
/// index kept in lockstep on every insert/removal, not derived on read.
/// Lets an author-filtered `query` narrow to a bounded candidate set (that
/// author's rows) via `range`, instead of decoding every row in `EVENTS`.
const BY_AUTHOR: TableDefinition<&str, &str> = TableDefinition::new("by_author");
/// Secondary index for `query` (issue #17): `"{kind:05}:{id_hex}" -> id_hex`
/// (zero-padded so byte-lexicographic order groups one kind's rows
/// contiguously), one row per currently-held event keyed by its kind. Same
/// narrowing purpose as [`BY_AUTHOR`], for kind-filtered queries.
const BY_KIND: TableDefinition<&str, &str> = TableDefinition::new("by_kind");
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
/// `OUTBOX_INTENTS` row. Value: a `StoredEventRecord`-encoded JSON blob
/// (reuses the exact `EVENTS` table encoding — see [`encode_stored_event`]/
/// [`decode_stored_event`]), so the stash round-trips through the same
/// decode path as any other row. Deleted durably by `promote_signed` (R6)
/// or `compensate_write`; never by `recover_outbox` (read-only).
const OUTBOX_DISPLACED: TableDefinition<&str, &str> = TableDefinition::new("outbox_displaced");
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

/// The `events` table's JSON value: the event's canonical NIP-01 JSON plus
/// its merged provenance and, iff the row is locally authored, its
/// [`LocalOrigin`] (issue #2's `Provenance::local` widening — `LocalOrigin`
/// already derives `Serialize`/`Deserialize`, so no separate mirror type is
/// needed here).
#[derive(Debug, Serialize, Deserialize)]
struct StoredEventRecord {
    event_json: String,
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

/// Encode `se` exactly as the `EVENTS` table stores a row — shared by every
/// door that writes a full [`StoredEvent`] back out (the durable
/// `OUTBOX_DISPLACED` stash, `compensate_write`'s restore path).
fn encode_stored_event(se: &StoredEvent) -> String {
    let record = StoredEventRecord {
        event_json: se.event.as_json(),
        provenance: se.provenance.seen.clone(),
        local: se.provenance.local,
    };
    serde_json::to_string(&record).expect("redb: encode stored event")
}

/// Decode one `EVENTS`/`OUTBOX_DISPLACED` JSON value into a [`StoredEvent`]
/// — the read-side counterpart of [`encode_stored_event`].
fn decode_stored_event(json: &str) -> StoredEvent {
    let record: StoredEventRecord = serde_json::from_str(json).expect("redb: decode stored event");
    let event = Event::from_json(&record.event_json).expect("redb: decode event json");
    StoredEvent {
        event,
        provenance: Provenance {
            seen: record.provenance,
            local: record.local,
        },
    }
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

/// [`BY_AUTHOR`]'s key for one stored row.
fn by_author_key(author: &PublicKey, id: &EventId) -> String {
    format!("{}:{}", author.to_hex(), id.to_hex())
}

/// The inclusive bounds of every [`by_author_key`] for `author`: the bare
/// `"{author_hex}:"` prefix sorts before any id suffixed onto it (a shorter
/// string is always `<` a longer string it prefixes), and 64 `'f'`s is the
/// greatest possible id hex — mirrors [`expiration_key_upper_bound`]'s
/// pattern.
fn by_author_range(author: &PublicKey) -> (String, String) {
    let hex = author.to_hex();
    (format!("{hex}:"), format!("{hex}:{}", "f".repeat(64)))
}

/// [`BY_KIND`]'s key for one stored row.
fn by_kind_key(kind: Kind, id: &EventId) -> String {
    format!("{:05}:{}", kind.as_u16(), id.to_hex())
}

/// The inclusive bounds of every [`by_kind_key`] for `kind` — same shape as
/// [`by_author_range`].
fn by_kind_range(kind: Kind) -> (String, String) {
    let prefix = format!("{:05}", kind.as_u16());
    (format!("{prefix}:"), format!("{prefix}:{}", "f".repeat(64)))
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
) -> bool {
    let key = id_tombstone_key(&event.id, &event.pubkey);
    if tombstones
        .get(key.as_str())
        .expect("redb: get tombstone")
        .is_some()
    {
        return true;
    }
    if let Some(key) = address_key_for(event) {
        let key_str = key.to_redb_key();
        if let Some(guard) = addr_tombstones
            .get(key_str.as_str())
            .expect("redb: get addr tombstone")
        {
            let rec: AddrTombstoneRecord =
                serde_json::from_str(guard.value()).expect("redb: decode addr tombstone");
            if event.created_at.as_secs() <= rec.ceiling {
                return true;
            }
        }
    }
    false
}

/// Remove `id`'s row within an already-open write transaction, iff
/// `predicate` accepts the decoded row — clearing the address index (if it
/// still points at `id`), the expiration index (if the row carried a
/// NIP-40 `expiration`), and the [`BY_AUTHOR`]/[`BY_KIND`] query indexes in
/// the same pass. Shared by the trait's own `remove` (`predicate` always
/// `true`) and kind:5 processing (`predicate` is the NIP-09 author-only
/// check).
#[allow(clippy::too_many_arguments)]
fn remove_row_in_txn(
    events: &mut redb::Table<'_, &str, &str>,
    addr_index: &mut redb::Table<'_, &str, &str>,
    expiration_index: &mut redb::Table<'_, &str, &str>,
    by_author: &mut redb::Table<'_, &str, &str>,
    by_kind: &mut redb::Table<'_, &str, &str>,
    id: EventId,
    predicate: impl FnOnce(&StoredEvent) -> bool,
) -> Option<StoredEvent> {
    let id_hex = id.to_hex();
    let json = events
        .get(id_hex.as_str())
        .expect("redb: get event")?
        .value()
        .to_string();
    let se = decode_stored_event(&json);
    if !predicate(&se) {
        return None;
    }

    events.remove(id_hex.as_str()).expect("redb: remove event");
    by_author
        .remove(by_author_key(&se.event.pubkey, &id).as_str())
        .expect("redb: remove by_author");
    by_kind
        .remove(by_kind_key(se.event.kind, &id).as_str())
        .expect("redb: remove by_kind");

    if let Some(addr_key) = address_key_for(&se.event) {
        let addr_key_str = addr_key.to_redb_key();
        let still_points_here = addr_index
            .get(addr_key_str.as_str())
            .expect("redb: get addr_index")
            .map(|guard| guard.value().to_string())
            == Some(id_hex.clone());
        if still_points_here {
            addr_index
                .remove(addr_key_str.as_str())
                .expect("redb: remove addr_index");
        }
    }

    if let Some(ts) = se.event.tags.expiration().copied() {
        let exp_key = expiration_key(ts, &id);
        expiration_index
            .remove(exp_key.as_str())
            .expect("redb: remove expiration_index");
    }

    Some(se)
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
    events: &mut redb::Table<'_, &str, &str>,
    addr_index: &mut redb::Table<'_, &str, &str>,
    tombstones: &mut redb::Table<'_, &str, &str>,
    addr_tombstones: &mut redb::Table<'_, &str, &str>,
    expiration_index: &mut redb::Table<'_, &str, &str>,
    by_author: &mut redb::Table<'_, &str, &str>,
    by_kind: &mut redb::Table<'_, &str, &str>,
    deleting: &Event,
) -> Vec<StoredEvent> {
    let mut deleted = Vec::new();
    let deleting_id_hex = deleting.id.to_hex();
    let deleting_author_hex = deleting.pubkey.to_hex();

    let target_ids: Vec<EventId> = deleting.tags.event_ids().copied().collect();
    for target_id in target_ids {
        if let Some(removed) = remove_row_in_txn(
            events,
            addr_index,
            expiration_index,
            by_author,
            by_kind,
            target_id,
            |se| se.event.pubkey == deleting.pubkey,
        ) {
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
            .expect("redb: insert tombstone");
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
            .expect("redb: get addr tombstone")
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
                .expect("redb: insert addr tombstone");
        }

        let current_id_hex = addr_index
            .get(key_str.as_str())
            .expect("redb: get addr_index")
            .map(|guard| guard.value().to_string());
        if let Some(current_id_hex) = current_id_hex {
            let current_id =
                EventId::from_hex(&current_id_hex).expect("redb: decode addr_index id");
            if let Some(removed) = remove_row_in_txn(
                events,
                addr_index,
                expiration_index,
                by_author,
                by_kind,
                current_id,
                |se| se.event.created_at <= deleting.created_at,
            ) {
                deleted.push(removed);
            }
        }
    }

    deleted
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
    events: &mut redb::Table<'_, &str, &str>,
    addr_index: &mut redb::Table<'_, &str, &str>,
    tombstones: &redb::Table<'_, &str, &str>,
    addr_tombstones: &redb::Table<'_, &str, &str>,
    expiration_index: &mut redb::Table<'_, &str, &str>,
    by_author: &mut redb::Table<'_, &str, &str>,
    by_kind: &mut redb::Table<'_, &str, &str>,
    se: StoredEvent,
) -> Option<StoredEvent> {
    let id_hex = se.event.id.to_hex();

    let existing_json = events
        .get(id_hex.as_str())
        .expect("redb: get event")
        .map(|guard| guard.value().to_string());
    if let Some(existing_json) = existing_json {
        // Extremely unlikely (a relay redelivered this exact id while it
        // sat in the stash) — merge whatever the stash's `seen` adds; keep
        // whatever `local` the live row already has.
        let mut record: StoredEventRecord =
            serde_json::from_str(&existing_json).expect("redb: decode stored event");
        let mut provenance = Provenance {
            seen: record.provenance,
            local: record.local,
        };
        for (relay, at) in &se.provenance.seen {
            provenance.merge_observation(&RelayObserved::new(relay.clone(), *at));
        }
        record.provenance = provenance.seen.clone();
        record.local = provenance.local;
        let encoded = serde_json::to_string(&record).expect("redb: encode stored event");
        events
            .insert(id_hex.as_str(), encoded.as_str())
            .expect("redb: update event provenance");
        let event = Event::from_json(&record.event_json).expect("redb: decode event json");
        return Some(StoredEvent { event, provenance });
    }
    if tombstone_refuses(tombstones, addr_tombstones, &se.event) {
        return None;
    }

    let encoded = encode_stored_event(&se);

    match address_key_for(&se.event) {
        None => {
            events
                .insert(id_hex.as_str(), encoded.as_str())
                .expect("redb: insert event");
            by_author
                .insert(
                    by_author_key(&se.event.pubkey, &se.event.id).as_str(),
                    id_hex.as_str(),
                )
                .expect("redb: insert by_author");
            by_kind
                .insert(
                    by_kind_key(se.event.kind, &se.event.id).as_str(),
                    id_hex.as_str(),
                )
                .expect("redb: insert by_kind");
            if let Some(ts) = se.event.tags.expiration().copied() {
                let exp_key = expiration_key(ts, &se.event.id);
                expiration_index
                    .insert(exp_key.as_str(), id_hex.as_str())
                    .expect("redb: insert expiration_index");
            }
            Some(se)
        }
        Some(addr_key) => {
            let addr_key_str = addr_key.to_redb_key();
            let current_id_hex = addr_index
                .get(addr_key_str.as_str())
                .expect("redb: get addr_index")
                .map(|guard| guard.value().to_string());

            match current_id_hex {
                None => {
                    events
                        .insert(id_hex.as_str(), encoded.as_str())
                        .expect("redb: insert event");
                    addr_index
                        .insert(addr_key_str.as_str(), id_hex.as_str())
                        .expect("redb: insert addr_index");
                    by_author
                        .insert(
                            by_author_key(&se.event.pubkey, &se.event.id).as_str(),
                            id_hex.as_str(),
                        )
                        .expect("redb: insert by_author");
                    by_kind
                        .insert(
                            by_kind_key(se.event.kind, &se.event.id).as_str(),
                            id_hex.as_str(),
                        )
                        .expect("redb: insert by_kind");
                    if let Some(ts) = se.event.tags.expiration().copied() {
                        let exp_key = expiration_key(ts, &se.event.id);
                        expiration_index
                            .insert(exp_key.as_str(), id_hex.as_str())
                            .expect("redb: insert expiration_index");
                    }
                    Some(se)
                }
                Some(current_id_hex) => {
                    let current_json = events
                        .get(current_id_hex.as_str())
                        .expect("redb: get current winner")
                        .expect("addr_index must always point at a stored event")
                        .value()
                        .to_string();
                    let current_event = decode_stored_event(&current_json).event;

                    if candidate_wins(&se.event, &current_event) {
                        let current_id =
                            EventId::from_hex(&current_id_hex).expect("redb: decode addr_index id");
                        remove_row_in_txn(
                            events,
                            addr_index,
                            expiration_index,
                            by_author,
                            by_kind,
                            current_id,
                            |_| true,
                        )
                        .expect("addr_index must always point at a stored event");

                        events
                            .insert(id_hex.as_str(), encoded.as_str())
                            .expect("redb: insert winning event");
                        addr_index
                            .insert(addr_key_str.as_str(), id_hex.as_str())
                            .expect("redb: update addr_index");
                        by_author
                            .insert(
                                by_author_key(&se.event.pubkey, &se.event.id).as_str(),
                                id_hex.as_str(),
                            )
                            .expect("redb: insert by_author");
                        by_kind
                            .insert(
                                by_kind_key(se.event.kind, &se.event.id).as_str(),
                                id_hex.as_str(),
                            )
                            .expect("redb: insert by_kind");
                        if let Some(ts) = se.event.tags.expiration().copied() {
                            let exp_key = expiration_key(ts, &se.event.id);
                            expiration_index
                                .insert(exp_key.as_str(), id_hex.as_str())
                                .expect("redb: insert expiration_index");
                        }
                        Some(se)
                    } else {
                        // Stale — §3.4: nothing churns.
                        None
                    }
                }
            }
        }
    }
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
pub struct RedbStore {
    db: Database,
    /// Test-only instrumentation for the `query`-indexing falsifier
    /// (`query_by_author_does_not_scan_all_rows`, issue #17): counts every
    /// row `query` actually JSON-decodes across a run, so a test can assert
    /// an author/kind-narrowed query decodes only its match set, never
    /// every row in `EVENTS`. Absent from the struct entirely outside
    /// `cfg(test)` — zero cost in a normal build.
    #[cfg(test)]
    examined_rows: AtomicU64,
}

impl RedbStore {
    /// Open (creating if absent) a `redb` database file at `path`, ensuring
    /// all tables exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(EVENTS)?;
            write_txn.open_table(ADDR_INDEX)?;
            write_txn.open_table(COVERAGE)?;
            write_txn.open_table(TOMBSTONES)?;
            write_txn.open_table(ADDR_TOMBSTONES)?;
            write_txn.open_table(EXPIRATION_INDEX)?;
            write_txn.open_table(BY_AUTHOR)?;
            write_txn.open_table(BY_KIND)?;
            write_txn.open_table(OUTBOX_INTENTS)?;
            write_txn.open_table(OUTBOX_DISPLACED)?;
            write_txn.open_table(OUTBOX_ATTEMPTS)?;
        }
        write_txn.commit()?;
        Ok(Self {
            db,
            #[cfg(test)]
            examined_rows: AtomicU64::new(0),
        })
    }

    /// Current value of [`Self::examined_rows`] — the `query`-indexing
    /// falsifier's read side.
    #[cfg(test)]
    fn examined_rows(&self) -> u64 {
        self.examined_rows.load(Ordering::Relaxed)
    }

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
        format!("{hex}:{}", relay.as_str())
    }

    /// Decode one `EVENTS` row's JSON value into a [`StoredEvent`] —
    /// `query`'s one decode point, so [`Self::examined_rows`] (test-only)
    /// counts every row `query` actually pays the JSON-decode cost for,
    /// regardless of which of `query`'s three paths (id/indexed/full-scan)
    /// reached it.
    fn decode_row(&self, json: &str) -> StoredEvent {
        #[cfg(test)]
        self.examined_rows.fetch_add(1, Ordering::Relaxed);
        decode_stored_event(json)
    }

    /// `query`'s index-narrowing step (issue #17): `Some(ids)` -- the
    /// bounded candidate set gathered from `BY_AUTHOR`/`BY_KIND` for
    /// whichever of `filter.authors`/`filter.kinds` is present (intersected
    /// if both are) -- or `None` iff the filter carries neither, in which
    /// case nothing narrows it and `query` must fall back to a full scan.
    /// Does not touch `filter.ids`; that is `query`'s own, even cheaper,
    /// fast path.
    fn candidate_ids(
        &self,
        read_txn: &redb::ReadTransaction,
        filter: &Filter,
    ) -> Option<HashSet<EventId>> {
        let by_authors = filter.authors.as_ref().map(|authors| {
            let by_author = read_txn
                .open_table(BY_AUTHOR)
                .expect("redb: open by_author");
            let mut ids = HashSet::new();
            for author in authors {
                let (lower, upper) = by_author_range(author);
                for entry in by_author
                    .range::<&str>(lower.as_str()..=upper.as_str())
                    .expect("redb: range by_author")
                {
                    let (_key, value) = entry.expect("redb: read by_author entry");
                    ids.insert(
                        EventId::from_hex(value.value()).expect("redb: decode by_author id"),
                    );
                }
            }
            ids
        });

        let by_kinds = filter.kinds.as_ref().map(|kinds| {
            let by_kind = read_txn.open_table(BY_KIND).expect("redb: open by_kind");
            let mut ids = HashSet::new();
            for kind in kinds {
                let (lower, upper) = by_kind_range(*kind);
                for entry in by_kind
                    .range::<&str>(lower.as_str()..=upper.as_str())
                    .expect("redb: range by_kind")
                {
                    let (_key, value) = entry.expect("redb: read by_kind entry");
                    ids.insert(EventId::from_hex(value.value()).expect("redb: decode by_kind id"));
                }
            }
            ids
        });

        match (by_authors, by_kinds) {
            (Some(a), Some(k)) => Some(a.intersection(&k).copied().collect()),
            (Some(a), None) => Some(a),
            (None, Some(k)) => Some(k),
            (None, None) => None,
        }
    }
}

impl EventStore for RedbStore {
    fn insert(&mut self, event: Event, from: RelayObserved) -> InsertOutcome {
        // Refused at the door FIRST: an already-expired event is never
        // stored, so it never touches dedup or supersession at all.
        if event.is_expired_at(&from.at) {
            return InsertOutcome::Refused(RefuseReason::AlreadyExpired);
        }

        let write_txn = self.db.begin_write().expect("redb: begin_write");
        let outcome = {
            let mut events = write_txn.open_table(EVENTS).expect("redb: open events");
            let mut addr_index = write_txn
                .open_table(ADDR_INDEX)
                .expect("redb: open addr_index");
            let mut tombstones = write_txn
                .open_table(TOMBSTONES)
                .expect("redb: open tombstones");
            let mut addr_tombstones = write_txn
                .open_table(ADDR_TOMBSTONES)
                .expect("redb: open addr_tombstones");
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .expect("redb: open expiration_index");
            let mut by_author = write_txn
                .open_table(BY_AUTHOR)
                .expect("redb: open by_author");
            let mut by_kind = write_txn.open_table(BY_KIND).expect("redb: open by_kind");
            let id_hex = event.id.to_hex();

            let existing_json = events
                .get(id_hex.as_str())
                .expect("redb: get event")
                .map(|guard| guard.value().to_string());

            if let Some(existing_json) = existing_json {
                // Dedup-by-id FIRST: merge provenance, no index churn. Goes
                // through `Provenance::merge_observation` (not a re-derived
                // copy) so the persisted backend can never diverge from
                // `MemoryStore`'s merge semantics.
                let mut record: StoredEventRecord =
                    serde_json::from_str(&existing_json).expect("redb: decode stored event");
                let mut provenance = Provenance {
                    seen: record.provenance,
                    local: record.local,
                };
                let grew = provenance.merge_observation(&from);
                // `merge_observation` never touches `local` (a relay echo
                // of an already-local row keeps its local provenance,
                // retraction doc §4.1) — `provenance.local` is unchanged,
                // written straight back.
                record.provenance = provenance.seen;
                record.local = provenance.local;
                let encoded = serde_json::to_string(&record).expect("redb: encode stored event");
                events
                    .insert(id_hex.as_str(), encoded.as_str())
                    .expect("redb: update event provenance");
                InsertOutcome::Duplicate {
                    provenance_grew: grew,
                }
            } else if tombstone_refuses(&tombstones, &addr_tombstones, &event) {
                // Tombstone check, AFTER dedup-by-id, BEFORE storage
                // (retraction-and-negative-deltas.md §2).
                InsertOutcome::Refused(RefuseReason::Tombstoned)
            } else {
                let is_deletion = event.kind == Kind::EventDeletion;
                let record = StoredEventRecord {
                    event_json: event.as_json(),
                    provenance: {
                        let mut m = BTreeMap::new();
                        m.insert(from.relay.clone(), from.at);
                        m
                    },
                    local: None,
                };
                let encoded = serde_json::to_string(&record).expect("redb: encode stored event");

                let outcome = match address_key_for(&event) {
                    None => {
                        events
                            .insert(id_hex.as_str(), encoded.as_str())
                            .expect("redb: insert event");
                        by_author
                            .insert(
                                by_author_key(&event.pubkey, &event.id).as_str(),
                                id_hex.as_str(),
                            )
                            .expect("redb: insert by_author");
                        by_kind
                            .insert(by_kind_key(event.kind, &event.id).as_str(), id_hex.as_str())
                            .expect("redb: insert by_kind");
                        if let Some(ts) = event.tags.expiration().copied() {
                            let exp_key = expiration_key(ts, &event.id);
                            expiration_index
                                .insert(exp_key.as_str(), id_hex.as_str())
                                .expect("redb: insert expiration_index");
                        }
                        InsertOutcome::Inserted
                    }
                    Some(addr_key) => {
                        let addr_key_str = addr_key.to_redb_key();
                        let current_id_hex = addr_index
                            .get(addr_key_str.as_str())
                            .expect("redb: get addr_index")
                            .map(|guard| guard.value().to_string());

                        match current_id_hex {
                            None => {
                                events
                                    .insert(id_hex.as_str(), encoded.as_str())
                                    .expect("redb: insert event");
                                addr_index
                                    .insert(addr_key_str.as_str(), id_hex.as_str())
                                    .expect("redb: insert addr_index");
                                by_author
                                    .insert(
                                        by_author_key(&event.pubkey, &event.id).as_str(),
                                        id_hex.as_str(),
                                    )
                                    .expect("redb: insert by_author");
                                by_kind
                                    .insert(
                                        by_kind_key(event.kind, &event.id).as_str(),
                                        id_hex.as_str(),
                                    )
                                    .expect("redb: insert by_kind");
                                if let Some(ts) = event.tags.expiration().copied() {
                                    let exp_key = expiration_key(ts, &event.id);
                                    expiration_index
                                        .insert(exp_key.as_str(), id_hex.as_str())
                                        .expect("redb: insert expiration_index");
                                }
                                InsertOutcome::Inserted
                            }
                            Some(current_id_hex) => {
                                let current_json = events
                                    .get(current_id_hex.as_str())
                                    .expect("redb: get current winner")
                                    .expect("addr_index must always point at a stored event")
                                    .value()
                                    .to_string();
                                let current_record: StoredEventRecord =
                                    serde_json::from_str(&current_json)
                                        .expect("redb: decode current winner");
                                let current_event = Event::from_json(&current_record.event_json)
                                    .expect("redb: decode current winner event json");

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
                                        .expect("redb: remove superseded event");
                                    by_author
                                        .remove(
                                            by_author_key(
                                                &replaced.event.pubkey,
                                                &replaced.event.id,
                                            )
                                            .as_str(),
                                        )
                                        .expect("redb: remove by_author");
                                    by_kind
                                        .remove(
                                            by_kind_key(replaced.event.kind, &replaced.event.id)
                                                .as_str(),
                                        )
                                        .expect("redb: remove by_kind");
                                    if let Some(ts) = replaced.event.tags.expiration().copied() {
                                        let exp_key = expiration_key(ts, &replaced.event.id);
                                        expiration_index
                                            .remove(exp_key.as_str())
                                            .expect("redb: remove expiration_index");
                                    }
                                    events
                                        .insert(id_hex.as_str(), encoded.as_str())
                                        .expect("redb: insert winning event");
                                    addr_index
                                        .insert(addr_key_str.as_str(), id_hex.as_str())
                                        .expect("redb: update addr_index");
                                    by_author
                                        .insert(
                                            by_author_key(&event.pubkey, &event.id).as_str(),
                                            id_hex.as_str(),
                                        )
                                        .expect("redb: insert by_author");
                                    by_kind
                                        .insert(
                                            by_kind_key(event.kind, &event.id).as_str(),
                                            id_hex.as_str(),
                                        )
                                        .expect("redb: insert by_kind");
                                    if let Some(ts) = event.tags.expiration().copied() {
                                        let exp_key = expiration_key(ts, &event.id);
                                        expiration_index
                                            .insert(exp_key.as_str(), id_hex.as_str())
                                            .expect("redb: insert expiration_index");
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
                            &mut by_author,
                            &mut by_kind,
                            &event,
                        );
                        InsertOutcome::Kind5Processed { deleted }
                    } else {
                        outcome
                    }
                } else {
                    outcome
                }
            }
        };
        write_txn.commit().expect("redb: commit insert");
        outcome
    }

    fn query(&self, filter: &Filter) -> Vec<StoredEvent> {
        let read_txn = self.db.begin_read().expect("redb: begin_read");
        let events = read_txn.open_table(EVENTS).expect("redb: open events");

        // Fast path: `filter.ids` narrows directly through `EVENTS`'s own
        // id-keyed rows -- no secondary index needed, bounded by `|ids|`
        // regardless of table size (issue #17).
        if let Some(ids) = &filter.ids {
            let mut out = Vec::new();
            for id in ids {
                let id_hex = id.to_hex();
                let Some(value) = events.get(id_hex.as_str()).expect("redb: get event") else {
                    continue;
                };
                let se = self.decode_row(value.value());
                if filter.match_event(&se.event, MatchEventOptions::new()) {
                    out.push(se);
                }
            }
            return out;
        }

        // Otherwise, narrow via `BY_AUTHOR`/`BY_KIND` when the filter
        // carries either -- bounded by the matching authors'/kinds' own row
        // counts, never the whole table. `candidate_ids` returns `None` iff
        // neither is present (e.g. a bare generic-tag or search-only
        // filter), in which case no index narrows it and the pre-existing
        // full scan is the only correct fallback.
        match self.candidate_ids(&read_txn, filter) {
            Some(candidates) => {
                let mut out = Vec::with_capacity(candidates.len());
                for id in candidates {
                    let id_hex = id.to_hex();
                    let Some(value) = events.get(id_hex.as_str()).expect("redb: get event") else {
                        // A stale index entry outliving its row (e.g. a GC'd
                        // event whose BY_AUTHOR/BY_KIND rows haven't been
                        // touched by that path) — harmless, just skip.
                        continue;
                    };
                    let se = self.decode_row(value.value());
                    if filter.match_event(&se.event, MatchEventOptions::new()) {
                        out.push(se);
                    }
                }
                out
            }
            None => {
                let mut out = Vec::new();
                for entry in events.iter().expect("redb: iter events") {
                    let (_key, value) = entry.expect("redb: read event entry");
                    let se = self.decode_row(value.value());
                    if filter.match_event(&se.event, MatchEventOptions::new()) {
                        out.push(se);
                    }
                }
                out
            }
        }
    }

    fn remove(&mut self, id: EventId, _reason: RetractReason) -> Option<StoredEvent> {
        let write_txn = self.db.begin_write().expect("redb: begin_write");
        let removed = {
            let mut events = write_txn.open_table(EVENTS).expect("redb: open events");
            let mut addr_index = write_txn
                .open_table(ADDR_INDEX)
                .expect("redb: open addr_index");
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .expect("redb: open expiration_index");
            let mut by_author = write_txn
                .open_table(BY_AUTHOR)
                .expect("redb: open by_author");
            let mut by_kind = write_txn.open_table(BY_KIND).expect("redb: open by_kind");
            remove_row_in_txn(
                &mut events,
                &mut addr_index,
                &mut expiration_index,
                &mut by_author,
                &mut by_kind,
                id,
                |_| true,
            )
        };
        write_txn.commit().expect("redb: commit remove");
        removed
    }

    fn expire_due(&mut self, now: Timestamp) -> Vec<StoredEvent> {
        let write_txn = self.db.begin_write().expect("redb: begin_write");
        let removed = {
            let mut events = write_txn.open_table(EVENTS).expect("redb: open events");
            let mut addr_index = write_txn
                .open_table(ADDR_INDEX)
                .expect("redb: open addr_index");
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .expect("redb: open expiration_index");
            let mut by_author = write_txn
                .open_table(BY_AUTHOR)
                .expect("redb: open by_author");
            let mut by_kind = write_txn.open_table(BY_KIND).expect("redb: open by_kind");

            let upper = expiration_key_upper_bound(now);
            let due_ids: Vec<EventId> = expiration_index
                .range::<&str>(..=upper.as_str())
                .expect("redb: range expiration_index")
                .map(|entry| {
                    let (_key, value) = entry.expect("redb: read expiration_index entry");
                    EventId::from_hex(value.value()).expect("redb: decode expiration_index id")
                })
                .collect();

            due_ids
                .into_iter()
                .filter_map(|id| {
                    remove_row_in_txn(
                        &mut events,
                        &mut addr_index,
                        &mut expiration_index,
                        &mut by_author,
                        &mut by_kind,
                        id,
                        |_| true,
                    )
                })
                .collect()
        };
        write_txn.commit().expect("redb: commit expire_due");
        removed
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
        filter: &ConcreteFilter,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) {
        let key = compute_coverage_key(filter);
        let shape = window_erase(filter);
        let row_key = Self::coverage_row_key(key, relay);

        let write_txn = self.db.begin_write().expect("redb: begin_write");
        {
            let mut coverage = write_txn.open_table(COVERAGE).expect("redb: open coverage");
            let existing = coverage
                .get(row_key.as_str())
                .expect("redb: get coverage row")
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
                .expect("redb: insert coverage row");
        }
        write_txn.commit().expect("redb: commit record_coverage");
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

    fn gc(&mut self, claims: &ClaimSet) -> GcReport {
        let mut report = GcReport::default();

        let write_txn = self.db.begin_write().expect("redb: begin_write");
        {
            let mut events = write_txn.open_table(EVENTS).expect("redb: open events");
            let mut coverage = write_txn.open_table(COVERAGE).expect("redb: open coverage");
            let mut by_author = write_txn
                .open_table(BY_AUTHOR)
                .expect("redb: open by_author");
            let mut by_kind = write_txn.open_table(BY_KIND).expect("redb: open by_kind");

            // Pass 1: find victims (regular events matched by no claim, and
            // not an open — unsigned — local intent: Fable checkpoint R5,
            // mirrors `MemoryStore::gc`'s exclusion exactly). Collected up
            // front into owned values so the removal pass below never holds
            // a borrow across a mutation.
            let mut victims: Vec<(String, Event)> = Vec::new();
            for entry in events.iter().expect("redb: iter events") {
                let (key, value) = entry.expect("redb: read event entry");
                let record: StoredEventRecord =
                    serde_json::from_str(value.value()).expect("redb: decode stored event");
                let event = Event::from_json(&record.event_json).expect("redb: decode event json");
                if address_key_for(&event).is_none()
                    && !is_open_local_intent(&record)
                    && !claims.is_claimed(&event)
                {
                    victims.push((key.value().to_string(), event));
                }
            }

            for (id_hex, event) in &victims {
                events
                    .remove(id_hex.as_str())
                    .expect("redb: remove gc victim");
                // Keep BY_AUTHOR/BY_KIND in lockstep with EVENTS -- a stale
                // index row surviving a gc'd event would keep costing
                // `query` a wasted `events.get` miss on every future hit
                // (harmless, see `query`'s `None` skip, but unbounded
                // growth otherwise).
                by_author
                    .remove(by_author_key(&event.pubkey, &event.id).as_str())
                    .expect("redb: remove by_author");
                by_kind
                    .remove(by_kind_key(event.kind, &event.id).as_str())
                    .expect("redb: remove by_kind");
                report.events_evicted += 1;
            }

            // Pass 2: shrink/delete every coverage row an evicted event
            // falls inside AND whose retained shape matches it. Same write
            // transaction as the event removals above — the shrink/delete
            // and the event delete commit atomically together (ruling §5:
            // never leave a watermark claiming coverage of evicted data).
            let mut row_updates: Vec<(String, Option<CoverageRowRecord>)> = Vec::new();
            for entry in coverage.iter().expect("redb: iter coverage") {
                let (row_key, value) = entry.expect("redb: read coverage entry");
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
                        coverage
                            .remove(row_key.as_str())
                            .expect("redb: remove coverage row");
                        report.coverage_rows_deleted += 1;
                    }
                    Some(record) => {
                        let encoded =
                            serde_json::to_string(&record).expect("redb: encode coverage row");
                        coverage
                            .insert(row_key.as_str(), encoded.as_str())
                            .expect("redb: update coverage row");
                        report.coverage_rows_shrunk += 1;
                    }
                }
            }
        }
        write_txn.commit().expect("redb: commit gc");

        report
    }

    fn accept_write(&mut self, accept: AcceptWrite) -> AcceptOutcome {
        let AcceptWrite {
            intent_id,
            receipt_id,
            frozen,
            expected_pubkey,
            signing_identity_ref,
            durability,
            routing,
            sig_state,
            accepted_at,
        } = accept;

        // Refused at the door FIRST, same as `insert`: never journaled,
        // nothing to recover (R3).
        if frozen.is_expired_at(&accepted_at) {
            return AcceptOutcome::Refused(RefuseReason::AlreadyExpired);
        }

        let write_txn = self.db.begin_write().expect("redb: begin_write");
        let outcome = {
            let mut events = write_txn.open_table(EVENTS).expect("redb: open events");
            let mut addr_index = write_txn
                .open_table(ADDR_INDEX)
                .expect("redb: open addr_index");
            let tombstones = write_txn
                .open_table(TOMBSTONES)
                .expect("redb: open tombstones");
            let addr_tombstones = write_txn
                .open_table(ADDR_TOMBSTONES)
                .expect("redb: open addr_tombstones");
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .expect("redb: open expiration_index");
            let mut by_author = write_txn
                .open_table(BY_AUTHOR)
                .expect("redb: open by_author");
            let mut by_kind = write_txn.open_table(BY_KIND).expect("redb: open by_kind");
            let mut outbox_intents = write_txn
                .open_table(OUTBOX_INTENTS)
                .expect("redb: open outbox_intents");
            let mut outbox_displaced = write_txn
                .open_table(OUTBOX_DISPLACED)
                .expect("redb: open outbox_displaced");

            let id_hex = frozen.id.to_hex();
            let existing_json = events
                .get(id_hex.as_str())
                .expect("redb: get event")
                .map(|guard| guard.value().to_string());

            // Same tombstone-refusal + dedup-by-id + replaceable/addressable
            // supersession rules `insert` runs — see this fn's own doc and
            // `AcceptOutcome`'s. `Refused` is the ONLY branch that skips the
            // journal write below (R3).
            let (result, displaced): (AcceptOutcome, Option<StoredEvent>) =
                if let Some(existing_json) = existing_json {
                    (
                        AcceptOutcome::Duplicate {
                            row: decode_stored_event(&existing_json),
                        },
                        None,
                    )
                } else if tombstone_refuses(&tombstones, &addr_tombstones, &frozen) {
                    (AcceptOutcome::Refused(RefuseReason::Tombstoned), None)
                } else {
                    let local = LocalOrigin {
                        intent_id,
                        sig_state: SigState::Pending,
                        accepted_at,
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
                                .insert(id_hex.as_str(), encoded.as_str())
                                .expect("redb: insert event");
                            by_author
                                .insert(
                                    by_author_key(&frozen.pubkey, &frozen.id).as_str(),
                                    id_hex.as_str(),
                                )
                                .expect("redb: insert by_author");
                            by_kind
                                .insert(
                                    by_kind_key(frozen.kind, &frozen.id).as_str(),
                                    id_hex.as_str(),
                                )
                                .expect("redb: insert by_kind");
                            if let Some(ts) = frozen.tags.expiration().copied() {
                                let exp_key = expiration_key(ts, &frozen.id);
                                expiration_index
                                    .insert(exp_key.as_str(), id_hex.as_str())
                                    .expect("redb: insert expiration_index");
                            }
                            (AcceptOutcome::Inserted { row: stored }, None)
                        }
                        Some(addr_key) => {
                            let addr_key_str = addr_key.to_redb_key();
                            let current_id_hex = addr_index
                                .get(addr_key_str.as_str())
                                .expect("redb: get addr_index")
                                .map(|guard| guard.value().to_string());

                            match current_id_hex {
                                None => {
                                    events
                                        .insert(id_hex.as_str(), encoded.as_str())
                                        .expect("redb: insert event");
                                    addr_index
                                        .insert(addr_key_str.as_str(), id_hex.as_str())
                                        .expect("redb: insert addr_index");
                                    by_author
                                        .insert(
                                            by_author_key(&frozen.pubkey, &frozen.id).as_str(),
                                            id_hex.as_str(),
                                        )
                                        .expect("redb: insert by_author");
                                    by_kind
                                        .insert(
                                            by_kind_key(frozen.kind, &frozen.id).as_str(),
                                            id_hex.as_str(),
                                        )
                                        .expect("redb: insert by_kind");
                                    if let Some(ts) = frozen.tags.expiration().copied() {
                                        let exp_key = expiration_key(ts, &frozen.id);
                                        expiration_index
                                            .insert(exp_key.as_str(), id_hex.as_str())
                                            .expect("redb: insert expiration_index");
                                    }
                                    (AcceptOutcome::Inserted { row: stored }, None)
                                }
                                Some(current_id_hex) => {
                                    let current_json = events
                                        .get(current_id_hex.as_str())
                                        .expect("redb: get current winner")
                                        .expect("addr_index must always point at a stored event")
                                        .value()
                                        .to_string();
                                    let current_event = decode_stored_event(&current_json).event;

                                    if candidate_wins(&frozen, &current_event) {
                                        let current_id = EventId::from_hex(&current_id_hex)
                                            .expect("redb: decode addr_index id");
                                        let replaced = remove_row_in_txn(
                                            &mut events,
                                            &mut addr_index,
                                            &mut expiration_index,
                                            &mut by_author,
                                            &mut by_kind,
                                            current_id,
                                            |_| true,
                                        )
                                        .expect("addr_index must always point at a stored event");

                                        events
                                            .insert(id_hex.as_str(), encoded.as_str())
                                            .expect("redb: insert winning event");
                                        addr_index
                                            .insert(addr_key_str.as_str(), id_hex.as_str())
                                            .expect("redb: update addr_index");
                                        by_author
                                            .insert(
                                                by_author_key(&frozen.pubkey, &frozen.id).as_str(),
                                                id_hex.as_str(),
                                            )
                                            .expect("redb: insert by_author");
                                        by_kind
                                            .insert(
                                                by_kind_key(frozen.kind, &frozen.id).as_str(),
                                                id_hex.as_str(),
                                            )
                                            .expect("redb: insert by_kind");
                                        if let Some(ts) = frozen.tags.expiration().copied() {
                                            let exp_key = expiration_key(ts, &frozen.id);
                                            expiration_index
                                                .insert(exp_key.as_str(), id_hex.as_str())
                                                .expect("redb: insert expiration_index");
                                        }
                                        (
                                            AcceptOutcome::Superseded {
                                                row: stored,
                                                replaced: Box::new(replaced.clone()),
                                            },
                                            Some(replaced),
                                        )
                                    } else {
                                        (AcceptOutcome::Stale, None)
                                    }
                                }
                            }
                        }
                    }
                };

            // R7: the intent's full journal payload commits in this SAME
            // transaction as the event-table mutation above — a crash here
            // leaves either nothing or a fully `recover_outbox`-able
            // `Accepted`. R3: `Refused` is the one outcome that journals
            // nothing at all.
            if !matches!(result, AcceptOutcome::Refused(_)) {
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
                    .expect("redb: insert outbox_intents");

                if let Some(displaced) = &displaced {
                    let encoded_displaced = encode_stored_event(displaced);
                    outbox_displaced
                        .insert(key.as_str(), encoded_displaced.as_str())
                        .expect("redb: insert outbox_displaced");
                }
            }

            result
        };
        write_txn.commit().expect("redb: commit accept_write");
        outcome
    }

    fn promote_signed(&mut self, id: EventId, sig: Signature) -> PromoteOutcome {
        let write_txn = self.db.begin_write().expect("redb: begin_write");
        let outcome = {
            let mut events = write_txn.open_table(EVENTS).expect("redb: open events");
            let mut outbox_intents = write_txn
                .open_table(OUTBOX_INTENTS)
                .expect("redb: open outbox_intents");
            let mut outbox_displaced = write_txn
                .open_table(OUTBOX_DISPLACED)
                .expect("redb: open outbox_displaced");

            let id_hex = id.to_hex();
            let existing_json = events
                .get(id_hex.as_str())
                .expect("redb: get event")
                .map(|guard| guard.value().to_string());

            match existing_json {
                None => PromoteOutcome::NotFound,
                Some(json) => {
                    let mut record: StoredEventRecord =
                        serde_json::from_str(&json).expect("redb: decode stored event");
                    match record.local {
                        None => PromoteOutcome::NotFound,
                        Some(mut local) => {
                            let key = intent_key(local.intent_id);
                            local.sig_state = SigState::Signed;
                            record.local = Some(local);

                            // Swap the sentinel for the real signature —
                            // same id (a NIP-01 id never depends on `sig`),
                            // so this is purely a value update: no
                            // EVENTS/ADDR_INDEX/BY_AUTHOR/BY_KIND key ever
                            // changes.
                            let mut event = Event::from_json(&record.event_json)
                                .expect("redb: decode event json");
                            event.sig = sig;
                            record.event_json = event.as_json();

                            let encoded =
                                serde_json::to_string(&record).expect("redb: encode stored event");
                            events
                                .insert(id_hex.as_str(), encoded.as_str())
                                .expect("redb: update promoted event");

                            // R6, same transaction: durably drop the
                            // displaced stash and flip the journal's
                            // sig_state so recovery after a promote never
                            // sees a stale stash or a pre-promotion state.
                            outbox_displaced
                                .remove(key.as_str())
                                .expect("redb: remove outbox_displaced");
                            if let Some(intent_json) = outbox_intents
                                .get(key.as_str())
                                .expect("redb: get outbox_intents")
                                .map(|guard| guard.value().to_string())
                            {
                                let mut intent_record: OutboxIntentRecord =
                                    serde_json::from_str(&intent_json)
                                        .expect("redb: decode outbox intent");
                                intent_record.sig_state = IntentSigState::Signed;
                                intent_record.frozen_json = event.as_json();
                                let encoded_intent = serde_json::to_string(&intent_record)
                                    .expect("redb: encode outbox intent");
                                outbox_intents
                                    .insert(key.as_str(), encoded_intent.as_str())
                                    .expect("redb: update outbox_intents");
                            }

                            let row = StoredEvent {
                                event,
                                provenance: Provenance {
                                    seen: record.provenance,
                                    local: record.local,
                                },
                            };
                            PromoteOutcome::Promoted { row: Box::new(row) }
                        }
                    }
                }
            }
        };
        write_txn.commit().expect("redb: commit promote_signed");
        outcome
    }

    fn compensate_write(&mut self, id: EventId) -> CompensateOutcome {
        let write_txn = self.db.begin_write().expect("redb: begin_write");
        let outcome = {
            let mut events = write_txn.open_table(EVENTS).expect("redb: open events");
            let mut addr_index = write_txn
                .open_table(ADDR_INDEX)
                .expect("redb: open addr_index");
            let tombstones = write_txn
                .open_table(TOMBSTONES)
                .expect("redb: open tombstones");
            let addr_tombstones = write_txn
                .open_table(ADDR_TOMBSTONES)
                .expect("redb: open addr_tombstones");
            let mut expiration_index = write_txn
                .open_table(EXPIRATION_INDEX)
                .expect("redb: open expiration_index");
            let mut by_author = write_txn
                .open_table(BY_AUTHOR)
                .expect("redb: open by_author");
            let mut by_kind = write_txn.open_table(BY_KIND).expect("redb: open by_kind");
            let mut outbox_intents = write_txn
                .open_table(OUTBOX_INTENTS)
                .expect("redb: open outbox_intents");
            let mut outbox_displaced = write_txn
                .open_table(OUTBOX_DISPLACED)
                .expect("redb: open outbox_displaced");

            let id_hex = id.to_hex();
            let intent_id = events
                .get(id_hex.as_str())
                .expect("redb: get event")
                .and_then(|guard| {
                    let record: StoredEventRecord =
                        serde_json::from_str(guard.value()).expect("redb: decode stored event");
                    record
                        .local
                        .filter(|local| local.sig_state == SigState::Pending)
                        .map(|local| local.intent_id)
                });

            match intent_id {
                None => CompensateOutcome::NotFound,
                Some(intent_id) => {
                    // §4.2: `remove(id, Rejected)` writes no tombstone
                    // (only kind:5 processing ever does), then re-insert
                    // the stashed predecessor through the SAME one door —
                    // ordinary supersession, never an un-supersede
                    // operation.
                    remove_row_in_txn(
                        &mut events,
                        &mut addr_index,
                        &mut expiration_index,
                        &mut by_author,
                        &mut by_kind,
                        id,
                        |_| true,
                    );

                    let key = intent_key(intent_id);
                    outbox_intents
                        .remove(key.as_str())
                        .expect("redb: remove outbox_intents");
                    let displaced_json = outbox_displaced
                        .remove(key.as_str())
                        .expect("redb: remove outbox_displaced")
                        .map(|guard| guard.value().to_string());

                    let restored = displaced_json
                        .and_then(|json| {
                            reinsert_stashed_in_txn(
                                &mut events,
                                &mut addr_index,
                                &tombstones,
                                &addr_tombstones,
                                &mut expiration_index,
                                &mut by_author,
                                &mut by_kind,
                                decode_stored_event(&json),
                            )
                        })
                        .map(Box::new);

                    CompensateOutcome::Compensated { restored }
                }
            }
        };
        write_txn.commit().expect("redb: commit compensate_write");
        outcome
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
}

fn decode_interval(json: &str) -> CoverageInterval {
    let record: CoverageRowRecord = serde_json::from_str(json).expect("redb: decode coverage row");
    CoverageInterval::new(
        Timestamp::from(record.from),
        Timestamp::from(record.through),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let key = compute_coverage_key(&filter);
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let row_key = RedbStore::coverage_row_key(key, &relay);

        let hex_part = row_key
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
        store.insert(
            target_event,
            RelayObserved::new(r1.clone(), Timestamp::from(1u64)),
        );

        // A pile of OTHER authors' rows -- large enough that a full-table
        // scan would dwarf the one-row match set below.
        for i in 0..200u64 {
            let noise_author = nostr::Keys::generate();
            let noise = EventBuilder::new(Kind::TextNote, "noise")
                .custom_created_at(Timestamp::from(100 + i))
                .sign_with_keys(&noise_author)
                .expect("sign noise event");
            store.insert(
                noise,
                RelayObserved::new(r1.clone(), Timestamp::from(100 + i)),
            );
        }

        let before = store.examined_rows();
        let results = store.query(&Filter::new().author(target.public_key()));
        let examined = store.examined_rows() - before;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event.id, target_id);
        assert_eq!(
            examined, 1,
            "author-filtered query decoded {examined} row(s) on a 201-row table; \
             expected exactly 1 (the match), not a full-table scan"
        );
    }
}
