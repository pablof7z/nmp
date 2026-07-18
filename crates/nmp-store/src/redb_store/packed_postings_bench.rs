//! Benchmark-only production-format qualification for issue #655.
//!
//! The production store writes one database row per event/index membership.
//! This comparator keeps canonical events, raw-id lookup, and provenance in
//! their existing physical shape, but publishes the four query indexes as
//! immutable transaction-generation segments with run-local ID dictionaries,
//! random-access postings, bounded immutable death blocks, and persisted-data
//! compaction. Redb and Fjall consume byte-identical values; this module is
//! qualification evidence, not the production backend.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use fjall::{
    KeyspaceCreateOptions, PersistMode, Readable, SingleWriterTxDatabase, SingleWriterTxKeyspace,
};
use nostr::{Event, RelayUrl};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

use super::canonical::observation_key;
use super::postings::{
    self, merge_dead_blocks, merge_posting_cursors, shard_for, validate_run_metas, DeadKeys,
    DictionaryView, EncodedRun, Family, Membership, MergeSource, Prefix, RunEvent, RunMeta,
    SegmentView,
};
use super::query::tag_index_prefix;
use super::schema::{
    EVENTS, EVENT_IDS, EVENT_OBSERVATIONS, REDB_CACHE_BYTES, RELAYS, RELAY_KEYS, RELAY_REFS,
};
use super::store_bench::{duration_ns, nearest_rank};
use super::{binary_event, StoreBenchProcessCounters};

const PACKED_SEGMENTS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("packed_postings_segments_v2");
const PACKED_DICTIONARIES: TableDefinition<u64, &[u8]> =
    TableDefinition::new("packed_postings_dictionaries_v1");
const PACKED_RUN_META: TableDefinition<u64, &[u8]> =
    TableDefinition::new("packed_postings_run_meta_v1");
