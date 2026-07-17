//! Issue #637 equivalent-work comparison of per-index Redb trees against one
//! collision-free prefixed index tree.

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use nmp_store::{
    prepare_equivalent_store_corpus, run_prepared_redb_store_bench,
    run_prepared_redb_unified_index_bench, StoreBenchPreparedMetrics, StoreBenchProcessCounters,
};
use nostr::{Event, JsonUtil};
use serde::{Deserialize, Serialize};

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

const SCHEMA: &str = "nmp-redb-index-layout-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IndexLayout {
    PerIndexTrees,
    UnifiedIndexTree,
}

impl IndexLayout {
    fn name(self) -> &'static str {
        match self {
            Self::PerIndexTrees => "per_index_trees",
            Self::UnifiedIndexTree => "unified_index_tree",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        [Self::PerIndexTrees, Self::UnifiedIndexTree]
            .into_iter()
            .find(|layout| layout.name() == value)
            .ok_or_else(|| format!("unknown index layout: {value}"))
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
struct RunRecord {
    schema: String,
    nmp_commit: String,
    git_dirty: bool,
    host: String,
    corpus: CorpusIdentity,
    layout: IndexLayout,
    repetition: usize,
    ordinal: usize,
    metrics: StoreBenchPreparedMetrics,
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
    transaction_batch_size: usize,
    repetitions: usize,
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
    let timeval_ns = |value: libc::timeval| {
        (value.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add((value.tv_usec as u64).saturating_mul(1_000))
    };
    timeval_ns(usage.ru_utime).saturating_add(timeval_ns(usage.ru_stime))
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
    layout: IndexLayout,
    batch_size: usize,
    repetition: usize,
    ordinal: usize,
) -> Result<RunRecord, String> {
    let (events, corpus_identity) = load_corpus(corpus_path)?;
    let prepared = prepare_equivalent_store_corpus(&events, batch_size)?;
    let scratch = tempfile::tempdir().map_err(|error| error.to_string())?;
    let database = scratch.path().join("store.redb");
    let metrics = match layout {
        IndexLayout::PerIndexTrees => {
            run_prepared_redb_store_bench(&database, &prepared, sample_process)?
        }
        IndexLayout::UnifiedIndexTree => {
            run_prepared_redb_unified_index_bench(&database, &prepared, sample_process)?
        }
    };
    if !metrics.exact_reopen {
        return Err(format!("{} failed exact logical reopen", layout.name()));
    }
    let database_allocated_bytes = std::fs::metadata(&database)
        .map_err(|error| error.to_string())?
        .blocks()
        * 512;
    let events_per_second = metrics.events as f64 * 1_000_000_000.0 / metrics.wall_ns as f64;
    Ok(RunRecord {
        schema: SCHEMA.to_owned(),
        nmp_commit: git_commit(),
        git_dirty: git_dirty(),
        host: host(),
        corpus: corpus_identity,
        layout,
        repetition,
        ordinal,
        metrics,
        events_per_second,
        database_allocated_bytes,
    })
}

fn run_matrix(
    corpus_path: &Path,
    output: &Path,
    batch_size: usize,
    repetitions: usize,
) -> Result<(), String> {
    if batch_size == 0 || repetitions == 0 {
        return Err("batch size and repetitions must be nonzero".to_owned());
    }
    let current_exe = env::current_exe().map_err(|error| error.to_string())?;
    let mut runs = Vec::new();
    for repetition in 0..repetitions {
        let mut layouts = [IndexLayout::PerIndexTrees, IndexLayout::UnifiedIndexTree];
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
                serde_json::from_slice::<RunRecord>(&child.stdout)
                    .map_err(|error| format!("decode child result: {error}"))?,
            );
        }
    }
    let first = runs
        .first()
        .ok_or_else(|| "matrix produced no runs".to_owned())?;
    if runs.iter().any(|run| {
        run.corpus.blake3 != first.corpus.blake3
            || run.corpus.events != first.corpus.events
            || run.nmp_commit != first.nmp_commit
            || run.git_dirty != first.git_dirty
    }) {
        return Err("matrix child identity changed during the run".to_owned());
    }
    let record = MatrixRecord {
        schema: SCHEMA.to_owned(),
        command: format!(
            "cargo run -q -p nmp-store --release --features bench-instrumentation --example redb_index_layout -- matrix {} {} {} {}",
            corpus_path.display(),
            output.display(),
            batch_size,
            repetitions
        ),
        nmp_commit: first.nmp_commit.clone(),
        git_dirty: first.git_dirty,
        host: first.host.clone(),
        corpus: first.corpus.clone(),
        transaction_batch_size: batch_size,
        repetitions,
        alternating_order: true,
        runs,
    };
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        output,
        serde_json::to_vec_pretty(&record).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    println!("wrote {}", output.display());
    Ok(())
}

fn main() -> Result<(), String> {
    let mut args = env::args_os().skip(1);
    let command = args.next().ok_or_else(|| {
        "usage: redb_index_layout <run|matrix> <corpus> <layout|output> ...".to_owned()
    })?;
    match command.to_string_lossy().as_ref() {
        "run" => {
            let corpus = PathBuf::from(
                args.next()
                    .ok_or_else(|| "run requires corpus path".to_owned())?,
            );
            let layout = IndexLayout::parse(
                &args
                    .next()
                    .ok_or_else(|| "run requires layout".to_owned())?
                    .to_string_lossy(),
            )?;
            let batch_size = args
                .next()
                .ok_or_else(|| "run requires batch size".to_owned())?
                .to_string_lossy()
                .parse()
                .map_err(|error| format!("invalid batch size: {error}"))?;
            let repetition = args
                .next()
                .map(|value| value.to_string_lossy().parse())
                .transpose()
                .map_err(|error| format!("invalid repetition: {error}"))?
                .unwrap_or(0);
            let ordinal = args
                .next()
                .map(|value| value.to_string_lossy().parse())
                .transpose()
                .map_err(|error| format!("invalid ordinal: {error}"))?
                .unwrap_or(0);
            let result = run_child(&corpus, layout, batch_size, repetition, ordinal)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&result).map_err(|error| error.to_string())?
            );
        }
        "matrix" => {
            let corpus = PathBuf::from(
                args.next()
                    .ok_or_else(|| "matrix requires corpus path".to_owned())?,
            );
            let output = PathBuf::from(
                args.next()
                    .ok_or_else(|| "matrix requires output path".to_owned())?,
            );
            let batch_size = args
                .next()
                .map(|value| value.to_string_lossy().parse())
                .transpose()
                .map_err(|error| format!("invalid batch size: {error}"))?
                .unwrap_or(4_096);
            let repetitions = args
                .next()
                .map(|value| value.to_string_lossy().parse())
                .transpose()
                .map_err(|error| format!("invalid repetitions: {error}"))?
                .unwrap_or(5);
            run_matrix(&corpus, &output, batch_size, repetitions)?;
        }
        other => return Err(format!("unknown command: {other}")),
    }
    Ok(())
}
