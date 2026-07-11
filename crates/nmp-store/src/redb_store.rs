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
use nostr::{Event, EventId, Filter, JsonUtil, RelayUrl, Timestamp};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::address_key::{address_key_for, candidate_wins};
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

/// The `events` table's JSON value: the event's canonical NIP-01 JSON plus
/// its merged provenance.
#[derive(Debug, Serialize, Deserialize)]
struct StoredEventRecord {
    event_json: String,
    provenance: BTreeMap<RelayUrl, Timestamp>,
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
    /// all three tables exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(EVENTS)?;
            write_txn.open_table(ADDR_INDEX)?;
            write_txn.open_table(COVERAGE)?;
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
            } else {
                let record = StoredEventRecord {
                    event_json: event.as_json(),
                    provenance: {
                        let mut m = BTreeMap::new();
                        m.insert(from.relay.clone(), from.at);
                        m
                    },
                };
                let encoded = serde_json::to_string(&record).expect("redb: encode stored event");

                match address_key_for(&event) {
                    None => {
                        events
                            .insert(id_hex.as_str(), encoded.as_str())
                            .expect("redb: insert event");
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
                                    events
                                        .insert(id_hex.as_str(), encoded.as_str())
                                        .expect("redb: insert winning event");
                                    addr_index
                                        .insert(addr_key_str.as_str(), id_hex.as_str())
                                        .expect("redb: update addr_index");
                                    InsertOutcome::Superseded {
                                        replaced: Box::new(replaced),
                                    }
                                } else {
                                    InsertOutcome::Stale
                                }
                            }
                        }
                    }
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
            let id_hex = id.to_hex();

            let existing_json = events
                .get(id_hex.as_str())
                .expect("redb: get event")
                .map(|guard| guard.value().to_string());

            match existing_json {
                None => None,
                Some(json) => {
                    let record: StoredEventRecord =
                        serde_json::from_str(&json).expect("redb: decode stored event");
                    let event =
                        Event::from_json(&record.event_json).expect("redb: decode event json");

                    events.remove(id_hex.as_str()).expect("redb: remove event");

                    // Clear the address index too, but only if it still
                    // points at the row we just removed.
                    if let Some(addr_key) = address_key_for(&event) {
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

                    Some(StoredEvent {
                        event,
                        provenance: Provenance {
                            seen: record.provenance,
                        },
                    })
                }
            }
        };
        write_txn.commit().expect("redb: commit remove");
        removed
    }

    fn expire_due(&mut self, now: Timestamp) -> Vec<StoredEvent> {
        // Minimal seam (no persistent expiration index yet): read
        // expiration straight off whatever `query` would return today, then
        // remove each due row through the same `remove` door.
        let due_ids: Vec<EventId> = self
            .query(&Filter::new())
            .into_iter()
            .filter(|se| se.event.is_expired_at(&now))
            .map(|se| se.event.id)
            .collect();

        due_ids
            .into_iter()
            .filter_map(|id| self.remove(id, RetractReason::Expired))
            .collect()
    }

    fn next_expiration(&self) -> Option<Timestamp> {
        self.query(&Filter::new())
            .into_iter()
            .filter_map(|se| se.event.tags.expiration().copied())
            .min()
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
