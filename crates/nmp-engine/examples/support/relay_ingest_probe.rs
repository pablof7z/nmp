use std::collections::{BTreeSet, HashSet};
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use nmp_engine::core::{HistoryQuery, RelayAdmissionPolicy, RowDelta};
use nmp_engine::runtime::{EngineThread, HistoryReceiver, RowsMsg, RowsReceiver};
use nmp_grammar::{AccessContext, Binding, Demand, Filter, SourceAuthority};
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_store::{EventStore, RedbStore};
use nmp_transport::PoolConfig;
use nostr::{EventBuilder, EventId, JsonUtil, Keys, Kind, RelayUrl, Timestamp};
use serde::Serialize;
use tungstenite::{accept, Message};

pub type ProbeError = Box<dyn Error + Send + Sync>;

const RESULT_SCHEMA: &str = "nmp-relay-ingest-probe-v6";
const CORPUS_SCHEMA: &str = "nmp-relay-ingest-corpus-v1";
const BASE_CREATED_AT: u64 = 1_700_000_000;

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub events: usize,
    pub relays: usize,
    pub passes: usize,
    pub payload_bytes: usize,
    pub queue_capacity: usize,
    pub verified_cache_capacity: usize,
    pub verifier_workers: usize,
    pub verify_batch_size: usize,
    pub engine_batch_size: usize,
    pub visible_limit: Option<usize>,
    pub trim_allocator_during_ingest: bool,
    pub frame_delay: Duration,
    pub expect_rejection: bool,
    pub timeout: Duration,
    pub store_path: Option<PathBuf>,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            events: 10_000,
            relays: 1,
            passes: 1,
            payload_bytes: 128,
            queue_capacity: 1_024,
            verified_cache_capacity: 131_072,
            verifier_workers: 0,
            verify_batch_size: 128,
            engine_batch_size: 128,
            visible_limit: Some(200),
            trim_allocator_during_ingest: false,
            frame_delay: Duration::ZERO,
            expect_rejection: false,
            timeout: Duration::from_secs(120),
            store_path: None,
        }
    }
}

