use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::ffi::{c_char, c_int, c_void, CString};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fjall::{
    KeyspaceCreateOptions, PersistMode, Readable, SingleWriterTxDatabase, SingleWriterTxKeyspace,
};
use memmap2::{Mmap, MmapOptions};
use nmp_store::{
    prepare_equivalent_store_corpus, run_prepared_redb_store_bench, run_store_bench_variant,
    EventStore, InsertOutcome, RedbStore, RelayObserved, StoreBenchMetrics,
    StoreBenchPreparedCorpus, StoreBenchPreparedMetrics, StoreBenchPreparedTable,
    StoreBenchProcessCounters, StoreBenchVariant,
};
use nostr::{Event, EventBuilder, Filter, JsonUtil, Keys, Kind, RelayUrl, Tag, Timestamp};
use rayon::prelude::*;
use redb::{Database as ProbeRedb, ReadableDatabase, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

const SCHEMA: &str = "nmp-nostrdb-direct-v1";
const CORPUS_SCHEMA: &str = "nmp-nostrdb-corpus-v1";
const BASE_CREATED_AT: u64 = 1_700_000_000;
const AUTHORS: usize = 64;
const QUERY_LIMIT: usize = 200;
const NOSTRDB_MAP_SIZE: u64 = 32 * 1024 * 1024 * 1024;
const EQUIVALENT_SCHEMA: &str = "nmp-storage-equivalent-v1";
const FJALL_KEYSPACES: [&str; 12] = [
    "events",
    "event_ids",
    "event_observations",
    "relays",
    "relay_keys",
    "relay_refs",
    "by_created_at",
    "by_author",
    "by_kind",
    "by_author_kind",
    "by_tag",
    "index_cardinality",
];

struct CountingAllocator;

static ALLOCATION_OPS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

#[repr(C)]
struct LmdbRecord {
    table: u32,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
}

unsafe extern "C" {
    fn bench_ndb_open(path: *const c_char, mapsize: u64, ingest_threads: c_int) -> *mut c_void;
    fn bench_ndb_ingest(handle: *mut c_void, jsonl: *const c_char, len: u64) -> c_int;
    fn bench_ndb_close(handle: *mut c_void);
    fn bench_ndb_query(
        handle: *mut c_void,
        filter_json: *const c_char,
        capacity: c_int,
        ids: *mut u8,
        created_at: *mut u32,
        count: *mut c_int,
    ) -> c_int;
    fn bench_ndb_note_count(handle: *mut c_void) -> u64;
    fn bench_lmdb_open(path: *const c_char, mapsize: u64, error_out: *mut c_int) -> *mut c_void;
    fn bench_lmdb_begin(handle: *mut c_void, error_out: *mut c_int) -> *mut c_void;
    fn bench_lmdb_put_batch(
        handle: *mut c_void,
        transaction: *mut c_void,
        records: *const LmdbRecord,
        count: usize,
    ) -> c_int;
    fn bench_lmdb_commit(transaction: *mut c_void) -> c_int;
    fn bench_lmdb_abort(transaction: *mut c_void);
    fn bench_lmdb_count(handle: *mut c_void, table: u32, error_out: *mut c_int) -> u64;
    fn bench_lmdb_has(
        handle: *mut c_void,
        table: u32,
        key: *const u8,
        key_len: usize,
        error_out: *mut c_int,
    ) -> c_int;
    fn bench_lmdb_error(error: c_int) -> *const c_char;
    fn bench_lmdb_close(handle: *mut c_void);
}

#[derive(Debug, Serialize, Deserialize)]
struct CorpusMeta {
    schema: String,
    events: u64,
    payload_bytes: usize,
    bytes: u64,
    blake3: String,
    author0: String,
    first_id: String,
    last_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PhaseResult {
    elapsed_ms: f64,
    events_per_second: f64,
    rss_before_bytes: Option<u64>,
    peak_rss_bytes: Option<u64>,
    #[serde(default)]
    parse_verify_ms: Option<f64>,
    #[serde(default)]
    store_commit_ms: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct QueryResult {
    name: String,
    filter_json: String,
    rows: usize,
    ids: Vec<String>,
    p50_ms: f64,
    p95_ms: f64,
    #[serde(default)]
    index_rows: Option<u64>,
    #[serde(default)]
    event_values: Option<u64>,
    #[serde(default)]
    owned_events_materialized: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BackendResult {
    schema: String,
    backend: String,
    nmp_commit: String,
    nostrdb_commit: String,
    nostrdb_secp256k1: String,
    host: String,
    corpus: CorpusMeta,
    workers: usize,
    transaction_batch_size: Option<usize>,
    ingest: PhaseResult,
    duplicate_ingest: PhaseResult,
    canonical_events: u64,
    database_logical_bytes: u64,
    database_allocated_bytes: u64,
    healthy_reopen_p50_ms: f64,
    queries: Vec<QueryResult>,
}

#[derive(Debug, Serialize)]
struct BackendFailure<'a> {
    schema: &'static str,
    backend: &'static str,
    nmp_commit: String,
    nostrdb_commit: String,
    nostrdb_secp256k1: String,
    host: String,
    corpus: &'a CorpusMeta,
    workers: usize,
    stage: &'a str,
    error: String,
    canonical_events_after_failure: Option<u64>,
}

#[derive(Debug, Serialize)]
struct Comparison {
    schema: &'static str,
    nmp_commit: String,
    nostrdb_commit: String,
    corpus_blake3: String,
    events: u64,
    queries_equal: bool,
    equivalent_queries: Vec<String>,
    mismatched_queries: Vec<QueryMismatch>,
    nmp_ingest_events_per_second: f64,
    nostrdb_ingest_events_per_second: f64,
    nostrdb_to_nmp_ingest_throughput_ratio: f64,
    nmp_duplicate_events_per_second: f64,
    nostrdb_duplicate_events_per_second: f64,
    nostrdb_to_nmp_duplicate_throughput_ratio: f64,
    nmp_database_logical_bytes: u64,
    nostrdb_database_logical_bytes: u64,
    nmp_to_nostrdb_logical_size_ratio: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct EquivalentRun {
    schema: String,
    backend: String,
    nmp_commit: String,
    nostrdb_commit: String,
    git_dirty: bool,
    host: String,
    corpus_blake3: String,
    events: u64,
    payload_bytes: usize,
    transaction_batch_size: usize,
    prepared_records: u64,
    prepared_record_bytes: u64,
    preparation_ns: u64,
    metrics: StoreBenchPreparedMetrics,
    events_per_second: f64,
    database_allocated_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct EquivalentMatrixEntry {
    repetition: usize,
    ordinal: usize,
    run: EquivalentRun,
}

#[derive(Debug, Serialize, Deserialize)]
struct EquivalentMatrix {
    schema: String,
    command: String,
    nmp_commit: String,
    nostrdb_commit: String,
    git_dirty: bool,
    host: String,
    corpus_blake3: String,
    repetitions: usize,
    alternating_order: bool,
    runs: Vec<EquivalentMatrixEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CrashProbeEngine {
    backend: String,
    durability: String,
    child_exit_code: i32,
    committed_rows_after_crash: u64,
    uncommitted_row_absent: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct CrashProbeResult {
    schema: String,
    nmp_commit: String,
    nostrdb_commit: String,
    host: String,
    engines: Vec<CrashProbeEngine>,
}

const CRASH_PROBE_TABLE: TableDefinition<&str, &str> =
    TableDefinition::new("storage_engine_crash_probe_v1");

struct LmdbHandle(*mut c_void);

impl LmdbHandle {
    fn open(path: &Path) -> Result<Self, String> {
        fs::create_dir_all(path).map_err(|error| error.to_string())?;
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
        let mut error = 0;
        let raw = unsafe { bench_lmdb_open(path.as_ptr(), NOSTRDB_MAP_SIZE, &mut error) };
        if raw.is_null() {
            Err(lmdb_error(error))
        } else {
            Ok(Self(raw))
        }
    }

    fn begin(&self) -> Result<LmdbTransaction, String> {
        let mut error = 0;
        let raw = unsafe { bench_lmdb_begin(self.0, &mut error) };
        if raw.is_null() {
            Err(lmdb_error(error))
        } else {
            Ok(LmdbTransaction(raw))
        }
    }

    fn count(&self, table: u32) -> Result<u64, String> {
        let mut error = 0;
        let count = unsafe { bench_lmdb_count(self.0, table, &mut error) };
        (error == 0)
            .then_some(count)
            .ok_or_else(|| lmdb_error(error))
    }

    fn contains(&self, table: u32, key: &[u8]) -> Result<bool, String> {
        let mut error = 0;
        let found = unsafe { bench_lmdb_has(self.0, table, key.as_ptr(), key.len(), &mut error) };
        if error != 0 || found < 0 {
            Err(lmdb_error(error))
        } else {
            Ok(found == 1)
        }
    }

    fn close(mut self) {
        unsafe { bench_lmdb_close(self.0) };
        self.0 = std::ptr::null_mut();
    }
}

impl Drop for LmdbHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { bench_lmdb_close(self.0) };
        }
    }
}

struct LmdbTransaction(*mut c_void);

impl LmdbTransaction {
    fn put_batch(&self, handle: &LmdbHandle, records: &[LmdbRecord]) -> Result<(), String> {
        let error =
            unsafe { bench_lmdb_put_batch(handle.0, self.0, records.as_ptr(), records.len()) };
        (error == 0).then_some(()).ok_or_else(|| lmdb_error(error))
    }

    fn commit(mut self) -> Result<(), String> {
        let error = unsafe { bench_lmdb_commit(self.0) };
        self.0 = std::ptr::null_mut();
        (error == 0).then_some(()).ok_or_else(|| lmdb_error(error))
    }
}

impl Drop for LmdbTransaction {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { bench_lmdb_abort(self.0) };
        }
    }
}

fn lmdb_error(error: c_int) -> String {
    let raw = unsafe { bench_lmdb_error(error) };
    if raw.is_null() {
        format!("LMDB error {error}")
    } else {
        unsafe { std::ffi::CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned()
    }
}

#[derive(Debug, Serialize)]
struct QueryMismatch {
    name: String,
    nmp_rows: usize,
    nostrdb_rows: usize,
    nmp_first_id: Option<String>,
    nostrdb_first_id: Option<String>,
}

struct NdbHandle(*mut c_void);

impl NdbHandle {
    fn open(path: &Path, workers: usize) -> Result<Self, String> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        let raw = unsafe { bench_ndb_open(path.as_ptr(), NOSTRDB_MAP_SIZE, workers as c_int) };
        if raw.is_null() {
            Err("nostrdb open failed".to_owned())
        } else {
            Ok(Self(raw))
        }
    }

    fn ingest(&self, corpus: &Mmap) -> Result<(), String> {
        let ok = unsafe {
            bench_ndb_ingest(
                self.0,
                corpus.as_ptr().cast::<c_char>(),
                corpus.len() as u64,
            )
        };
        (ok != 0)
            .then_some(())
            .ok_or_else(|| "nostrdb ingest enqueue failed".to_owned())
    }

    fn note_count(&self) -> Result<u64, String> {
        let count = unsafe { bench_ndb_note_count(self.0) };
        (count != u64::MAX)
            .then_some(count)
            .ok_or_else(|| "nostrdb stat failed".to_owned())
    }

    fn close(mut self) {
        unsafe { bench_ndb_close(self.0) };
        self.0 = std::ptr::null_mut();
    }
}

impl Drop for NdbHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { bench_ndb_close(self.0) };
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let command = args
        .next()
        .ok_or(
            "usage: nmp-nostrdb-compare <generate|describe-corpus|run-nmp|run-nostrdb|compare|run-equivalent|matrix-equivalent|matrix-prepared|crash-probe|crash-child> ...",
        )?;
    match command.to_string_lossy().as_ref() {
        "generate" => {
            let corpus = path_arg(&mut args, "corpus")?;
            let events = number_arg::<u64>(&mut args, "events")?;
            let payload = number_arg::<usize>(&mut args, "payload-bytes")?;
            generate(&corpus, events, payload)?;
        }
        "describe-corpus" => {
            let corpus = path_arg(&mut args, "corpus")?;
            describe_corpus(&corpus)?;
        }
        "run-nmp" => {
            let corpus = path_arg(&mut args, "corpus")?;
            let database = path_arg(&mut args, "database")?;
            let batch = number_arg::<usize>(&mut args, "batch-size")?;
            let workers = number_arg::<usize>(&mut args, "verify-workers")?;
            let iterations = number_arg::<usize>(&mut args, "query-iterations")?;
            let output = path_arg(&mut args, "output")?;
            run_nmp(&corpus, &database, batch, workers, iterations, &output)?;
        }
        "run-nostrdb" => {
            let corpus = path_arg(&mut args, "corpus")?;
            let database = path_arg(&mut args, "database-directory")?;
            let workers = number_arg::<usize>(&mut args, "ingest-workers")?;
            let iterations = number_arg::<usize>(&mut args, "query-iterations")?;
            let output = path_arg(&mut args, "output")?;
            run_nostrdb(&corpus, &database, workers, iterations, &output)?;
        }
        "compare" => {
            let nmp = path_arg(&mut args, "nmp-result")?;
            let nostrdb = path_arg(&mut args, "nostrdb-result")?;
            let output = path_arg(&mut args, "output")?;
            compare(&nmp, &nostrdb, &output)?;
        }
        "run-equivalent" => {
            let corpus = path_arg(&mut args, "corpus")?;
            let backend = args
                .next()
                .ok_or("missing backend (redb-prepared|lmdb-prepared|fjall-prepared|redb-full)")?
                .to_string_lossy()
                .into_owned();
            let batch = number_arg::<usize>(&mut args, "batch-size")?;
            let output = path_arg(&mut args, "output")?;
            run_equivalent(&corpus, &backend, batch, &output)?;
        }
        "matrix-equivalent" => {
            let corpus = path_arg(&mut args, "corpus")?;
            let repetitions = number_arg::<usize>(&mut args, "repetitions")?;
            let output = path_arg(&mut args, "output")?;
            run_equivalent_matrix(&corpus, repetitions, &output, true)?;
        }
        "matrix-prepared" => {
            let corpus = path_arg(&mut args, "corpus")?;
            let repetitions = number_arg::<usize>(&mut args, "repetitions")?;
            let output = path_arg(&mut args, "output")?;
            run_equivalent_matrix(&corpus, repetitions, &output, false)?;
        }
        "crash-probe" => {
            let output = path_arg(&mut args, "output")?;
            run_crash_probe(&output)?;
        }
        "crash-child" => {
            let backend = args
                .next()
                .ok_or("missing crash backend")?
                .to_string_lossy()
                .into_owned();
            let database = path_arg(&mut args, "database")?;
            run_crash_child(&backend, &database)?;
        }
        other => return Err(format!("unknown command {other}").into()),
    }
    Ok(())
}

fn parse_prevalidated_events(corpus: &Mmap) -> Result<Vec<Event>, String> {
    corpus
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            Event::from_json(line).map_err(|error| format!("parse corpus event: {error:?}"))
        })
        .collect()
}

