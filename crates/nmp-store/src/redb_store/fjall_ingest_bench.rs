//! Benchmark-only Fjall adapter for the governed relay-ingest policy.
//!
//! This is deliberately not an `EventStore` implementation. It proves that
//! the real policy in `ingest`/`mutation` can execute against a second atomic
//! transaction engine before NMP accepts the much larger query, coverage,
//! outbox-recovery, packaging, and migration surface tracked by #629.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::Instant;

use fjall::{
    KeyspaceCreateOptions, PersistMode, Readable, SingleWriterTxDatabase, SingleWriterTxKeyspace,
    SingleWriterWriteTx, UserValue,
};
use nostr::{Event, RelayUrl, Timestamp};
use serde::{Deserialize, Serialize};

use super::ingest::insert_with_tables;
use super::ingest_txn::{GovernedIngestTxn, GovernedStringMap};
use super::query::{
    author_cardinality_key, by_author_key, by_kind_key, created_at_key, global_cardinality_key,
    kind_cardinality_key, tag_cardinality_key, tag_index_key,
};
use super::schema::EventKey;
use super::{
    binary_event, EventId, InsertOutcome, LocalOrigin, PersistenceError, Provenance, RelayObserved,
    StoreBenchProcessCounters, StoredEvent, StoredEventView,
};

const CACHE_BYTES: u64 = 16 * 1_024 * 1_024;
const WRITE_BUFFER_BYTES: u64 = 32 * 1_024 * 1_024;
const MEMTABLE_BYTES: u64 = 4 * 1_024 * 1_024;
const WORKERS: usize = 2;
const NEXT_EVENT_KEY: &[u8] = b"next_event_key";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FjallGovernedIngestMetrics {
    pub worker_threads: usize,
    pub cache_bytes: u64,
    pub write_buffer_bytes: u64,
    pub per_keyspace_memtable_bytes: u64,
    pub events: u64,
    pub transaction_batch_size: usize,
    pub transactions: u64,
    pub wall_ns: u64,
    pub policy_apply_ns: u64,
    pub point_read_ns: u64,
    pub encode_ns: u64,
    pub index_mutation_ns: u64,
    pub commit_ns: u64,
    pub commit_p50_ns: u64,
    pub commit_p95_ns: u64,
    pub commit_p99_ns: u64,
    pub cpu_ns: u64,
    pub allocation_ops: u64,
    pub allocated_bytes: u64,
    pub rss_before_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    pub process_write_bytes: Option<u64>,
    pub database_logical_bytes: u64,
    pub database_stored_bytes: u64,
    pub reopened_rows: u64,
    pub exact_reopen: bool,
}

struct FjallKeyspaces {
    events: SingleWriterTxKeyspace,
    event_ids: SingleWriterTxKeyspace,
    provenance: SingleWriterTxKeyspace,
    meta: SingleWriterTxKeyspace,
    addr_index: SingleWriterTxKeyspace,
    tombstones: SingleWriterTxKeyspace,
    addr_tombstones: SingleWriterTxKeyspace,
    expiration: SingleWriterTxKeyspace,
    by_created_at: SingleWriterTxKeyspace,
    by_author: SingleWriterTxKeyspace,
    by_kind: SingleWriterTxKeyspace,
    by_tag: SingleWriterTxKeyspace,
    cardinality: SingleWriterTxKeyspace,
    outbox_intents: SingleWriterTxKeyspace,
    outbox_receipts: SingleWriterTxKeyspace,
    outbox_displaced: SingleWriterTxKeyspace,
    outbox_kind5_claims: SingleWriterTxKeyspace,
    outbox_suppress_by_id: SingleWriterTxKeyspace,
    outbox_suppress_by_addr: SingleWriterTxKeyspace,
}