impl ProbeConfig {
    fn validate(&self) -> Result<(), ProbeError> {
        if self.events == 0 {
            return Err("events must be nonzero".into());
        }
        if self.relays == 0 {
            return Err("relays must be nonzero".into());
        }
        if self.passes == 0 {
            return Err("passes must be nonzero".into());
        }
        if self.queue_capacity == 0 {
            return Err("queue-capacity must be nonzero".into());
        }
        if self.verify_batch_size == 0 {
            return Err("verify-batch-size must be nonzero".into());
        }
        if self.engine_batch_size == 0 {
            return Err("engine-batch-size must be nonzero".into());
        }
        if self.visible_limit == Some(0) {
            return Err("visible-limit must be nonzero".into());
        }
        if self.expect_rejection && (self.events != 1 || self.relays != 1 || self.passes != 1) {
            return Err(
                "expect-rejection requires exactly one event, one relay, and one pass".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct ProbeResult {
    pub schema: &'static str,
    pub git_commit: String,
    pub rustc: String,
    pub host_os: &'static str,
    pub host_arch: &'static str,
    pub host_kernel: String,
    pub host_cpu: String,
    pub host_logical_cpus: usize,
    pub events: usize,
    pub relays: usize,
    pub passes: usize,
    pub payload_bytes: usize,
    pub queue_capacity: usize,
    pub verified_cache_capacity: usize,
    pub verifier_workers: usize,
    pub verify_batch_size: usize,
    pub engine_batch_size: usize,
    pub visible_limit: Option<usize>,
    pub delivery_mode: &'static str,
    pub trim_allocator_during_ingest: bool,
    pub frame_delay_us: u128,
    pub expect_rejection: bool,
    pub expected_relay_frames: u64,
    pub observed_relay_frames: u64,
    pub observed_added_rows: u64,
    pub observed_removed_rows: u64,
    pub observed_source_growth_deltas: u64,
    pub final_visible_rows: usize,
    pub all_sources_reconciled: bool,
    pub corpus_bytes: u64,
    pub database_bytes: u64,
    pub generation_ms: f64,
    pub ingest_ms: f64,
    pub relay_frames_per_second: f64,
    pub first_row_ms: f64,
    pub last_row_ms: f64,
    pub apply_latency_p50_ms: f64,
    pub apply_latency_p95_ms: f64,
    pub apply_latency_p99_ms: f64,
    pub apply_latency_max_ms: f64,
    pub rss_before_ingest_bytes: Option<u64>,
    pub anonymous_before_ingest_bytes: Option<u64>,
    pub peak_ingest_rss_bytes: Option<u64>,
    pub peak_ingest_rss_growth_bytes: Option<u64>,
    pub peak_ingest_anonymous_bytes: Option<u64>,
    pub rss_after_ingest_bytes: Option<u64>,
    pub anonymous_after_ingest_bytes: Option<u64>,
    pub rss_after_shutdown_bytes: Option<u64>,
    pub anonymous_after_shutdown_bytes: Option<u64>,
    pub rss_after_probe_buffers_release_bytes: Option<u64>,
    pub anonymous_after_probe_buffers_release_bytes: Option<u64>,
    pub rss_after_rows_release_bytes: Option<u64>,
    pub anonymous_after_rows_release_bytes: Option<u64>,
    pub rss_after_handle_release_bytes: Option<u64>,
    pub anonymous_after_handle_release_bytes: Option<u64>,
    pub allocator_trim_attempted: bool,
    pub rss_after_allocator_trim_bytes: Option<u64>,
    pub anonymous_after_allocator_trim_bytes: Option<u64>,
    pub shutdown_ms: f64,
    pub reopen_and_verify_ms: f64,
    pub first_event_id: String,
    pub last_event_id: String,
    pub server_send_ms: Vec<f64>,
    pub server_bytes: Vec<u64>,
    pub ingest_attribution: Option<serde_json::Value>,
}

struct Corpus {
    path: PathBuf,
    bytes: u64,
    author: String,
    first_id: String,
    last_id: String,
    generation: Duration,
}

#[derive(Debug)]
struct ServerStats {
    frames: u64,
    bytes: u64,
    send_elapsed: Duration,
}

struct Server {
    url: RelayUrl,
    join: thread::JoinHandle<Result<ServerStats, String>>,
}

#[derive(Clone)]
struct ServerConfig {
    relay_index: usize,
    corpus_path: PathBuf,
    events: usize,
    passes: usize,
    frame_delay: Duration,
    expect_rejection: bool,
}

#[derive(Clone, Copy, Default)]
struct MemorySample {
    rss_bytes: Option<u64>,
    anonymous_bytes: Option<u64>,
}

struct MemorySampler {
    stop: Arc<AtomicBool>,
    join: thread::JoinHandle<MemorySample>,
}

impl MemorySampler {
    fn start(trim_during_ingest: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = thread::spawn(move || {
            let mut peak = current_memory();
            let mut next_trim = Instant::now() + Duration::from_millis(100);
            while !thread_stop.load(Ordering::Relaxed) {
                if trim_during_ingest && Instant::now() >= next_trim {
                    trim_allocator();
                    next_trim = Instant::now() + Duration::from_millis(100);
                }
                let current = current_memory();
                peak.rss_bytes = max_optional(peak.rss_bytes, current.rss_bytes);
                peak.anonymous_bytes = max_optional(peak.anonymous_bytes, current.anonymous_bytes);
                thread::sleep(Duration::from_millis(10));
            }
            peak
        });
        Self { stop, join }
    }

    fn stop(self) -> MemorySample {
        self.stop.store(true, Ordering::Relaxed);
        self.join.join().unwrap_or_default()
    }
}

struct ObservationState {
    added_rows: u64,
    removed_rows: u64,
    source_growth: u64,
    visible_ids: Option<HashSet<EventId>>,
    latencies_ns: Vec<u64>,
    first_row: Option<Duration>,
    last_row: Option<Duration>,
}

enum ProbeRows {
    Unbounded(RowsReceiver),
    Windowed(HistoryReceiver),
}

impl ProbeRows {
    fn recv_timeout(&self, timeout: Duration) -> Result<RowsMsg, mpsc::RecvTimeoutError> {
        match self {
            Self::Unbounded(rows) => rows.recv_timeout(timeout),
            Self::Windowed(batches) => batches
                .recv_timeout(timeout)
                .map(|batch| (batch.deltas, batch.evidence)),
        }
    }
}

impl ObservationState {
    fn new(config: &ProbeConfig) -> Self {
        Self {
            added_rows: 0,
            removed_rows: 0,
            source_growth: 0,
            visible_ids: config.visible_limit.map(|_| HashSet::new()),
            latencies_ns: Vec::with_capacity(config.events.min(1_000_000)),
            first_row: None,
            last_row: None,
        }
    }

    fn apply(
        &mut self,
        deltas: Vec<RowDelta>,
        config: &ProbeConfig,
        sent_at: &[AtomicU64],
        base: Instant,
        ingest_started: Instant,
    ) -> Result<(), ProbeError> {
        for delta in deltas {
            match delta {
                RowDelta::Added(row) => {
                    let ordinal = parse_ordinal(&row.event.content)?;
                    if ordinal >= config.events {
                        return Err(format!("out-of-range event ordinal {ordinal}").into());
                    }
                    let sent = sent_at[ordinal].load(Ordering::Acquire);
                    if sent == 0 {
                        return Err(format!(
                            "event ordinal {ordinal} arrived before send timestamp"
                        )
                        .into());
                    }
                    self.latencies_ns
                        .push(elapsed_ns(base).saturating_sub(sent));
                    self.added_rows += 1;
                    if config.visible_limit.is_none() && self.added_rows > config.events as u64 {
                        return Err("more Added deltas than canonical corpus rows".into());
                    }
                    if let Some(visible_ids) = &mut self.visible_ids {
                        if !visible_ids.insert(row.event.id) {
                            return Err("duplicate Added delta for a visible row".into());
                        }
                    }
                    self.first_row
                        .get_or_insert_with(|| ingest_started.elapsed());
                    self.last_row = Some(ingest_started.elapsed());
                }
                RowDelta::SourcesGrew { .. } => self.source_growth += 1,
                RowDelta::Removed(id) => {
                    self.removed_rows += 1;
                    let Some(visible_ids) = &mut self.visible_ids else {
                        return Err(
                            format!("unexpected removal during unlimited ingest: {id}").into()
                        );
                    };
                    if !visible_ids.remove(&id) {
                        return Err(format!("removed row was not visible: {id}").into());
                    }
                }
            }
        }
        Ok(())
    }

    fn final_visible_rows(&self, config: &ProbeConfig) -> usize {
        self.visible_ids
            .as_ref()
            .map_or(config.events, HashSet::len)
    }
}

pub fn run(config: ProbeConfig) -> Result<ProbeResult, ProbeError> {
    config.validate()?;
    let scratch = tempfile::tempdir()?;
    let corpus = generate_corpus(scratch.path(), &config)?;
    let store_path = config
        .store_path
        .clone()
        .unwrap_or_else(|| scratch.path().join("probe.redb"));
    if store_path.exists() {
        return Err(format!(
            "refusing to overwrite existing store {}",
            store_path.display()
        )
        .into());
    }
    if let Some(parent) = store_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let base = Instant::now();
    let sent_at = Arc::new(
        (0..config.events)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>(),
    );
    let server_sent_frames = Arc::new(AtomicU64::new(0));
    let mut servers = Vec::with_capacity(config.relays);
    for relay_index in 0..config.relays {
        servers.push(spawn_server(
            ServerConfig {
                relay_index,
                corpus_path: corpus.path.clone(),
                events: config.events,
                passes: config.passes,
                frame_delay: config.frame_delay,
                expect_rejection: config.expect_rejection,
            },
            base,
            Arc::clone(&sent_at),
            Arc::clone(&server_sent_frames),
        )?);
    }
    let relay_urls: BTreeSet<_> = servers.iter().map(|server| server.url.clone()).collect();

    let selection = Filter {
        kinds: Some(BTreeSet::from([Kind::TextNote.as_u16()])),
        authors: Some(Binding::Literal(BTreeSet::from([corpus.author.clone()]))),
        limit: None,
        ..Filter::default()
    };
    let demand = Demand::new(
        selection.clone(),
        SourceAuthority::Pinned(relay_urls.clone()),
        AccessContext::Public,
    )?;
    let store = RedbStore::open(&store_path)?;
    #[cfg(feature = "bench-instrumentation")]
    nmp_engine::ingest_attribution::reset();
    let queue_capacity = config.queue_capacity;
    let verified_cache_capacity = config.verified_cache_capacity;
    let verifier_workers = config.verifier_workers;
    let verify_batch_size = config.verify_batch_size;
    let engine_batch_size = config.engine_batch_size;
    let (engine_thread, handle) = EngineThread::spawn(
        store,
        FixtureDirectory::new(),
        config.relays,
        PoolConfig {
            max_relays: config.relays,
            ingest_queue_capacity: queue_capacity,
            command_queue_capacity: queue_capacity,
            event_sink_queue_capacity: queue_capacity,
            verifier_queue_capacity: queue_capacity,
            verified_cache_capacity,
            verifier_workers,
            max_verify_batch: verify_batch_size,
            max_engine_batch: engine_batch_size,
            reconnect_delay_initial: Some(Duration::from_secs(3600)),
            reconnect_jitter_max: Some(Duration::ZERO),
            ..PoolConfig::default()
        },
        RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
    )?;
    let live_query = LiveQuery(demand);
    let rows = match config.visible_limit {
        Some(limit) => {
            let (_, rows) =
                handle.subscribe_history(HistoryQuery::new(live_query, limit, limit))?;
            ProbeRows::Windowed(rows)
        }
        None => {
            let (_, rows) = handle.subscribe(live_query)?;
            ProbeRows::Unbounded(rows)
        }
    };
    let (diagnostics_handle, diagnostics) = handle.observe_diagnostics();
    let observed_relay_frames = Arc::new(AtomicU64::new(0));
    let diagnostic_count = Arc::clone(&observed_relay_frames);
    let diagnostics_join = thread::spawn(move || {
        while let Some(snapshot) = diagnostics.recv() {
            let frames = snapshot
                .relays
                .iter()
                .flat_map(|relay| relay.events_by_kind.iter())
                .map(|(_, count)| *count)
                .sum();
            diagnostic_count.fetch_max(frames, Ordering::Release);
        }
    });

    let expected_frames = (config.events as u64)
        .checked_mul(config.relays as u64)
        .and_then(|value| value.checked_mul(config.passes as u64))
        .ok_or("expected frame count overflow")?;
    let ingest_started = Instant::now();
    let memory_before_ingest = current_memory();
    let memory_sampler = MemorySampler::start(config.trim_allocator_during_ingest);
    let deadline = Instant::now() + config.timeout;
    let mut observations = ObservationState::new(&config);
    let mut all_sources_reconciled = false;
    let mut rejection_quiet_since = None;
    let mut accepted_quiet_since = None;

    loop {
        let observed_frames = observed_relay_frames.load(Ordering::Acquire);
        let sent_frames = server_sent_frames.load(Ordering::Acquire);
        if config.expect_rejection && sent_frames == expected_frames && observed_frames == 0 {
            rejection_quiet_since.get_or_insert_with(Instant::now);
        } else {
            rejection_quiet_since = None;
        }
        let rejection_complete = rejection_quiet_since
            .is_some_and(|started| started.elapsed() >= Duration::from_secs(1));
        let expected_visible_rows = config
            .visible_limit
            .map_or(config.events, |limit| config.events.min(limit));
        let rows_complete = observations.final_visible_rows(&config) == expected_visible_rows;
        if observed_frames >= expected_frames && rows_complete {
            accepted_quiet_since.get_or_insert_with(Instant::now);
        } else {
            accepted_quiet_since = None;
        }
        let accepted_complete =
            accepted_quiet_since.is_some_and(|started| started.elapsed() >= Duration::from_secs(1));
        if (config.expect_rejection && rejection_complete)
            || (!config.expect_rejection && accepted_complete)
        {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            handle.shutdown();
            engine_thread.join();
            return Err(format!(
                "timed out: sent={sent_frames}/{expected_frames} observed={observed_frames} added={}/{} reconciled={all_sources_reconciled}",
                observations.added_rows, config.events,
            )
            .into());
        }
        match rows.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok((deltas, evidence)) => {
                accepted_quiet_since = None;
                observations.apply(deltas, &config, &sent_at, base, ingest_started)?;
                all_sources_reconciled = evidence.sources.len() == config.relays
                    && evidence
                        .sources
                        .iter()
                        .all(|source| source.reconciled_through.is_some());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("row stream disconnected before completion".into());
            }
        }
    }

    while let Ok((deltas, _)) = rows.recv_timeout(Duration::from_millis(100)) {
        observations.apply(deltas, &config, &sent_at, base, ingest_started)?;
    }
    let ingest_elapsed = ingest_started.elapsed();
    let memory_after_ingest = current_memory();
    let peak_ingest_memory = memory_sampler.stop();

    diagnostics_handle.cancel();
    diagnostics_join
        .join()
        .map_err(|_| "diagnostics observer thread panicked")?;
    let observed_relay_frames = observed_relay_frames.load(Ordering::Acquire);
    let expected_observed_frames = if config.expect_rejection {
        0
    } else {
        expected_frames
    };
    if observed_relay_frames != expected_observed_frames {
        return Err(format!(
            "diagnostics counted {observed_relay_frames} relay frames, expected {expected_observed_frames}"
        )
        .into());
    }

    let shutdown_started = Instant::now();
    handle.shutdown();
    engine_thread.join();
    let shutdown_elapsed = shutdown_started.elapsed();
    #[cfg(feature = "bench-instrumentation")]
    let ingest_attribution = Some(ingest_attribution_json());
    #[cfg(not(feature = "bench-instrumentation"))]
    let ingest_attribution = None;
    while let Ok((deltas, _)) = rows.recv_timeout(Duration::ZERO) {
        observations.apply(deltas, &config, &sent_at, base, ingest_started)?;
    }
    let memory_after_shutdown = current_memory();

    let mut server_stats = Vec::with_capacity(servers.len());
    for server in servers {
        let stats = server
            .join
            .join()
            .map_err(|_| "relay server thread panicked")?
            .map_err(|error| format!("relay server failed: {error}"))?;
        if stats.frames != (config.events * config.passes) as u64 {
            return Err(format!(
                "relay sent {} frames, expected {}",
                stats.frames,
                config.events * config.passes
            )
            .into());
        }
        server_stats.push(stats);
    }

    let expected_stored_events = if config.expect_rejection {
        0
    } else {
        config.events
    };
    let expected_visible_rows = config
        .visible_limit
        .map_or(expected_stored_events, |limit| {
            expected_stored_events.min(limit)
        });
    let final_visible_rows = observations.final_visible_rows(&config);
    if final_visible_rows != expected_visible_rows {
        return Err(format!(
            "visible query ended with {final_visible_rows} rows, expected {expected_visible_rows}"
        )
        .into());
    }
    if !config.expect_rejection
        && observations.visible_ids.as_ref().is_some_and(|ids| {
            !ids.iter()
                .any(|event_id| event_id.to_hex() == corpus.last_id)
        })
    {
        return Err("bounded visible query is missing the newest event".into());
    }

    observations.latencies_ns.sort_unstable();
    let observed_added_rows = observations.added_rows;
    let observed_removed_rows = observations.removed_rows;
    let observed_source_growth_deltas = observations.source_growth;
    let first_row_ms = duration_ms(observations.first_row.unwrap_or_default());
    let last_row_ms = duration_ms(observations.last_row.unwrap_or_default());
    let apply_latency_p50_ms = ns_ms(percentile(&observations.latencies_ns, 50));
    let apply_latency_p95_ms = ns_ms(percentile(&observations.latencies_ns, 95));
    let apply_latency_p99_ms = ns_ms(percentile(&observations.latencies_ns, 99));
    let apply_latency_max_ms = ns_ms(*observations.latencies_ns.last().unwrap_or(&0));
    drop(observations);
    drop(sent_at);
    let memory_after_probe_buffers_release = current_memory();
    drop(rows);
    let memory_after_rows_release = current_memory();
    drop(handle);
    let memory_after_handle_release = current_memory();
    let allocator_trim_attempted = trim_allocator();
    let memory_after_allocator_trim = current_memory();

    let verify_started = Instant::now();
    let reopened = RedbStore::open(&store_path)?;
    let mut persisted_selection = selection.clone();
    persisted_selection.limit = None;
    let stored = reopened.query(&selection_to_nostr(&persisted_selection)?)?;
    if stored.len() != expected_stored_events {
        return Err(format!(
            "reopened cardinality {}, expected {}",
            stored.len(),
            expected_stored_events
        )
        .into());
    }
    if stored
        .iter()
        .any(|row| row.provenance.seen.len() != config.relays)
    {
        return Err("one or more rows did not retain every relay provenance".into());
    }
    let ids: HashSet<_> = stored.iter().map(|row| row.event.id.to_hex()).collect();
    if !config.expect_rejection
        && (!ids.contains(&corpus.first_id) || !ids.contains(&corpus.last_id))
    {
        return Err("reopened store is missing the corpus boundary ids".into());
    }
    drop(stored);
    drop(reopened);
    let reopen_and_verify = verify_started.elapsed();

    let ingest_seconds = ingest_elapsed.as_secs_f64();
    Ok(ProbeResult {
        schema: RESULT_SCHEMA,
        git_commit: command_output("git", &["rev-parse", "HEAD"]),
        rustc: command_output("rustc", &["--version"]),
        host_os: std::env::consts::OS,
        host_arch: std::env::consts::ARCH,
        host_kernel: command_output("uname", &["-sr"]),
        host_cpu: host_cpu(),
        host_logical_cpus: thread::available_parallelism().map_or(1, usize::from),
        events: config.events,
        relays: config.relays,
        passes: config.passes,
        payload_bytes: config.payload_bytes,
        queue_capacity,
        verified_cache_capacity,
        verifier_workers,
        verify_batch_size,
        engine_batch_size,
        visible_limit: config.visible_limit,
        delivery_mode: if config.visible_limit.is_some() {
            "bounded-latest-window"
        } else {
            "exact-rebased-delta"
        },
        trim_allocator_during_ingest: config.trim_allocator_during_ingest,
        frame_delay_us: config.frame_delay.as_micros(),
        expect_rejection: config.expect_rejection,
        expected_relay_frames: expected_frames,
        observed_relay_frames,
        observed_added_rows,
        observed_removed_rows,
        observed_source_growth_deltas,
        final_visible_rows,
        all_sources_reconciled,
        corpus_bytes: corpus.bytes,
        database_bytes: fs::metadata(&store_path)?.len(),
        generation_ms: duration_ms(corpus.generation),
        ingest_ms: duration_ms(ingest_elapsed),
        relay_frames_per_second: expected_frames as f64 / ingest_seconds,
        first_row_ms,
        last_row_ms,
        apply_latency_p50_ms,
        apply_latency_p95_ms,
        apply_latency_p99_ms,
        apply_latency_max_ms,
        rss_before_ingest_bytes: memory_before_ingest.rss_bytes,
        anonymous_before_ingest_bytes: memory_before_ingest.anonymous_bytes,
        peak_ingest_rss_bytes: peak_ingest_memory.rss_bytes,
        peak_ingest_rss_growth_bytes: peak_ingest_memory
            .rss_bytes
            .zip(memory_before_ingest.rss_bytes)
            .map(|(peak, before)| peak.saturating_sub(before)),
        peak_ingest_anonymous_bytes: peak_ingest_memory.anonymous_bytes,
        rss_after_ingest_bytes: memory_after_ingest.rss_bytes,
        anonymous_after_ingest_bytes: memory_after_ingest.anonymous_bytes,
        rss_after_shutdown_bytes: memory_after_shutdown.rss_bytes,
        anonymous_after_shutdown_bytes: memory_after_shutdown.anonymous_bytes,
        rss_after_probe_buffers_release_bytes: memory_after_probe_buffers_release.rss_bytes,
        anonymous_after_probe_buffers_release_bytes: memory_after_probe_buffers_release
            .anonymous_bytes,
        rss_after_rows_release_bytes: memory_after_rows_release.rss_bytes,
        anonymous_after_rows_release_bytes: memory_after_rows_release.anonymous_bytes,
        rss_after_handle_release_bytes: memory_after_handle_release.rss_bytes,
        anonymous_after_handle_release_bytes: memory_after_handle_release.anonymous_bytes,
        allocator_trim_attempted,
        rss_after_allocator_trim_bytes: memory_after_allocator_trim.rss_bytes,
        anonymous_after_allocator_trim_bytes: memory_after_allocator_trim.anonymous_bytes,
        shutdown_ms: duration_ms(shutdown_elapsed),
        reopen_and_verify_ms: duration_ms(reopen_and_verify),
        first_event_id: corpus.first_id,
        last_event_id: corpus.last_id,
        server_send_ms: server_stats
            .iter()
            .map(|stats| duration_ms(stats.send_elapsed))
            .collect(),
        server_bytes: server_stats.iter().map(|stats| stats.bytes).collect(),
        ingest_attribution,
    })
}

#[cfg(feature = "bench-instrumentation")]
fn ingest_attribution_json() -> serde_json::Value {
    let transport = nmp_transport::ingest_attribution::snapshot();
    let engine = nmp_engine::ingest_attribution::snapshot();
    let resolver = nmp_resolver::ingest_attribution::snapshot();
    let store = nmp_store::ingest_attribution::snapshot();
    serde_json::json!({
        "transport": {
            "parse_attempts": transport.parse_attempts, "parsed_frames": transport.parsed_frames,
            "parse_ns": transport.parse_ns, "translator_bursts": transport.translator_bursts,
            "translator_events": transport.translator_events, "max_translator_burst": transport.max_translator_burst,
            "verify_batches": transport.verify_batches, "verify_candidates": transport.verify_candidates,
            "verify_ns": transport.verify_ns, "delivered_events": transport.delivered_events,
            "delivery_ns": transport.delivery_ns
        },
        "engine": {
            "bridge_batches": engine.bridge_batches, "bridge_frames": engine.bridge_frames,
            "max_bridge_batch": engine.max_bridge_batch, "bridge_send_ns": engine.bridge_send_ns,
            "bridge_applied_wait_ns": engine.bridge_applied_wait_ns,
            "engine_batch_process_ns": engine.engine_batch_process_ns
        },
        "resolver": {
            "batches": resolver.batches, "events": resolver.events, "max_batch_events": resolver.max_batch_events,
            "total_ns": resolver.total_ns, "prepare_ns": resolver.prepare_ns, "store_ns": resolver.store_ns,
            "classify_ns": resolver.classify_ns, "react_and_affected_ns": resolver.react_and_affected_ns
        },
        "store": {
            "batches": store.batches, "events": store.events, "max_batch_events": store.max_batch_events,
            "transaction_total_ns": store.transaction_total_ns, "begin_write_ns": store.begin_write_ns,
            "open_tables_ns": store.open_tables_ns, "apply_events_ns": store.apply_events_ns,
            "flush_ns": store.flush_ns, "commit_ns": store.commit_ns, "encode_event_ns": store.encode_event_ns,
            "encoded_event_bytes": store.encoded_event_bytes, "canonical_insert_ns": store.canonical_insert_ns,
            "index_insert_ns": store.index_insert_ns
        }
    })
}

fn generate_corpus(dir: &Path, config: &ProbeConfig) -> Result<Corpus, ProbeError> {
    let started = Instant::now();
    let path = dir.join("events.jsonl");
    let mut writer = BufWriter::new(File::create(&path)?);
    let keys = Keys::parse(&format!("{:064x}", 1u8))?;
    let author = keys.public_key().to_hex();
    let mut first_id = None;
    let mut last_id = String::new();
    for ordinal in 0..config.events {
        let prefix = format!("{CORPUS_SCHEMA} ordinal={ordinal} ");
        let content = if prefix.len() < config.payload_bytes {
            let mut content = String::with_capacity(config.payload_bytes);
            content.push_str(&prefix);
            content.extend(std::iter::repeat_n(
                'x',
                config.payload_bytes - prefix.len(),
            ));
            content
        } else {
            prefix
        };
        let event = EventBuilder::new(Kind::TextNote, content)
            .custom_created_at(Timestamp::from(BASE_CREATED_AT + ordinal as u64))
            .sign_with_keys(&keys)?;
        first_id.get_or_insert_with(|| event.id.to_hex());
        last_id = event.id.to_hex();
        writeln!(writer, "{}", event.as_json())?;
    }
    writer.flush()?;
    Ok(Corpus {
        bytes: fs::metadata(&path)?.len(),
        path,
        author,
        first_id: first_id.expect("nonempty corpus"),
        last_id,
        generation: started.elapsed(),
    })
}

fn spawn_server(
    config: ServerConfig,
    base: Instant,
    sent_at: Arc<Vec<AtomicU64>>,
    server_sent_frames: Arc<AtomicU64>,
) -> Result<Server, ProbeError> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let address = listener.local_addr()?;
    let url = RelayUrl::parse(&format!("ws://{address}"))?;
    listener.set_nonblocking(true)?;
    let join = thread::Builder::new()
        .name(format!("nmp-load-relay-{}", config.relay_index))
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let (stats_tx, stats_rx) = mpsc::sync_channel(1);
            let claimed = Arc::new(AtomicBool::new(false));
            let mut connections: Vec<thread::JoinHandle<()>> = Vec::new();
            loop {
                if let Ok(stats) = stats_rx.try_recv() {
                    for connection in connections {
                        let _ = connection.join();
                    }
                    return stats;
                }
                if Instant::now() >= deadline && !claimed.load(Ordering::Acquire) {
                    return Err("timed out waiting for websocket client".to_string());
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        // `listener` is non-blocking so this accept loop can race the
                        // deadline/`claimed` checks above. On Linux, a socket returned
                        // by `accept()` does NOT inherit the listening socket's
                        // `O_NONBLOCK` flag and always comes back blocking. On macOS
                        // (and BSD generally) it DOES inherit it, so without this reset
                        // the per-connection socket below is silently non-blocking:
                        // `tungstenite::accept`'s handshake read usually still succeeds
                        // because the client's HTTP upgrade bytes are typically already
                        // in the kernel receive buffer by the time this thread runs, but
                        // the very next blocking-style `socket.read()` in `serve_corpus`
                        // (waiting for the first REQ frame) can hit the socket before the
                        // client has flushed it, return `WouldBlock`, and — since that
                        // read loop treats any error as fatal — tear the connection down
                        // before the client's REQ arrives. That is the exact "ECONNRESET
                        // before the first REQ flushes" race from #538: forcing the
                        // accepted socket back to blocking mode makes every platform
                        // behave like the thread-per-connection design already assumes.
                        if let Err(error) = stream.set_nonblocking(false) {
                            return Err(format!(
                                "failed to clear O_NONBLOCK on accepted connection: {error}"
                            ));
                        }
                        // Bound writes so a rejected-message test can't hang the mock
                        // for the full probe timeout: a client that detects an
                        // over-ceiling frame is only required to stop trusting the
                        // relay, not to promptly close/reset the raw TCP connection.
                        // If it just stops draining the socket, an unbounded blocking
                        // write here can stall on a full receive window until
                        // whatever eventually tears the connection down (in the worst
                        // case, the probe's own top-level timeout). A write deadline
                        // turns that stall into a prompt, tolerated send error
                        // instead (see `expect_rejection` handling in serve_corpus).
                        if let Err(error) = stream.set_write_timeout(Some(Duration::from_secs(2))) {
                            return Err(format!(
                                "failed to set write timeout on accepted connection: {error}"
                            ));
                        }
                        let claimed = Arc::clone(&claimed);
                        let stats_tx = stats_tx.clone();
                        let config = config.clone();
                        let sent_at = Arc::clone(&sent_at);
                        let server_sent_frames = Arc::clone(&server_sent_frames);
                        connections.push(thread::spawn(move || {
                            let Ok(mut socket) = accept(stream) else {
                                return;
                            };
                            if claimed.swap(true, Ordering::AcqRel) {
                                let _ = socket.close(None);
                                return;
                            }
                            let result = serve_corpus(
                                &mut socket,
                                &config,
                                base,
                                &sent_at,
                                &server_sent_frames,
                            );
                            let _ = stats_tx.send(result);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => return Err(format!("accept failed: {error}")),
                }
            }
        })?;
    Ok(Server { url, join })
}

fn serve_corpus(
    socket: &mut tungstenite::WebSocket<std::net::TcpStream>,
    config: &ServerConfig,
    base: Instant,
    sent_at: &[AtomicU64],
    server_sent_frames: &AtomicU64,
) -> Result<ServerStats, String> {
    let subscription = loop {
        let message = socket.read().map_err(|error| error.to_string())?;
        let Message::Text(text) = message else {
            continue;
        };
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if value.get(0).and_then(serde_json::Value::as_str) == Some("REQ") {
            break value
                .get(1)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "REQ has no subscription id".to_string())?
                .to_owned();
        }
    };
    let encoded_subscription = serde_json::to_string(&subscription).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut frames = 0u64;
    let mut bytes = 0u64;
    'passes: for _ in 0..config.passes {
        let reader = BufReader::new(File::open(&config.corpus_path).map_err(|e| e.to_string())?);
        for (ordinal, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| e.to_string())?;
            if ordinal >= config.events {
                return Err("corpus contains more rows than declared".to_string());
            }
            let now = elapsed_ns(base).max(1);
            let _ = sent_at[ordinal].compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire);
            let frame = format!("[\"EVENT\",{encoded_subscription},{line}]");
            bytes = bytes.saturating_add(frame.len() as u64);
            if let Err(error) = socket.send(Message::Text(frame.into())) {
                if !config.expect_rejection {
                    return Err(error.to_string());
                }
                // A rejected message is expected to make the client close or
                // reset the connection as soon as it observes the frame
                // exceeds the ceiling. A payload this large needs several
                // underlying TCP writes, so the client can hang up mid-write
                // (racing this thread's send with the client's own close).
                // Still count the frame as sent — the probe's completion
                // detection watches `server_sent_frames` to know the mock is
                // done offering data, and a write the client already
                // rejected should not make it wait out the full timeout.
                frames += 1;
                server_sent_frames.fetch_add(1, Ordering::Release);
                break 'passes;
            }
            frames += 1;
            server_sent_frames.fetch_add(1, Ordering::Release);
            if !config.frame_delay.is_zero() {
                thread::sleep(config.frame_delay);
            }
        }
    }
    let eose = format!("[\"EOSE\",{encoded_subscription}]");
    if let Err(error) = socket.send(Message::Text(eose.into())) {
        if !config.expect_rejection {
            return Err(error.to_string());
        }
    }
    if let Err(error) = socket.flush() {
        if !config.expect_rejection {
            return Err(error.to_string());
        }
    }
    let send_elapsed = started.elapsed();
    if !config.expect_rejection {
        // Real subscription sockets remain open after EOSE. Waiting for the
        // client close also proves it consumed the final TCP window.
        loop {
            match socket.read() {
                Ok(Message::Close(_))
                | Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed) => break,
                Ok(_) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
    }
    Ok(ServerStats {
        frames,
        bytes,
        send_elapsed,
    })
}

