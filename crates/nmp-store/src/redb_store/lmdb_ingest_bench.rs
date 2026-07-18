//! Benchmark-only synchronous LMDB adapter for issue #658.
//!
//! This is deliberately narrower than an [`crate::EventStore`] backend. It
//! reuses the production governed-ingest policy and every portable event and
//! packed-postings codec, but exposes no app-facing storage surface. LMDB is
//! opened with its synchronous defaults: no `MDB_NOSYNC`, `MDB_NOMETASYNC`,
//! `MDB_WRITEMAP`, or `MDB_MAPASYNC` flags.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use heed::types::Bytes;
use heed::{Database as LmdbDatabase, Env, EnvOpenOptions, RoTxn, RwTxn};
use nostr::{Event, EventId, Filter, RelayUrl, Timestamp};
use serde::{Deserialize, Serialize};

use crate::MemoryStore;

use super::canonical::{observation_key, observation_relay_key};
use super::ingest::insert_with_tables;
use super::ingest_txn::{GovernedIngestTxn, GovernedStringMap};
use super::postings::{
    compact_segment, encode_dictionary, encode_run, merge_dead_blocks, shard_for,
    validate_run_metas, CompactionSegmentSource, DeadKeys, DictionaryView, Family, Membership,
    Prefix, RunEvent, RunMeta, SegmentView, MAX_DEATH_BLOCKS, SHARD_MASK,
};
use super::query::{
    author_cardinality_key, event_is_cardinality_sample, global_cardinality_key,
    kind_cardinality_key, tag_cardinality_key, tag_index_prefix,
};
use super::schema::{EventKey, RelayKey};
use super::{
    binary_event, EventStore, LocalOrigin, PersistenceError, Provenance, RelayObserved,
    StoreBenchProcessCounters, StoredEvent, StoredEventView,
};

type BytesDb = LmdbDatabase<Bytes, Bytes>;