const PACKED_DEAD_KEYS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("packed_postings_dead_keys_v2");
const FAMILY_COUNT: usize = 4;
const PACKED_REDB_CACHE_BYTES: usize = 16 * 1_024 * 1_024;
const FJALL_CACHE_BYTES: u64 = 16 * 1_024 * 1_024;
const FJALL_WRITE_BUFFER_BYTES: u64 = 32 * 1_024 * 1_024;
const FJALL_MEMTABLE_BYTES: u64 = 4 * 1_024 * 1_024;
const FJALL_WORKERS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackedPostingsBackend {
    Redb,
    Fjall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackedPostingsMetrics {
    pub backend: PackedPostingsBackend,
    pub events: u64,
    pub transaction_batch_size: usize,
    pub transactions: u64,
    pub wall_ns: u64,
    pub segment_build_ns: u64,
    pub packed_encode_ns: u64,
    pub packed_dictionary_build_ns: u64,
    pub packed_membership_sort_ns: u64,
    pub packed_segment_encode_ns: u64,
    pub commit_ns: u64,
    pub commit_p50_ns: u64,
    pub commit_p95_ns: u64,
    pub commit_p99_ns: u64,
    pub deletion_events: u64,
    pub deletion_overlay_rows: u64,
    pub deletion_overlay_bytes: u64,
    pub deletion_ns: u64,
    pub deletion_process_write_bytes: Option<u64>,
    pub maintenance_ns: u64,
    pub maintenance_process_write_bytes: Option<u64>,
    pub cpu_ns: u64,
    pub allocation_ops: u64,
    pub allocated_bytes: u64,
    pub rss_before_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    pub process_write_bytes: Option<u64>,
    pub encoded_event_bytes: u64,
    pub segment_rows: u64,
    pub segment_bytes: u64,
    pub dictionary_rows: u64,
    pub dictionary_bytes: u64,
    pub run_meta_rows: u64,
    pub run_meta_bytes: u64,
    pub prefix_records: u64,
    pub packed_postings: u64,
    pub posting_bytes: u64,
    pub seek_directory_bytes: u64,
    pub active_segment_rows: u64,
    pub active_segment_bytes: u64,
    pub active_dictionary_rows: u64,
    pub active_dictionary_bytes: u64,
    pub active_run_meta_rows: u64,
    pub active_run_meta_bytes: u64,
    pub memberships: [u64; FAMILY_COUNT],
    pub database_logical_bytes: u64,
    pub database_stored_bytes: u64,
    pub reopened_rows: u64,
    pub reopened_memberships: [u64; FAMILY_COUNT],
    pub exact_reopen: bool,
    pub queries: Vec<PackedQueryMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackedQueryMetrics {
    pub name: String,
    pub returned_rows: u64,
    pub iterations: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub exact: bool,
}

type BatchSegments = Vec<Membership>;

pub fn run_packed_postings_bench(
    backend: PackedPostingsBackend,
    path: &Path,
    events: Vec<Event>,
    batch_size: usize,
    sample_process: fn() -> StoreBenchProcessCounters,
) -> Result<PackedPostingsMetrics, String> {
    if events.is_empty() {
        return Err("benchmark corpus must not be empty".to_owned());
    }
    if batch_size == 0 {
        return Err("transaction batch size must be nonzero".to_owned());
    }
    match backend {
        PackedPostingsBackend::Redb => run_redb(path, events, batch_size, sample_process),
        PackedPostingsBackend::Fjall => run_fjall(path, events, batch_size, sample_process),
    }
}

fn run_redb(
    path: &Path,
    events: Vec<Event>,
    batch_size: usize,
    sample_process: fn() -> StoreBenchProcessCounters,
) -> Result<PackedPostingsMetrics, String> {
    let db = Database::builder()
        .set_cache_size(PACKED_REDB_CACHE_BYTES.min(REDB_CACHE_BYTES))
        .create(path)
        .map_err(|error| error.to_string())?;
    let init = db.begin_write().map_err(|error| error.to_string())?;
    init.open_table(EVENTS).map_err(|error| error.to_string())?;
    init.open_table(EVENT_IDS)
        .map_err(|error| error.to_string())?;
    init.open_table(EVENT_OBSERVATIONS)
        .map_err(|error| error.to_string())?;
    init.open_table(RELAYS).map_err(|error| error.to_string())?;
    init.open_table(RELAY_KEYS)
        .map_err(|error| error.to_string())?;
    init.open_table(RELAY_REFS)
        .map_err(|error| error.to_string())?;
    init.open_table(PACKED_SEGMENTS)
        .map_err(|error| error.to_string())?;
    init.open_table(PACKED_DICTIONARIES)
        .map_err(|error| error.to_string())?;
    init.open_table(PACKED_RUN_META)
        .map_err(|error| error.to_string())?;
    init.open_table(PACKED_DEAD_KEYS)
        .map_err(|error| error.to_string())?;
    init.commit().map_err(|error| error.to_string())?;

    let relay =
        RelayUrl::parse("wss://packed-postings.invalid").map_err(|error| error.to_string())?;
    let observed_at = observed_at(&events);
    let event_count = events.len() as u64;
    let process_before = sample_process();
    let started = Instant::now();
    let mut totals = RunTotals::default();
    let mut commit_latencies = Vec::new();

    for (batch_index, batch) in events.chunks(batch_size).enumerate() {
        let write = db.begin_write().map_err(|error| error.to_string())?;
        let mut event_rows = write
            .open_table(EVENTS)
            .map_err(|error| error.to_string())?;
        let mut event_ids = write
            .open_table(EVENT_IDS)
            .map_err(|error| error.to_string())?;
        let mut observations = write
            .open_table(EVENT_OBSERVATIONS)
            .map_err(|error| error.to_string())?;
        let mut relays = write
            .open_table(RELAYS)
            .map_err(|error| error.to_string())?;
        let mut relay_keys = write
            .open_table(RELAY_KEYS)
            .map_err(|error| error.to_string())?;
        let mut relay_refs = write
            .open_table(RELAY_REFS)
            .map_err(|error| error.to_string())?;
        let mut segments = write
            .open_table(PACKED_SEGMENTS)
            .map_err(|error| error.to_string())?;
        let mut dictionaries = write
            .open_table(PACKED_DICTIONARIES)
            .map_err(|error| error.to_string())?;
        let mut run_meta = write
            .open_table(PACKED_RUN_META)
            .map_err(|error| error.to_string())?;

        if batch_index == 0 {
            relays
                .insert(1, relay.as_str())
                .map_err(|e| e.to_string())?;
            relay_keys
                .insert(relay.as_str(), 1)
                .map_err(|e| e.to_string())?;
        }

        let first_key = first_event_key(batch_index, batch_size)?;
        let build_started = Instant::now();
        let mut grouped = BatchSegments::with_capacity(batch.len().saturating_mul(5));
        for (offset, event) in batch.iter().enumerate() {
            let event_key = first_key + offset as u64;
            let encoded = binary_event::encode_event(event)
                .map_err(|error| format!("encode event: {error}"))?;
            totals.encoded_event_bytes = totals
                .encoded_event_bytes
                .saturating_add(encoded.len() as u64);
            event_rows
                .insert(event_key, encoded.as_slice())
                .map_err(|error| error.to_string())?;
            event_ids
                .insert(event.id.as_bytes(), event_key)
                .map_err(|error| error.to_string())?;
            observations
                .insert(&observation_key(event_key, 1), observed_at)
                .map_err(|error| error.to_string())?;
            add_event_memberships(&mut grouped, &mut totals.memberships, event, event_key);
        }
        let encode_started = Instant::now();
        let encoded_run = encode_segments(grouped)?;
        totals.packed_encode_ns = totals
            .packed_encode_ns
            .saturating_add(duration_ns(encode_started));
        totals.packed_dictionary_build_ns = totals
            .packed_dictionary_build_ns
            .saturating_add(encoded_run.dictionary_build_ns);
        totals.packed_membership_sort_ns = totals
            .packed_membership_sort_ns
            .saturating_add(encoded_run.membership_sort_ns);
        totals.packed_segment_encode_ns = totals
            .packed_segment_encode_ns
            .saturating_add(encoded_run.segment_encode_ns);
        totals.segment_build_ns = totals
            .segment_build_ns
            .saturating_add(duration_ns(build_started));
        let run_id = batch_index as u64;
        totals.dictionary_rows += 1;
        totals.dictionary_bytes = totals
            .dictionary_bytes
            .saturating_add(encoded_run.dictionary.len() as u64);
        totals.prefix_records = totals.prefix_records.saturating_add(encoded_run.prefixes);
        totals.packed_postings = totals.packed_postings.saturating_add(encoded_run.postings);
        totals.posting_bytes = totals
            .posting_bytes
            .saturating_add(encoded_run.posting_bytes);
        dictionaries
            .insert(run_id, encoded_run.dictionary.as_slice())
            .map_err(|error| error.to_string())?;
        let meta = RunMeta {
            run_id,
            level: 0,
            min_event_key: first_key,
            max_event_key: first_key + batch.len() as u64 - 1,
            live_events: batch.len() as u64,
        }
        .encode()?;
        totals.run_meta_rows += 1;
        totals.run_meta_bytes = totals.run_meta_bytes.saturating_add(meta.len() as u64);
        run_meta
            .insert(run_id, meta.as_slice())
            .map_err(|error| error.to_string())?;
        for (family, shard, value) in encoded_run.segments {
            let key = segment_key(family, shard, run_id);
            totals.segment_rows += 1;
            totals.segment_bytes = totals.segment_bytes.saturating_add(value.len() as u64);
            totals.segment_keys.push(key);
            segments
                .insert(key.as_slice(), value.as_slice())
                .map_err(|error| error.to_string())?;
        }
        relay_refs
            .insert(1, first_key + batch.len() as u64 - 1)
            .map_err(|error| error.to_string())?;

        drop(run_meta);
        drop(dictionaries);
        drop(segments);
        drop(relay_refs);
        drop(relay_keys);
        drop(relays);
        drop(observations);
        drop(event_ids);
        drop(event_rows);
        let commit_started = Instant::now();
        write.commit().map_err(|error| error.to_string())?;
        let latency = duration_ns(commit_started);
        totals.commit_ns = totals.commit_ns.saturating_add(latency);
        commit_latencies.push(latency);
        totals.transactions += 1;
    }
    let wall_ns = duration_ns(started);
    let process_after_ingest = sample_process();
    let process = process_after_ingest.delta(process_before);
    let mut deletion = apply_redb_deletion_overlay(&db, &events, batch_size)?;
    let process_after_deletion = sample_process();
    deletion.process_write_bytes = process_after_deletion
        .delta(process_after_ingest)
        .process_write_bytes;
    let mut maintenance = compact_redb_segments(
        &db,
        &events,
        batch_size,
        &totals.segment_keys,
        &deletion.event_keys,
    )?;
    maintenance.process_write_bytes = sample_process()
        .delta(process_after_deletion)
        .process_write_bytes;
    let stats = db.begin_write().map_err(|error| error.to_string())?;
    let stored_bytes = stats
        .stats()
        .map_err(|error| error.to_string())?
        .stored_bytes();
    drop(stats);
    drop(db);

    let reopened = Database::open(path).map_err(|error| error.to_string())?;
    let read = reopened.begin_read().map_err(|error| error.to_string())?;
    let reopened_rows = read
        .open_table(EVENTS)
        .map_err(|error| error.to_string())?
        .len()
        .map_err(|error| error.to_string())?;
    let segment_table = read
        .open_table(PACKED_SEGMENTS)
        .map_err(|error| error.to_string())?;
    let dictionary_table = read
        .open_table(PACKED_DICTIONARIES)
        .map_err(|error| error.to_string())?;
    let run_meta_table = read
        .open_table(PACKED_RUN_META)
        .map_err(|error| error.to_string())?;
    let dead_key_table = read
        .open_table(PACKED_DEAD_KEYS)
        .map_err(|error| error.to_string())?;
    let mut reopened_memberships = [0u64; FAMILY_COUNT];
    let mut reopened_segment_rows = 0u64;
    let mut active_segment_bytes = 0u64;
    let active_dictionary_rows = dictionary_table.len().map_err(|error| error.to_string())?;
    let mut active_dictionary_bytes = 0u64;
    for entry in dictionary_table.iter().map_err(|error| error.to_string())? {
        let (_, value) = entry.map_err(|error| error.to_string())?;
        active_dictionary_bytes =
            active_dictionary_bytes.saturating_add(value.value().len() as u64);
    }
    let mut metas = Vec::new();
    let mut active_run_meta_bytes = 0u64;
    for entry in run_meta_table.iter().map_err(|error| error.to_string())? {
        let (key, value) = entry.map_err(|error| error.to_string())?;
        active_run_meta_bytes = active_run_meta_bytes.saturating_add(value.value().len() as u64);
        let meta = RunMeta::decode(value.value())?;
        if meta.run_id != key.value() {
            return Err("run metadata key/value id mismatch".to_owned());
        }
        metas.push(meta);
    }
    validate_run_metas(&metas)?;
    let active_run_meta_rows = metas.len() as u64;
    for entry in segment_table.iter().map_err(|error| error.to_string())? {
        let (key, value) = entry.map_err(|error| error.to_string())?;
        let key: [u8; 10] = key
            .value()
            .try_into()
            .map_err(|_| "invalid Redb segment key width".to_owned())?;
        let run_id = segment_generation(&key);
        let dictionary = dictionary_table
            .get(run_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("missing run dictionary {run_id}"))?;
        let dictionary = DictionaryView::parse(dictionary.value())?.validate()?;
        let segment = SegmentView::parse(value.value())?;
        if segment.family as u8 != key[0] || segment.shard != key[1] {
            return Err("segment key/value family or shard mismatch".to_owned());
        }
        let postings = segment.validate(dictionary)?;
        let family = segment.family;
        reopened_memberships[family as usize] =
            reopened_memberships[family as usize].saturating_add(postings);
        reopened_segment_rows += 1;
        active_segment_bytes = active_segment_bytes.saturating_add(value.value().len() as u64);
    }
    let _active_dead_keys = decode_redb_dead_keys(&dead_key_table)?;
    let queries = run_query_benchmarks(&events, &deletion.event_keys, |family, prefix, limit| {
        query_redb(&read, family, prefix, limit)
    })?;
    drop(dead_key_table);
    drop(run_meta_table);
    drop(dictionary_table);
    drop(segment_table);
    drop(read);
    drop(reopened);

    finish_metrics(
        PackedPostingsBackend::Redb,
        path,
        event_count,
        batch_size,
        wall_ns,
        totals,
        &commit_latencies,
        process,
        stored_bytes,
        reopened_rows,
        reopened_segment_rows,
        reopened_memberships,
        active_segment_bytes,
        active_dictionary_rows,
        active_dictionary_bytes,
        active_run_meta_rows,
        active_run_meta_bytes,
        deletion,
        maintenance,
        queries,
    )
}

struct FjallKeyspaces {
    events: SingleWriterTxKeyspace,
    event_ids: SingleWriterTxKeyspace,
    observations: SingleWriterTxKeyspace,
    relays: SingleWriterTxKeyspace,
    relay_keys: SingleWriterTxKeyspace,
    relay_refs: SingleWriterTxKeyspace,
    segments: SingleWriterTxKeyspace,
    dictionaries: SingleWriterTxKeyspace,
    run_meta: SingleWriterTxKeyspace,
    dead_keys: SingleWriterTxKeyspace,
}

impl FjallKeyspaces {
    fn open(database: &SingleWriterTxDatabase) -> Result<Self, String> {
        let open = |name: &str| {
            database
                .keyspace(name, || {
                    KeyspaceCreateOptions::default().max_memtable_size(FJALL_MEMTABLE_BYTES)
                })
                .map_err(|error| error.to_string())
        };
        Ok(Self {
            events: open("packed_events")?,
            event_ids: open("packed_event_ids")?,
            observations: open("packed_observations")?,
            relays: open("packed_relays")?,
            relay_keys: open("packed_relay_keys")?,
            relay_refs: open("packed_relay_refs")?,
            segments: open("packed_segments_v2")?,
            dictionaries: open("packed_dictionaries_v1")?,
            run_meta: open("packed_run_meta_v1")?,
            dead_keys: open("packed_dead_keys_v2")?,
        })
    }
}

#[allow(deprecated)]
fn run_fjall(
    path: &Path,
    events: Vec<Event>,
    batch_size: usize,
    sample_process: fn() -> StoreBenchProcessCounters,
) -> Result<PackedPostingsMetrics, String> {
    let database = SingleWriterTxDatabase::builder(path)
        .worker_threads(FJALL_WORKERS)
        .cache_size(FJALL_CACHE_BYTES)
        .max_write_buffer_size(Some(FJALL_WRITE_BUFFER_BYTES))
        .open()
        .map_err(|error| error.to_string())?;
    let keyspaces = FjallKeyspaces::open(&database)?;
    database
        .persist(PersistMode::SyncAll)
        .map_err(|error| error.to_string())?;

    let relay =
        RelayUrl::parse("wss://packed-postings.invalid").map_err(|error| error.to_string())?;
    let observed_at = observed_at(&events);
    let event_count = events.len() as u64;
    let process_before = sample_process();
    let started = Instant::now();
    let mut totals = RunTotals::default();
    let mut commit_latencies = Vec::new();

    for (batch_index, batch) in events.chunks(batch_size).enumerate() {
        let mut write = database.write_tx().durability(Some(PersistMode::SyncAll));
        if batch_index == 0 {
            write.insert(&keyspaces.relays, 1u32.to_be_bytes(), relay.as_str());
            write.insert(&keyspaces.relay_keys, relay.as_str(), 1u32.to_be_bytes());
        }
        let first_key = first_event_key(batch_index, batch_size)?;
        let build_started = Instant::now();
        let mut grouped = BatchSegments::with_capacity(batch.len().saturating_mul(5));
        for (offset, event) in batch.iter().enumerate() {
            let event_key = first_key + offset as u64;
            let encoded = binary_event::encode_event(event)
                .map_err(|error| format!("encode event: {error}"))?;
            totals.encoded_event_bytes = totals
                .encoded_event_bytes
                .saturating_add(encoded.len() as u64);
            write.insert(&keyspaces.events, event_key.to_be_bytes(), encoded);
            write.insert(
                &keyspaces.event_ids,
                event.id.as_bytes(),
                event_key.to_be_bytes(),
            );
            write.insert(
                &keyspaces.observations,
                observation_key(event_key, 1),
                observed_at.to_be_bytes(),
            );
            add_event_memberships(&mut grouped, &mut totals.memberships, event, event_key);
        }
        let encode_started = Instant::now();
        let encoded_run = encode_segments(grouped)?;
        totals.packed_encode_ns = totals
            .packed_encode_ns
            .saturating_add(duration_ns(encode_started));
        totals.packed_dictionary_build_ns = totals
            .packed_dictionary_build_ns
            .saturating_add(encoded_run.dictionary_build_ns);
        totals.packed_membership_sort_ns = totals
            .packed_membership_sort_ns
            .saturating_add(encoded_run.membership_sort_ns);
        totals.packed_segment_encode_ns = totals
            .packed_segment_encode_ns
            .saturating_add(encoded_run.segment_encode_ns);
        totals.segment_build_ns = totals
            .segment_build_ns
            .saturating_add(duration_ns(build_started));
        let run_id = batch_index as u64;
        totals.dictionary_rows += 1;
        totals.dictionary_bytes = totals
            .dictionary_bytes
            .saturating_add(encoded_run.dictionary.len() as u64);
        totals.prefix_records = totals.prefix_records.saturating_add(encoded_run.prefixes);
        totals.packed_postings = totals.packed_postings.saturating_add(encoded_run.postings);
        totals.posting_bytes = totals
            .posting_bytes
            .saturating_add(encoded_run.posting_bytes);
        write.insert(
            &keyspaces.dictionaries,
            run_id.to_be_bytes(),
            encoded_run.dictionary,
        );
        let meta = RunMeta {
            run_id,
            level: 0,
            min_event_key: first_key,
            max_event_key: first_key + batch.len() as u64 - 1,
            live_events: batch.len() as u64,
        }
        .encode()?;
        totals.run_meta_rows += 1;
        totals.run_meta_bytes = totals.run_meta_bytes.saturating_add(meta.len() as u64);
        write.insert(&keyspaces.run_meta, run_id.to_be_bytes(), meta);
        for (family, shard, value) in encoded_run.segments {
            let key = segment_key(family, shard, run_id);
            totals.segment_rows += 1;
            totals.segment_bytes = totals.segment_bytes.saturating_add(value.len() as u64);
            totals.segment_keys.push(key);
            write.insert(&keyspaces.segments, key, value);
        }
        write.insert(
            &keyspaces.relay_refs,
            1u32.to_be_bytes(),
            (first_key + batch.len() as u64 - 1).to_be_bytes(),
        );

        let commit_started = Instant::now();
        write.commit().map_err(|error| error.to_string())?;
        let latency = duration_ns(commit_started);
        totals.commit_ns = totals.commit_ns.saturating_add(latency);
        commit_latencies.push(latency);
        totals.transactions += 1;
    }
    let wall_ns = duration_ns(started);
    let process_after_ingest = sample_process();
    let process = process_after_ingest.delta(process_before);
    let mut deletion = apply_fjall_deletion_overlay(&database, &keyspaces, &events, batch_size)?;
    let process_after_deletion = sample_process();
    deletion.process_write_bytes = process_after_deletion
        .delta(process_after_ingest)
        .process_write_bytes;
    let mut maintenance = compact_fjall_segments(
        &database,
        &keyspaces,
        &events,
        batch_size,
        &totals.segment_keys,
        &deletion.event_keys,
    )?;
    maintenance.process_write_bytes = sample_process()
        .delta(process_after_deletion)
        .process_write_bytes;
    let stored_bytes = database.disk_space().map_err(|error| error.to_string())?;
    drop(keyspaces);
    drop(database);
    let logical_bytes = directory_bytes(path).map_err(|error| error.to_string())?;

    let reopened = SingleWriterTxDatabase::builder(path)
        .worker_threads(FJALL_WORKERS)
        .cache_size(FJALL_CACHE_BYTES)
        .max_write_buffer_size(Some(FJALL_WRITE_BUFFER_BYTES))
        .open()
        .map_err(|error| error.to_string())?;
    let reopened_keyspaces = FjallKeyspaces::open(&reopened)?;
    let read = reopened.read_tx();
    let reopened_rows = read
        .len(&reopened_keyspaces.events)
        .map_err(|error| error.to_string())? as u64;
    let mut reopened_memberships = [0u64; FAMILY_COUNT];
    let mut reopened_segment_rows = 0u64;
    let mut active_segment_bytes = 0u64;
    let active_dictionary_rows = read
        .len(&reopened_keyspaces.dictionaries)
        .map_err(|error| error.to_string())? as u64;
    let mut active_dictionary_bytes = 0u64;
    for entry in read.iter(&reopened_keyspaces.dictionaries) {
        let (_, value) = entry.into_inner().map_err(|error| error.to_string())?;
        active_dictionary_bytes = active_dictionary_bytes.saturating_add(value.len() as u64);
    }
    let mut metas = Vec::new();
    let mut active_run_meta_bytes = 0u64;
    for entry in read.iter(&reopened_keyspaces.run_meta) {
        let (key, value) = entry.into_inner().map_err(|error| error.to_string())?;
        active_run_meta_bytes = active_run_meta_bytes.saturating_add(value.len() as u64);
        let key = u64::from_be_bytes(
            key.as_ref()
                .try_into()
                .map_err(|_| "invalid Fjall run metadata key width".to_owned())?,
        );
        let meta = RunMeta::decode(&value)?;
        if meta.run_id != key {
            return Err("run metadata key/value id mismatch".to_owned());
        }
        metas.push(meta);
    }
    validate_run_metas(&metas)?;
    let active_run_meta_rows = metas.len() as u64;
    for entry in read.iter(&reopened_keyspaces.segments) {
        let (key, value) = entry.into_inner().map_err(|error| error.to_string())?;
        let key: [u8; 10] = key
            .as_ref()
            .try_into()
            .map_err(|_| "invalid Fjall segment key width".to_owned())?;
        let run_id = segment_generation(&key);
        let dictionary = read
            .get(&reopened_keyspaces.dictionaries, run_id.to_be_bytes())
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("missing run dictionary {run_id}"))?;
        let dictionary = DictionaryView::parse(&dictionary)?.validate()?;
        let segment = SegmentView::parse(&value)?;
        if segment.family as u8 != key[0] || segment.shard != key[1] {
            return Err("segment key/value family or shard mismatch".to_owned());
        }
        let postings = segment.validate(dictionary)?;
        let family = segment.family;
        reopened_memberships[family as usize] =
            reopened_memberships[family as usize].saturating_add(postings);
        reopened_segment_rows += 1;
        active_segment_bytes = active_segment_bytes.saturating_add(value.len() as u64);
    }
    let _active_dead_keys = decode_fjall_dead_keys(&read, &reopened_keyspaces)?;
    let queries = run_query_benchmarks(&events, &deletion.event_keys, |family, prefix, limit| {
        query_fjall(&read, &reopened_keyspaces, family, prefix, limit)
    })?;
    drop(read);
    drop(reopened_keyspaces);
    drop(reopened);

    let mut metrics = finish_metrics(
        PackedPostingsBackend::Fjall,
        path,
        event_count,
        batch_size,
        wall_ns,
        totals,
        &commit_latencies,
        process,
        stored_bytes,
        reopened_rows,
        reopened_segment_rows,
        reopened_memberships,
        active_segment_bytes,
        active_dictionary_rows,
        active_dictionary_bytes,
        active_run_meta_rows,
        active_run_meta_bytes,
        deletion,
        maintenance,
        queries,
    )?;
    metrics.database_logical_bytes = logical_bytes;
    Ok(metrics)
}

#[derive(Default)]
struct RunTotals {
    transactions: u64,
    segment_build_ns: u64,
    packed_encode_ns: u64,
    packed_dictionary_build_ns: u64,
    packed_membership_sort_ns: u64,
    packed_segment_encode_ns: u64,
    commit_ns: u64,
    encoded_event_bytes: u64,
    segment_rows: u64,
    segment_bytes: u64,
    dictionary_rows: u64,
    dictionary_bytes: u64,
    run_meta_rows: u64,
    run_meta_bytes: u64,
    prefix_records: u64,
    packed_postings: u64,
    posting_bytes: u64,
    memberships: [u64; FAMILY_COUNT],
    segment_keys: Vec<[u8; 10]>,
}

#[derive(Default)]
struct MaintenanceMetrics {
    wall_ns: u64,
    process_write_bytes: Option<u64>,
    active_segment_rows: u64,
    active_memberships: [u64; FAMILY_COUNT],
}

#[derive(Default)]
struct DeletionMetrics {
    event_keys: BTreeSet<u64>,
    overlay_rows: u64,
    overlay_bytes: u64,
    wall_ns: u64,
    process_write_bytes: Option<u64>,
}

const COMPACTION_FAN_IN: usize = 8;
const COMPACTED_GENERATION_BIT: u64 = 1 << 63;
const DELETION_STRIDE: u64 = 8;

fn deletion_event_keys(events: &[Event]) -> BTreeSet<u64> {
    (1..=events.len() as u64)
        .filter(|event_key| event_key % DELETION_STRIDE == 0)
        .collect()
}

fn deletion_blocks(
    event_keys: &BTreeSet<u64>,
    batch_size: usize,
) -> Result<Vec<(u64, u64, Vec<u8>)>, String> {
    let mut grouped = BTreeMap::<u64, Vec<u64>>::new();
    for &event_key in event_keys {
        let generation = (event_key - 1) / batch_size as u64;
        grouped.entry(generation).or_default().push(event_key);
    }
    grouped
        .into_iter()
        .map(|(generation, keys)| Ok((generation, 0, encode_dead_keys(&keys)?)))
        .collect()
}

fn apply_redb_deletion_overlay(
    db: &Database,
    events: &[Event],
    batch_size: usize,
) -> Result<DeletionMetrics, String> {
    let started = Instant::now();
    let event_keys = deletion_event_keys(events);
    let blocks = deletion_blocks(&event_keys, batch_size)?;
    let overlay_rows = blocks.len() as u64;
    let overlay_bytes = blocks.iter().map(|(_, _, value)| value.len() as u64).sum();
    let write = db.begin_write().map_err(|error| error.to_string())?;
    let mut event_rows = write
        .open_table(EVENTS)
        .map_err(|error| error.to_string())?;
    let mut event_ids = write
        .open_table(EVENT_IDS)
        .map_err(|error| error.to_string())?;
    let mut observations = write
        .open_table(EVENT_OBSERVATIONS)
        .map_err(|error| error.to_string())?;
    let mut dead_key_table = write
        .open_table(PACKED_DEAD_KEYS)
        .map_err(|error| error.to_string())?;
    for &event_key in &event_keys {
        let event = &events[event_key as usize - 1];
        event_rows
            .remove(event_key)
            .map_err(|error| error.to_string())?;
        event_ids
            .remove(event.id.as_bytes())
            .map_err(|error| error.to_string())?;
        observations
            .remove(&observation_key(event_key, 1))
            .map_err(|error| error.to_string())?;
    }
    for (generation, sequence, value) in blocks {
        let key = dead_block_key(generation, sequence);
        dead_key_table
            .insert(key.as_slice(), value.as_slice())
            .map_err(|error| error.to_string())?;
    }
    drop(dead_key_table);
    drop(observations);
    drop(event_ids);
    drop(event_rows);
    write.commit().map_err(|error| error.to_string())?;
    Ok(DeletionMetrics {
        event_keys,
        overlay_rows,
        overlay_bytes,
        wall_ns: duration_ns(started),
        process_write_bytes: None,
    })
}

fn apply_fjall_deletion_overlay(
    database: &SingleWriterTxDatabase,
    keyspaces: &FjallKeyspaces,
    events: &[Event],
    batch_size: usize,
) -> Result<DeletionMetrics, String> {
    let started = Instant::now();
    let event_keys = deletion_event_keys(events);
    let blocks = deletion_blocks(&event_keys, batch_size)?;
    let overlay_rows = blocks.len() as u64;
    let overlay_bytes = blocks.iter().map(|(_, _, value)| value.len() as u64).sum();
    let mut write = database.write_tx().durability(Some(PersistMode::SyncAll));
    for &event_key in &event_keys {
        let event = &events[event_key as usize - 1];
        write.remove(&keyspaces.events, event_key.to_be_bytes());
        write.remove(&keyspaces.event_ids, event.id.as_bytes());
        write.remove(&keyspaces.observations, observation_key(event_key, 1));
    }
    for (generation, sequence, value) in blocks {
        write.insert(
            &keyspaces.dead_keys,
            dead_block_key(generation, sequence),
            value,
        );
    }
    write.commit().map_err(|error| error.to_string())?;
    Ok(DeletionMetrics {
        event_keys,
        overlay_rows,
        overlay_bytes,
        wall_ns: duration_ns(started),
        process_write_bytes: None,
    })
}

fn expected_active_memberships(
    events: &[Event],
    batch_size: usize,
    dead_keys: &BTreeSet<u64>,
) -> [u64; FAMILY_COUNT] {
    let total_batches = events.len().div_ceil(batch_size);
    let compacted_events = (total_batches / COMPACTION_FAN_IN)
        .saturating_mul(COMPACTION_FAN_IN)
        .saturating_mul(batch_size)
        .min(events.len());
    let mut counts = [0u64; FAMILY_COUNT];
    let mut ignored = BatchSegments::new();
    for (index, event) in events.iter().enumerate() {
        let event_key = index as u64 + 1;
        if index < compacted_events && dead_keys.contains(&event_key) {
            continue;
        }
        add_event_memberships(&mut ignored, &mut counts, event, event_key);
        ignored.clear();
    }
    counts
}

fn compact_redb_segments(
    db: &Database,
    events: &[Event],
    batch_size: usize,
    _initial_keys: &[[u8; 10]],
    dead_keys: &BTreeSet<u64>,
) -> Result<MaintenanceMetrics, String> {
    let started = Instant::now();
    compact_run_levels(
        events.len().div_ceil(batch_size),
        |level, output_run, sources| {
            let input = load_redb_compaction_input(db, sources)?;
            let run_id = compacted_generation(level, output_run);
            let encoded = if input.memberships.is_empty() {
                None
            } else {
                Some(encode_segments(input.memberships)?)
            };
            let meta = encoded
                .as_ref()
                .map(|encoded| {
                    RunMeta {
                        run_id,
                        level,
                        min_event_key: input.min_event_key,
                        max_event_key: input.max_event_key,
                        live_events: encoded.dictionary_entries,
                    }
                    .encode()
                })
                .transpose()?;
            let write = db.begin_write().map_err(|error| error.to_string())?;
            let mut segments = write
                .open_table(PACKED_SEGMENTS)
                .map_err(|error| error.to_string())?;
            let mut dictionaries = write
                .open_table(PACKED_DICTIONARIES)
                .map_err(|error| error.to_string())?;
            let mut run_meta = write
                .open_table(PACKED_RUN_META)
                .map_err(|error| error.to_string())?;
            let mut dead_key_table = write
                .open_table(PACKED_DEAD_KEYS)
                .map_err(|error| error.to_string())?;
            for source in sources {
                for family in Family::ALL {
                    let last_shard = if family == Family::Global {
                        0
                    } else {
                        postings::SHARD_MASK
                    };
                    for shard in 0..=last_shard {
                        let key = segment_key(family, shard, *source);
                        segments
                            .remove(key.as_slice())
                            .map_err(|error| error.to_string())?;
                    }
                }
                for sequence in 0..postings::MAX_DEATH_BLOCKS as u64 {
                    let key = dead_block_key(*source, sequence);
                    dead_key_table
                        .remove(key.as_slice())
                        .map_err(|error| error.to_string())?;
                }
                dictionaries
                    .remove(*source)
                    .map_err(|error| error.to_string())?;
                run_meta
                    .remove(*source)
                    .map_err(|error| error.to_string())?;
            }
            if let Some(encoded) = encoded {
                dictionaries
                    .insert(run_id, encoded.dictionary.as_slice())
                    .map_err(|error| error.to_string())?;
                run_meta
                    .insert(
                        run_id,
                        meta.as_ref().expect("live run has metadata").as_slice(),
                    )
                    .map_err(|error| error.to_string())?;
                for (family, shard, value) in encoded.segments {
                    let key = segment_key(family, shard, run_id);
                    segments
                        .insert(key.as_slice(), value.as_slice())
                        .map_err(|error| error.to_string())?;
                }
            }
            drop(dead_key_table);
            drop(run_meta);
            drop(dictionaries);
            drop(segments);
            write.commit().map_err(|error| error.to_string())?;
            Ok(meta.is_some())
        },
    )?;
    let read = db.begin_read().map_err(|error| error.to_string())?;
    let active_segment_rows = read
        .open_table(PACKED_SEGMENTS)
        .map_err(|error| error.to_string())?
        .len()
        .map_err(|error| error.to_string())?;
    Ok(MaintenanceMetrics {
        wall_ns: duration_ns(started),
        process_write_bytes: None,
        active_segment_rows,
        active_memberships: expected_active_memberships(events, batch_size, dead_keys),
    })
}

fn compact_fjall_segments(
    database: &SingleWriterTxDatabase,
    keyspaces: &FjallKeyspaces,
    events: &[Event],
    batch_size: usize,
    _initial_keys: &[[u8; 10]],
    dead_keys: &BTreeSet<u64>,
) -> Result<MaintenanceMetrics, String> {
    let started = Instant::now();
    compact_run_levels(
        events.len().div_ceil(batch_size),
        |level, output_run, sources| {
            let input = load_fjall_compaction_input(database, keyspaces, sources)?;
            let run_id = compacted_generation(level, output_run);
            let encoded = if input.memberships.is_empty() {
                None
            } else {
                Some(encode_segments(input.memberships)?)
            };
            let meta = encoded
                .as_ref()
                .map(|encoded| {
                    RunMeta {
                        run_id,
                        level,
                        min_event_key: input.min_event_key,
                        max_event_key: input.max_event_key,
                        live_events: encoded.dictionary_entries,
                    }
                    .encode()
                })
                .transpose()?;
            let read = database.read_tx();
            let mut removed = Vec::new();
            let mut removed_death_blocks = Vec::new();
            for source in sources {
                for family in Family::ALL {
                    let last_shard = if family == Family::Global {
                        0
                    } else {
                        postings::SHARD_MASK
                    };
                    for shard in 0..=last_shard {
                        let key = segment_key(family, shard, *source);
                        if read
                            .get(&keyspaces.segments, key)
                            .map_err(|error| error.to_string())?
                            .is_some()
                        {
                            removed.push(key);
                        }
                    }
                }
                for entry in read.prefix(&keyspaces.dead_keys, source.to_be_bytes()) {
                    let (key, _) = entry.into_inner().map_err(|error| error.to_string())?;
                    let key: [u8; 16] = key
                        .as_ref()
                        .try_into()
                        .map_err(|_| "invalid Fjall dead-block key width".to_owned())?;
                    removed_death_blocks.push(key);
                }
            }
            drop(read);
            let mut write = database.write_tx().durability(Some(PersistMode::SyncAll));
            for key in removed {
                write.remove(&keyspaces.segments, key);
            }
            for key in removed_death_blocks {
                write.remove(&keyspaces.dead_keys, key);
            }
            for source in sources {
                write.remove(&keyspaces.dictionaries, source.to_be_bytes());
                write.remove(&keyspaces.run_meta, source.to_be_bytes());
            }
            if let Some(encoded) = encoded {
                write.insert(
                    &keyspaces.dictionaries,
                    run_id.to_be_bytes(),
                    encoded.dictionary,
                );
                write.insert(
                    &keyspaces.run_meta,
                    run_id.to_be_bytes(),
                    meta.as_ref().expect("live run has metadata"),
                );
                for (family, shard, value) in encoded.segments {
                    let key = segment_key(family, shard, run_id);
                    write.insert(&keyspaces.segments, key, value);
                }
            }
            write.commit().map_err(|error| error.to_string())?;
            Ok(meta.is_some())
        },
    )?;
    let read = database.read_tx();
    let active_segment_rows = read
        .len(&keyspaces.segments)
        .map_err(|error| error.to_string())? as u64;
    Ok(MaintenanceMetrics {
        wall_ns: duration_ns(started),
        process_write_bytes: None,
        active_segment_rows,
        active_memberships: expected_active_memberships(events, batch_size, dead_keys),
    })
}

struct CompactionInput {
    memberships: Vec<Membership>,
    min_event_key: u64,
    max_event_key: u64,
}

fn compact_run_levels(
    total_batches: usize,
    mut compact: impl FnMut(u8, usize, &BTreeSet<u64>) -> Result<bool, String>,
) -> Result<(), String> {
    let mut source_runs: Vec<_> = (0..total_batches as u64).collect();
    let mut level = 1u8;
    while source_runs.len() >= COMPACTION_FAN_IN {
        let mut output_runs = Vec::with_capacity(source_runs.len() / COMPACTION_FAN_IN);
        for (output_run, source_chunk) in source_runs
            .as_chunks::<COMPACTION_FAN_IN>()
            .0
            .iter()
            .enumerate()
        {
            let source_generations = source_chunk.iter().copied().collect();
            if compact(level, output_run, &source_generations)? {
                output_runs.push(compacted_generation(level, output_run));
            }
        }
        source_runs = output_runs;
        level = level
            .checked_add(1)
            .ok_or_else(|| "compaction level overflow".to_owned())?;
    }
    Ok(())
}

fn load_redb_compaction_input(
    db: &Database,
    sources: &BTreeSet<u64>,
) -> Result<CompactionInput, String> {
    let read = db.begin_read().map_err(|error| error.to_string())?;
    let segments = read
        .open_table(PACKED_SEGMENTS)
        .map_err(|error| error.to_string())?;
    let dictionaries = read
        .open_table(PACKED_DICTIONARIES)
        .map_err(|error| error.to_string())?;
    let run_meta = read
        .open_table(PACKED_RUN_META)
        .map_err(|error| error.to_string())?;
    let dead_keys = read
        .open_table(PACKED_DEAD_KEYS)
        .map_err(|error| error.to_string())?;
    let mut memberships = Vec::new();
    let mut metas = Vec::new();
    for source in sources {
        let meta = run_meta
            .get(*source)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("missing source run metadata {source}"))?;
        let meta = RunMeta::decode(meta.value())?;
        if meta.run_id != *source {
            return Err("source run metadata id mismatch".to_owned());
        }
        metas.push(meta);
    }
    validate_contiguous_sources(&metas)?;
    let source_ids: Vec<_> = sources.iter().copied().collect();
    let mut dictionary_values = Vec::with_capacity(source_ids.len());
    let mut deaths = Vec::with_capacity(source_ids.len());
    for source in &source_ids {
        dictionary_values.push(
            dictionaries
                .get(*source)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("missing source run dictionary {source}"))?,
        );
        deaths.push(load_redb_run_deaths(&dead_keys, *source)?);
    }
    let dictionaries: Vec<_> = dictionary_values
        .iter()
        .map(|value| DictionaryView::parse(value.value())?.validate())
        .collect::<Result<_, String>>()?;
    for (source_index, source) in source_ids.iter().enumerate() {
        for family in Family::ALL {
            let last_shard = if family == Family::Global {
                0
            } else {
                postings::SHARD_MASK
            };
            for shard in 0..=last_shard {
                let key = segment_key(family, shard, *source);
                let Some(value) = segments
                    .get(key.as_slice())
                    .map_err(|error| error.to_string())?
                else {
                    continue;
                };
                let segment = SegmentView::parse(value.value())?;
                for membership in segment.memberships(dictionaries[source_index])? {
                    if deaths[source_index]
                        .as_ref()
                        .is_none_or(|keys| !keys.contains(membership.event.event_key))
                    {
                        memberships.push(membership);
                    }
                }
            }
        }
    }
    compaction_input(memberships, &metas)
}

