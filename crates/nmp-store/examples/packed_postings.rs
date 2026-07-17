//! Fresh-process Redb/Fjall packed-postings ceiling matrix for issue #648.
//!
//! Usage:
//! `cargo run -p nmp-store --release --features bench-instrumentation --example packed_postings -- matrix <events.jsonl> <output.json> [repetitions] [batch_size]`

use std::alloc::{GlobalAlloc, Layout as AllocLayout, System};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use nmp_store::{
    run_packed_postings_bench, run_store_bench_variant, PackedPostingsBackend,
    PackedPostingsMetrics, StoreBenchMetrics, StoreBenchProcessCounters, StoreBenchVariant,
};
use nostr::{Event, JsonUtil};
use serde::{Deserialize, Serialize};

struct CountingAllocator;

static ALLOCATION_OPS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: AllocLayout) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: AllocLayout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: AllocLayout, new_size: usize) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

const SCHEMA: &str = "nmp-packed-postings-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Layout {
    RowRedb,
    PackedRedb,
    PackedFjall,
}

impl Layout {
    fn name(self) -> &'static str {
        match self {
            Self::RowRedb => "row_redb",
            Self::PackedRedb => "packed_redb",
            Self::PackedFjall => "packed_fjall",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        [Self::RowRedb, Self::PackedRedb, Self::PackedFjall]
            .into_iter()
            .find(|layout| layout.name() == value)
            .ok_or_else(|| format!("unknown packed-postings layout {value}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CorpusIdentity {
    path: String,
    bytes: u64,
    blake3: String,
    events: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum Metrics {
    Row(StoreBenchMetrics),
    Packed(PackedPostingsMetrics),
}

impl Metrics {
    fn events(&self) -> u64 {
        match self {
            Self::Row(metrics) => metrics.events,
            Self::Packed(metrics) => metrics.events,
        }
    }

    fn wall_ns(&self) -> u64 {
        match self {
            Self::Row(metrics) => metrics.wall_ns,
            Self::Packed(metrics) => metrics.wall_ns,
        }
    }

    fn exact_reopen(&self) -> bool {
        match self {
            Self::Row(metrics) => metrics.exact_reopen,
            Self::Packed(metrics) => metrics.exact_reopen,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunRecord {
    schema: String,
    nmp_commit: String,
    git_dirty: bool,
    host: String,
    corpus: CorpusIdentity,
    layout: Layout,
    repetition: usize,
    ordinal: usize,
    metrics: Metrics,
    events_per_second: f64,
    database_allocated_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct MatrixRecord {
    schema: String,
    command: String,
    nmp_commit: String,
    git_dirty: bool,
    host: String,
    corpus: CorpusIdentity,
    repetitions: usize,
    transaction_batch_size: usize,
    alternating_order: bool,
    runs: Vec<RunRecord>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonical repository root")
}

fn command_output(args: &[&str]) -> String {
    Command::new(args[0])
        .args(&args[1..])
        .current_dir(repo_root())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn git_commit() -> String {
    command_output(&["git", "rev-parse", "HEAD"])
}

fn git_dirty() -> bool {
    !command_output(&["git", "status", "--porcelain"]).is_empty()
}

fn host() -> String {
    format!(
        "{}-{}-{}",
        command_output(&["hostname"]),
        env::consts::OS,
        env::consts::ARCH
    )
}

fn proc_status_kib(name: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            let value = line.strip_prefix(name)?.trim().strip_suffix(" kB")?;
            value.trim().parse().ok()
        })
}

fn process_cpu_ns() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(rc, 0, "getrusage(RUSAGE_SELF) must succeed");
    let usage = unsafe { usage.assume_init() };
    let ns = |value: libc::timeval| {
        (value.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add((value.tv_usec as u64).saturating_mul(1_000))
    };
    ns(usage.ru_utime).saturating_add(ns(usage.ru_stime))
}

fn process_write_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/self/io")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("write_bytes:")?.trim().parse().ok())
}

fn sample_process() -> StoreBenchProcessCounters {
    StoreBenchProcessCounters {
        cpu_ns: process_cpu_ns(),
        allocation_ops: ALLOCATION_OPS.load(Ordering::Relaxed),
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
        current_rss_bytes: proc_status_kib("VmRSS:").map(|value| value * 1024),
        peak_rss_bytes: proc_status_kib("VmHWM:").map(|value| value * 1024),
        process_write_bytes: process_write_bytes(),
    }
}

fn load_corpus(path: &Path) -> Result<(Vec<Event>, CorpusIdentity), String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut hasher = blake3::Hasher::new();
    let mut events = Vec::new();
    let mut bytes = 0u64;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        bytes = bytes.saturating_add(read as u64);
        hasher.update(&line);
        while line
            .last()
            .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
        {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        let event = Event::from_json(line.as_slice())
            .map_err(|error| format!("parse event {}: {error:?}", events.len()))?;
        event
            .verify()
            .map_err(|error| format!("verify event {}: {error}", events.len()))?;
        events.push(event);
    }
    let identity = CorpusIdentity {
        path: path.display().to_string(),
        bytes,
        blake3: hasher.finalize().to_hex().to_string(),
        events: events.len() as u64,
    };
    Ok((events, identity))
}

fn run_child(
    corpus_path: &Path,
    layout: Layout,
    batch_size: usize,
    repetition: usize,
    ordinal: usize,
) -> Result<RunRecord, String> {
    let (events, corpus) = load_corpus(corpus_path)?;
    let scratch = tempfile::tempdir().map_err(|error| error.to_string())?;
    let database = match layout {
        Layout::PackedFjall => scratch.path().join("store.fjall"),
        _ => scratch.path().join("store.redb"),
    };
    let metrics = match layout {
        Layout::RowRedb => Metrics::Row(run_store_bench_variant(
            &database,
            events,
            batch_size,
            StoreBenchVariant::AllIndexesSampledCardinality,
            sample_process,
        )?),
        Layout::PackedRedb => Metrics::Packed(run_packed_postings_bench(
            PackedPostingsBackend::Redb,
            &database,
            events,
            batch_size,
            sample_process,
        )?),
        Layout::PackedFjall => Metrics::Packed(run_packed_postings_bench(
            PackedPostingsBackend::Fjall,
            &database,
            events,
            batch_size,
            sample_process,
        )?),
    };
    if !metrics.exact_reopen() {
        return Err(format!("{} failed exact reopen", layout.name()));
    }
    let database_allocated_bytes = allocated_path_bytes(&database)?;
    let events_per_second = metrics.events() as f64 * 1_000_000_000.0 / metrics.wall_ns() as f64;
    Ok(RunRecord {
        schema: SCHEMA.to_owned(),
        nmp_commit: git_commit(),
        git_dirty: git_dirty(),
        host: host(),
        corpus,
        layout,
        repetition,
        ordinal,
        metrics,
        events_per_second,
        database_allocated_bytes,
    })
}

fn allocated_path_bytes(path: &Path) -> Result<u64, String> {
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if metadata.is_dir() {
        let mut total = 0u64;
        for entry in std::fs::read_dir(path).map_err(|error| error.to_string())? {
            total = total.saturating_add(allocated_path_bytes(
                &entry.map_err(|error| error.to_string())?.path(),
            )?);
        }
        Ok(total)
    } else {
        Ok(metadata.blocks() * 512)
    }
}

fn run_matrix(
    corpus_path: &Path,
    output: &Path,
    repetitions: usize,
    batch_size: usize,
) -> Result<(), String> {
    if repetitions == 0 || batch_size == 0 {
        return Err("repetitions and batch size must be nonzero".to_owned());
    }
    let current_exe = env::current_exe().map_err(|error| error.to_string())?;
    let mut runs: Vec<RunRecord> = Vec::new();
    for repetition in 0..repetitions {
        let mut layouts = [Layout::RowRedb, Layout::PackedRedb, Layout::PackedFjall];
        if repetition % 2 == 1 {
            layouts.reverse();
        }
        for (ordinal, layout) in layouts.into_iter().enumerate() {
            eprintln!(
                "repetition={repetition} ordinal={ordinal} layout={}",
                layout.name()
            );
            let child = Command::new(&current_exe)
                .arg("run")
                .arg(corpus_path)
                .arg(layout.name())
                .arg(batch_size.to_string())
                .arg(repetition.to_string())
                .arg(ordinal.to_string())
                .output()
                .map_err(|error| error.to_string())?;
            if !child.status.success() {
                return Err(format!(
                    "child failed for {}: {}",
                    layout.name(),
                    String::from_utf8_lossy(&child.stderr)
                ));
            }
            runs.push(
                serde_json::from_slice(&child.stdout)
                    .map_err(|error| format!("decode child result: {error}"))?,
            );
        }
    }
    let first = runs
        .first()
        .ok_or_else(|| "matrix produced no runs".to_owned())?;
    let matrix = MatrixRecord {
        schema: SCHEMA.to_owned(),
        command: "matrix".to_owned(),
        nmp_commit: first.nmp_commit.clone(),
        git_dirty: runs.iter().any(|run| run.git_dirty),
        host: first.host.clone(),
        corpus: first.corpus.clone(),
        repetitions,
        transaction_batch_size: batch_size,
        alternating_order: true,
        runs,
    };
    std::fs::write(
        output,
        serde_json::to_vec_pretty(&matrix).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn main() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("run") => {
            let corpus = Path::new(args.get(2).ok_or("missing corpus")?);
            let layout = Layout::parse(args.get(3).ok_or("missing layout")?)?;
            let batch_size = args
                .get(4)
                .ok_or("missing batch size")?
                .parse()
                .map_err(|error| format!("invalid batch size: {error}"))?;
            let repetition = args
                .get(5)
                .ok_or("missing repetition")?
                .parse()
                .map_err(|error| format!("invalid repetition: {error}"))?;
            let ordinal = args
                .get(6)
                .ok_or("missing ordinal")?
                .parse()
                .map_err(|error| format!("invalid ordinal: {error}"))?;
            let record = run_child(corpus, layout, batch_size, repetition, ordinal)?;
            println!(
                "{}",
                serde_json::to_string(&record).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        Some("matrix") => {
            let corpus = Path::new(args.get(2).ok_or("missing corpus")?);
            let output = Path::new(args.get(3).ok_or("missing output")?);
            let repetitions = args
                .get(4)
                .map(String::as_str)
                .unwrap_or("5")
                .parse()
                .map_err(|error| format!("invalid repetitions: {error}"))?;
            let batch_size = args
                .get(5)
                .map(String::as_str)
                .unwrap_or("4096")
                .parse()
                .map_err(|error| format!("invalid batch size: {error}"))?;
            run_matrix(corpus, output, repetitions, batch_size)
        }
        _ => Err(
            "usage: packed_postings matrix <events.jsonl> <output.json> [repetitions] [batch_size]"
                .to_owned(),
        ),
    }
}