const LMDB_MAP_SIZE: usize = 16 * 1024 * 1024 * 1024;
const BASE_RUN_FAN_IN: usize = 8;
const LARGE_RUN_FAN_IN: usize = 6;
const CARDINALITY_SAMPLE_KEY: [u8; 32] = [0x42; 32];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LmdbPackedWork {
    pub runs_published: u64,
    pub compactions: u64,
    pub compaction_input_runs: u64,
    pub compaction_events_rewritten: u64,
    pub dictionary_input_bytes: u64,
    pub dictionary_output_bytes: u64,
    pub segment_input_bytes: u64,
    pub segment_output_bytes: u64,
    pub logical_puts: u64,
    pub logical_deletes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LmdbGovernedIngestMetrics {
    pub events: u64,
    pub transaction_batch_size: usize,
    pub transactions: u64,
    pub wall_ns: u64,
    pub apply_ns: u64,
    pub canonical_flush_ns: u64,
    pub postings_flush_ns: u64,
    pub commit_ns: u64,
    pub commit_p50_ns: u64,
    pub commit_p95_ns: u64,
    pub commit_p99_ns: u64,
    pub reopen_ns: u64,
    pub cpu_ns: u64,
    pub allocation_ops: u64,
    pub allocated_bytes: u64,
    pub rss_before_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    pub process_write_bytes: Option<u64>,
    pub database_file_bytes: u64,
    pub database_allocated_bytes: u64,
    pub expected_rows: u64,
    pub reopened_rows: u64,
    pub canonical_ids_exact: bool,
    pub packed_exact: bool,
    pub exact_reopen: bool,
    pub packed_work: LmdbPackedWork,
}

#[derive(Clone)]
struct LmdbDatabases {
    events: BytesDb,
    event_ids: BytesDb,
    event_local: BytesDb,
    event_store_meta: BytesDb,
    observations: BytesDb,
    relays: BytesDb,
    relay_keys: BytesDb,
    relay_refs: BytesDb,
    relay_meta: BytesDb,
    cardinality: BytesDb,
    addr_index: BytesDb,
    tombstones: BytesDb,
    addr_tombstones: BytesDb,
    expiration: BytesDb,
    outbox_intents: BytesDb,
    outbox_receipts: BytesDb,
    outbox_displaced: BytesDb,
    outbox_kind5_claims: BytesDb,
    outbox_suppress_by_id: BytesDb,
    outbox_suppress_by_addr: BytesDb,
    segments: BytesDb,
    dictionaries: BytesDb,
    run_meta: BytesDb,
    run_by_min: BytesDb,
    dead_keys: BytesDb,
    postings_meta: BytesDb,
}

impl LmdbDatabases {
    fn create(env: &Env, txn: &mut RwTxn<'_>) -> Result<Self, PersistenceError> {
        let mut create = |name| {
            env.create_database::<Bytes, Bytes>(txn, Some(name))
                .map_err(lmdb_err)
        };
        Ok(Self {
            events: create("events_v6")?,
            event_ids: create("event_ids_v6")?,
            event_local: create("event_local_v6")?,
            event_store_meta: create("event_store_meta_v6")?,
            observations: create("event_observations_v6")?,
            relays: create("relays_v6")?,
            relay_keys: create("relay_keys_v6")?,
            relay_refs: create("relay_refs_v6")?,
            relay_meta: create("relay_meta_v6")?,
            cardinality: create("index_cardinality_v1")?,
            addr_index: create("addr_index_v6")?,
            tombstones: create("tombstones")?,
            addr_tombstones: create("addr_tombstones")?,
            expiration: create("expiration_index_v6")?,
            outbox_intents: create("outbox_intents")?,
            outbox_receipts: create("outbox_receipts")?,
            outbox_displaced: create("outbox_displaced_v6")?,
            outbox_kind5_claims: create("outbox_kind5_claims")?,
            outbox_suppress_by_id: create("outbox_suppress_by_id")?,
            outbox_suppress_by_addr: create("outbox_suppress_by_addr")?,
            segments: create("postings_segments_v8")?,
            dictionaries: create("postings_dictionaries_v8")?,
            run_meta: create("postings_run_meta_v8")?,
            run_by_min: create("postings_run_by_min_v8")?,
            dead_keys: create("postings_dead_keys_v8")?,
            postings_meta: create("postings_meta_v8")?,
        })
    }
}

pub fn run_lmdb_governed_ingest_bench(
    path: &Path,
    events: Vec<Event>,
    batch_size: usize,
    sample_process: fn() -> StoreBenchProcessCounters,
) -> Result<LmdbGovernedIngestMetrics, String> {
    if events.is_empty() {
        return Err("benchmark corpus must not be empty".to_owned());
    }
    if batch_size == 0 {
        return Err("transaction batch size must be nonzero".to_owned());
    }
    fs::create_dir_all(path).map_err(|error| error.to_string())?;
    let env = unsafe {
        EnvOpenOptions::new()
            .map_size(LMDB_MAP_SIZE)
            .max_dbs(32)
            .open(path)
            .map_err(|error| error.to_string())?
    };
    let mut init = env.write_txn().map_err(|error| error.to_string())?;
    let databases = LmdbDatabases::create(&env, &mut init).map_err(|error| error.0)?;
    init.commit().map_err(|error| error.to_string())?;

    let relay = RelayUrl::parse("wss://lmdb-ceiling.invalid").map_err(|error| error.to_string())?;
    let observed_at = events
        .iter()
        .map(|event| event.created_at.as_secs())
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let process_before = sample_process();
    let started = Instant::now();
    let mut apply_ns = 0u64;
    let mut canonical_flush_ns = 0u64;
    let mut postings_flush_ns = 0u64;
    let mut commit_ns = 0u64;
    let mut commit_latencies = Vec::with_capacity(events.len().div_ceil(batch_size));
    let mut packed_work = LmdbPackedWork::default();

    for batch in events.chunks(batch_size) {
        let mut write = env.write_txn().map_err(|error| error.to_string())?;
        let mut postings = LmdbPostingsBatch::default();
        let mut canonical =
            LmdbIngestTxn::open(&databases, &mut write, &mut postings).map_err(|error| error.0)?;
        let apply_started = Instant::now();
        for event in batch {
            insert_with_tables(
                &mut canonical,
                event.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(observed_at)),
            )
            .map_err(|error| error.0)?;
        }
        apply_ns = apply_ns.saturating_add(elapsed_ns(apply_started));
        let canonical_flush_started = Instant::now();
        canonical.flush_pending().map_err(|error| error.0)?;
        canonical_flush_ns = canonical_flush_ns.saturating_add(elapsed_ns(canonical_flush_started));
        drop(canonical);
        let postings_started = Instant::now();
        postings
            .flush(&databases, &mut write, &mut packed_work)
            .map_err(|error| error.0)?;
        postings_flush_ns = postings_flush_ns.saturating_add(elapsed_ns(postings_started));
        let commit_started = Instant::now();
        write.commit().map_err(|error| error.to_string())?;
        let latency = elapsed_ns(commit_started);
        commit_ns = commit_ns.saturating_add(latency);
        commit_latencies.push(latency);
    }
    let wall_ns = elapsed_ns(started);
    let process = sample_process().delta(process_before);
    drop(env);

    let data_path = path.join("data.mdb");
    let metadata = fs::metadata(&data_path).map_err(|error| error.to_string())?;
    let database_file_bytes = metadata.len();
    #[cfg(unix)]
    let database_allocated_bytes = {
        use std::os::unix::fs::MetadataExt;
        metadata.blocks().saturating_mul(512)
    };
    #[cfg(not(unix))]
    let database_allocated_bytes = database_file_bytes;

    // Build the independent policy oracle outside the timed and process-sampled
    // region so its allocations do not inflate the LMDB peak-RSS measurement.
    let mut oracle = MemoryStore::default();
    for event in &events {
        oracle
            .insert(
                event.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(observed_at)),
            )
            .map_err(|error| error.0)?;
    }
    let expected_ids: HashSet<_> = oracle
        .query(&Filter::new())
        .map_err(|error| error.0)?
        .into_iter()
        .map(|row| *row.event.id.as_bytes())
        .collect();
    drop(oracle);

    let reopen_started = Instant::now();
    let reopened = unsafe {
        EnvOpenOptions::new()
            .map_size(LMDB_MAP_SIZE)
            .max_dbs(32)
            .open(path)
            .map_err(|error| error.to_string())?
    };
    let read = reopened.read_txn().map_err(|error| error.to_string())?;
    let events_db = reopened
        .open_database::<Bytes, Bytes>(&read, Some("events_v6"))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "reopened LMDB has no events table".to_owned())?;
    let reopened_rows = events_db.len(&read).map_err(|error| error.to_string())?;
    let event_ids_db = reopened
        .open_database::<Bytes, Bytes>(&read, Some("event_ids_v6"))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "reopened LMDB has no event-id table".to_owned())?;
    let reopened_ids = event_ids_db
        .iter(&read)
        .map_err(|error| error.to_string())?
        .map(|row| {
            let (key, _) = row.map_err(|error| error.to_string())?;
            key.try_into()
                .map_err(|_| "reopened event id is not 32 bytes".to_owned())
        })
        .collect::<Result<HashSet<[u8; 32]>, _>>()?;
    let canonical_ids_exact = reopened_ids == expected_ids;
    // Keep the reopen measurement aligned with the Redb comparator: open the
    // environment and enumerate canonical IDs. The deeper packed-format
    // verifier below remains a required correctness check, but is not reopen
    // latency.
    let reopen_ns = elapsed_ns(reopen_started);
    let reopened_databases = open_databases(&reopened, &read)?;
    let packed_exact = validate_packed(&reopened_databases, &read)?;
    let exact_reopen =
        reopened_rows == expected_ids.len() as u64 && canonical_ids_exact && packed_exact;
    drop(read);
    drop(reopened);

    Ok(LmdbGovernedIngestMetrics {
        events: events.len() as u64,
        transaction_batch_size: batch_size,
        transactions: commit_latencies.len() as u64,
        wall_ns,
        apply_ns,
        canonical_flush_ns,
        postings_flush_ns,
        commit_ns,
        commit_p50_ns: nearest_rank(&commit_latencies, 50),
        commit_p95_ns: nearest_rank(&commit_latencies, 95),
        commit_p99_ns: nearest_rank(&commit_latencies, 99),
        reopen_ns,
        cpu_ns: process.cpu_ns,
        allocation_ops: process.allocation_ops,
        allocated_bytes: process.allocated_bytes,
        rss_before_bytes: process.current_rss_bytes,
        peak_rss_bytes: process.peak_rss_bytes,
        process_write_bytes: process.process_write_bytes,
        database_file_bytes,
        database_allocated_bytes,
        expected_rows: expected_ids.len() as u64,
        reopened_rows,
        canonical_ids_exact,
        packed_exact,
        exact_reopen,
        packed_work,
    })
}

fn open_databases(env: &Env, txn: &RoTxn<'_>) -> Result<LmdbDatabases, String> {
    let open = |name| {
        env.open_database::<Bytes, Bytes>(txn, Some(name))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("reopened LMDB has no {name} database"))
    };
    Ok(LmdbDatabases {
        events: open("events_v6")?,
        event_ids: open("event_ids_v6")?,
        event_local: open("event_local_v6")?,
        event_store_meta: open("event_store_meta_v6")?,
        observations: open("event_observations_v6")?,
        relays: open("relays_v6")?,
        relay_keys: open("relay_keys_v6")?,
        relay_refs: open("relay_refs_v6")?,
        relay_meta: open("relay_meta_v6")?,
        cardinality: open("index_cardinality_v1")?,
        addr_index: open("addr_index_v6")?,
        tombstones: open("tombstones")?,
        addr_tombstones: open("addr_tombstones")?,
        expiration: open("expiration_index_v6")?,
        outbox_intents: open("outbox_intents")?,
        outbox_receipts: open("outbox_receipts")?,
        outbox_displaced: open("outbox_displaced_v6")?,
        outbox_kind5_claims: open("outbox_kind5_claims")?,
        outbox_suppress_by_id: open("outbox_suppress_by_id")?,
        outbox_suppress_by_addr: open("outbox_suppress_by_addr")?,
        segments: open("postings_segments_v8")?,
        dictionaries: open("postings_dictionaries_v8")?,
        run_meta: open("postings_run_meta_v8")?,
        run_by_min: open("postings_run_by_min_v8")?,
        dead_keys: open("postings_dead_keys_v8")?,
        postings_meta: open("postings_meta_v8")?,
    })
}