fn lmdb_descriptors(corpus: &StoreBenchPreparedCorpus) -> Vec<Vec<LmdbRecord>> {
    corpus
        .batches
        .iter()
        .map(|batch| {
            batch
                .records
                .iter()
                .map(|record| LmdbRecord {
                    table: record.table as u32,
                    key: record.key.as_ptr(),
                    key_len: record.key.len(),
                    value: record.value.as_ptr(),
                    value_len: record.value.len(),
                })
                .collect()
        })
        .collect()
}

fn counters_delta(
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

fn run_prepared_lmdb(
    path: &Path,
    corpus: &StoreBenchPreparedCorpus,
) -> Result<StoreBenchPreparedMetrics, String> {
    let descriptors = lmdb_descriptors(corpus);
    let handle = LmdbHandle::open(path)?;
    let process_before = sample_process();
    let started = Instant::now();
    let mut commit_ns = 0u64;
    for batch in &descriptors {
        let transaction = handle.begin()?;
        transaction.put_batch(&handle, batch)?;
        let commit_started = Instant::now();
        transaction.commit()?;
        commit_ns = commit_ns.saturating_add(duration_ns(commit_started));
    }
    let wall_ns = duration_ns(started);
    let process = counters_delta(sample_process(), process_before);
    handle.close();
    let (database_logical_bytes, _) = path_size(path).map_err(|error| error.to_string())?;

    let reopen_started = Instant::now();
    let reopened = LmdbHandle::open(path)?;
    let reopened_table_rows = (0..=11)
        .map(|table| reopened.count(table))
        .collect::<Result<Vec<_>, _>>()?;
    let reopened_rows = reopened_table_rows[0];
    reopened.close();
    let reopen_ns = duration_ns(reopen_started);

    Ok(StoreBenchPreparedMetrics {
        events: corpus.events,
        transactions: corpus.batches.len() as u64,
        wall_ns,
        commit_ns,
        reopen_ns,
        cpu_ns: process.cpu_ns,
        allocation_ops: process.allocation_ops,
        allocated_bytes: process.allocated_bytes,
        rss_before_bytes: process.current_rss_bytes,
        peak_rss_bytes: process.peak_rss_bytes,
        process_write_bytes: process.process_write_bytes,
        database_logical_bytes,
        database_stored_bytes: database_logical_bytes,
        reopened_rows,
        expected_table_rows: corpus.expected_table_rows.clone(),
        exact_reopen: reopened_table_rows == corpus.expected_table_rows,
        reopened_table_rows,
    })
}

fn open_fjall_keyspaces(
    database: &SingleWriterTxDatabase,
) -> Result<Vec<SingleWriterTxKeyspace>, String> {
    FJALL_KEYSPACES
        .iter()
        .map(|name| {
            database
                .keyspace(name, KeyspaceCreateOptions::default)
                .map_err(|error| error.to_string())
        })
        .collect()
}

fn run_prepared_fjall(
    path: &Path,
    corpus: &StoreBenchPreparedCorpus,
) -> Result<StoreBenchPreparedMetrics, String> {
    let database = SingleWriterTxDatabase::builder(path)
        .open()
        .map_err(|error| error.to_string())?;
    let keyspaces = open_fjall_keyspaces(&database)?;
    database
        .persist(PersistMode::SyncAll)
        .map_err(|error| error.to_string())?;

    let process_before = sample_process();
    let started = Instant::now();
    let mut commit_ns = 0u64;
    for batch in &corpus.batches {
        let mut transaction = database.write_tx().durability(Some(PersistMode::SyncAll));
        for record in &batch.records {
            transaction.insert(
                &keyspaces[record.table as usize],
                record.key.as_slice(),
                record.value.as_slice(),
            );
        }
        let commit_started = Instant::now();
        transaction.commit().map_err(|error| error.to_string())?;
        commit_ns = commit_ns.saturating_add(duration_ns(commit_started));
    }
    let wall_ns = duration_ns(started);
    let process = counters_delta(sample_process(), process_before);
    let database_stored_bytes = database.disk_space().map_err(|error| error.to_string())?;
    drop(keyspaces);
    drop(database);
    let (database_logical_bytes, _) = path_size(path).map_err(|error| error.to_string())?;

    let reopen_started = Instant::now();
    let reopened = SingleWriterTxDatabase::builder(path)
        .open()
        .map_err(|error| error.to_string())?;
    let reopened_keyspaces = open_fjall_keyspaces(&reopened)?;
    let read = reopened.read_tx();
    let reopened_table_rows = reopened_keyspaces
        .iter()
        .map(|keyspace| {
            read.len(keyspace)
                .map(|rows| rows as u64)
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let reopened_rows = reopened_table_rows[StoreBenchPreparedTable::Events as usize];
    drop(read);
    drop(reopened_keyspaces);
    drop(reopened);
    let reopen_ns = duration_ns(reopen_started);

    Ok(StoreBenchPreparedMetrics {
        events: corpus.events,
        transactions: corpus.batches.len() as u64,
        wall_ns,
        commit_ns,
        reopen_ns,
        cpu_ns: process.cpu_ns,
        allocation_ops: process.allocation_ops,
        allocated_bytes: process.allocated_bytes,
        rss_before_bytes: process.current_rss_bytes,
        peak_rss_bytes: process.peak_rss_bytes,
        process_write_bytes: process.process_write_bytes,
        database_logical_bytes,
        database_stored_bytes,
        reopened_rows,
        expected_table_rows: corpus.expected_table_rows.clone(),
        exact_reopen: reopened_table_rows == corpus.expected_table_rows,
        reopened_table_rows,
    })
}

fn full_metrics_to_prepared(metrics: StoreBenchMetrics) -> StoreBenchPreparedMetrics {
    StoreBenchPreparedMetrics {
        events: metrics.events,
        transactions: metrics.transactions,
        wall_ns: metrics.wall_ns,
        commit_ns: metrics.commit_ns,
        reopen_ns: 0,
        cpu_ns: metrics.cpu_ns,
        allocation_ops: metrics.allocation_ops,
        allocated_bytes: metrics.allocated_bytes,
        rss_before_bytes: metrics.rss_before_bytes,
        peak_rss_bytes: metrics.peak_rss_bytes,
        process_write_bytes: metrics.process_write_bytes,
        database_logical_bytes: metrics.database_logical_bytes,
        database_stored_bytes: metrics.database_stored_bytes,
        reopened_rows: metrics.reopened_rows,
        expected_table_rows: Vec::new(),
        reopened_table_rows: Vec::new(),
        exact_reopen: metrics.exact_reopen,
    }
}

fn run_equivalent(
    corpus_path: &Path,
    backend: &str,
    batch_size: usize,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let (corpus, meta) = open_corpus(corpus_path)?;
    let events = parse_prevalidated_events(&corpus)?;
    let preparation_started = Instant::now();
    let prepared = (backend != "redb-full")
        .then(|| prepare_equivalent_store_corpus(&events, batch_size))
        .transpose()?;
    let preparation_ns = prepared
        .as_ref()
        .map(|_| duration_ns(preparation_started))
        .unwrap_or(0);
    let prepared_records = prepared
        .as_ref()
        .map(|prepared| {
            prepared
                .batches
                .iter()
                .map(|batch| batch.records.len() as u64)
                .sum()
        })
        .unwrap_or(0);
    let prepared_record_bytes = prepared
        .as_ref()
        .map(|prepared| prepared.record_bytes)
        .unwrap_or(0);
    let scratch = tempfile::tempdir()?;
    let database = match backend {
        "redb-prepared" | "redb-full" => scratch.path().join("store.redb"),
        "lmdb-prepared" => scratch.path().join("store.lmdb"),
        "fjall-prepared" => scratch.path().join("store.fjall"),
        _ => return Err(format!("unknown equivalent backend {backend}").into()),
    };
    let metrics = match backend {
        "redb-prepared" => run_prepared_redb_store_bench(
            &database,
            prepared.as_ref().expect("prepared backend has corpus"),
            sample_process,
        )?,
        "lmdb-prepared" => run_prepared_lmdb(
            &database,
            prepared.as_ref().expect("prepared backend has corpus"),
        )?,
        "fjall-prepared" => run_prepared_fjall(
            &database,
            prepared.as_ref().expect("prepared backend has corpus"),
        )?,
        "redb-full" => full_metrics_to_prepared(run_store_bench_variant(
            &database,
            events,
            batch_size,
            StoreBenchVariant::FullGoverned,
            sample_process,
        )?),
        _ => unreachable!(),
    };
    if !metrics.exact_reopen {
        return Err(format!(
            "{backend} reopened {} of {} events",
            metrics.reopened_rows, metrics.events
        )
        .into());
    }
    let (_, database_allocated_bytes) = path_size(&database)?;
    let result = EquivalentRun {
        schema: EQUIVALENT_SCHEMA.to_owned(),
        backend: backend.to_owned(),
        nmp_commit: git_commit(repo_root()),
        nostrdb_commit: nostrdb_commit(),
        git_dirty: git_dirty(repo_root()),
        host: host(),
        corpus_blake3: meta.blake3,
        events: meta.events,
        payload_bytes: meta.payload_bytes,
        transaction_batch_size: batch_size,
        prepared_records,
        prepared_record_bytes,
        preparation_ns,
        events_per_second: metrics.events as f64 * 1_000_000_000.0 / metrics.wall_ns as f64,
        metrics,
        database_allocated_bytes,
    };
    write_json(output, &result)?;
    println!("wrote {}", output.display());
    Ok(())
}

fn run_equivalent_matrix(
    corpus: &Path,
    repetitions: usize,
    output: &Path,
    include_full_governed: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if repetitions == 0 {
        return Err("equivalent matrix repetitions must be nonzero".into());
    }
    let executable = env::current_exe()?;
    let mut base_cells = vec![
        ("redb-prepared", 128usize),
        ("lmdb-prepared", 128usize),
        ("fjall-prepared", 128usize),
        ("redb-prepared", 4_096usize),
        ("lmdb-prepared", 4_096usize),
        ("fjall-prepared", 4_096usize),
    ];
    if include_full_governed {
        base_cells.push(("redb-full", 4_096usize));
    }
    let scratch = tempfile::tempdir()?;
    let mut runs = Vec::new();
    for repetition in 0..repetitions {
        let mut cells = base_cells.clone();
        if !repetition.is_multiple_of(2) {
            cells.reverse();
        }
        for (ordinal, (backend, batch_size)) in cells.into_iter().enumerate() {
            eprintln!(
                "repetition={repetition} ordinal={ordinal} backend={backend} batch={batch_size}"
            );
            let child_output = scratch.path().join(format!(
                "{repetition}-{ordinal}-{backend}-{batch_size}.json"
            ));
            let child = Command::new(&executable)
                .arg("run-equivalent")
                .arg(corpus)
                .arg(backend)
                .arg(batch_size.to_string())
                .arg(&child_output)
                .output()?;
            if !child.status.success() {
                return Err(format!(
                    "equivalent child failed for {backend}/{batch_size}: {}",
                    String::from_utf8_lossy(&child.stderr)
                )
                .into());
            }
            let run: EquivalentRun = serde_json::from_slice(&fs::read(&child_output)?)?;
            runs.push(EquivalentMatrixEntry {
                repetition,
                ordinal,
                run,
            });
        }
    }
    let first = runs.first().ok_or("equivalent matrix produced no runs")?;
    if runs.iter().any(|entry| {
        entry.run.nmp_commit != first.run.nmp_commit
            || entry.run.nostrdb_commit != first.run.nostrdb_commit
            || entry.run.git_dirty != first.run.git_dirty
            || entry.run.corpus_blake3 != first.run.corpus_blake3
    }) {
        return Err("equivalent matrix identity changed during the run".into());
    }
    let matrix = EquivalentMatrix {
        schema: EQUIVALENT_SCHEMA.to_owned(),
        command: format!(
            "NOSTRDB_DIR={} cargo run --release --manifest-path benchmarks/nostrdb-compare/Cargo.toml -- {} {} {} {}",
            nostrdb_root().display(),
            if include_full_governed {
                "matrix-equivalent"
            } else {
                "matrix-prepared"
            },
            corpus.display(),
            repetitions,
            output.display()
        ),
        nmp_commit: first.run.nmp_commit.clone(),
        nostrdb_commit: first.run.nostrdb_commit.clone(),
        git_dirty: first.run.git_dirty,
        host: first.run.host.clone(),
        corpus_blake3: first.run.corpus_blake3.clone(),
        repetitions,
        alternating_order: true,
        runs,
    };
    write_json(output, &matrix)?;
    println!("wrote {}", output.display());
    Ok(())
}

fn run_crash_child(backend: &str, database: &Path) -> Result<(), Box<dyn std::error::Error>> {
    match backend {
        "redb" => {
            let db = ProbeRedb::create(database)?;
            let committed = db.begin_write()?;
            {
                let mut table = committed.open_table(CRASH_PROBE_TABLE)?;
                table.insert("committed", "visible")?;
            }
            committed.commit()?;
            let uncommitted = db.begin_write()?;
            {
                let mut table = uncommitted.open_table(CRASH_PROBE_TABLE)?;
                table.insert("uncommitted", "must-not-survive")?;
            }
            unsafe { libc::_exit(73) };
        }
        "lmdb" => {
            let handle = LmdbHandle::open(database)?;
            let committed_key = b"committed";
            let committed_value = b"visible";
            let committed_record = LmdbRecord {
                table: 0,
                key: committed_key.as_ptr(),
                key_len: committed_key.len(),
                value: committed_value.as_ptr(),
                value_len: committed_value.len(),
            };
            let committed = handle.begin()?;
            committed.put_batch(&handle, std::slice::from_ref(&committed_record))?;
            committed.commit()?;
            let uncommitted_key = b"uncommitted";
            let uncommitted_value = b"must-not-survive";
            let uncommitted_record = LmdbRecord {
                table: 0,
                key: uncommitted_key.as_ptr(),
                key_len: uncommitted_key.len(),
                value: uncommitted_value.as_ptr(),
                value_len: uncommitted_value.len(),
            };
            let uncommitted = handle.begin()?;
            uncommitted.put_batch(&handle, std::slice::from_ref(&uncommitted_record))?;
            unsafe { libc::_exit(73) };
        }
        "fjall" => {
            let db = SingleWriterTxDatabase::builder(database).open()?;
            let keyspace = db.keyspace("crash_probe", KeyspaceCreateOptions::default)?;
            db.persist(PersistMode::SyncAll)?;
            let mut committed = db.write_tx().durability(Some(PersistMode::SyncAll));
            committed.insert(&keyspace, "committed", "visible");
            committed.commit()?;
            let mut uncommitted = db.write_tx().durability(Some(PersistMode::SyncAll));
            uncommitted.insert(&keyspace, "uncommitted", "must-not-survive");
            unsafe { libc::_exit(73) };
        }
        _ => Err(format!("unknown crash backend {backend}").into()),
    }
}

fn reopen_redb_crash_probe(path: &Path) -> Result<(u64, bool), Box<dyn std::error::Error>> {
    let db = ProbeRedb::open(path)?;
    let read = db.begin_read()?;
    let table = read.open_table(CRASH_PROBE_TABLE)?;
    let rows = table.len()?;
    let uncommitted_absent = table.get("uncommitted")?.is_none();
    Ok((rows, uncommitted_absent))
}

fn reopen_lmdb_crash_probe(path: &Path) -> Result<(u64, bool), Box<dyn std::error::Error>> {
    let handle = LmdbHandle::open(path)?;
    let rows = handle.count(0)?;
    let committed_present = handle.contains(0, b"committed")?;
    let uncommitted_absent = !handle.contains(0, b"uncommitted")?;
    handle.close();
    Ok((rows, committed_present && uncommitted_absent))
}

fn reopen_fjall_crash_probe(path: &Path) -> Result<(u64, bool), Box<dyn std::error::Error>> {
    let db = SingleWriterTxDatabase::builder(path).open()?;
    let keyspace = db.keyspace("crash_probe", KeyspaceCreateOptions::default)?;
    let read = db.read_tx();
    let rows = read.len(&keyspace)? as u64;
    let committed_present = read.contains_key(&keyspace, b"committed")?;
    let uncommitted_absent = !read.contains_key(&keyspace, b"uncommitted")?;
    Ok((rows, committed_present && uncommitted_absent))
}

fn run_crash_probe(output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let executable = env::current_exe()?;
    let scratch = tempfile::tempdir()?;
    let mut engines = Vec::new();
    for backend in ["redb", "lmdb", "fjall"] {
        let database = match backend {
            "redb" => scratch.path().join("crash.redb"),
            "lmdb" => scratch.path().join("crash.lmdb"),
            "fjall" => scratch.path().join("crash.fjall"),
            _ => unreachable!(),
        };
        let status = Command::new(&executable)
            .arg("crash-child")
            .arg(backend)
            .arg(&database)
            .status()?;
        let exit_code = status.code().unwrap_or(-1);
        if exit_code != 73 {
            return Err(format!("{backend} crash child exited {exit_code}, expected 73").into());
        }
        let (rows, uncommitted_absent) = match backend {
            "redb" => reopen_redb_crash_probe(&database)?,
            "lmdb" => reopen_lmdb_crash_probe(&database)?,
            "fjall" => reopen_fjall_crash_probe(&database)?,
            _ => unreachable!(),
        };
        if rows != 1 || !uncommitted_absent {
            return Err(format!(
                "{backend} crash reopen had {rows} rows; uncommitted_absent={uncommitted_absent}"
            )
            .into());
        }
        engines.push(CrashProbeEngine {
            backend: backend.to_owned(),
            durability: if backend == "fjall" {
                "explicit PersistMode::SyncAll per committed transaction".to_owned()
            } else {
                "synchronous default; no no-sync flags".to_owned()
            },
            child_exit_code: exit_code,
            committed_rows_after_crash: rows,
            uncommitted_row_absent: uncommitted_absent,
        });
    }
    let result = CrashProbeResult {
        schema: EQUIVALENT_SCHEMA.to_owned(),
        nmp_commit: git_commit(repo_root()),
        nostrdb_commit: nostrdb_commit(),
        host: host(),
        engines,
    };
    write_json(output, &result)?;
    println!("wrote {}", output.display());
    Ok(())
}

fn path_arg(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<PathBuf, String> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}"))
}

fn number_arg<T>(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    args.next()
        .ok_or_else(|| format!("missing {name}"))?
        .to_string_lossy()
        .parse()
        .map_err(|e| format!("invalid {name}: {e}"))
}

fn meta_path(corpus: &Path) -> PathBuf {
    let mut path = corpus.as_os_str().to_os_string();
    path.push(".meta.json");
    PathBuf::from(path)
}

fn deterministic_keys(index: usize) -> Keys {
    Keys::parse(&format!("{:064x}", index + 1)).expect("nonzero deterministic secret")
}

fn tag(name: &str, value: impl Into<String>) -> Tag {
    Tag::parse([name.to_owned(), value.into()]).expect("two-field tag")
}

fn event_for(ordinal: u64, payload_bytes: usize, authors: &[Keys]) -> Event {
    let author = if ordinal.is_multiple_of(4) {
        0
    } else {
        ordinal as usize % AUTHORS
    };
    let kind = if ordinal.is_multiple_of(5) {
        Kind::from(42u16)
    } else {
        Kind::from(9u16)
    };
    let room = if ordinal.is_multiple_of(10) {
        "hot-room".to_owned()
    } else {
        format!("room-{}", ordinal % 4096)
    };
    let mut tags = vec![tag("h", room)];
    if ordinal.is_multiple_of(4) {
        tags.push(tag(
            "p",
            authors[(author + 1) % AUTHORS].public_key().to_hex(),
        ));
    }
    let prefix = format!("ordinal={ordinal} author={author} ");
    let mut content = prefix;
    if content.len() < payload_bytes {
        content.extend(std::iter::repeat_n('x', payload_bytes - content.len()));
    } else {
        content.truncate(payload_bytes);
    }
    EventBuilder::new(kind, content)
        .tags(tags)
        .custom_created_at(Timestamp::from(BASE_CREATED_AT + ordinal))
        .sign_with_keys(&authors[author])
        .expect("sign corpus event")
}

fn generate(
    path: &Path,
    events: u64,
    payload_bytes: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    if events == 0 {
        return Err("events must be nonzero".into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let authors: Vec<_> = (0..AUTHORS).map(deterministic_keys).collect();
    let file = File::create(path)?;
    let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, file);
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0u64;
    let mut first_id = None;
    let mut last_id = None;
    let chunk_size = 4096u64;
    for start in (0..events).step_by(chunk_size as usize) {
        let end = events.min(start + chunk_size);
        let generated: Vec<_> = (start..end)
            .into_par_iter()
            .map(|ordinal| {
                let event = event_for(ordinal, payload_bytes, &authors);
                (event.id.to_hex(), event.as_json())
            })
            .collect();
        for (id, json) in generated {
            first_id.get_or_insert_with(|| id.clone());
            last_id = Some(id);
            hasher.update(json.as_bytes());
            hasher.update(b"\n");
            writer.write_all(json.as_bytes())?;
            writer.write_all(b"\n")?;
            bytes += json.len() as u64 + 1;
        }
        if end.is_multiple_of(100_000) || end == events {
            eprintln!("generated={end}/{events}");
        }
    }
    writer.flush()?;
    let meta = CorpusMeta {
        schema: CORPUS_SCHEMA.to_owned(),
        events,
        payload_bytes,
        bytes,
        blake3: hasher.finalize().to_hex().to_string(),
        author0: authors[0].public_key().to_hex(),
        first_id: first_id.expect("nonempty corpus"),
        last_id: last_id.expect("nonempty corpus"),
    };
    write_json(&meta_path(path), &meta)?;
    println!("{}", serde_json::to_string_pretty(&meta)?);
    Ok(())
}

fn describe_corpus(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let corpus = unsafe { MmapOptions::new().map(&file)? };
    let events = parse_prevalidated_events(&corpus)?;
    let first = events.first().ok_or("corpus must not be empty")?;
    let last = events.last().expect("nonempty corpus has a last event");
    let meta = CorpusMeta {
        schema: CORPUS_SCHEMA.to_owned(),
        events: events.len() as u64,
        payload_bytes: 0,
        bytes: corpus.len() as u64,
        blake3: blake3::hash(&corpus).to_hex().to_string(),
        author0: first.pubkey.to_hex(),
        first_id: first.id.to_hex(),
        last_id: last.id.to_hex(),
    };
    write_json(&meta_path(path), &meta)?;
    println!("{}", serde_json::to_string_pretty(&meta)?);
    Ok(())
}

fn open_corpus(path: &Path) -> Result<(Mmap, CorpusMeta), Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let mmap = unsafe { MmapOptions::new().map(&file)? };
    let meta: CorpusMeta = serde_json::from_slice(&fs::read(meta_path(path))?)?;
    if meta.schema != CORPUS_SCHEMA || meta.bytes != mmap.len() as u64 {
        return Err("corpus metadata mismatch".into());
    }
    let hash = blake3::hash(&mmap).to_hex().to_string();
    if hash != meta.blake3 {
        return Err("corpus hash mismatch".into());
    }
    Ok((mmap, meta))
}

fn nmp_ingest_pass(
    path: &Path,
    corpus: &Mmap,
    events: u64,
    batch_size: usize,
    workers: usize,
    duplicate: bool,
) -> Result<PhaseResult, Box<dyn std::error::Error>> {
    let mut store = RedbStore::open(path)?;
    let relay = RelayUrl::parse("wss://direct-benchmark.invalid")?;
    let observed = Timestamp::from(BASE_CREATED_AT + events + 1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()?;
    let rss_before = current_rss_bytes();
    let started = Instant::now();
    let mut parse_verify_elapsed = Duration::ZERO;
    let mut store_commit_elapsed = Duration::ZERO;
    let mut lines = corpus
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty());
    let mut seen = 0u64;
    loop {
        let raw: Vec<_> = lines.by_ref().take(batch_size).collect();
        if raw.is_empty() {
            break;
        }
        let parse_started = Instant::now();
        let parsed: Result<Vec<Event>, String> = pool.install(|| {
            raw.par_iter()
                .map(|json| {
                    let event = Event::from_json(json).map_err(|e| format!("parse: {e:?}"))?;
                    event.verify().map_err(|e| format!("verify: {e}"))?;
                    Ok(event)
                })
                .collect()
        });
        parse_verify_elapsed += parse_started.elapsed();
        let rows: Vec<_> = parsed?
            .into_iter()
            .map(|event| (event, RelayObserved::new(relay.clone(), observed)))
            .collect();
        let store_started = Instant::now();
        let outcomes = store.insert_batch(rows)?;
        store_commit_elapsed += store_started.elapsed();
        for outcome in outcomes {
            match (duplicate, outcome) {
                (false, InsertOutcome::Inserted) => {}
                (true, InsertOutcome::Duplicate { .. }) => {}
                (_, other) => return Err(format!("unexpected NMP outcome: {other:?}").into()),
            }
            seen += 1;
        }
    }
    drop(store);
    let elapsed = started.elapsed();
    if seen != events {
        return Err(format!("NMP processed {seen}, expected {events}").into());
    }
    Ok(phase(
        events,
        elapsed.as_secs_f64(),
        rss_before,
        Some(parse_verify_elapsed),
        Some(store_commit_elapsed),
    ))
}

fn run_nmp(
    corpus_path: &Path,
    database: &Path,
    batch_size: usize,
    workers: usize,
    iterations: usize,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if batch_size == 0 || workers == 0 || iterations == 0 {
        return Err("batch size, workers, and iterations must be nonzero".into());
    }
    let (corpus, meta) = open_corpus(corpus_path)?;
    if database.exists() {
        fs::remove_file(database)?;
    }
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent)?;
    }
    let ingest = nmp_ingest_pass(database, &corpus, meta.events, batch_size, workers, false)?;
    let duplicate_ingest =
        nmp_ingest_pass(database, &corpus, meta.events, batch_size, workers, true)?;
    let store = RedbStore::open(database)?;
    let queries = query_specs(&meta)
        .into_iter()
        .map(|(name, json)| nmp_query(&store, name, json, iterations))
        .collect::<Result<Vec<_>, _>>()?;
    drop(store);
    let reopen = reopen_nmp(database, iterations.min(10))?;
    let (logical, allocated) = path_size(database)?;
    let result = BackendResult {
        schema: SCHEMA.to_owned(),
        backend: "nmp-v2".to_owned(),
        nmp_commit: git_commit(repo_root()),
        nostrdb_commit: nostrdb_commit(),
        nostrdb_secp256k1: secp_version(),
        host: host(),
        corpus: meta,
        workers,
        transaction_batch_size: Some(batch_size),
        ingest,
        duplicate_ingest,
        canonical_events: count_lines(&corpus) as u64,
        database_logical_bytes: logical,
        database_allocated_bytes: allocated,
        healthy_reopen_p50_ms: reopen,
        queries,
    };
    write_json(output, &result)?;
    println!("wrote {}", output.display());
    Ok(())
}