impl FjallKeyspaces {
    fn open(database: &SingleWriterTxDatabase) -> Result<Self, PersistenceError> {
        let open = |name: &str| {
            database
                .keyspace(name, || {
                    KeyspaceCreateOptions::default().max_memtable_size(MEMTABLE_BYTES)
                })
                .map_err(persist_err)
        };
        Ok(Self {
            events: open("governed_events")?,
            event_ids: open("governed_event_ids")?,
            provenance: open("governed_provenance")?,
            meta: open("governed_meta")?,
            addr_index: open("governed_addr_index")?,
            tombstones: open("governed_tombstones")?,
            addr_tombstones: open("governed_addr_tombstones")?,
            expiration: open("governed_expiration")?,
            by_created_at: open("governed_by_created_at")?,
            by_author: open("governed_by_author")?,
            by_kind: open("governed_by_kind")?,
            by_tag: open("governed_by_tag")?,
            cardinality: open("governed_cardinality")?,
            outbox_intents: open("governed_outbox_intents")?,
            outbox_receipts: open("governed_outbox_receipts")?,
            outbox_displaced: open("governed_outbox_displaced")?,
            outbox_kind5_claims: open("governed_outbox_kind5_claims")?,
            outbox_suppress_by_id: open("governed_outbox_suppress_by_id")?,
            outbox_suppress_by_addr: open("governed_outbox_suppress_by_addr")?,
        })
    }

    fn string_map(&self, map: GovernedStringMap) -> &SingleWriterTxKeyspace {
        match map {
            GovernedStringMap::Tombstones => &self.tombstones,
            GovernedStringMap::AddrTombstones => &self.addr_tombstones,
            GovernedStringMap::OutboxIntents => &self.outbox_intents,
            GovernedStringMap::OutboxReceipts => &self.outbox_receipts,
            GovernedStringMap::OutboxKind5Claims => &self.outbox_kind5_claims,
            GovernedStringMap::OutboxSuppressById => &self.outbox_suppress_by_id,
            GovernedStringMap::OutboxSuppressByAddr => &self.outbox_suppress_by_addr,
        }
    }
}

struct FjallIngestTxn<'borrow, 'txn> {
    transaction: &'borrow mut SingleWriterWriteTx<'txn>,
    keyspaces: &'borrow FjallKeyspaces,
    next_event_key: EventKey,
    event_allocator_dirty: bool,
    cardinality_deltas: HashMap<Vec<u8>, i64>,
    point_read_ns: Cell<u64>,
    encode_ns: u64,
    index_mutation_ns: u64,
}

impl<'borrow, 'txn> FjallIngestTxn<'borrow, 'txn> {
    fn open(
        transaction: &'borrow mut SingleWriterWriteTx<'txn>,
        keyspaces: &'borrow FjallKeyspaces,
    ) -> Result<Self, PersistenceError> {
        let next_event_key = transaction
            .get(&keyspaces.meta, NEXT_EVENT_KEY)
            .map_err(persist_err)?
            .map(|value| decode_u64(&value))
            .transpose()?
            .unwrap_or(1);
        Ok(Self {
            transaction,
            keyspaces,
            next_event_key,
            event_allocator_dirty: false,
            cardinality_deltas: HashMap::new(),
            point_read_ns: Cell::new(0),
            encode_ns: 0,
            index_mutation_ns: 0,
        })
    }

    fn get(
        &self,
        keyspace: &SingleWriterTxKeyspace,
        key: impl AsRef<[u8]>,
    ) -> Result<Option<UserValue>, PersistenceError> {
        let started = Instant::now();
        let result = self.transaction.get(keyspace, key).map_err(persist_err);
        self.point_read_ns.set(
            self.point_read_ns
                .get()
                .saturating_add(duration_ns(started)),
        );
        result
    }

