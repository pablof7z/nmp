//! Benchmark-only packed ordered-postings prototype for issue #648.
//!
//! The production store writes one database row per event/index membership.
//! This prototype keeps canonical events, raw-id lookup, and provenance in
//! their existing physical shape, but publishes the four query indexes as
//! immutable transaction-generation segments. Each exact prefix is stored
//! once per shard/transaction and its postings retain exact
//! `created_at DESC, event_id ASC` order. Redb and Fjall consume byte-identical
//! segment values; this module is evidence, not a production backend.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

use fjall::{
    KeyspaceCreateOptions, PersistMode, Readable, SingleWriterTxDatabase, SingleWriterTxKeyspace,
};
use nostr::{Event, RelayUrl};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

use super::canonical::observation_key;
use super::query::tag_index_prefix;
use super::schema::{
    EVENTS, EVENT_IDS, EVENT_OBSERVATIONS, REDB_CACHE_BYTES, RELAYS, RELAY_KEYS, RELAY_REFS,
};
use super::store_bench::{duration_ns, nearest_rank};
use super::{binary_event, StoreBenchProcessCounters};

const PACKED_SEGMENTS: TableDefinition<&[u8; 10], &[u8]> =
    TableDefinition::new("packed_postings_segments_v1");
const SEGMENT_MAGIC: &[u8; 8] = b"NMPPS\0\x01\0";
const SHARD_KEY: [u8; 32] = [0x91; 32];
const SHARD_MASK: u8 = 0x3f;
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
    pub encoded_event_bytes: u64,
    pub segment_rows: u64,
    pub segment_bytes: u64,
    pub memberships: [u64; FAMILY_COUNT],
    pub database_logical_bytes: u64,
    pub database_stored_bytes: u64,
    pub reopened_rows: u64,
    pub reopened_memberships: [u64; FAMILY_COUNT],
    pub exact_reopen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
enum Family {
    Global = 0,
    Author = 1,
    Kind = 2,
    Tag = 3,
}

impl Family {
    const ALL: [Self; FAMILY_COUNT] = [Self::Global, Self::Author, Self::Kind, Self::Tag];