fn load_fjall_compaction_input(
    database: &SingleWriterTxDatabase,
    keyspaces: &FjallKeyspaces,
    sources: &BTreeSet<u64>,
) -> Result<CompactionInput, String> {
    let read = database.read_tx();
    let mut metas = Vec::new();
    for source in sources {
        let value = read
            .get(&keyspaces.run_meta, source.to_be_bytes())
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("missing source run metadata {source}"))?;
        let meta = RunMeta::decode(&value)?;
        if meta.run_id != *source {
            return Err("source run metadata id mismatch".to_owned());
        }
        metas.push(meta);
    }
    validate_contiguous_sources(&metas)?;
    let source_ids: Vec<_> = sources.iter().copied().collect();
    let mut dictionary_values = Vec::with_capacity(source_ids.len());
    let mut deaths = Vec::with_capacity(source_ids.len());
    for source in &source_ids {
        dictionary_values.push(
            read.get(&keyspaces.dictionaries, source.to_be_bytes())
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("missing source run dictionary {source}"))?,
        );
        deaths.push(load_fjall_run_deaths(&read, keyspaces, *source)?);
    }
    let dictionaries: Vec<_> = dictionary_values
        .iter()
        .map(|value| DictionaryView::parse(value)?.validate())
        .collect::<Result<_, String>>()?;
    let mut memberships = Vec::new();
    for (source_index, source) in source_ids.iter().enumerate() {
        for family in Family::ALL {
            let last_shard = if family == Family::Global {
                0
            } else {
                postings::SHARD_MASK
            };
            for shard in 0..=last_shard {
                let key = segment_key(family, shard, *source);
                let Some(value) = read
                    .get(&keyspaces.segments, key)
                    .map_err(|error| error.to_string())?
                else {
                    continue;
                };
                let segment = SegmentView::parse(&value)?;
                for membership in segment.memberships(dictionaries[source_index])? {
                    if deaths[source_index]
                        .as_ref()
                        .is_none_or(|keys| !keys.contains(membership.event.event_key))
                    {
                        memberships.push(membership);
                    }
                }
            }
        }
    }
    compaction_input(memberships, &metas)
}