    fn flush_pending(&mut self) -> Result<(), PersistenceError> {
        if self.event_allocator_dirty {
            self.transaction.insert(
                &self.keyspaces.meta,
                NEXT_EVENT_KEY,
                self.next_event_key.to_be_bytes(),
            );
            self.event_allocator_dirty = false;
        }
        for (key, delta) in std::mem::take(&mut self.cardinality_deltas) {
            if delta == 0 {
                continue;
            }
            let current = self
                .get(&self.keyspaces.cardinality, &key)?
                .map(|value| decode_u64(&value))
                .transpose()?
                .unwrap_or(0);
            let next = if delta > 0 {
                current.checked_add(delta as u64)
            } else {
                current.checked_sub(delta.unsigned_abs())
            }
            .ok_or_else(|| PersistenceError("Fjall cardinality overflow/underflow".to_owned()))?;
            if next == 0 {
                self.transaction.remove(&self.keyspaces.cardinality, key);
            } else {
                self.transaction
                    .insert(&self.keyspaces.cardinality, key, next.to_be_bytes());
            }
        }
        Ok(())
    }

    fn load_provenance(&self, key: EventKey) -> Result<Provenance, PersistenceError> {
        let encoded = self
            .get(&self.keyspaces.provenance, event_key(key))?
            .ok_or_else(|| PersistenceError(format!("missing Fjall provenance for {key}")))?;
        binary_event::decode_provenance(&encoded)
            .map_err(|error| PersistenceError(format!("decode Fjall provenance: {error:?}")))
    }

    fn store_provenance(
        &mut self,
        key: EventKey,
        provenance: &Provenance,
    ) -> Result<(), PersistenceError> {
        let encoded = binary_event::encode_provenance(provenance)
            .map_err(|error| PersistenceError(format!("encode Fjall provenance: {error:?}")))?;
        self.transaction
            .insert(&self.keyspaces.provenance, event_key(key), encoded);
        Ok(())
    }

    fn adjust_cardinality(&mut self, key: Vec<u8>, delta: i64) -> Result<(), PersistenceError> {
        let current = self.cardinality_deltas.entry(key).or_default();
        *current = current
            .checked_add(delta)
            .ok_or_else(|| PersistenceError("Fjall cardinality delta overflow".to_owned()))?;
        Ok(())
    }

    fn mutate_index_rows(
        &mut self,
        event: &Event,
        key: EventKey,
        insert: bool,
    ) -> Result<(), PersistenceError> {
        let started = Instant::now();
        let value = event_key(key);
        let fixed = [
            (
                &self.keyspaces.by_created_at,
                created_at_key(event).to_vec(),
            ),
            (&self.keyspaces.by_author, by_author_key(event).to_vec()),
            (&self.keyspaces.by_kind, by_kind_key(event).to_vec()),
        ];
        for (keyspace, index_key) in fixed {
            if insert {
                self.transaction.insert(keyspace, index_key, value);
            } else {
                self.transaction.remove(keyspace, index_key);
            }
        }
        let mut tag_cardinalities = BTreeSet::new();
        for tag in event.tags.iter() {
            let (Some(letter), Some(content)) = (tag.single_letter_tag(), tag.content()) else {
                continue;
            };
            let index_key = tag_index_key(letter, content, event.created_at, &event.id);
            if insert {
                self.transaction
                    .insert(&self.keyspaces.by_tag, index_key, value);
            } else {
                self.transaction.remove(&self.keyspaces.by_tag, index_key);
            }
            tag_cardinalities.insert(tag_cardinality_key(letter, content));
        }
        let delta = if insert { 1 } else { -1 };
        self.adjust_cardinality(global_cardinality_key(), delta)?;
        self.adjust_cardinality(author_cardinality_key(&event.pubkey), delta)?;
        self.adjust_cardinality(kind_cardinality_key(event.kind), delta)?;
        for cardinality in tag_cardinalities {
            self.adjust_cardinality(cardinality, delta)?;
        }
        self.index_mutation_ns = self.index_mutation_ns.saturating_add(duration_ns(started));
        Ok(())
    }
}

impl GovernedIngestTxn for FjallIngestTxn<'_, '_> {
    fn key_for_id(&self, id: &EventId) -> Result<Option<EventKey>, PersistenceError> {
        self.get(&self.keyspaces.event_ids, id.as_bytes())?
            .map(|value| decode_u64(&value))
            .transpose()
    }

