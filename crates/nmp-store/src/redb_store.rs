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

use std::collections::BTreeMap;
use std::path::Path;

use nmp_grammar::ConcreteFilter;
use nostr::filter::MatchEventOptions;
use nostr::{Event, EventId, Filter, JsonUtil, Kind, PublicKey, RelayUrl, Timestamp};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::address_key::{address_key_for, address_key_for_coordinate, candidate_wins};
use crate::coverage::{
    coverage_key as compute_coverage_key, merge_interval, shape_matches, shrink_after_eviction,
    window_erase, ShapeRecord,
};
use crate::{
    ClaimSet, CoverageInterval, CoverageKey, EventStore, GcReport, InsertOutcome, Provenance,
    RefuseReason, RelayObserved, RetractReason, StoredEvent,
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

/// The `events` table's JSON value: the event's canonical NIP-01 JSON plus
/// its merged provenance.
#[derive(Debug, Serialize, Deserialize)]
struct StoredEventRecord {
    event_json: String,
    provenance: BTreeMap<RelayUrl, Timestamp>,
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
/// still points at `id`) and the expiration index (if the row carried a
/// NIP-40 `expiration`) in the same pass. Shared by the trait's own
/// `remove` (`predicate` always `true`) and kind:5 processing (`predicate`
/// is the NIP-09 author-only check).
fn remove_row_in_txn(
    events: &mut redb::Table<'_, &str, &str>,
    addr_index: &mut redb::Table<'_, &str, &str>,
    expiration_index: &mut redb::Table<'_, &str, &str>,
    id: EventId,
    predicate: impl FnOnce(&StoredEvent) -> bool,
) -> Option<StoredEvent> {
    let id_hex = id.to_hex();
    let json = events
        .get(id_hex.as_str())
        .expect("redb: get event")?
        .value()
        .to_string();
    let record: StoredEventRecord = serde_json::from_str(&json).expect("redb: decode stored event");
    let event = Event::from_json(&record.event_json).expect("redb: decode event json");
    let se = StoredEvent {
        event,
        provenance: Provenance {
            seen: record.provenance,
        },
    };
    if !predicate(&se) {
        return None;
    }

    events.remove(id_hex.as_str()).expect("redb: remove event");

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
    deleting: &Event,
) -> Vec<StoredEvent> {
    let mut deleted = Vec::new();
    let deleting_id_hex = deleting.id.to_hex();
    let deleting_author_hex = deleting.pubkey.to_hex();

    let target_ids: Vec<EventId> = deleting.tags.event_ids().copied().collect();
    for target_id in target_ids {
        if let Some(removed) =
            remove_row_in_txn(events, addr_index, expiration_index, target_id, |se| {
                se.event.pubkey == deleting.pubkey
            })
        {
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
            if let Some(removed) =
                remove_row_in_txn(events, addr_index, expiration_index, current_id, |se| {
                    se.event.created_at <= deleting.created_at
                })
            {
                deleted.push(removed);
            }
        }
    }

    deleted
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
}

impl RedbStore {
    /// Open (creating if absent) a `redb` database file at `path`, ensuring
    /// all six tables exist.
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
        }
        write_txn.commit()?;
        Ok(Self { db })
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
                };
                let grew = provenance.merge_observation(&from);
                record.provenance = provenance.seen;
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
                };
                let encoded = serde_json::to_string(&record).expect("redb: encode stored event");

                let outcome = match address_key_for(&event) {
                    None => {
                        events
                            .insert(id_hex.as_str(), encoded.as_str())
                            .expect("redb: insert event");
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
                                        },
                                    };
                                    events
                                        .remove(current_id_hex.as_str())
                                        .expect("redb: remove superseded event");
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

        let mut out = Vec::new();
        for entry in events.iter().expect("redb: iter events") {
            let (_key, value) = entry.expect("redb: read event entry");
            let record: StoredEventRecord =
                serde_json::from_str(value.value()).expect("redb: decode stored event");
            let event = Event::from_json(&record.event_json).expect("redb: decode event json");
            if filter.match_event(&event, MatchEventOptions::new()) {
                out.push(StoredEvent {
                    event,
                    provenance: Provenance {
                        seen: record.provenance,
                    },
                });
            }
        }
        out
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
            remove_row_in_txn(
                &mut events,
                &mut addr_index,
                &mut expiration_index,
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

            // Pass 1: find victims (regular events matched by no claim).
            // Collected up front into owned values so the removal pass below
            // never holds a borrow across a mutation.
            let mut victims: Vec<(String, Event)> = Vec::new();
            for entry in events.iter().expect("redb: iter events") {
                let (key, value) = entry.expect("redb: read event entry");
                let record: StoredEventRecord =
                    serde_json::from_str(value.value()).expect("redb: decode stored event");
                let event = Event::from_json(&record.event_json).expect("redb: decode event json");
                if address_key_for(&event).is_none() && !claims.is_claimed(&event) {
                    victims.push((key.value().to_string(), event));
                }
            }

            for (id_hex, _) in &victims {
                events
                    .remove(id_hex.as_str())
                    .expect("redb: remove gc victim");
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
}