fn parse_ordinal(content: &str) -> Result<usize, ProbeError> {
    let marker = "ordinal=";
    let start = content.find(marker).ok_or("event content has no ordinal")? + marker.len();
    let end = content[start..]
        .find(' ')
        .map(|offset| start + offset)
        .unwrap_or(content.len());
    Ok(content[start..end].parse()?)
}

fn selection_to_nostr(selection: &Filter) -> Result<nostr::Filter, ProbeError> {
    let authors = match &selection.authors {
        Some(Binding::Literal(authors)) => authors,
        _ => return Err("probe selection must have literal authors".into()),
    };
    let mut filter = nostr::Filter::new();
    if let Some(kinds) = &selection.kinds {
        filter = filter.kinds(kinds.iter().copied().map(Kind::from));
    }
    for author in authors {
        filter = filter.author(nostr::PublicKey::parse(author)?);
    }
    Ok(filter)
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[(sorted.len() - 1) * percentile / 100]
}

fn elapsed_ns(base: Instant) -> u64 {
    u64::try_from(base.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn ns_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

fn command_output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(target_os = "linux")]
fn host_cpu() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|cpuinfo| {
            cpuinfo.lines().find_map(|line| {
                line.strip_prefix("model name")
                    .and_then(|value| value.split_once(':'))
                    .map(|(_, value)| value.trim().to_owned())
            })
        })
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(not(target_os = "linux"))]
fn host_cpu() -> String {
    "unknown".to_string()
}

fn max_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    }
}

#[cfg(target_os = "linux")]
fn current_memory() -> MemorySample {
    let Ok(rollup) = fs::read_to_string("/proc/self/smaps_rollup") else {
        return MemorySample::default();
    };
    MemorySample {
        rss_bytes: memory_field_bytes(&rollup, "Rss:"),
        anonymous_bytes: memory_field_bytes(&rollup, "Anonymous:"),
    }
}

#[cfg(target_os = "linux")]
fn memory_field_bytes(rollup: &str, field: &str) -> Option<u64> {
    let line = rollup.lines().find(|line| line.starts_with(field))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1_024)
}

#[cfg(not(target_os = "linux"))]
fn current_memory() -> MemorySample {
    MemorySample::default()
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_allocator() -> bool {
    // This is measurement-only: it distinguishes reclaimable glibc pages
    // from live process state after every probe-owned per-event buffer drops.
    unsafe { libc::malloc_trim(0) != 0 }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim_allocator() -> bool {
    false
}