fn validate_contiguous_sources(metas: &[RunMeta]) -> Result<(), String> {
    validate_run_metas(metas)?;
    let mut ordered = metas.to_vec();
    ordered.sort_unstable_by_key(|meta| meta.min_event_key);
    if ordered
        .windows(2)
        .any(|pair| pair[0].max_event_key.checked_add(1) != Some(pair[1].min_event_key))
    {
        return Err("compaction source run ranges are not contiguous".to_owned());
    }
    Ok(())
}

fn compaction_input(
    memberships: Vec<Membership>,
    metas: &[RunMeta],
) -> Result<CompactionInput, String> {
    Ok(CompactionInput {
        memberships,
        min_event_key: metas
            .iter()
            .map(|meta| meta.min_event_key)
            .min()
            .ok_or_else(|| "compaction has no source metadata".to_owned())?,
        max_event_key: metas
            .iter()
            .map(|meta| meta.max_event_key)
            .max()
            .ok_or_else(|| "compaction has no source metadata".to_owned())?,
    })
}

fn compacted_generation(level: u8, run: usize) -> u64 {
    COMPACTED_GENERATION_BIT | (u64::from(level) << 56) | run as u64
}

fn segment_generation(key: &[u8; 10]) -> u64 {
    u64::from_be_bytes(key[2..].try_into().expect("fixed segment generation width"))
}