fn validate_packed(db: &LmdbDatabases, txn: &RoTxn<'_>) -> Result<bool, String> {
    let mut canonical = BTreeMap::new();
    for row in db.events.iter(txn).map_err(|error| error.to_string())? {
        let (key, value) = row.map_err(|error| error.to_string())?;
        let key = decode_u64(key).map_err(|error| error.0)?;
        let event = StoredEventView::from_trusted(value)
            .map_err(|error| format!("decode reopened LMDB event: {error:?}"))?
            .materialize_event()
            .map_err(|error| format!("materialize reopened LMDB event: {error:?}"))?;
        canonical.insert(key, event);
    }
    let metas = load_run_metas(db, txn).map_err(|error| error.0)?;
    validate_run_metas(&metas)?;
    let mut actual = Vec::new();
    for meta in &metas {
        let dictionary_bytes = db
            .dictionaries
            .get(txn, &meta.run_id.to_be_bytes())
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("missing reopened dictionary {}", meta.run_id))?;
        let dictionary = DictionaryView::parse(dictionary_bytes)?.validate()?;
        let dead = load_run_deaths(db, txn, meta.run_id).map_err(|error| error.0)?;
        let mut live_keys = BTreeSet::new();
        for family in Family::ALL {
            for shard in 0..=SHARD_MASK {
                let Some(value) = db
                    .segments
                    .get(txn, &segment_key(family, shard, meta.run_id))
                    .map_err(|error| error.to_string())?
                else {
                    continue;
                };
                let segment = SegmentView::parse(value)?;
                segment.validate(dictionary)?;
                for membership in segment.memberships(dictionary)? {
                    if dead
                        .as_ref()
                        .is_some_and(|keys| keys.contains(membership.event.event_key))
                    {
                        continue;
                    }
                    live_keys.insert(membership.event.event_key);
                    actual.push(membership_tuple(membership));
                }
            }
        }
        if live_keys.len() as u64 != meta.live_events {
            return Ok(false);
        }
    }
    let mut expected = memberships_for_events(&canonical)
        .into_iter()
        .map(membership_tuple)
        .collect::<Vec<_>>();
    actual.sort_unstable();
    expected.sort_unstable();
    Ok(actual == expected)
}

fn membership_tuple(membership: Membership) -> (u8, u8, Vec<u8>, u64, [u8; 32], EventKey) {
    (
        membership.family as u8,
        membership.shard,
        membership.prefix.as_bytes().to_vec(),
        membership.event.created_at,
        membership.event.id,
        membership.event.event_key,
    )
}

struct LmdbIngestTxn<'db, 'txn, 'batch> {
    db: &'db LmdbDatabases,
    txn: &'txn mut RwTxn<'db>,
    postings: &'batch mut LmdbPostingsBatch,
    next_event_key: EventKey,
    next_relay_key: RelayKey,
    event_allocator_dirty: bool,
    relay_allocator_dirty: bool,
    relay_ref_counts: HashMap<RelayKey, u64>,
    cardinality_deltas: HashMap<Vec<u8>, i64>,
}

impl<'db, 'txn, 'batch> LmdbIngestTxn<'db, 'txn, 'batch> {
    fn open(
        db: &'db LmdbDatabases,
        txn: &'txn mut RwTxn<'db>,
        postings: &'batch mut LmdbPostingsBatch,
    ) -> Result<Self, PersistenceError> {
        let next_event_key = get_u64(db.event_store_meta, txn, b"next_event_key")?.unwrap_or(1);
        let next_relay_key = get_u32(db.relay_meta, txn, b"next_relay_key")?.unwrap_or(1);
        Ok(Self {
            db,
            txn,
            postings,
            next_event_key,
            next_relay_key,
            event_allocator_dirty: false,
            relay_allocator_dirty: false,
            relay_ref_counts: HashMap::new(),
            cardinality_deltas: HashMap::new(),
        })
    }

    fn allocate_event_key(&mut self) -> Result<EventKey, PersistenceError> {
        let key = self.next_event_key;
        self.next_event_key = key
            .checked_add(1)
            .ok_or_else(|| PersistenceError("canonical event key space exhausted".to_owned()))?;
        self.event_allocator_dirty = true;
        Ok(key)
    }

    fn allocate_relay_key(&mut self) -> Result<RelayKey, PersistenceError> {
        let key = self.next_relay_key;
        self.next_relay_key = key
            .checked_add(1)
            .ok_or_else(|| PersistenceError("relay key space exhausted".to_owned()))?;
        self.relay_allocator_dirty = true;
        Ok(key)
    }

    fn intern_relay(&mut self, relay: &RelayUrl) -> Result<RelayKey, PersistenceError> {
        if let Some(value) = self
            .db
            .relay_keys
            .get(self.txn, relay.as_str().as_bytes())
            .map_err(lmdb_err)?
        {
            return decode_u32(value);
        }
        let key = self.allocate_relay_key()?;
        self.db
            .relays
            .put(self.txn, &key.to_be_bytes(), relay.as_str().as_bytes())
            .map_err(lmdb_err)?;
        self.db
            .relay_keys
            .put(self.txn, relay.as_str().as_bytes(), &key.to_be_bytes())
            .map_err(lmdb_err)?;
        self.db
            .relay_refs
            .put(self.txn, &key.to_be_bytes(), &0u64.to_be_bytes())
            .map_err(lmdb_err)?;
        Ok(key)
    }

    fn effective_relay_ref(&mut self, key: RelayKey) -> Result<u64, PersistenceError> {
        if let Some(value) = self.relay_ref_counts.get(&key) {
            return Ok(*value);
        }
        let value = get_u64(self.db.relay_refs, self.txn, &key.to_be_bytes())?
            .ok_or_else(|| PersistenceError("interned relay has no refcount".to_owned()))?;
        self.relay_ref_counts.insert(key, value);
        Ok(value)
    }

    fn adjust_relay_ref(&mut self, key: RelayKey, delta: i64) -> Result<(), PersistenceError> {
        let current = self.effective_relay_ref(key)?;
        let next = if delta > 0 {
            current.checked_add(delta as u64)
        } else {
            current.checked_sub(delta.unsigned_abs())
        }
        .ok_or_else(|| PersistenceError("relay reference count overflow/underflow".to_owned()))?;
        self.relay_ref_counts.insert(key, next);
        Ok(())
    }

    fn load_seen(
        &self,
        event_key: EventKey,
    ) -> Result<BTreeMap<RelayUrl, Timestamp>, PersistenceError> {
        let mut seen = BTreeMap::new();
        for row in self.db.observations.iter(self.txn).map_err(lmdb_err)? {
            let (key, at) = row.map_err(lmdb_err)?;
            if key.len() != 12 || key[..8] != event_key.to_be_bytes() {
                continue;
            }
            let relay_key = observation_relay_key(key);
            let relay = self
                .db
                .relays
                .get(self.txn, &relay_key.to_be_bytes())
                .map_err(lmdb_err)?
                .ok_or_else(|| PersistenceError("observation relay is absent".to_owned()))?;
            let relay = std::str::from_utf8(relay)
                .map_err(lmdb_err)
                .and_then(|value| RelayUrl::parse(value).map_err(lmdb_err))?;
            seen.insert(relay, Timestamp::from(decode_u64(at)?));
        }
        Ok(seen)
    }