    fn from_u8(value: u8) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|family| *family as u8 == value)
            .ok_or_else(|| format!("unknown packed-postings family {value}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Posting {
    created_at: u64,
    id: [u8; 32],
    event_key: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Prefix {
    Global,
    Author([u8; 32]),
    Kind([u8; 2]),
    Tag(Vec<u8>),
}

impl Prefix {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Global => &[],
            Self::Author(value) => value,
            Self::Kind(value) => value,
            Self::Tag(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Membership {
    family: Family,
    shard: u8,
    prefix: Prefix,
    posting: Posting,
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
        let encoded_segments = encode_segments(grouped)?;
        totals.segment_build_ns = totals
            .segment_build_ns
            .saturating_add(duration_ns(build_started));
        for (family, shard, value) in encoded_segments {
            let key = segment_key(family, shard, batch_index as u64);
            totals.segment_rows += 1;
            totals.segment_bytes = totals.segment_bytes.saturating_add(value.len() as u64);
            segments
                .insert(&key, value.as_slice())
                .map_err(|error| error.to_string())?;
        }
        relay_refs
            .insert(1, first_key + batch.len() as u64 - 1)
            .map_err(|error| error.to_string())?;

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
    let process = sample_process().delta(process_before);
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
    let mut reopened_memberships = [0u64; FAMILY_COUNT];
    let mut reopened_segment_rows = 0u64;
    for entry in segment_table.iter().map_err(|error| error.to_string())? {
        let (key, value) = entry.map_err(|error| error.to_string())?;
        let family = Family::from_u8(key.value()[0])?;
        let decoded = decode_segment(value.value())?;
        if decoded.family != family {
            return Err("segment key/value family mismatch".to_owned());
        }
        reopened_memberships[family as usize] =
            reopened_memberships[family as usize].saturating_add(decoded.postings);
        reopened_segment_rows += 1;
    }
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
            segments: open("packed_segments")?,
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
        let encoded_segments = encode_segments(grouped)?;
        totals.segment_build_ns = totals
            .segment_build_ns
            .saturating_add(duration_ns(build_started));
        for (family, shard, value) in encoded_segments {
            let key = segment_key(family, shard, batch_index as u64);
            totals.segment_rows += 1;
            totals.segment_bytes = totals.segment_bytes.saturating_add(value.len() as u64);
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
    let process = sample_process().delta(process_before);
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
    for entry in read.iter(&reopened_keyspaces.segments) {
        let (key, value) = entry.into_inner().map_err(|error| error.to_string())?;
        let family = Family::from_u8(
            *key.first()
                .ok_or_else(|| "empty Fjall segment key".to_owned())?,
        )?;
        let decoded = decode_segment(&value)?;
        if decoded.family != family {
            return Err("segment key/value family mismatch".to_owned());
        }
        reopened_memberships[family as usize] =
            reopened_memberships[family as usize].saturating_add(decoded.postings);
        reopened_segment_rows += 1;
    }
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
    )?;
    metrics.database_logical_bytes = logical_bytes;
    Ok(metrics)
}

#[derive(Default)]
struct RunTotals {
    transactions: u64,
    segment_build_ns: u64,
    commit_ns: u64,
    encoded_event_bytes: u64,
    segment_rows: u64,
    segment_bytes: u64,
    memberships: [u64; FAMILY_COUNT],
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
) -> Result<PackedPostingsMetrics, String> {
    let exact_reopen = reopened_rows == event_count
        && reopened_segment_rows == totals.segment_rows
        && reopened_memberships == totals.memberships;
    Ok(PackedPostingsMetrics {
        backend,
        events: event_count,
        transaction_batch_size: batch_size,
        transactions: totals.transactions,
        wall_ns,
        segment_build_ns: totals.segment_build_ns,
        commit_ns: totals.commit_ns,
        commit_p50_ns: nearest_rank(commit_latencies, 50).unwrap_or(0),
        commit_p95_ns: nearest_rank(commit_latencies, 95).unwrap_or(0),
        commit_p99_ns: nearest_rank(commit_latencies, 99).unwrap_or(0),
        cpu_ns: process.cpu_ns,
        allocation_ops: process.allocation_ops,
        allocated_bytes: process.allocated_bytes,
        rss_before_bytes: process.current_rss_bytes,
        peak_rss_bytes: process.peak_rss_bytes,
        process_write_bytes: process.process_write_bytes,
        encoded_event_bytes: totals.encoded_event_bytes,
        segment_rows: totals.segment_rows,
        segment_bytes: totals.segment_bytes,
        memberships: totals.memberships,
        database_logical_bytes: path_size(path).map_err(|error| error.to_string())?,
        database_stored_bytes: stored_bytes,
        reopened_rows,
        reopened_memberships,
        exact_reopen,
    })
}

fn add_event_memberships(
    segments: &mut BatchSegments,
    counts: &mut [u64; FAMILY_COUNT],
    event: &Event,
    event_key: u64,
) {
    let posting = Posting {
        created_at: event.created_at.as_secs(),
        id: *event.id.as_bytes(),
        event_key,
    };
    push_membership(segments, counts, Family::Global, Prefix::Global, posting);
    push_membership(
        segments,
        counts,
        Family::Author,
        Prefix::Author(*event.pubkey.as_bytes()),
        posting,
    );
    push_membership(
        segments,
        counts,
        Family::Kind,
        Prefix::Kind(event.kind.as_u16().to_be_bytes()),
        posting,
    );
    let mut tag_prefixes = BTreeSet::new();
    for tag in event.tags.iter() {
        let (Some(letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
            continue;
        };
        tag_prefixes.insert(tag_index_prefix(letter, value));
    }
    for prefix in tag_prefixes {
        push_membership(segments, counts, Family::Tag, Prefix::Tag(prefix), posting);
    }
}

fn push_membership(
    segments: &mut BatchSegments,
    counts: &mut [u64; FAMILY_COUNT],
    family: Family,
    prefix: Prefix,
    posting: Posting,
) {
    let shard = shard_for(family, prefix.as_bytes());
    segments.push(Membership {
        family,
        shard,
        prefix,
        posting,
    });
    counts[family as usize] = counts[family as usize].saturating_add(1);
}

fn shard_for(family: Family, prefix: &[u8]) -> u8 {
    if family == Family::Global {
        return 0;
    }
    let mut hasher = blake3::Hasher::new_keyed(&SHARD_KEY);
    hasher.update(&[family as u8]);
    hasher.update(prefix);
    hasher.finalize().as_bytes()[0] & SHARD_MASK
}

fn encode_segments(mut memberships: BatchSegments) -> Result<Vec<(Family, u8, Vec<u8>)>, String> {
    memberships.sort_unstable_by(|left, right| {
        left.family
            .cmp(&right.family)
            .then_with(|| left.shard.cmp(&right.shard))
            .then_with(|| left.prefix.as_bytes().cmp(right.prefix.as_bytes()))
            .then_with(|| posting_order(&left.posting, &right.posting))
    });
    let mut encoded = Vec::new();
    let mut segment_start = 0usize;
    while segment_start < memberships.len() {
        let family = memberships[segment_start].family;
        let shard = memberships[segment_start].shard;
        let mut segment_end = segment_start + 1;
        while segment_end < memberships.len()
            && memberships[segment_end].family == family
            && memberships[segment_end].shard == shard
        {
            segment_end += 1;
        }
        encoded.push((
            family,
            shard,
            encode_segment(family, &memberships[segment_start..segment_end])?,
        ));
        segment_start = segment_end;
    }
    Ok(encoded)
}

fn posting_order(left: &Posting, right: &Posting) -> std::cmp::Ordering {
    right
        .created_at
        .cmp(&left.created_at)
        .then_with(|| left.id.cmp(&right.id))
        .then_with(|| left.event_key.cmp(&right.event_key))
}

fn encode_segment(family: Family, memberships: &[Membership]) -> Result<Vec<u8>, String> {
    if memberships.is_empty() {
        return Err("cannot encode an empty segment".to_owned());
    }
    let prefix_count = 1 + memberships
        .windows(2)
        .filter(|pair| pair[0].prefix.as_bytes() != pair[1].prefix.as_bytes())
        .count();
    let mut value = Vec::new();
    value.extend_from_slice(SEGMENT_MAGIC);
    value.push(family as u8);
    put_varint(&mut value, prefix_count as u64);
    let mut prefix_start = 0usize;
    while prefix_start < memberships.len() {
        let prefix = memberships[prefix_start].prefix.as_bytes();
        let mut prefix_end = prefix_start + 1;
        while prefix_end < memberships.len() && memberships[prefix_end].prefix.as_bytes() == prefix
        {
            prefix_end += 1;
        }
        let postings = &memberships[prefix_start..prefix_end];
        put_varint(&mut value, prefix.len() as u64);
        value.extend_from_slice(prefix);
        put_varint(&mut value, postings.len() as u64);
        let mut previous_created_at: Option<u64> = None;
        let mut previous_event_key: Option<u64> = None;
        for membership in postings {
            let posting = membership.posting;
            match previous_created_at {
                None => value.extend_from_slice(&posting.created_at.to_be_bytes()),
                Some(previous) => put_varint(
                    &mut value,
                    previous
                        .checked_sub(posting.created_at)
                        .ok_or_else(|| "postings are not newest-first".to_owned())?,
                ),
            }
            match previous_event_key {
                None => put_varint(&mut value, posting.event_key),
                Some(previous) => put_varint(
                    &mut value,
                    zigzag_encode(
                        i64::try_from(i128::from(posting.event_key) - i128::from(previous))
                            .map_err(|_| "event-key delta does not fit i64".to_owned())?,
                    ),
                ),
            }
            previous_created_at = Some(posting.created_at);
            previous_event_key = Some(posting.event_key);
        }
        prefix_start = prefix_end;
    }
    Ok(value)
}

struct DecodedSegment {
    family: Family,
    postings: u64,
}

fn decode_segment(value: &[u8]) -> Result<DecodedSegment, String> {
    let mut cursor = 0usize;
    if take(value, &mut cursor, SEGMENT_MAGIC.len())? != SEGMENT_MAGIC {
        return Err("invalid packed-postings magic".to_owned());
    }
    let family = Family::from_u8(*take(value, &mut cursor, 1)?.first().unwrap())?;
    let prefix_count = read_varint(value, &mut cursor)?;
    let mut previous_prefix: Option<Vec<u8>> = None;
    let mut total = 0u64;
    for _ in 0..prefix_count {
        let prefix_len = usize::try_from(read_varint(value, &mut cursor)?)
            .map_err(|_| "prefix length does not fit usize".to_owned())?;
        let prefix = take(value, &mut cursor, prefix_len)?.to_vec();
        if previous_prefix
            .as_ref()
            .is_some_and(|prior| prior >= &prefix)
        {
            return Err("segment prefixes are not strictly ordered".to_owned());
        }
        previous_prefix = Some(prefix);
        let posting_count = read_varint(value, &mut cursor)?;
        if posting_count == 0 {
            return Err("empty posting list".to_owned());
        }
        let mut previous_created_at: Option<u64> = None;
        let mut previous_event_key: Option<u64> = None;
        for ordinal in 0..posting_count {
            let created_at = if ordinal == 0 {
                u64::from_be_bytes(
                    take(value, &mut cursor, 8)?
                        .try_into()
                        .expect("fixed timestamp width"),
                )
            } else {
                previous_created_at
                    .expect("non-first posting has predecessor")
                    .checked_sub(read_varint(value, &mut cursor)?)
                    .ok_or_else(|| "timestamp delta underflow".to_owned())?
            };
            let encoded_event_key = read_varint(value, &mut cursor)?;
            let event_key = match previous_event_key {
                None => encoded_event_key,
                Some(previous) => {
                    let delta = i128::from(zigzag_decode(encoded_event_key));
                    u64::try_from(i128::from(previous) + delta)
                        .map_err(|_| "event-key delta overflow".to_owned())?
                }
            };
            if previous_created_at.is_some_and(|previous| previous < created_at) {
                return Err("posting list violates newest-first order".to_owned());
            }
            previous_created_at = Some(created_at);
            previous_event_key = Some(event_key);
            total = total.saturating_add(1);
        }
    }
    if cursor != value.len() {
        return Err("trailing bytes in packed-postings segment".to_owned());
    }
    Ok(DecodedSegment {
        family,
        postings: total,
    })
}

fn segment_key(family: Family, shard: u8, generation: u64) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = family as u8;
    key[1] = shard;
    key[2..].copy_from_slice(&generation.to_be_bytes());
    key
}

fn put_varint(target: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        target.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    target.push(value as u8);
}

fn zigzag_encode(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn zigzag_decode(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..=63).step_by(7) {
        let byte = *take(bytes, cursor, 1)?.first().unwrap();
        if shift == 63 && byte > 1 {
            return Err("varint overflow".to_owned());
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("unterminated varint".to_owned())
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8], String> {
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| "segment cursor overflow".to_owned())?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| "truncated packed-postings segment".to_owned())?;
    *cursor = end;
    Ok(value)
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
    use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

    use super::*;

    #[test]
    fn segment_roundtrip_preserves_equal_timestamp_order_and_counts() {
        let postings = [
            Posting {
                created_at: 10,
                id: [3; 32],
                event_key: 3,
            },
            Posting {
                created_at: 11,
                id: [9; 32],
                event_key: 1,
            },
            Posting {
                created_at: 10,
                id: [1; 32],
                event_key: 2,
            },
        ];
        let mut memberships: Vec<_> = postings
            .into_iter()
            .map(|posting| Membership {
                family: Family::Tag,
                shard: 0,
                prefix: Prefix::Tag(b"prefix".to_vec()),
                posting,
            })
            .collect();
        memberships.sort_unstable_by(|left, right| posting_order(&left.posting, &right.posting));
        let encoded = encode_segment(Family::Tag, &memberships).unwrap();
        let decoded = decode_segment(&encoded).unwrap();
        assert_eq!(decoded.family, Family::Tag);
        assert_eq!(decoded.postings, 3);
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
            prefix: Prefix::Global,
            posting: Posting {
                created_at: 1,
                id: [0; 32],
                event_key: 1,
            },
        };
        let mut encoded = encode_segment(Family::Global, &[membership]).unwrap();
        encoded.push(0);
        assert!(decode_segment(&encoded).is_err());
    }
}