#[allow(clippy::too_many_arguments)]
fn finish_metrics(
    backend: PackedPostingsBackend,
    path: &Path,
    event_count: u64,
    batch_size: usize,
    wall_ns: u64,
    totals: RunTotals,
    commit_latencies: &[u64],
    process: StoreBenchProcessCounters,
    stored_bytes: u64,
    reopened_rows: u64,
    reopened_segment_rows: u64,
    reopened_memberships: [u64; FAMILY_COUNT],
    active_segment_bytes: u64,
    active_dictionary_rows: u64,
    active_dictionary_bytes: u64,
    active_run_meta_rows: u64,
    active_run_meta_bytes: u64,
    deletion: DeletionMetrics,
    maintenance: MaintenanceMetrics,
    queries: Vec<PackedQueryMetrics>,
) -> Result<PackedPostingsMetrics, String> {
    let exact_reopen = reopened_rows
        == event_count.saturating_sub(deletion.event_keys.len() as u64)
        && reopened_segment_rows == maintenance.active_segment_rows
        && reopened_memberships == maintenance.active_memberships;
    Ok(PackedPostingsMetrics {
        backend,
        events: event_count,
        transaction_batch_size: batch_size,
        transactions: totals.transactions,
        wall_ns,
        segment_build_ns: totals.segment_build_ns,
        packed_encode_ns: totals.packed_encode_ns,
        packed_dictionary_build_ns: totals.packed_dictionary_build_ns,
        packed_membership_sort_ns: totals.packed_membership_sort_ns,
        packed_segment_encode_ns: totals.packed_segment_encode_ns,
        commit_ns: totals.commit_ns,
        commit_p50_ns: nearest_rank(commit_latencies, 50).unwrap_or(0),
        commit_p95_ns: nearest_rank(commit_latencies, 95).unwrap_or(0),
        commit_p99_ns: nearest_rank(commit_latencies, 99).unwrap_or(0),
        deletion_events: deletion.event_keys.len() as u64,
        deletion_overlay_rows: deletion.overlay_rows,
        deletion_overlay_bytes: deletion.overlay_bytes,
        deletion_ns: deletion.wall_ns,
        deletion_process_write_bytes: deletion.process_write_bytes,
        maintenance_ns: maintenance.wall_ns,
        maintenance_process_write_bytes: maintenance.process_write_bytes,
        cpu_ns: process.cpu_ns,
        allocation_ops: process.allocation_ops,
        allocated_bytes: process.allocated_bytes,
        rss_before_bytes: process.current_rss_bytes,
        peak_rss_bytes: process.peak_rss_bytes,
        process_write_bytes: process.process_write_bytes,
        encoded_event_bytes: totals.encoded_event_bytes,
        segment_rows: totals.segment_rows,
        segment_bytes: totals.segment_bytes,
        dictionary_rows: totals.dictionary_rows,
        dictionary_bytes: totals.dictionary_bytes,
        run_meta_rows: totals.run_meta_rows,
        run_meta_bytes: totals.run_meta_bytes,
        prefix_records: totals.prefix_records,
        packed_postings: totals.packed_postings,
        posting_bytes: totals.posting_bytes,
        seek_directory_bytes: 0,
        active_segment_rows: maintenance.active_segment_rows,
        active_segment_bytes,
        active_dictionary_rows,
        active_dictionary_bytes,
        active_run_meta_rows,
        active_run_meta_bytes,
        memberships: totals.memberships,
        database_logical_bytes: path_size(path).map_err(|error| error.to_string())?,
        database_stored_bytes: stored_bytes,
        reopened_rows,
        reopened_memberships,
        exact_reopen,
        queries,
    })
}