    fn remove_all_observations(&mut self, event_key: EventKey) -> Result<(), PersistenceError> {
        let mut keys = Vec::new();
        for row in self.db.observations.iter(self.txn).map_err(lmdb_err)? {
            let (key, _) = row.map_err(lmdb_err)?;
            if key.len() == 12 && key[..8] == event_key.to_be_bytes() {
                keys.push(key.to_vec());
            }
        }
        for key in keys {
            let relay_key = observation_relay_key(&key);
            if self
                .db
                .observations
                .delete(self.txn, &key)
                .map_err(lmdb_err)?
            {
                self.adjust_relay_ref(relay_key, -1)?;
            }
        }
        Ok(())
    }

    fn adjust_cardinality(&mut self, key: Vec<u8>, delta: i64) -> Result<(), PersistenceError> {
        let current = self.cardinality_deltas.entry(key).or_default();
        *current = current
            .checked_add(delta)
            .ok_or_else(|| PersistenceError("cardinality delta overflow".to_owned()))?;
        Ok(())
    }

    fn update_event_cardinalities(
        &mut self,
        event: &Event,
        delta: i64,
    ) -> Result<(), PersistenceError> {
        if !event_is_cardinality_sample(&CARDINALITY_SAMPLE_KEY, &event.id) {
            return Ok(());
        }
        self.adjust_cardinality(global_cardinality_key(), delta)?;
        self.adjust_cardinality(author_cardinality_key(&event.pubkey), delta)?;
        self.adjust_cardinality(kind_cardinality_key(event.kind), delta)?;
        let mut tags = BTreeSet::new();
        for tag in event.tags.iter() {
            let (Some(name), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
                continue;
            };
            tags.insert(tag_cardinality_key(name, value));
        }
        for key in tags {
            self.adjust_cardinality(key, delta)?;
        }
        Ok(())
    }

    fn flush_pending(&mut self) -> Result<(), PersistenceError> {
        if self.event_allocator_dirty {
            self.db
                .event_store_meta
                .put(
                    self.txn,
                    b"next_event_key",
                    &self.next_event_key.to_be_bytes(),
                )
                .map_err(lmdb_err)?;
        }
        if self.relay_allocator_dirty {
            self.db
                .relay_meta
                .put(
                    self.txn,
                    b"next_relay_key",
                    &self.next_relay_key.to_be_bytes(),
                )
                .map_err(lmdb_err)?;
        }
        for (relay_key, effective) in std::mem::take(&mut self.relay_ref_counts) {
            let key = relay_key.to_be_bytes();
            let persisted = get_u64(self.db.relay_refs, self.txn, &key)?
                .ok_or_else(|| PersistenceError("interned relay has no refcount".to_owned()))?;
            if effective > 0 {
                if effective != persisted {
                    self.db
                        .relay_refs
                        .put(self.txn, &key, &effective.to_be_bytes())
                        .map_err(lmdb_err)?;
                }
                continue;
            }
            let relay = self
                .db
                .relays
                .get(self.txn, &key)
                .map_err(lmdb_err)?
                .ok_or_else(|| PersistenceError("interned relay is absent".to_owned()))?
                .to_vec();
            self.db
                .relay_refs
                .delete(self.txn, &key)
                .map_err(lmdb_err)?;
            self.db.relays.delete(self.txn, &key).map_err(lmdb_err)?;
            self.db
                .relay_keys
                .delete(self.txn, &relay)
                .map_err(lmdb_err)?;
        }
        for (key, delta) in std::mem::take(&mut self.cardinality_deltas) {
            if delta == 0 {
                continue;
            }
            let persisted = get_u64(self.db.cardinality, self.txn, &key)?.unwrap_or(0);
            let effective = if delta > 0 {
                persisted.checked_add(delta as u64)
            } else {
                persisted.checked_sub(delta.unsigned_abs())
            }
            .ok_or_else(|| PersistenceError("cardinality overflow/underflow".to_owned()))?;
            if effective == 0 {
                self.db
                    .cardinality
                    .delete(self.txn, &key)
                    .map_err(lmdb_err)?;
            } else {
                self.db
                    .cardinality
                    .put(self.txn, &key, &effective.to_be_bytes())
                    .map_err(lmdb_err)?;
            }
        }
        Ok(())
    }

    fn string_db(&self, map: GovernedStringMap) -> BytesDb {
        match map {
            GovernedStringMap::Tombstones => self.db.tombstones,
            GovernedStringMap::AddrTombstones => self.db.addr_tombstones,
            GovernedStringMap::OutboxIntents => self.db.outbox_intents,
            GovernedStringMap::OutboxReceipts => self.db.outbox_receipts,
            GovernedStringMap::OutboxKind5Claims => self.db.outbox_kind5_claims,
            GovernedStringMap::OutboxSuppressById => self.db.outbox_suppress_by_id,
            GovernedStringMap::OutboxSuppressByAddr => self.db.outbox_suppress_by_addr,
        }
    }
}

