use std::env;
use std::ffi::{c_char, c_int, c_void, CString};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use memmap2::{Mmap, MmapOptions};
use nmp_store::{EventStore, InsertOutcome, RedbStore, RelayObserved};
use nostr::{Event, EventBuilder, Filter, JsonUtil, Keys, Kind, RelayUrl, Tag, Timestamp};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

const SCHEMA: &str = "nmp-nostrdb-direct-v1";
const CORPUS_SCHEMA: &str = "nmp-nostrdb-corpus-v1";
const BASE_CREATED_AT: u64 = 1_700_000_000;
const AUTHORS: usize = 64;
const QUERY_LIMIT: usize = 200;
const NOSTRDB_MAP_SIZE: u64 = 32 * 1024 * 1024 * 1024;

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
        .ok_or("usage: nmp-nostrdb-compare <generate|run-nmp|run-nostrdb|compare> ...")?;
    match command.to_string_lossy().as_ref() {
        "generate" => {
            let corpus = path_arg(&mut args, "corpus")?;
            let events = number_arg::<u64>(&mut args, "events")?;
            let payload = number_arg::<usize>(&mut args, "payload-bytes")?;
            generate(&corpus, events, payload)?;
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
        other => return Err(format!("unknown command {other}").into()),
    }
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
    let warm = store.query_newest(&filter, QUERY_LIMIT)?;
    let expected: Vec<_> = warm.iter().map(|row| row.event.id.to_hex()).collect();
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let rows = store.query_newest(&filter, QUERY_LIMIT)?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        let ids: Vec<_> = rows.iter().map(|row| row.event.id.to_hex()).collect();
        if ids != expected {
            return Err(format!("unstable NMP query {name}").into());
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