const QUERY_ITERATIONS: usize = 50;
const QUERY_LIMIT: usize = 200;

struct QueryRequest {
    name: &'static str,
    family: Family,
    prefix: Vec<u8>,
    limit: usize,
    expected: Vec<u64>,
}

fn run_query_benchmarks(
    events: &[Event],
    dead_keys: &BTreeSet<u64>,
    mut query: impl FnMut(Family, &[u8], usize) -> Result<Vec<u64>, String>,
) -> Result<Vec<PackedQueryMetrics>, String> {
    let requests = representative_query_requests(events, dead_keys)?;
    let mut metrics = Vec::with_capacity(requests.len());
    for request in requests {
        let mut latencies = Vec::with_capacity(QUERY_ITERATIONS);
        let mut exact = true;
        for _ in 0..QUERY_ITERATIONS {
            let started = Instant::now();
            let rows = query(request.family, &request.prefix, request.limit)?;
            latencies.push(duration_ns(started));
            exact &= rows == request.expected;
        }
        if !exact {
            return Err(format!(
                "packed query {} disagrees with corpus oracle",
                request.name
            ));
        }
        metrics.push(PackedQueryMetrics {
            name: request.name.to_owned(),
            returned_rows: request.expected.len() as u64,
            iterations: QUERY_ITERATIONS as u64,
            p50_ns: nearest_rank(&latencies, 50).unwrap_or(0),
            p95_ns: nearest_rank(&latencies, 95).unwrap_or(0),
            p99_ns: nearest_rank(&latencies, 99).unwrap_or(0),
            exact,
        });
    }
    Ok(metrics)
}