impl GovernedIngestTxn for LmdbIngestTxn<'_, '_, '_> {
    fn key_for_id(&self, id: &EventId) -> Result<Option<EventKey>, PersistenceError> {
        self.db
            .event_ids
            .get(self.txn, id.as_bytes())
            .map_err(lmdb_err)?
            .map(decode_u64)
            .transpose()
    }

    fn load_by_key(&self, key: EventKey) -> Result<Option<StoredEvent>, PersistenceError> {
        let Some(bytes) = self
            .db
            .events
            .get(self.txn, &key.to_be_bytes())
            .map_err(lmdb_err)?
        else {
            return Ok(None);
        };
        let event = StoredEventView::from_trusted(bytes)
            .map_err(lmdb_err)?
            .materialize_event()
            .map_err(lmdb_err)?;
        let local = self.load_local(key)?;
        Ok(Some(StoredEvent {
            event,
            provenance: Provenance {
                seen: self.load_seen(key)?,
                local,
            },
        }))
    }

    fn load_by_id(
        &self,
        id: &EventId,
    ) -> Result<Option<(EventKey, StoredEvent)>, PersistenceError> {
        let Some(key) = self.key_for_id(id)? else {
            return Ok(None);
        };
        Ok(self.load_by_key(key)?.map(|event| (key, event)))
    }

    fn load_local(&self, key: EventKey) -> Result<Option<LocalOrigin>, PersistenceError> {
        self.db
            .event_local
            .get(self.txn, &key.to_be_bytes())
            .map_err(lmdb_err)?
            .map(|bytes| binary_event::decode_local(bytes).map_err(lmdb_err))
            .transpose()
    }

    fn merge_observation(
        &mut self,
        key: EventKey,
        relay: &RelayUrl,
        at: Timestamp,
    ) -> Result<bool, PersistenceError> {
        let relay_key = self.intern_relay(relay)?;
        let observation = observation_key(key, relay_key);
        let existing = get_u64(self.db.observations, self.txn, &observation)?;
        if existing.is_some_and(|value| value >= at.as_secs()) {
            return Ok(false);
        }
        self.db
            .observations
            .put(self.txn, &observation, &at.as_secs().to_be_bytes())
            .map_err(lmdb_err)?;
        if existing.is_none() {
            self.adjust_relay_ref(relay_key, 1)?;
        }
        Ok(true)
    }

    fn replace_event(&mut self, key: EventKey, event: &Event) -> Result<(), PersistenceError> {
        let encoded = binary_event::encode_event(event).map_err(lmdb_err)?;
        self.db
            .events
            .put(self.txn, &key.to_be_bytes(), &encoded)
            .map_err(lmdb_err)
    }

    fn replace_local(
        &mut self,
        key: EventKey,
        local: Option<LocalOrigin>,
    ) -> Result<(), PersistenceError> {
        let key = key.to_be_bytes();
        if let Some(local) = local {
            let encoded = binary_event::encode_local(&local).map_err(lmdb_err)?;
            self.db
                .event_local
                .put(self.txn, &key, &encoded)
                .map_err(lmdb_err)?;
        } else {
            self.db
                .event_local
                .delete(self.txn, &key)
                .map_err(lmdb_err)?;
        }
        Ok(())
    }

    fn insert_new(
        &mut self,
        event: &Event,
        provenance: &Provenance,
    ) -> Result<EventKey, PersistenceError> {
        let key = self.allocate_event_key()?;
        let encoded = binary_event::encode_event(event).map_err(lmdb_err)?;
        self.db
            .events
            .put(self.txn, &key.to_be_bytes(), &encoded)
            .map_err(lmdb_err)?;
        self.db
            .event_ids
            .put(self.txn, event.id.as_bytes(), &key.to_be_bytes())
            .map_err(lmdb_err)?;
        if let Some(local) = &provenance.local {
            let encoded = binary_event::encode_local(local).map_err(lmdb_err)?;
            self.db
                .event_local
                .put(self.txn, &key.to_be_bytes(), &encoded)
                .map_err(lmdb_err)?;
        }
        for (relay, at) in &provenance.seen {
            self.merge_observation(key, relay, *at)?;
        }
        Ok(key)
    }

    fn remove_canonical(&mut self, key: EventKey, id: &EventId) -> Result<(), PersistenceError> {
        let key_bytes = key.to_be_bytes();
        self.db
            .events
            .delete(self.txn, &key_bytes)
            .map_err(lmdb_err)?;
        self.db
            .event_ids
            .delete(self.txn, id.as_bytes())
            .map_err(lmdb_err)?;
        self.db
            .event_local
            .delete(self.txn, &key_bytes)
            .map_err(lmdb_err)?;
        self.remove_all_observations(key)
    }

    fn insert_indexes(&mut self, event: &Event, key: EventKey) -> Result<(), PersistenceError> {
        self.update_event_cardinalities(event, 1)?;
        self.postings.insert(event, key);
        Ok(())
    }

    fn remove_indexes(&mut self, event: &Event, key: EventKey) -> Result<(), PersistenceError> {
        self.update_event_cardinalities(event, -1)?;
        self.postings.remove(key);
        Ok(())
    }

    fn address_get(&self, key: &str) -> Result<Option<EventKey>, PersistenceError> {
        self.db
            .addr_index
            .get(self.txn, key.as_bytes())
            .map_err(lmdb_err)?
            .map(decode_u64)
            .transpose()
    }

    fn address_put(&mut self, key: &str, value: EventKey) -> Result<(), PersistenceError> {
        self.db
            .addr_index
            .put(self.txn, key.as_bytes(), &value.to_be_bytes())
            .map_err(lmdb_err)
    }

    fn address_remove(&mut self, key: &str) -> Result<(), PersistenceError> {
        self.db
            .addr_index
            .delete(self.txn, key.as_bytes())
            .map_err(lmdb_err)?;
        Ok(())
    }

    fn expiration_put(&mut self, key: &[u8; 40], value: EventKey) -> Result<(), PersistenceError> {
        self.db
            .expiration
            .put(self.txn, key, &value.to_be_bytes())
            .map_err(lmdb_err)
    }

    fn expiration_remove(&mut self, key: &[u8; 40]) -> Result<(), PersistenceError> {
        self.db.expiration.delete(self.txn, key).map_err(lmdb_err)?;
        Ok(())
    }

    fn string_get(
        &self,
        map: GovernedStringMap,
        key: &str,
    ) -> Result<Option<String>, PersistenceError> {
        self.string_db(map)
            .get(self.txn, key.as_bytes())
            .map_err(lmdb_err)?
            .map(|bytes| {
                std::str::from_utf8(bytes)
                    .map(str::to_owned)
                    .map_err(lmdb_err)
            })
            .transpose()
    }

    fn string_put(
        &mut self,
        map: GovernedStringMap,
        key: &str,
        value: &str,
    ) -> Result<(), PersistenceError> {
        self.string_db(map)
            .put(self.txn, key.as_bytes(), value.as_bytes())
            .map_err(lmdb_err)
    }

    fn string_remove(
        &mut self,
        map: GovernedStringMap,
        key: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let db = self.string_db(map);
        let value = db
            .get(self.txn, key.as_bytes())
            .map_err(lmdb_err)?
            .map(|bytes| bytes.to_vec());
        db.delete(self.txn, key.as_bytes()).map_err(lmdb_err)?;
        value
            .map(|bytes| String::from_utf8(bytes).map_err(lmdb_err))
            .transpose()
    }

    fn displaced_remove(&mut self, key: &str) -> Result<Option<Vec<u8>>, PersistenceError> {
        let value = self
            .db
            .outbox_displaced
            .get(self.txn, key.as_bytes())
            .map_err(lmdb_err)?
            .map(<[u8]>::to_vec);
        self.db
            .outbox_displaced
            .delete(self.txn, key.as_bytes())
            .map_err(lmdb_err)?;
        Ok(value)
    }
}

#[derive(Default)]
struct LmdbPostingsBatch {
    additions: BTreeMap<EventKey, Event>,
    deaths: BTreeSet<EventKey>,
}

impl LmdbPostingsBatch {
    fn insert(&mut self, event: &Event, key: EventKey) {
        self.deaths.remove(&key);
        self.additions.insert(key, event.clone());
    }

    fn remove(&mut self, key: EventKey) {
        if self.additions.remove(&key).is_none() {
            self.deaths.insert(key);
        }
    }

    fn flush(
        &mut self,
        db: &LmdbDatabases,
        txn: &mut RwTxn<'_>,
        work: &mut LmdbPackedWork,
    ) -> Result<(), PersistenceError> {
        if !self.deaths.is_empty() {
            apply_deaths(db, txn, &self.deaths, work)?;
        }
        if !self.additions.is_empty() {
            publish_events(db, txn, &self.additions, work)?;
        }
        self.additions.clear();
        self.deaths.clear();
        Ok(())
    }
}