fn run_nostrdb(
    corpus_path: &Path,
    database: &Path,
    workers: usize,
    iterations: usize,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if workers == 0 || iterations == 0 {
        return Err("workers and iterations must be nonzero".into());
    }
    let (corpus, meta) = open_corpus(corpus_path)?;
    if database.exists() {
        fs::remove_dir_all(database)?;
    }
    fs::create_dir_all(database)?;
    let handle = NdbHandle::open(database, workers)?;
    let rss_before = current_rss_bytes();
    let started = Instant::now();
    if let Err(error) = handle.ingest(&corpus) {
        std::mem::forget(handle);
        write_failure(output, &meta, workers, "initial_ingest", &error, None)?;
        return Err(error.into());
    }
    handle.close();
    let ingest = phase(
        meta.events,
        started.elapsed().as_secs_f64(),
        rss_before,
        None,
        None,
    );

    let handle = NdbHandle::open(database, workers)?;
    let rss_before = current_rss_bytes();
    let started = Instant::now();
    if let Err(error) = handle.ingest(&corpus) {
        std::mem::forget(handle);
        write_failure(output, &meta, workers, "duplicate_ingest", &error, None)?;
        return Err(error.into());
    }
    handle.close();
    let duplicate_ingest = phase(
        meta.events,
        started.elapsed().as_secs_f64(),
        rss_before,
        None,
        None,
    );

    let handle = NdbHandle::open(database, workers)?;
    let canonical_events = handle.note_count()?;
    if canonical_events != meta.events {
        let error = format!(
            "nostrdb stored {canonical_events}, expected {}",
            meta.events
        );
        write_failure(
            output,
            &meta,
            workers,
            "post_ingest_cardinality",
            &error,
            Some(canonical_events),
        )?;
        return Err(format!(
            "nostrdb stored {canonical_events}, expected {}",
            meta.events
        )
        .into());
    }
    let queries = query_specs(&meta)
        .into_iter()
        .map(|(name, json)| ndb_query(&handle, name, json, iterations))
        .collect::<Result<Vec<_>, _>>()?;
    handle.close();
    let reopen = reopen_ndb(database, workers, iterations.min(10))?;
    let (logical, allocated) = path_size(database)?;
    let result = BackendResult {
        schema: SCHEMA.to_owned(),
        backend: "nostrdb".to_owned(),
        nmp_commit: git_commit(repo_root()),
        nostrdb_commit: nostrdb_commit(),
        nostrdb_secp256k1: secp_version(),
        host: host(),
        corpus: meta,
        workers,
        transaction_batch_size: None,
        ingest,
        duplicate_ingest,
        canonical_events,
        database_logical_bytes: logical,
        database_allocated_bytes: allocated,
        healthy_reopen_p50_ms: reopen,
        queries,
    };
    write_json(output, &result)?;
    println!("wrote {}", output.display());
    Ok(())
}