fn representative_query_requests(
    events: &[Event],
    dead_keys: &BTreeSet<u64>,
) -> Result<Vec<QueryRequest>, String> {
    let first = events
        .first()
        .ok_or_else(|| "query corpus must not be empty".to_owned())?;
    let mut kind_counts = BTreeMap::<[u8; 2], u64>::new();
    let mut tag_counts = BTreeMap::<Vec<u8>, u64>::new();
    for event in events {
        *kind_counts
            .entry(event.kind.as_u16().to_be_bytes())
            .or_default() += 1;
        let mut unique_tags = BTreeSet::new();
        for tag in event.tags.iter() {
            let (Some(letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
                continue;
            };
            unique_tags.insert(tag_index_prefix(letter, value));
        }
        for prefix in unique_tags {
            *tag_counts.entry(prefix).or_default() += 1;
        }
    }
    let busiest_kind = kind_counts
        .into_iter()
        .max_by(|(left_prefix, left_count), (right_prefix, right_count)| {
            left_count
                .cmp(right_count)
                .then_with(|| right_prefix.cmp(left_prefix))
        })
        .map(|(prefix, _)| prefix.to_vec())
        .ok_or_else(|| "query corpus has no kind".to_owned())?;
    let busiest_tag = tag_counts
        .into_iter()
        .max_by(|(left_prefix, left_count), (right_prefix, right_count)| {
            left_count
                .cmp(right_count)
                .then_with(|| right_prefix.cmp(left_prefix))
        })
        .map(|(prefix, _)| prefix)
        .ok_or_else(|| "query corpus has no indexed tag".to_owned())?;

    let shapes = [
        ("global_newest_200", Family::Global, Vec::new(), QUERY_LIMIT),
        (
            "first_author",
            Family::Author,
            first.pubkey.as_bytes().to_vec(),
            QUERY_LIMIT,
        ),
        ("busiest_kind_200", Family::Kind, busiest_kind, QUERY_LIMIT),
        ("busiest_tag_200", Family::Tag, busiest_tag, QUERY_LIMIT),
    ];
    Ok(shapes
        .into_iter()
        .map(|(name, family, prefix, limit)| QueryRequest {
            name,
            expected: expected_query_keys(events, dead_keys, family, &prefix, limit),
            family,
            prefix,
            limit,
        })
        .collect())
}

fn expected_query_keys(
    events: &[Event],
    dead_keys: &BTreeSet<u64>,
    family: Family,
    prefix: &[u8],
    limit: usize,
) -> Vec<u64> {
    let mut rows: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(index, _)| !dead_keys.contains(&(*index as u64 + 1)))
        .filter(|(_, event)| event_matches_prefix(event, family, prefix))
        .map(|(index, event)| RunEvent {
            created_at: event.created_at.as_secs(),
            id: *event.id.as_bytes(),
            event_key: index as u64 + 1,
        })
        .collect();
    rows.sort_unstable_by(posting_order);
    rows.truncate(limit);
    rows.into_iter().map(|row| row.event_key).collect()
}

fn event_matches_prefix(event: &Event, family: Family, prefix: &[u8]) -> bool {
    match family {
        Family::Global => true,
        Family::Author => event.pubkey.as_bytes() == prefix,
        Family::Kind => event.kind.as_u16().to_be_bytes() == prefix,
        Family::Tag => event.tags.iter().any(|tag| {
            let (Some(letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
                return false;
            };
            tag_index_prefix(letter, value) == prefix
        }),
    }
}

fn decode_redb_dead_keys(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
) -> Result<BTreeSet<u64>, String> {
    let mut dead_keys = BTreeSet::new();
    for entry in table.iter().map_err(|error| error.to_string())? {
        let (_block_key, value) = entry.map_err(|error| error.to_string())?;
        dead_keys.extend(decode_dead_keys(value.value())?);
    }
    Ok(dead_keys)
}

fn decode_fjall_dead_keys(
    read: &fjall::Snapshot,
    keyspaces: &FjallKeyspaces,
) -> Result<BTreeSet<u64>, String> {
    let mut dead_keys = BTreeSet::new();
    for entry in read.iter(&keyspaces.dead_keys) {
        let (_block_key, value) = entry.into_inner().map_err(|error| error.to_string())?;
        dead_keys.extend(decode_dead_keys(&value)?);
    }
    Ok(dead_keys)
}

fn query_redb(
    read: &redb::ReadTransaction,
    family: Family,
    prefix: &[u8],
    limit: usize,
) -> Result<Vec<u64>, String> {
    let shard = shard_for(family, prefix);
    let lower = segment_key(family, shard, 0);
    let upper = segment_key(family, shard, u64::MAX);
    let segments = read
        .open_table(PACKED_SEGMENTS)
        .map_err(|error| error.to_string())?;
    let dictionaries = read
        .open_table(PACKED_DICTIONARIES)
        .map_err(|error| error.to_string())?;
    let dead_keys = read
        .open_table(PACKED_DEAD_KEYS)
        .map_err(|error| error.to_string())?;
    let mut run_ids = Vec::new();
    let mut segment_values = Vec::new();
    for entry in segments
        .range(lower.as_slice()..=upper.as_slice())
        .map_err(|error| error.to_string())?
    {
        let (key, value) = entry.map_err(|error| error.to_string())?;
        let run_id = segment_generation(
            key.value()
                .try_into()
                .map_err(|_| "invalid Redb segment key width".to_owned())?,
        );
        run_ids.push(run_id);
        segment_values.push(value);
    }
    let mut dictionary_values = Vec::with_capacity(run_ids.len());
    let mut decoded_dead_keys = Vec::with_capacity(run_ids.len());
    for run_id in &run_ids {
        dictionary_values.push(
            dictionaries
                .get(*run_id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("missing run dictionary {run_id}"))?,
        );
        decoded_dead_keys.push(load_redb_run_deaths(&dead_keys, *run_id)?);
    }
    let mut sources = Vec::new();
    for index in 0..run_ids.len() {
        let dictionary = DictionaryView::parse(dictionary_values[index].value())?;
        let segment = SegmentView::parse(segment_values[index].value())?;
        if segment.family != family || segment.shard != shard {
            return Err("packed query opened the wrong family or shard".to_owned());
        }
        if let Some(list) = segment.prefix(prefix)? {
            sources.push(MergeSource {
                cursor: list.cursor(dictionary, None, 0, u64::MAX)?,
                dead: decoded_dead_keys[index].as_ref(),
            });
        }
    }
    Ok(merge_posting_cursors(sources, limit)?
        .into_iter()
        .map(|event| event.event_key)
        .collect())
}

fn query_fjall(
    read: &fjall::Snapshot,
    keyspaces: &FjallKeyspaces,
    family: Family,
    prefix: &[u8],
    limit: usize,
) -> Result<Vec<u64>, String> {
    let shard = shard_for(family, prefix);
    let key_prefix = [family as u8, shard];
    let mut runs = Vec::new();
    for entry in read.prefix(&keyspaces.segments, key_prefix) {
        let (key, value) = entry.into_inner().map_err(|error| error.to_string())?;
        let key: [u8; 10] = key
            .as_ref()
            .try_into()
            .map_err(|_| "invalid Fjall segment key width".to_owned())?;
        let run_id = segment_generation(&key);
        let dictionary = read
            .get(&keyspaces.dictionaries, run_id.to_be_bytes())
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("missing run dictionary {run_id}"))?;
        let dead = load_fjall_run_deaths(read, keyspaces, run_id)?;
        runs.push(OwnedQueryRun {
            dictionary: dictionary.to_vec(),
            segment: value.to_vec(),
            dead,
        });
    }
    query_owned_runs(&runs, family, shard, prefix, limit)
}

struct OwnedQueryRun {
    dictionary: Vec<u8>,
    segment: Vec<u8>,
    dead: Option<DeadKeys>,
}

fn query_owned_runs(
    runs: &[OwnedQueryRun],
    family: Family,
    shard: u8,
    prefix: &[u8],
    limit: usize,
) -> Result<Vec<u64>, String> {
    let mut sources = Vec::new();
    for run in runs {
        let dictionary = DictionaryView::parse(&run.dictionary)?;
        let segment = SegmentView::parse(&run.segment)?;
        if segment.family != family || segment.shard != shard {
            return Err("packed query opened the wrong family or shard".to_owned());
        }
        if let Some(list) = segment.prefix(prefix)? {
            sources.push(MergeSource {
                cursor: list.cursor(dictionary, None, 0, u64::MAX)?,
                dead: run.dead.as_ref(),
            });
        }
    }
    Ok(merge_posting_cursors(sources, limit)?
        .into_iter()
        .map(|event| event.event_key)
        .collect())
}

fn add_event_memberships(
    segments: &mut BatchSegments,
    counts: &mut [u64; FAMILY_COUNT],
    event: &Event,
    event_key: u64,
) {
    let posting = Arc::new(RunEvent {
        created_at: event.created_at.as_secs(),
        id: *event.id.as_bytes(),
        event_key,
    });
    push_membership(segments, counts, Family::Global, Prefix::global(), &posting);
    push_membership(
        segments,
        counts,
        Family::Author,
        Prefix::author(*event.pubkey.as_bytes()),
        &posting,
    );
    push_membership(
        segments,
        counts,
        Family::Kind,
        Prefix::kind(event.kind.as_u16().to_be_bytes()),
        &posting,
    );
    let mut tag_prefixes = BTreeSet::new();
    for tag in event.tags.iter() {
        let (Some(letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
            continue;
        };
        tag_prefixes.insert(tag_index_prefix(letter, value));
    }
    for prefix in tag_prefixes {
        push_membership(
            segments,
            counts,
            Family::Tag,
            Prefix::tag(prefix.into()),
            &posting,
        );
    }
}

fn push_membership(
    segments: &mut BatchSegments,
    counts: &mut [u64; FAMILY_COUNT],
    family: Family,
    prefix: Prefix,
    event: &Arc<RunEvent>,
) {
    let shard = shard_for(family, prefix.as_bytes());
    segments.push(Membership {
        family,
        shard,
        prefix,
        event: event.clone(),
    });
    counts[family as usize] = counts[family as usize].saturating_add(1);
}

fn posting_order(left: &RunEvent, right: &RunEvent) -> std::cmp::Ordering {
    right
        .created_at
        .cmp(&left.created_at)
        .then_with(|| left.id.cmp(&right.id))
        .then_with(|| left.event_key.cmp(&right.event_key))
}

fn encode_segments(memberships: BatchSegments) -> Result<EncodedRun, String> {
    postings::encode_run(memberships)
}

fn encode_dead_keys(event_keys: &[u64]) -> Result<Vec<u8>, String> {
    DeadKeys::new(event_keys.to_vec())?.encode()
}

fn decode_dead_keys(value: &[u8]) -> Result<Vec<u64>, String> {
    Ok(DeadKeys::decode(value)?.iter().collect())
}

fn segment_key(family: Family, shard: u8, generation: u64) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = family as u8;
    key[1] = shard;
    key[2..].copy_from_slice(&generation.to_be_bytes());
    key
}