fn publish_events(
    db: &LmdbDatabases,
    txn: &mut RwTxn<'_>,
    events: &BTreeMap<EventKey, Event>,
    work: &mut LmdbPackedWork,
) -> Result<(), PersistenceError> {
    let run_id = allocate_run_id(db, txn, work)?;
    let encoded = encode_run(memberships_for_events(events)).map_err(packed_err)?;
    let meta = RunMeta {
        run_id,
        level: 0,
        min_event_key: *events.first_key_value().expect("nonempty additions").0,
        max_event_key: *events.last_key_value().expect("nonempty additions").0,
        live_events: encoded.dictionary_entries,
    };
    insert_run(db, txn, meta, encoded, work)?;
    work.runs_published = work.runs_published.saturating_add(1);
    compact_overfull_levels(db, txn, work)
}

fn memberships_for_events(events: &BTreeMap<EventKey, Event>) -> Vec<Membership> {
    let mut memberships = Vec::new();
    for (&event_key, event) in events {
        let run_event = Arc::new(RunEvent {
            created_at: event.created_at.as_secs(),
            id: *event.id.as_bytes(),
            event_key,
        });
        push_membership(
            &mut memberships,
            Family::Global,
            Prefix::global(),
            &run_event,
        );
        push_membership(
            &mut memberships,
            Family::Author,
            Prefix::author(*event.pubkey.as_bytes()),
            &run_event,
        );
        push_membership(
            &mut memberships,
            Family::Kind,
            Prefix::kind(event.kind.as_u16().to_be_bytes()),
            &run_event,
        );
        let mut tags = BTreeSet::new();
        for tag in event.tags.iter() {
            let (Some(name), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
                continue;
            };
            tags.insert(tag_index_prefix(name, value));
        }
        for tag in tags {
            push_membership(
                &mut memberships,
                Family::Tag,
                Prefix::tag(tag.into()),
                &run_event,
            );
        }
    }
    memberships
}

fn push_membership(
    memberships: &mut Vec<Membership>,
    family: Family,
    prefix: Prefix,
    event: &Arc<RunEvent>,
) {
    memberships.push(Membership {
        family,
        shard: shard_for(family, prefix.as_bytes()),
        prefix,
        event: Arc::clone(event),
    });
}

fn allocate_run_id(
    db: &LmdbDatabases,
    txn: &mut RwTxn<'_>,
    work: &mut LmdbPackedWork,
) -> Result<u64, PersistenceError> {
    let run_id = get_u64(db.postings_meta, txn, b"next_run_id")?.unwrap_or(1);
    let next = run_id
        .checked_add(1)
        .ok_or_else(|| packed_err("run id space exhausted"))?;
    put(
        db.postings_meta,
        txn,
        b"next_run_id",
        &next.to_be_bytes(),
        work,
    )?;
    Ok(run_id)
}

fn insert_run(
    db: &LmdbDatabases,
    txn: &mut RwTxn<'_>,
    meta: RunMeta,
    encoded: super::postings::EncodedRun,
    work: &mut LmdbPackedWork,
) -> Result<(), PersistenceError> {
    work.dictionary_output_bytes = work
        .dictionary_output_bytes
        .saturating_add(encoded.dictionary.len() as u64);
    put(
        db.dictionaries,
        txn,
        &meta.run_id.to_be_bytes(),
        &encoded.dictionary,
        work,
    )?;
    for (family, shard, value) in encoded.segments {
        work.segment_output_bytes = work.segment_output_bytes.saturating_add(value.len() as u64);
        put(
            db.segments,
            txn,
            &segment_key(family, shard, meta.run_id),
            &value,
            work,
        )?;
    }
    insert_run_catalog(db, txn, meta, work)
}

fn insert_run_catalog(
    db: &LmdbDatabases,
    txn: &mut RwTxn<'_>,
    meta: RunMeta,
    work: &mut LmdbPackedWork,
) -> Result<(), PersistenceError> {
    let encoded = meta.encode().map_err(packed_err)?;
    put(db.run_meta, txn, &meta.run_id.to_be_bytes(), &encoded, work)?;
    put(
        db.run_by_min,
        txn,
        &meta.min_event_key.to_be_bytes(),
        &meta.run_id.to_be_bytes(),
        work,
    )
}

fn apply_deaths(
    db: &LmdbDatabases,
    txn: &mut RwTxn<'_>,
    deaths: &BTreeSet<EventKey>,
    work: &mut LmdbPackedWork,
) -> Result<(), PersistenceError> {
    let metas = load_run_metas(db, txn)?;
    let mut by_run: BTreeMap<u64, Vec<EventKey>> = BTreeMap::new();
    for &event_key in deaths {
        if let Some(meta) = metas
            .iter()
            .find(|meta| (meta.min_event_key..=meta.max_event_key).contains(&event_key))
        {
            by_run.entry(meta.run_id).or_default().push(event_key);
        }
    }
    for (run_id, keys) in by_run {
        apply_run_deaths(db, txn, run_id, keys, work)?;
    }
    Ok(())
}

fn compact_overfull_levels(
    db: &LmdbDatabases,
    txn: &mut RwTxn<'_>,
    work: &mut LmdbPackedWork,
) -> Result<(), PersistenceError> {
    let mut level = 0u8;
    loop {
        loop {
            let fan_in = if level == 0 {
                BASE_RUN_FAN_IN
            } else {
                LARGE_RUN_FAN_IN
            };
            let mut cohort = Vec::new();
            let mut has_higher_level = false;
            for meta in load_run_metas(db, txn)? {
                if meta.level == level {
                    cohort.push(meta);
                } else if meta.level > level {
                    has_higher_level = true;
                }
            }
            if cohort.len() < fan_in {
                if has_higher_level {
                    level = level
                        .checked_add(1)
                        .ok_or_else(|| packed_err("packed run level space exhausted"))?;
                } else {
                    return Ok(());
                }
                break;
            }
            cohort.sort_unstable_by_key(|meta| meta.min_event_key);
            cohort.truncate(fan_in);
            compact_cohort(db, txn, level, &cohort, work)?;
        }
    }
}

fn apply_run_deaths(
    db: &LmdbDatabases,
    txn: &mut RwTxn<'_>,
    run_id: u64,
    keys: Vec<EventKey>,
    work: &mut LmdbPackedWork,
) -> Result<(), PersistenceError> {
    let mut meta = load_run_meta(db, txn, run_id)?
        .ok_or_else(|| packed_err("death target has no run metadata"))?;
    let mut existing = Vec::new();
    for level in 0..MAX_DEATH_BLOCKS {
        if let Some(value) = db
            .dead_keys
            .get(txn, &death_key(run_id, level))
            .map_err(lmdb_err)?
        {
            existing.push(DeadKeys::decode(value).map_err(packed_err)?);
        }
    }
    let existing_union = merge_dead_blocks(&existing).map_err(packed_err)?;
    let mut fresh: Vec<_> = keys
        .into_iter()
        .filter(|key| {
            existing_union
                .as_ref()
                .is_none_or(|dead| !dead.contains(*key))
        })
        .collect();
    fresh.sort_unstable();
    fresh.dedup();
    if fresh.is_empty() {
        return Ok(());
    }
    let fresh_count = fresh.len() as u64;
    if fresh_count > meta.live_events {
        return Err(packed_err("death count exceeds run live count"));
    }
    meta.live_events -= fresh_count;
    if meta.live_events == 0 {
        return delete_run(db, txn, meta, work);
    }

    let mut carry = DeadKeys::new(fresh).map_err(packed_err)?;
    for level in 0..MAX_DEATH_BLOCKS {
        let key = death_key(run_id, level);
        let prior = db
            .dead_keys
            .get(txn, &key)
            .map_err(lmdb_err)?
            .map(|value| DeadKeys::decode(value).map_err(packed_err))
            .transpose()?;
        if let Some(prior) = prior {
            delete(db.dead_keys, txn, &key, work)?;
            carry = merge_dead_blocks(&[prior, carry])
                .map_err(packed_err)?
                .expect("two nonempty death blocks");
        } else {
            let encoded = carry.encode().map_err(packed_err)?;
            put(db.dead_keys, txn, &key, &encoded, work)?;
            let encoded_meta = meta.encode().map_err(packed_err)?;
            put(db.run_meta, txn, &run_id.to_be_bytes(), &encoded_meta, work)?;
            return Ok(());
        }
    }

    existing.push(carry);
    let all_dead = merge_dead_blocks(&existing)
        .map_err(packed_err)?
        .expect("fresh deaths are nonempty");
    rewrite_run_without_dead(db, txn, meta, &all_dead, work)
}