fn write_failure(
    output: &Path,
    corpus: &CorpusMeta,
    workers: usize,
    stage: &str,
    error: &str,
    canonical_events_after_failure: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    write_json(
        output,
        &BackendFailure {
            schema: "nmp-nostrdb-direct-failure-v1",
            backend: "nostrdb",
            nmp_commit: git_commit(repo_root()),
            nostrdb_commit: nostrdb_commit(),
            nostrdb_secp256k1: secp_version(),
            host: host(),
            corpus,
            workers,
            stage,
            error: error.to_owned(),
            canonical_events_after_failure,
        },
    )
}

fn query_specs(meta: &CorpusMeta) -> Vec<(&'static str, String)> {
    vec![
        ("global_top_200", format!(r#"{{"limit":{QUERY_LIMIT}}}"#)),
        (
            "kind9_top_200",
            format!(r#"{{"kinds":[9],"limit":{QUERY_LIMIT}}}"#),
        ),
        (
            "hot_tag_top_200",
            format!(r##"{{"#h":["hot-room"],"limit":{QUERY_LIMIT}}}"##),
        ),
        (
            "author0_top_200",
            format!(
                r#"{{"authors":["{}"],"limit":{QUERY_LIMIT}}}"#,
                meta.author0
            ),
        ),
        (
            "author0_kind9_top_200",
            format!(
                r#"{{"authors":["{}"],"kinds":[9],"limit":{QUERY_LIMIT}}}"#,
                meta.author0
            ),
        ),
        (
            "exact_first_id",
            format!(r#"{{"ids":["{}"],"limit":{QUERY_LIMIT}}}"#, meta.first_id),
        ),
    ]
}

fn nmp_query(
    store: &RedbStore,
    name: &str,
    filter_json: String,
    iterations: usize,
) -> Result<QueryResult, Box<dyn std::error::Error>> {
    let filter = Filter::from_json(&filter_json)?;
    store.reset_query_work();
    let expected: Vec<_> = store
        .query_newest_ids(&filter, QUERY_LIMIT)?
        .into_iter()
        .map(|id| id.to_hex())
        .collect();
    let expected_work = store.query_work();
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        store.reset_query_work();
        let started = Instant::now();
        let ids: Vec<_> = store
            .query_newest_ids(&filter, QUERY_LIMIT)?
            .into_iter()
            .map(|id| id.to_hex())
            .collect();
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        if ids != expected {
            return Err(format!("unstable NMP query {name}").into());
        }
        if store.query_work() != expected_work {
            return Err(format!("unstable NMP query work {name}").into());
        }
    }
    let (p50, p95) = percentiles(&mut samples);
    Ok(QueryResult {
        name: name.to_owned(),
        filter_json,
        rows: expected.len(),
        ids: expected,
        p50_ms: p50,
        p95_ms: p95,
        index_rows: Some(expected_work.0),
        event_values: Some(expected_work.1),
        owned_events_materialized: Some(expected_work.2),
    })
}

fn ndb_query(
    handle: &NdbHandle,
    name: &str,
    filter_json: String,
    iterations: usize,
) -> Result<QueryResult, Box<dyn std::error::Error>> {
    let filter = CString::new(filter_json.clone())?;
    let run = || -> Result<Vec<String>, String> {
        let mut ids = vec![0u8; QUERY_LIMIT * 32];
        let mut created_at = vec![0u32; QUERY_LIMIT];
        let mut count = 0i32;
        let ok = unsafe {
            bench_ndb_query(
                handle.0,
                filter.as_ptr(),
                QUERY_LIMIT as c_int,
                ids.as_mut_ptr(),
                created_at.as_mut_ptr(),
                &mut count,
            )
        };
        if ok == 0 || count < 0 || count as usize > QUERY_LIMIT {
            return Err(format!("nostrdb query {name} failed"));
        }
        Ok(ids[..count as usize * 32].chunks(32).map(hex).collect())
    };
    let expected = run()?;
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let ids = run()?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        if ids != expected {
            return Err(format!("unstable nostrdb query {name}").into());
        }
    }
    let (p50, p95) = percentiles(&mut samples);
    Ok(QueryResult {
        name: name.to_owned(),
        filter_json,
        rows: expected.len(),
        ids: expected,
        p50_ms: p50,
        p95_ms: p95,
        index_rows: None,
        event_values: None,
        owned_events_materialized: None,
    })
}

fn compare(
    nmp_path: &Path,
    ndb_path: &Path,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let nmp: BackendResult = serde_json::from_slice(&fs::read(nmp_path)?)?;
    let ndb: BackendResult = serde_json::from_slice(&fs::read(ndb_path)?)?;
    if nmp.schema != SCHEMA
        || ndb.schema != SCHEMA
        || nmp.backend != "nmp-v2"
        || ndb.backend != "nostrdb"
    {
        return Err("result schema/backend mismatch".into());
    }
    if nmp.corpus.blake3 != ndb.corpus.blake3 || nmp.corpus.events != ndb.corpus.events {
        return Err("results used different corpora".into());
    }
    if nmp.canonical_events != ndb.canonical_events || nmp.canonical_events != nmp.corpus.events {
        return Err("canonical event count mismatch".into());
    }
    if nmp.queries.len() != ndb.queries.len() {
        return Err("query matrix length mismatch".into());
    }
    let mut equivalent_queries = Vec::new();
    let mut mismatched_queries = Vec::new();
    for (left, right) in nmp.queries.iter().zip(&ndb.queries) {
        if left.name != right.name {
            return Err(format!("query name mismatch for {} / {}", left.name, right.name).into());
        }
        if left.ids == right.ids {
            equivalent_queries.push(left.name.clone());
        } else {
            mismatched_queries.push(QueryMismatch {
                name: left.name.clone(),
                nmp_rows: left.rows,
                nostrdb_rows: right.rows,
                nmp_first_id: left.ids.first().cloned(),
                nostrdb_first_id: right.ids.first().cloned(),
            });
        }
    }
    let result = Comparison {
        schema: SCHEMA,
        nmp_commit: nmp.nmp_commit,
        nostrdb_commit: ndb.nostrdb_commit,
        corpus_blake3: nmp.corpus.blake3,
        events: nmp.corpus.events,
        queries_equal: mismatched_queries.is_empty(),
        equivalent_queries,
        mismatched_queries,
        nmp_ingest_events_per_second: nmp.ingest.events_per_second,
        nostrdb_ingest_events_per_second: ndb.ingest.events_per_second,
        nostrdb_to_nmp_ingest_throughput_ratio: ndb.ingest.events_per_second
            / nmp.ingest.events_per_second,
        nmp_duplicate_events_per_second: nmp.duplicate_ingest.events_per_second,
        nostrdb_duplicate_events_per_second: ndb.duplicate_ingest.events_per_second,
        nostrdb_to_nmp_duplicate_throughput_ratio: ndb.duplicate_ingest.events_per_second
            / nmp.duplicate_ingest.events_per_second,
        nmp_database_logical_bytes: nmp.database_logical_bytes,
        nostrdb_database_logical_bytes: ndb.database_logical_bytes,
        nmp_to_nostrdb_logical_size_ratio: nmp.database_logical_bytes as f64
            / ndb.database_logical_bytes as f64,
    };
    write_json(output, &result)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn phase(
    events: u64,
    elapsed_seconds: f64,
    rss_before: Option<u64>,
    parse_verify: Option<Duration>,
    store_commit: Option<Duration>,
) -> PhaseResult {
    PhaseResult {
        elapsed_ms: elapsed_seconds * 1000.0,
        events_per_second: events as f64 / elapsed_seconds,
        rss_before_bytes: rss_before,
        peak_rss_bytes: peak_rss_bytes(),
        parse_verify_ms: parse_verify.map(|elapsed| elapsed.as_secs_f64() * 1000.0),
        store_commit_ms: store_commit.map(|elapsed| elapsed.as_secs_f64() * 1000.0),
    }
}

fn percentiles(samples: &mut [f64]) -> (f64, f64) {
    samples.sort_by(f64::total_cmp);
    let last = samples.len() - 1;
    (samples[last * 50 / 100], samples[last * 95 / 100])
}

fn count_lines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(out, "{byte:02x}").expect("write string");
    }
    out
}

fn reopen_nmp(path: &Path, iterations: usize) -> Result<f64, Box<dyn std::error::Error>> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        drop(RedbStore::open(path)?);
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(percentiles(&mut samples).0)
}

fn reopen_ndb(
    path: &Path,
    workers: usize,
    iterations: usize,
) -> Result<f64, Box<dyn std::error::Error>> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        NdbHandle::open(path, workers)?.close();
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(percentiles(&mut samples).0)
}