fn dead_block_key(run_id: u64, sequence: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&run_id.to_be_bytes());
    key[8..].copy_from_slice(&sequence.to_be_bytes());
    key
}

fn dead_block_run(key: &[u8; 16]) -> u64 {
    u64::from_be_bytes(key[..8].try_into().expect("dead-block run width"))
}

fn load_redb_run_deaths(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    run_id: u64,
) -> Result<Option<DeadKeys>, String> {
    let lower = dead_block_key(run_id, 0);
    let upper = dead_block_key(run_id, u64::MAX);
    let mut blocks = Vec::new();
    for entry in table
        .range(lower.as_slice()..=upper.as_slice())
        .map_err(|error| error.to_string())?
    {
        let (key, value) = entry.map_err(|error| error.to_string())?;
        let key: [u8; 16] = key
            .value()
            .try_into()
            .map_err(|_| "invalid Redb dead-block key width".to_owned())?;
        if dead_block_run(&key) != run_id {
            return Err("Redb dead-block range crossed run boundary".to_owned());
        }
        blocks.push(DeadKeys::decode(value.value())?);
    }
    merge_dead_blocks(&blocks)
}

fn load_fjall_run_deaths(
    read: &fjall::Snapshot,
    keyspaces: &FjallKeyspaces,
    run_id: u64,
) -> Result<Option<DeadKeys>, String> {
    let mut blocks = Vec::new();
    for entry in read.prefix(&keyspaces.dead_keys, run_id.to_be_bytes()) {
        let (key, value) = entry.into_inner().map_err(|error| error.to_string())?;
        let key: [u8; 16] = key
            .as_ref()
            .try_into()
            .map_err(|_| "invalid Fjall dead-block key width".to_owned())?;
        if dead_block_run(&key) != run_id {
            return Err("Fjall dead-block prefix crossed run boundary".to_owned());
        }
        blocks.push(DeadKeys::decode(&value)?);
    }
    merge_dead_blocks(&blocks)
}

fn observed_at(events: &[Event]) -> u64 {
    events
        .iter()
        .map(|event| event.created_at.as_secs())
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

fn first_event_key(batch_index: usize, batch_size: usize) -> Result<u64, String> {
    batch_index
        .checked_mul(batch_size)
        .and_then(|value| value.checked_add(1))
        .map(|value| value as u64)
        .ok_or_else(|| "event key overflow".to_owned())
}

fn path_size(path: &Path) -> std::io::Result<u64> {
    if path.is_dir() {
        directory_bytes(path)
    } else {
        Ok(std::fs::metadata(path)?.len())
    }
}

fn directory_bytes(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        total = total.saturating_add(if metadata.is_dir() {
            directory_bytes(&entry.path())?
        } else {
            metadata.len()
        });
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

    use super::*;

    #[test]
    fn segment_roundtrip_preserves_equal_timestamp_order_and_counts() {
        let postings = [
            RunEvent {
                created_at: 10,
                id: [3; 32],
                event_key: 3,
            },
            RunEvent {
                created_at: 11,
                id: [9; 32],
                event_key: 1,
            },
            RunEvent {
                created_at: 10,
                id: [1; 32],
                event_key: 2,
            },
        ];
        let mut memberships: Vec<_> = postings
            .into_iter()
            .map(|event| Membership {
                family: Family::Tag,
                shard: shard_for(Family::Tag, b"prefix"),
                prefix: Prefix::tag(b"prefix".as_slice().into()),
                event: event.into(),
            })
            .collect();
        memberships.sort_unstable_by(|left, right| posting_order(&left.event, &right.event));
        let encoded = encode_segments(memberships).unwrap();
        let dictionary = DictionaryView::parse(&encoded.dictionary).unwrap();
        let (_, _, segment) = &encoded.segments[0];
        assert_eq!(
            SegmentView::parse(segment)
                .unwrap()
                .validate(dictionary)
                .unwrap(),
            3
        );
    }

    #[test]
    fn duplicate_tag_membership_is_stored_once() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "hello")
            .tags([Tag::hashtag("nmp"), Tag::hashtag("nmp")])
            .custom_created_at(Timestamp::from(10))
            .sign_with_keys(&keys)
            .unwrap();
        let mut segments = BatchSegments::new();
        let mut counts = [0; FAMILY_COUNT];
        add_event_memberships(&mut segments, &mut counts, &event, 1);
        assert_eq!(counts, [1, 1, 1, 1]);
    }

    #[test]
    fn decoder_rejects_trailing_bytes() {
        let membership = Membership {
            family: Family::Global,
            shard: 0,
            prefix: Prefix::global(),
            event: RunEvent {
                created_at: 1,
                id: [0; 32],
                event_key: 1,
            }
            .into(),
        };
        let encoded = encode_segments(vec![membership]).unwrap();
        let dictionary = DictionaryView::parse(&encoded.dictionary).unwrap();
        let mut segment = encoded.segments[0].2.clone();
        segment.push(0);
        assert!(SegmentView::parse(&segment)
            .and_then(|view| view.validate(dictionary))
            .is_err());
    }

    #[test]
    fn dead_key_block_roundtrip_is_delta_encoded_and_exact() {
        let keys = [8, 16, 24, 4_096];
        let encoded = encode_dead_keys(&keys).unwrap();
        assert_eq!(decode_dead_keys(&encoded).unwrap(), keys);
        assert!(encoded.len() < 8 + keys.len() * std::mem::size_of::<u64>());
    }

    #[test]
    fn fully_dead_compaction_cohort_is_valid_and_emits_no_followup_run() {
        let input = compaction_input(
            Vec::new(),
            &[
                RunMeta {
                    run_id: 1,
                    level: 0,
                    min_event_key: 1,
                    max_event_key: 4,
                    live_events: 4,
                },
                RunMeta {
                    run_id: 2,
                    level: 0,
                    min_event_key: 5,
                    max_event_key: 8,
                    live_events: 4,
                },
            ],
        )
        .unwrap();
        assert!(input.memberships.is_empty());
        assert_eq!((input.min_event_key, input.max_event_key), (1, 8));

        let mut cohorts = Vec::new();
        compact_run_levels(COMPACTION_FAN_IN, |level, output, sources| {
            cohorts.push((level, output, sources.clone()));
            Ok(false)
        })
        .unwrap();
        assert_eq!(cohorts.len(), 1);
        assert_eq!(cohorts[0].0, 1);
        assert_eq!(cohorts[0].2, (0..COMPACTION_FAN_IN as u64).collect());
    }

    #[test]
    fn packed_postings_crash_worker() {
        let Ok(path) = std::env::var("NMP_PACKED_POSTINGS_CRASH_PATH") else {
            return;
        };
        let db = Database::create(path).unwrap();
        let init = db.begin_write().unwrap();
        init.open_table(EVENTS).unwrap();
        init.open_table(PACKED_SEGMENTS).unwrap();
        init.commit().unwrap();

        let committed = db.begin_write().unwrap();
        committed
            .open_table(EVENTS)
            .unwrap()
            .insert(1, &[1][..])
            .unwrap();
        committed
            .open_table(PACKED_SEGMENTS)
            .unwrap()
            .insert(&[0, 0, 0][..], &[1][..])
            .unwrap();
        committed.commit().unwrap();

        let staged = db.begin_write().unwrap();
        staged
            .open_table(EVENTS)
            .unwrap()
            .insert(2, &[2][..])
            .unwrap();
        staged
            .open_table(PACKED_SEGMENTS)
            .unwrap()
            .insert(&[0, 0, 1][..], &[2][..])
            .unwrap();
        unsafe { libc::_exit(73) }
    }

    #[test]
    fn committed_segment_survives_abrupt_exit_and_staged_generation_does_not() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("packed-crash.redb");
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("redb_store::packed_postings_bench::tests::packed_postings_crash_worker")
            .arg("--nocapture")
            .env("NMP_PACKED_POSTINGS_CRASH_PATH", &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(73));

        let reopened = Database::open(path).unwrap();
        let read = reopened.begin_read().unwrap();
        let events = read.open_table(EVENTS).unwrap();
        let segments = read.open_table(PACKED_SEGMENTS).unwrap();
        assert_eq!(events.len().unwrap(), 1);
        assert_eq!(segments.len().unwrap(), 1);
        assert_eq!(events.get(1).unwrap().unwrap().value(), &[1]);
        assert!(events.get(2).unwrap().is_none());
        assert!(segments.get(&[0, 0, 0][..]).unwrap().is_some());
        assert!(segments.get(&[0, 0, 1][..]).unwrap().is_none());
    }
}