fn compact_cohort(
    db: &LmdbDatabases,
    txn: &mut RwTxn<'_>,
    level: u8,
    cohort: &[RunMeta],
    work: &mut LmdbPackedWork,
) -> Result<(), PersistenceError> {
    let output_level = level
        .checked_add(1)
        .ok_or_else(|| packed_err("packed run level space exhausted"))?;
    let dead = cohort
        .iter()
        .map(|meta| load_run_deaths(db, txn, meta.run_id))
        .collect::<Result<Vec<_>, _>>()?;
    let output = stream_compaction_cohort(db, txn, cohort, &dead, work)?;
    for &meta in cohort {
        delete_run(db, txn, meta, work)?;
    }
    work.compactions = work.compactions.saturating_add(1);
    work.compaction_input_runs = work
        .compaction_input_runs
        .saturating_add(cohort.len() as u64);
    let Some((run_id, min_event_key, max_event_key, live_events)) = output else {
        return Ok(());
    };
    work.compaction_events_rewritten = work.compaction_events_rewritten.saturating_add(live_events);
    insert_run_catalog(
        db,
        txn,
        RunMeta {
            run_id,
            level: output_level,
            min_event_key,
            max_event_key,
            live_events,
        },
        work,
    )
}

fn rewrite_run_without_dead(
    db: &LmdbDatabases,
    txn: &mut RwTxn<'_>,
    old_meta: RunMeta,
    dead: &DeadKeys,
    work: &mut LmdbPackedWork,
) -> Result<(), PersistenceError> {
    let output = stream_compaction_cohort(db, txn, &[old_meta], &[Some(dead.clone())], work)?;
    delete_run(db, txn, old_meta, work)?;
    work.compactions = work.compactions.saturating_add(1);
    work.compaction_input_runs = work.compaction_input_runs.saturating_add(1);
    let Some((run_id, min_event_key, max_event_key, live_events)) = output else {
        return Ok(());
    };
    work.compaction_events_rewritten = work.compaction_events_rewritten.saturating_add(live_events);
    insert_run_catalog(
        db,
        txn,
        RunMeta {
            run_id,
            level: old_meta.level,
            min_event_key,
            max_event_key,
            live_events,
        },
        work,
    )
}

fn stream_compaction_cohort(
    db: &LmdbDatabases,
    txn: &mut RwTxn<'_>,
    cohort: &[RunMeta],
    dead: &[Option<DeadKeys>],
    work: &mut LmdbPackedWork,
) -> Result<Option<(u64, u64, u64, u64)>, PersistenceError> {
    if cohort.len() != dead.len() {
        return Err(packed_err("compaction death-map count mismatch"));
    }
    let mut dictionary_entries = Vec::new();
    let mut ordinal_maps = Vec::with_capacity(cohort.len());
    for (source, meta) in cohort.iter().enumerate() {
        let dictionary_bytes = db
            .dictionaries
            .get(txn, &meta.run_id.to_be_bytes())
            .map_err(lmdb_err)?
            .ok_or_else(|| packed_err("run has no dictionary"))?;
        work.dictionary_input_bytes = work
            .dictionary_input_bytes
            .saturating_add(dictionary_bytes.len() as u64);
        let dictionary = DictionaryView::parse(dictionary_bytes)
            .and_then(DictionaryView::validate)
            .map_err(packed_err)?;
        let mut ordinal_map = Vec::with_capacity(dictionary.len());
        for ordinal in 0..dictionary.len() {
            let (event_key, id) = dictionary.entry(ordinal).map_err(packed_err)?;
            if dead[source]
                .as_ref()
                .is_some_and(|keys| keys.contains(event_key))
            {
                ordinal_map.push(None);
                continue;
            }
            if dictionary_entries
                .last()
                .is_some_and(|(prior, _)| *prior >= event_key)
            {
                return Err(packed_err(
                    "compaction cohort dictionaries are not range ordered",
                ));
            }
            let output_ordinal = u32::try_from(dictionary_entries.len())
                .map_err(|_| packed_err("compaction dictionary exceeds u32"))?;
            dictionary_entries.push((event_key, id));
            ordinal_map.push(Some(output_ordinal));
        }
        ordinal_maps.push(ordinal_map);
    }
    if dictionary_entries.is_empty() {
        return Ok(None);
    }
    let min_event_key = dictionary_entries
        .first()
        .expect("nonempty compaction dictionary")
        .0;
    let max_event_key = dictionary_entries
        .last()
        .expect("nonempty compaction dictionary")
        .0;
    let live_events = dictionary_entries.len() as u64;
    let dictionary = encode_dictionary(&dictionary_entries).map_err(packed_err)?;
    let run_id = allocate_run_id(db, txn, work)?;
    work.dictionary_output_bytes = work
        .dictionary_output_bytes
        .saturating_add(dictionary.len() as u64);
    put(
        db.dictionaries,
        txn,
        &run_id.to_be_bytes(),
        &dictionary,
        work,
    )?;
    let output_dictionary = DictionaryView::parse(&dictionary).map_err(packed_err)?;

    let mut postings = 0u64;
    for family in Family::ALL {
        for shard in 0..=SHARD_MASK {
            let mut segment_values = Vec::new();
            for (source, meta) in cohort.iter().enumerate() {
                let key = segment_key(family, shard, meta.run_id);
                let Some(value) = db.segments.get(txn, &key).map_err(lmdb_err)? else {
                    continue;
                };
                work.segment_input_bytes =
                    work.segment_input_bytes.saturating_add(value.len() as u64);
                segment_values.push((source, value.to_vec()));
            }
            if segment_values.is_empty() {
                continue;
            }
            let segment_views = segment_values
                .iter()
                .map(|(source, value)| {
                    SegmentView::parse(value)
                        .map(|segment| (*source, segment))
                        .map_err(packed_err)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let sources = segment_views
                .iter()
                .map(|(source, segment)| CompactionSegmentSource {
                    segment: *segment,
                    ordinal_map: &ordinal_maps[*source],
                })
                .collect::<Vec<_>>();
            let Some(compacted) =
                compact_segment(family, shard, &sources, output_dictionary).map_err(packed_err)?
            else {
                continue;
            };
            postings = postings.saturating_add(compacted.postings);
            work.segment_output_bytes = work
                .segment_output_bytes
                .saturating_add(compacted.value.len() as u64);
            put(
                db.segments,
                txn,
                &segment_key(family, shard, run_id),
                &compacted.value,
                work,
            )?;
        }
    }
    if postings == 0 {
        return Err(packed_err(
            "nonempty compaction dictionary produced no live segments",
        ));
    }
    Ok(Some((run_id, min_event_key, max_event_key, live_events)))
}

fn load_run_metas(db: &LmdbDatabases, txn: &RoTxn<'_>) -> Result<Vec<RunMeta>, PersistenceError> {
    db.run_meta
        .iter(txn)
        .map_err(lmdb_err)?
        .map(|row| {
            let (key, value) = row.map_err(lmdb_err)?;
            let meta = RunMeta::decode(value).map_err(packed_err)?;
            if decode_u64(key)? != meta.run_id {
                return Err(packed_err("run metadata key disagrees with value"));
            }
            Ok(meta)
        })
        .collect()
}

fn load_run_meta(
    db: &LmdbDatabases,
    txn: &RoTxn<'_>,
    run_id: u64,
) -> Result<Option<RunMeta>, PersistenceError> {
    db.run_meta
        .get(txn, &run_id.to_be_bytes())
        .map_err(lmdb_err)?
        .map(|value| RunMeta::decode(value).map_err(packed_err))
        .transpose()
}

fn load_run_deaths(
    db: &LmdbDatabases,
    txn: &RoTxn<'_>,
    run_id: u64,
) -> Result<Option<DeadKeys>, PersistenceError> {
    let mut blocks = Vec::new();
    for level in 0..MAX_DEATH_BLOCKS {
        if let Some(value) = db
            .dead_keys
            .get(txn, &death_key(run_id, level))
            .map_err(lmdb_err)?
        {
            blocks.push(DeadKeys::decode(value).map_err(packed_err)?);
        }
    }
    merge_dead_blocks(&blocks).map_err(packed_err)
}

fn delete_run(
    db: &LmdbDatabases,
    txn: &mut RwTxn<'_>,
    meta: RunMeta,
    work: &mut LmdbPackedWork,
) -> Result<(), PersistenceError> {
    for family in Family::ALL {
        for shard in 0..=SHARD_MASK {
            delete(
                db.segments,
                txn,
                &segment_key(family, shard, meta.run_id),
                work,
            )?;
        }
    }
    delete(db.dictionaries, txn, &meta.run_id.to_be_bytes(), work)?;
    delete(db.run_meta, txn, &meta.run_id.to_be_bytes(), work)?;
    delete(db.run_by_min, txn, &meta.min_event_key.to_be_bytes(), work)?;
    for level in 0..MAX_DEATH_BLOCKS {
        delete(db.dead_keys, txn, &death_key(meta.run_id, level), work)?;
    }
    Ok(())
}

fn segment_key(family: Family, shard: u8, run_id: u64) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = family as u8;
    key[1] = shard;
    key[2..].copy_from_slice(&run_id.to_be_bytes());
    key
}

fn death_key(run_id: u64, level: usize) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[..8].copy_from_slice(&run_id.to_be_bytes());
    key[8] = level as u8;
    key
}