fn path_size(path: &Path) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        return Ok((metadata.len(), metadata.blocks() * 512));
    }
    let mut logical = 0;
    let mut allocated = 0;
    for entry in fs::read_dir(path)? {
        let (child_logical, child_allocated) = path_size(&entry?.path())?;
        logical += child_logical;
        allocated += child_allocated;
    }
    Ok((logical, allocated))
}

fn current_rss_bytes() -> Option<u64> {
    proc_status_kib("VmRSS").map(|value| value * 1024)
}

fn process_write_bytes() -> Option<u64> {
    fs::read_to_string("/proc/self/io")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("write_bytes:")?.trim().parse().ok())
}

fn process_cpu_ns() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(rc, 0, "getrusage(RUSAGE_SELF) must succeed");
    let usage = unsafe { usage.assume_init() };
    let timeval_ns = |value: libc::timeval| {
        (value.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add((value.tv_usec as u64).saturating_mul(1_000))
    };
    timeval_ns(usage.ru_utime).saturating_add(timeval_ns(usage.ru_stime))
}

fn sample_process() -> StoreBenchProcessCounters {
    StoreBenchProcessCounters {
        cpu_ns: process_cpu_ns(),
        allocation_ops: ALLOCATION_OPS.load(Ordering::Relaxed),
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
        current_rss_bytes: current_rss_bytes(),
        peak_rss_bytes: peak_rss_bytes(),
        process_write_bytes: process_write_bytes(),
    }
}

fn duration_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

fn peak_rss_bytes() -> Option<u64> {
    proc_status_kib("VmHWM").map(|value| value * 1024)
}

fn proc_status_kib(key: &str) -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name == key)
            .then(|| value.split_whitespace().next()?.parse().ok())
            .flatten()
    })
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{}\n", serde_json::to_string_pretty(value)?))?;
    Ok(())
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn nostrdb_root() -> PathBuf {
    PathBuf::from(env!("NOSTRDB_DIR_PINNED"))
}

fn git_commit(path: PathBuf) -> String {
    command_output(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"]),
    )
}

fn git_dirty(path: PathBuf) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .map(|output| !output.stdout.is_empty())
        .unwrap_or(true)
}

fn nostrdb_commit() -> String {
    git_commit(nostrdb_root())
}

fn secp_version() -> String {
    git_commit(nostrdb_root().join("deps/secp256k1"))
}

fn host() -> String {
    command_output(Command::new("uname").arg("-a"))
}

fn command_output(command: &mut Command) -> String {
    command
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}