    fn load_by_key(&self, key: EventKey) -> Result<Option<StoredEvent>, PersistenceError> {
        let Some(encoded) = self.get(&self.keyspaces.events, event_key(key))? else {
            return Ok(None);
        };
        let event = StoredEventView::from_trusted(&encoded)
            .map_err(|error| PersistenceError(format!("decode Fjall event: {error:?}")))?
            .materialize_event()
            .map_err(|error| PersistenceError(format!("materialize Fjall event: {error:?}")))?;
        Ok(Some(StoredEvent {
            event,
            provenance: self.load_provenance(key)?,
        }))
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

    fn load_local(&self, key: EventKey) -> Result<Option<LocalOrigin>, PersistenceError> {
        Ok(self.load_provenance(key)?.local)
    }

    fn merge_observation(
        &mut self,
        key: EventKey,
        relay: &RelayUrl,
        at: Timestamp,
    ) -> Result<bool, PersistenceError> {
        let mut provenance = self.load_provenance(key)?;
        let from = RelayObserved::new(relay.clone(), at);
        let grew = provenance.merge_observation(&from);
        if grew {
            self.store_provenance(key, &provenance)?;
        }
        Ok(grew)
    }

    fn replace_event(&mut self, key: EventKey, event: &Event) -> Result<(), PersistenceError> {
        let started = Instant::now();
        let encoded = binary_event::encode_event(event)
            .map_err(|error| PersistenceError(format!("encode Fjall event: {error:?}")))?;
        self.encode_ns = self.encode_ns.saturating_add(duration_ns(started));
        self.transaction
            .insert(&self.keyspaces.events, event_key(key), encoded);
        Ok(())
    }

    fn replace_local(
        &mut self,
        key: EventKey,
        local: Option<LocalOrigin>,
    ) -> Result<(), PersistenceError> {
        let mut provenance = self.load_provenance(key)?;
        provenance.local = local;
        self.store_provenance(key, &provenance)
    }

    fn insert_new(
        &mut self,
        event: &Event,
        provenance: &Provenance,
    ) -> Result<EventKey, PersistenceError> {
        let key = self.next_event_key;
        self.next_event_key = key
            .checked_add(1)
            .ok_or_else(|| PersistenceError("Fjall event key space exhausted".to_owned()))?;
        self.event_allocator_dirty = true;
        let encode_started = Instant::now();
        let encoded = binary_event::encode_event(event)
            .map_err(|error| PersistenceError(format!("encode Fjall event: {error:?}")))?;
        self.encode_ns = self.encode_ns.saturating_add(duration_ns(encode_started));
        self.transaction
            .insert(&self.keyspaces.events, event_key(key), encoded);
        self.transaction.insert(
            &self.keyspaces.event_ids,
            event.id.as_bytes(),
            event_key(key),
        );
        self.store_provenance(key, provenance)?;
        Ok(key)
    }

    fn remove_canonical(&mut self, key: EventKey, id: &EventId) -> Result<(), PersistenceError> {
        self.transaction
            .remove(&self.keyspaces.events, event_key(key));
        self.transaction
            .remove(&self.keyspaces.event_ids, id.as_bytes());
        self.transaction
            .remove(&self.keyspaces.provenance, event_key(key));
        Ok(())
    }

    fn insert_indexes(&mut self, event: &Event, key: EventKey) -> Result<(), PersistenceError> {
        self.mutate_index_rows(event, key, true)
    }

    fn remove_indexes(&mut self, event: &Event, _key: EventKey) -> Result<(), PersistenceError> {
        self.mutate_index_rows(event, 0, false)
    }

    fn address_get(&self, key: &str) -> Result<Option<EventKey>, PersistenceError> {
        self.get(&self.keyspaces.addr_index, key)?
            .map(|value| decode_u64(&value))
            .transpose()
    }

    fn address_put(&mut self, key: &str, value: EventKey) -> Result<(), PersistenceError> {
        self.transaction
            .insert(&self.keyspaces.addr_index, key, event_key(value));
        Ok(())
    }

    fn address_remove(&mut self, key: &str) -> Result<(), PersistenceError> {
        self.transaction.remove(&self.keyspaces.addr_index, key);
        Ok(())
    }

    fn expiration_put(&mut self, key: &[u8; 40], value: EventKey) -> Result<(), PersistenceError> {
        self.transaction
            .insert(&self.keyspaces.expiration, key, event_key(value));
        Ok(())
    }

    fn expiration_remove(&mut self, key: &[u8; 40]) -> Result<(), PersistenceError> {
        self.transaction.remove(&self.keyspaces.expiration, key);
        Ok(())
    }

    fn string_get(
        &self,
        map: GovernedStringMap,
        key: &str,
    ) -> Result<Option<String>, PersistenceError> {
        self.get(self.keyspaces.string_map(map), key)?
            .map(|value| {
                String::from_utf8(value.to_vec())
                    .map_err(|error| PersistenceError(format!("decode Fjall string: {error}")))
            })
            .transpose()
    }

    fn string_put(
        &mut self,
        map: GovernedStringMap,
        key: &str,
        value: &str,
    ) -> Result<(), PersistenceError> {
        self.transaction
            .insert(self.keyspaces.string_map(map), key, value);
        Ok(())
    }

    fn string_remove(
        &mut self,
        map: GovernedStringMap,
        key: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let keyspace = self.keyspaces.string_map(map);
        let previous = self
            .get(keyspace, key)?
            .map(|value| {
                String::from_utf8(value.to_vec())
                    .map_err(|error| PersistenceError(format!("decode Fjall string: {error}")))
            })
            .transpose()?;
        self.transaction.remove(keyspace, key);
        Ok(previous)
    }

    fn displaced_remove(&mut self, key: &str) -> Result<Option<Vec<u8>>, PersistenceError> {
        let previous = self
            .get(&self.keyspaces.outbox_displaced, key)?
            .map(|value| value.to_vec());
        self.transaction
            .remove(&self.keyspaces.outbox_displaced, key);
        Ok(previous)
    }
}

#[allow(deprecated)]
pub fn run_fjall_governed_ingest_bench(
    path: &Path,
    events: Vec<Event>,
    batch_size: usize,
    sample_process: fn() -> StoreBenchProcessCounters,
) -> Result<FjallGovernedIngestMetrics, String> {
    if events.is_empty() {
        return Err("benchmark corpus must not be empty".to_owned());
    }
    if batch_size == 0 {
        return Err("transaction batch size must be nonzero".to_owned());
    }
    let database = SingleWriterTxDatabase::builder(path)
        .worker_threads(WORKERS)
        .cache_size(CACHE_BYTES)
        .max_write_buffer_size(Some(WRITE_BUFFER_BYTES))
        .open()
        .map_err(|error| error.to_string())?;
    let keyspaces = FjallKeyspaces::open(&database).map_err(|error| error.to_string())?;
    database
        .persist(PersistMode::SyncAll)
        .map_err(|error| error.to_string())?;

    let relay =
        RelayUrl::parse("wss://governed-fjall.invalid").map_err(|error| error.to_string())?;
    let observed_at = Timestamp::from(
        events
            .iter()
            .map(|event| event.created_at.as_secs())
            .max()
            .unwrap_or(0)
            .saturating_add(1),
    );
    let event_count = events.len() as u64;
    let process_before = sample_process();
    let started = Instant::now();
    let mut commit_ns = 0u64;
    let mut policy_apply_ns = 0u64;
    let mut point_read_ns = 0u64;
    let mut encode_ns = 0u64;
    let mut index_mutation_ns = 0u64;
    let mut commit_latencies = Vec::new();
    let mut transactions = 0u64;
    for batch in events.chunks(batch_size) {
        let mut transaction = database.write_tx().durability(Some(PersistMode::SyncAll));
        {
            let apply_started = Instant::now();
            let mut adapter =
                FjallIngestTxn::open(&mut transaction, &keyspaces).map_err(|e| e.to_string())?;
            for event in batch {
                let outcome = insert_with_tables(
                    &mut adapter,
                    event.clone(),
                    RelayObserved::new(relay.clone(), observed_at),
                )
                .map_err(|error| error.to_string())?;
                if !matches!(outcome, InsertOutcome::Inserted) {
                    return Err(format!("governed Fjall benchmark produced {outcome:?}"));
                }
            }
            adapter.flush_pending().map_err(|error| error.to_string())?;
            policy_apply_ns = policy_apply_ns.saturating_add(duration_ns(apply_started));
            point_read_ns = point_read_ns.saturating_add(adapter.point_read_ns.get());
            encode_ns = encode_ns.saturating_add(adapter.encode_ns);
            index_mutation_ns = index_mutation_ns.saturating_add(adapter.index_mutation_ns);
        }
        let commit_started = Instant::now();
        transaction.commit().map_err(|error| error.to_string())?;
        let latency = duration_ns(commit_started);
        commit_ns = commit_ns.saturating_add(latency);
        commit_latencies.push(latency);
        transactions += 1;
    }
    let wall_ns = duration_ns(started);
    let process = process_delta(sample_process(), process_before);
    let stored_bytes = database.disk_space().map_err(|error| error.to_string())?;
    drop(keyspaces);
    drop(database);
    let logical_bytes = directory_bytes(path).map_err(|error| error.to_string())?;

    let reopened = SingleWriterTxDatabase::builder(path)
        .worker_threads(WORKERS)
        .cache_size(CACHE_BYTES)
        .max_write_buffer_size(Some(WRITE_BUFFER_BYTES))
        .open()
        .map_err(|error| error.to_string())?;
    let reopened_keyspaces = FjallKeyspaces::open(&reopened).map_err(|error| error.to_string())?;
    let read = reopened.read_tx();
    let reopened_rows = read
        .len(&reopened_keyspaces.events)
        .map_err(|error| error.to_string())? as u64;

    Ok(FjallGovernedIngestMetrics {
        worker_threads: WORKERS,
        cache_bytes: CACHE_BYTES,
        write_buffer_bytes: WRITE_BUFFER_BYTES,
        per_keyspace_memtable_bytes: MEMTABLE_BYTES,
        events: event_count,
        transaction_batch_size: batch_size,
        transactions,
        wall_ns,
        policy_apply_ns,
        point_read_ns,
        encode_ns,
        index_mutation_ns,
        commit_ns,
        commit_p50_ns: nearest_rank(&commit_latencies, 50),
        commit_p95_ns: nearest_rank(&commit_latencies, 95),
        commit_p99_ns: nearest_rank(&commit_latencies, 99),
        cpu_ns: process.cpu_ns,
        allocation_ops: process.allocation_ops,
        allocated_bytes: process.allocated_bytes,
        rss_before_bytes: process.current_rss_bytes,
        peak_rss_bytes: process.peak_rss_bytes,
        process_write_bytes: process.process_write_bytes,
        database_logical_bytes: logical_bytes,
        database_stored_bytes: stored_bytes,
        reopened_rows,
        exact_reopen: reopened_rows == event_count,
    })
}

fn event_key(key: EventKey) -> [u8; 8] {
    key.to_be_bytes()
}

fn decode_u64(value: &[u8]) -> Result<u64, PersistenceError> {
    let bytes: [u8; 8] = value
        .try_into()
        .map_err(|_| PersistenceError("Fjall u64 value has wrong width".to_owned()))?;
    Ok(u64::from_be_bytes(bytes))
}

fn persist_err(error: impl std::fmt::Display) -> PersistenceError {
    PersistenceError(error.to_string())
}

fn duration_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

fn nearest_rank(values: &[u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    let rank = percentile.saturating_mul(values.len()).saturating_add(99) / 100;
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

fn process_delta(
    after: StoreBenchProcessCounters,
    before: StoreBenchProcessCounters,
) -> StoreBenchProcessCounters {
    StoreBenchProcessCounters {
        cpu_ns: after.cpu_ns.saturating_sub(before.cpu_ns),
        allocation_ops: after.allocation_ops.saturating_sub(before.allocation_ops),
        allocated_bytes: after.allocated_bytes.saturating_sub(before.allocated_bytes),
        current_rss_bytes: before.current_rss_bytes,
        peak_rss_bytes: after.peak_rss_bytes,
        process_write_bytes: after
            .process_write_bytes
            .zip(before.process_write_bytes)
            .map(|(after, before)| after.saturating_sub(before)),
    }
}

fn directory_bytes(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total = total.saturating_add(directory_bytes(&entry.path())?);
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use nostr::{EventBuilder, Keys, Kind, Tag};

    use super::*;

    fn event(keys: &Keys, created_at: u64, kind: u16, tags: Vec<Tag>) -> Event {
        EventBuilder::new(Kind::from(kind), format!("event-{created_at}"))
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    fn with_tx<T>(
        database: &SingleWriterTxDatabase,
        keyspaces: &FjallKeyspaces,
        f: impl FnOnce(&mut FjallIngestTxn<'_, '_>) -> T,
    ) -> T {
        let mut write = database.write_tx().durability(Some(PersistMode::SyncAll));
        let mut adapter = FjallIngestTxn::open(&mut write, keyspaces).unwrap();
        let result = f(&mut adapter);
        adapter.flush_pending().unwrap();
        drop(adapter);
        write.commit().unwrap();
        result
    }

    #[test]
    fn fjall_adapter_runs_governed_insert_duplicate_replace_and_delete_policy() {
        let temp = tempfile::tempdir().unwrap();
        let database = SingleWriterTxDatabase::builder(temp.path()).open().unwrap();
        let keyspaces = FjallKeyspaces::open(&database).unwrap();
        let relay_a = RelayUrl::parse("wss://a.invalid").unwrap();
        let relay_b = RelayUrl::parse("wss://b.invalid").unwrap();
        let keys = Keys::generate();
        let first = event(&keys, 10, 10_000, Vec::new());
        let newer = event(&keys, 11, 10_000, Vec::new());

        let first_outcome = with_tx(&database, &keyspaces, |txn| {
            insert_with_tables(
                txn,
                first.clone(),
                RelayObserved::new(relay_a.clone(), Timestamp::from(20)),
            )
            .unwrap()
        });
        assert!(matches!(first_outcome, InsertOutcome::Inserted));

        let duplicate = with_tx(&database, &keyspaces, |txn| {
            insert_with_tables(
                txn,
                first.clone(),
                RelayObserved::new(relay_b.clone(), Timestamp::from(21)),
            )
            .unwrap()
        });
        assert!(matches!(
            duplicate,
            InsertOutcome::Duplicate {
                provenance_grew: true,
                ..
            }
        ));

        let superseded = with_tx(&database, &keyspaces, |txn| {
            insert_with_tables(
                txn,
                newer.clone(),
                RelayObserved::new(relay_a.clone(), Timestamp::from(22)),
            )
            .unwrap()
        });
        assert!(matches!(superseded, InsertOutcome::Superseded { .. }));

        let deletion = event(
            &keys,
            12,
            Kind::EventDeletion.as_u16(),
            vec![Tag::event(newer.id)],
        );
        let deleted = with_tx(&database, &keyspaces, |txn| {
            insert_with_tables(
                txn,
                deletion,
                RelayObserved::new(relay_a, Timestamp::from(23)),
            )
            .unwrap()
        });
        assert!(matches!(
            deleted,
            InsertOutcome::Kind5Processed { ref deleted } if deleted.len() == 1
        ));

        let read = database.read_tx();
        assert_eq!(read.len(&keyspaces.events).unwrap(), 1);
        assert!(read
            .contains_key(
                &keyspaces.tombstones,
                format!("{}:{}", newer.id, newer.pubkey)
            )
            .unwrap());
    }

    #[test]
    fn governed_benchmark_reopens_exactly() {
        let temp = tempfile::tempdir().unwrap();
        let keys = Keys::generate();
        let events = (0..8)
            .map(|ordinal| event(&keys, 100 + ordinal, 9, Vec::new()))
            .collect();
        let metrics = run_fjall_governed_ingest_bench(
            temp.path(),
            events,
            3,
            StoreBenchProcessCounters::default,
        )
        .unwrap();
        assert_eq!(metrics.transactions, 3);
        assert_eq!(metrics.reopened_rows, 8);
        assert!(metrics.exact_reopen);
    }

    #[test]
    fn adapter_indexes_expiration_and_single_letter_tags() {
        let temp = tempfile::tempdir().unwrap();
        let database = SingleWriterTxDatabase::builder(temp.path()).open().unwrap();
        let keyspaces = FjallKeyspaces::open(&database).unwrap();
        let relay = RelayUrl::parse("wss://a.invalid").unwrap();
        let keys = Keys::generate();
        let event = event(
            &keys,
            30,
            9,
            vec![
                Tag::parse(["h", "room"]).unwrap(),
                Tag::expiration(Timestamp::from(40)),
            ],
        );
        with_tx(&database, &keyspaces, |txn| {
            insert_with_tables(txn, event, RelayObserved::new(relay, Timestamp::from(35))).unwrap()
        });
        let read = database.read_tx();
        assert_eq!(read.len(&keyspaces.expiration).unwrap(), 1);
        assert_eq!(read.len(&keyspaces.by_tag).unwrap(), 1);
        assert_eq!(read.len(&keyspaces.cardinality).unwrap(), 4);
    }

    #[test]
    fn fjall_governed_crash_worker() {
        let Ok(path) = std::env::var("NMP_FJALL_GOVERNED_CRASH_PATH") else {
            return;
        };
        let database = SingleWriterTxDatabase::builder(path).open().unwrap();
        let keyspaces = FjallKeyspaces::open(&database).unwrap();
        let relay = RelayUrl::parse("wss://crash.invalid").unwrap();
        let keys = Keys::generate();
        let committed = event(&keys, 50, 9, Vec::new());
        let uncommitted = event(&keys, 51, 9, Vec::new());
        with_tx(&database, &keyspaces, |txn| {
            insert_with_tables(
                txn,
                committed,
                RelayObserved::new(relay.clone(), Timestamp::from(60)),
            )
            .unwrap()
        });

        let mut write = database.write_tx().durability(Some(PersistMode::SyncAll));
        let mut adapter = FjallIngestTxn::open(&mut write, &keyspaces).unwrap();
        insert_with_tables(
            &mut adapter,
            uncommitted,
            RelayObserved::new(relay, Timestamp::from(61)),
        )
        .unwrap();
        adapter.flush_pending().unwrap();
        drop(adapter);
        unsafe { libc::_exit(73) };
    }

    #[test]
    fn abrupt_exit_keeps_committed_batch_and_discards_uncommitted_batch() {
        let temp = tempfile::tempdir().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "redb_store::fjall_ingest_bench::tests::fjall_governed_crash_worker",
                "--nocapture",
            ])
            .env("NMP_FJALL_GOVERNED_CRASH_PATH", temp.path())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(73));

        let reopened = SingleWriterTxDatabase::builder(temp.path()).open().unwrap();
        let keyspaces = FjallKeyspaces::open(&reopened).unwrap();
        let read = reopened.read_tx();
        assert_eq!(read.len(&keyspaces.events).unwrap(), 1);
        assert_eq!(read.len(&keyspaces.event_ids).unwrap(), 1);
        assert_eq!(read.len(&keyspaces.provenance).unwrap(), 1);
    }
}