fn put(
    db: BytesDb,
    txn: &mut RwTxn<'_>,
    key: &[u8],
    value: &[u8],
    work: &mut LmdbPackedWork,
) -> Result<(), PersistenceError> {
    db.put(txn, key, value).map_err(lmdb_err)?;
    work.logical_puts = work.logical_puts.saturating_add(1);
    Ok(())
}

fn delete(
    db: BytesDb,
    txn: &mut RwTxn<'_>,
    key: &[u8],
    work: &mut LmdbPackedWork,
) -> Result<(), PersistenceError> {
    if db.delete(txn, key).map_err(lmdb_err)? {
        work.logical_deletes = work.logical_deletes.saturating_add(1);
    }
    Ok(())
}

fn get_u64(db: BytesDb, txn: &RoTxn<'_>, key: &[u8]) -> Result<Option<u64>, PersistenceError> {
    db.get(txn, key)
        .map_err(lmdb_err)?
        .map(decode_u64)
        .transpose()
}

fn get_u32(db: BytesDb, txn: &RoTxn<'_>, key: &[u8]) -> Result<Option<u32>, PersistenceError> {
    db.get(txn, key)
        .map_err(lmdb_err)?
        .map(decode_u32)
        .transpose()
}

fn decode_u64(bytes: &[u8]) -> Result<u64, PersistenceError> {
    bytes
        .try_into()
        .map(u64::from_be_bytes)
        .map_err(|_| PersistenceError("invalid LMDB u64 width".to_owned()))
}

fn decode_u32(bytes: &[u8]) -> Result<u32, PersistenceError> {
    bytes
        .try_into()
        .map(u32::from_be_bytes)
        .map_err(|_| PersistenceError("invalid LMDB u32 width".to_owned()))
}

fn lmdb_err(error: impl std::fmt::Display) -> PersistenceError {
    PersistenceError(format!("LMDB benchmark: {error}"))
}

fn packed_err(error: impl std::fmt::Display) -> PersistenceError {
    PersistenceError(format!("packed postings: {error}"))
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

fn nearest_rank(values: &[u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = sorted.len().saturating_mul(percentile).saturating_add(99) / 100;
    sorted[rank.saturating_sub(1)]
}

#[cfg(test)]
mod tests {
    use nostr::{EventBuilder, Keys, Kind, Tag};

    use super::*;

    fn counters() -> StoreBenchProcessCounters {
        StoreBenchProcessCounters::default()
    }

    #[test]
    fn lmdb_adapter_reuses_governed_policy_and_production_packed_compaction() {
        let keys = Keys::generate();
        let mut events = (0..20)
            .map(|index| {
                EventBuilder::new(Kind::TextNote, format!("regular-{index}"))
                    .custom_created_at(Timestamp::from(100 + index))
                    .sign_with_keys(&keys)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let target = events[0].clone();
        events.push(
            EventBuilder::new(Kind::ContactList, "old")
                .custom_created_at(Timestamp::from(200))
                .sign_with_keys(&keys)
                .unwrap(),
        );
        events.push(
            EventBuilder::new(Kind::ContactList, "new")
                .custom_created_at(Timestamp::from(201))
                .sign_with_keys(&keys)
                .unwrap(),
        );
        events.push(
            EventBuilder::new(Kind::EventDeletion, "")
                .tag(Tag::event(target.id))
                .custom_created_at(Timestamp::from(202))
                .sign_with_keys(&keys)
                .unwrap(),
        );
        let temp = tempfile::tempdir().unwrap();
        let metrics = run_lmdb_governed_ingest_bench(temp.path(), events, 2, counters).unwrap();
        assert!(metrics.exact_reopen);
        assert!(metrics.canonical_ids_exact);
        assert!(metrics.packed_exact);
        assert!(metrics.packed_work.compactions > 0);
        assert!(metrics.packed_work.compaction_events_rewritten > 0);
        assert_eq!(metrics.expected_rows, metrics.reopened_rows);
    }
}
