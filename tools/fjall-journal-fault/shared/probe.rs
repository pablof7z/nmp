//! Shared probe body for #818 -- the Fjall journal-write-error falsifier.
//!
//! This single source file is compiled verbatim against three pinned Fjall
//! releases (`v3_1_6`, `v3_1_7`, `v3_1_8`). Sharing one body is the point: the
//! baseline, the failing multi-keyspace transaction, the fault arming, and the
//! recorded evidence are byte-identical across releases, so a difference in the
//! observed result can only come from the release under test.
//!
//! What is being falsified
//! -----------------------
//! `fjall::batch::Batch::commit` writes the batch to the journal and then
//! applies it to the memtables. In 3.1.6 that call site reads
//!
//! ```text
//! let _ = journal_writer.write_batch(self.data.iter(), self.data.len(), batch_seqno);
//! ```
//!
//! and in 3.1.7/3.1.8 it reads
//!
//! ```text
//! journal_writer.write_batch(self.data.iter(), self.data.len(), batch_seqno)?;
//! ```
//!
//! `write_batch` itself is identical in all three releases; the defect and its
//! repair live entirely at that call site. So the observable under a real
//! journal write failure is: 3.1.6 acknowledges a transaction whose journal
//! record is truncated, 3.1.7/3.1.8 return the error instead.
//!
//! The fault
//! ---------
//! A real filesystem write failure, not an error returned from probe-owned
//! code. `RLIMIT_FSIZE` is armed to just above the journal's current write
//! offset, so the next journal extension crosses it. Linux writes up to the
//! limit, then fails the following `write(2)` with `EFBIG` and raises
//! `SIGXFSZ` on the writing thread.
//!
//! The fault must be **one-shot**: a persistent limit would make the later
//! `PersistMode::SyncAll` path fail too, and then 3.1.6 also returns an error
//! (`Error::Poisoned`) for an unrelated reason -- which would false-green the
//! whole regression. See [`Mode::Persistent`], which exists to demonstrate
//! exactly that.
//!
//! `setrlimit` is not async-signal-safe. The `SIGXFSZ` handler therefore does
//! nothing but an async-signal-safe `write`/`read` on a pre-created pipe pair;
//! a helper thread raises the soft limit and acknowledges, and only then does
//! the handler return to Fjall.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fjall::{
    KeyspaceCreateOptions, PersistMode, Readable, SingleWriterTxDatabase, SingleWriterTxKeyspace,
};

/// Mirror of the private `fjall::journal::writer::JOURNAL_BUFFER_BYTES`.
///
/// The journal writer wraps its file in a `BufWriter` of exactly this
/// capacity. A batch smaller than this never reaches the file descriptor
/// inside `write_batch` -- it is still sitting in the buffer, and the fault
/// would instead land on the later `persist` flush. That is a different code
/// path with a different outcome, so the harness must refuse such a run rather
/// than accept it as a pass. [`Mode::Undersized`] produces that situation on
/// purpose.
const JOURNAL_BUFFER_BYTES: usize = 8 * 1_024;

/// How far above the journal's current write offset the soft limit is armed.
///
/// Small enough that the first buffered journal flush (one
/// `JOURNAL_BUFFER_BYTES` chunk) always crosses it, and large enough to absorb
/// the few bytes of slack in [`journal_written_len`].
const FAULT_MARGIN_BYTES: u64 = 512;

/// Raised soft limit once the one-shot fault has fired, and the hard limit for
/// the whole probe. Comfortably above anything this fixture writes, so nothing
/// *except* the armed window can fail for a size reason.
const HARD_LIMIT_BYTES: u64 = 128 * 1_024 * 1_024;

/// The active journal, relative to the database directory. `Database::create_new`
/// uses the database root as the journal folder and names the first journal
/// `0.jnl`; `1.jnl` appearing means the journal rotated, which moves the write
/// offset and invalidates the armed limit.
const ACTIVE_JOURNAL: &str = "0.jnl";
const ROTATED_JOURNAL: &str = "1.jnl";

/// Only the first megabyte of the pre-allocated journal is scanned for the
/// write offset. This fixture writes a few kilobytes; a journal offset beyond
/// this window means the fixture is not what this probe assumes.
const JOURNAL_SCAN_WINDOW: u64 = 1_024 * 1_024;

/// The transaction under test spans three keyspaces, so "no partial
/// transaction state" is a claim about a genuinely multi-keyspace batch.
const KEYSPACES: [&str; 3] = ["alpha", "beta", "gamma"];

/// Baseline rows per keyspace, written and synced before the fault is armed.
const PRE_STATE_ROWS: usize = 4;

/// Target-transaction rows per keyspace, and the value width. Twelve rows of
/// 1 KiB is ~12.5 KiB of journal record -- comfortably past
/// [`JOURNAL_BUFFER_BYTES`], so `write_batch` must reach the file descriptor
/// before it returns.
const TARGET_ROWS_PER_KEYSPACE: usize = 4;
const TARGET_VALUE_BYTES: usize = 1_024;

/// Deliberately below [`JOURNAL_BUFFER_BYTES`] but above [`FAULT_MARGIN_BYTES`],
/// for the undersized control: the record stays inside the `BufWriter`, so
/// `write_batch` returns without touching the file descriptor and the armed
/// fault lands on the later `persist` flush instead. Every release then reports
/// the same `Error::Poisoned`, which is precisely the false green the harness
/// has to refuse.
const UNDERSIZED_ROWS_PER_KEYSPACE: usize = 1;
const UNDERSIZED_VALUE_BYTES: usize = 700;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No fault. Proves the fixture commits and reopens cleanly, so a failure
    /// in the other modes is attributable to the injected fault and not to a
    /// broken fixture.
    Healthy,
    /// The real regression: one journal extension fails, later persistence is
    /// healthy.
    OneShot,
    /// The soft limit is never raised, so the later persist path fails too.
    /// Control: proves a persistence failure is a distinguishable outcome and
    /// cannot stand in for the one-shot journal result.
    Persistent,
    /// One-shot fault armed, but the batch is below [`JOURNAL_BUFFER_BYTES`],
    /// so `write_batch` never reaches the file descriptor. Control: the
    /// harness must refuse rather than silently pass on the later flush error.
    Undersized,
    /// One-shot fault armed, but consumed by a write to a scratch file before
    /// the transaction. Control: a fault that did not land on the journal must
    /// be refused as mis-injected.
    MisInjected,
}

impl Mode {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "healthy" => Self::Healthy,
            "one-shot" => Self::OneShot,
            "persistent" => Self::Persistent,
            "undersized" => Self::Undersized,
            "misinjected" => Self::MisInjected,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::OneShot => "one-shot",
            Self::Persistent => "persistent",
            Self::Undersized => "undersized",
            Self::MisInjected => "misinjected",
        }
    }

    /// Whether the handler raises the soft limit on the first fault.
    fn one_shot(self) -> bool {
        !matches!(self, Self::Persistent)
    }

    fn arms_fault(self) -> bool {
        !matches!(self, Self::Healthy)
    }

    fn rows_per_keyspace(self) -> usize {
        match self {
            Self::Undersized => UNDERSIZED_ROWS_PER_KEYSPACE,
            _ => TARGET_ROWS_PER_KEYSPACE,
        }
    }

    fn value_bytes(self) -> usize {
        match self {
            Self::Undersized => UNDERSIZED_VALUE_BYTES,
            _ => TARGET_VALUE_BYTES,
        }
    }
}

// ---------------------------------------------------------------------------
// One-shot `RLIMIT_FSIZE` fault
// ---------------------------------------------------------------------------

mod fault {
    use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

    static SIGNALS: AtomicU32 = AtomicU32::new(0);
    static NOTIFY_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
    static ACK_READ_FD: AtomicI32 = AtomicI32::new(-1);
    /// 1 while the handler should ask the helper thread to raise the limit.
    /// Cleared by the first fault so the fault is genuinely one-shot, and never
    /// set at all in the persistent control.
    static RAISE_ON_FAULT: AtomicU32 = AtomicU32::new(0);

    /// `SIGXFSZ` handler.
    ///
    /// Async-signal-safety: this executes only `write(2)`/`read(2)` on
    /// pre-created pipe file descriptors plus relaxed atomic loads and stores.
    /// It does not allocate, take a lock, or call `setrlimit` -- `setrlimit` is
    /// not async-signal-safe, so raising the limit is delegated to a helper
    /// thread that acknowledges through the ack pipe before the handler returns
    /// to Fjall's `write_all`.
    extern "C" fn handler(_signal: libc::c_int) {
        SIGNALS.fetch_add(1, Ordering::Relaxed);

        // Consume the one-shot permit. A second fault must NOT raise again; the
        // probe reports the signal count and the harness refuses a run that
        // injected more than one failure.
        if RAISE_ON_FAULT.swap(0, Ordering::Relaxed) == 0 {
            return;
        }

        let notify = NOTIFY_WRITE_FD.load(Ordering::Relaxed);
        let ack = ACK_READ_FD.load(Ordering::Relaxed);
        let request = b'r';
        let mut reply = 0u8;

        // SAFETY: both descriptors are pipe ends created in `arm` before the
        // handler was installed, and both calls are async-signal-safe.
        unsafe {
            if libc::write(notify, std::ptr::addr_of!(request).cast(), 1) != 1 {
                libc::_exit(90);
            }
            if libc::read(ack, std::ptr::addr_of_mut!(reply).cast(), 1) != 1 {
                libc::_exit(91);
            }
        }
    }

    fn set_limit(soft: u64, hard: u64) -> std::io::Result<()> {
        let limit = libc::rlimit {
            rlim_cur: soft,
            rlim_max: hard,
        };
        // SAFETY: `limit` is a fully initialised `rlimit`.
        if unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limit) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Installs the handler and helper thread, then arms the soft limit.
    ///
    /// `raise_on_fault` false leaves the limit in place, which is the
    /// persistent-fault control.
    pub fn arm(soft: u64, hard: u64, raise_on_fault: bool) -> std::io::Result<()> {
        let mut notify = [0i32; 2];
        let mut ack = [0i32; 2];
        // SAFETY: both arrays are two-element `c_int` buffers, as `pipe(2)` requires.
        unsafe {
            if libc::pipe(notify.as_mut_ptr()) != 0 || libc::pipe(ack.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        NOTIFY_WRITE_FD.store(notify[1], Ordering::Relaxed);
        ACK_READ_FD.store(ack[0], Ordering::Relaxed);

        let notify_read = notify[0];
        let ack_write = ack[1];
        std::thread::Builder::new()
            .name("fsize-limit-raiser".into())
            .spawn(move || {
                // The helper must never take the signal itself: it is the only
                // thread that can service the handler.
                // SAFETY: `set` is initialised by `sigemptyset`/`sigaddset` before use.
                unsafe {
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, libc::SIGXFSZ);
                    libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
                }
                loop {
                    let mut request = 0u8;
                    // SAFETY: `notify_read` is a live pipe read end owned by this thread.
                    let read = unsafe {
                        libc::read(notify_read, std::ptr::addr_of_mut!(request).cast(), 1)
                    };
                    if read != 1 {
                        return;
                    }
                    // Raising the soft limit here -- on an ordinary thread --
                    // is what keeps the handler async-signal-safe.
                    let _ = set_limit(hard, hard);
                    let reply = b'k';
                    // SAFETY: `ack_write` is a live pipe write end owned by this thread.
                    if unsafe { libc::write(ack_write, std::ptr::addr_of!(reply).cast(), 1) } != 1 {
                        return;
                    }
                }
            })?;

        RAISE_ON_FAULT.store(u32::from(raise_on_fault), Ordering::Relaxed);

        // SAFETY: `action` is zeroed then fully populated; `SIGXFSZ` is a valid signal.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            // No SA_RESTART: the failing `write(2)` must return EFBIG to Fjall
            // rather than being restarted by the kernel.
            action.sa_flags = 0;
            if libc::sigaction(libc::SIGXFSZ, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        set_limit(soft, hard)
    }

    /// Restores a permissive limit. Called immediately after the target commit
    /// so that state capture, close, and reopen can never trip the fault.
    pub fn disarm(hard: u64) -> std::io::Result<()> {
        RAISE_ON_FAULT.store(0, Ordering::Relaxed);
        set_limit(hard, hard)
    }

    pub fn signal_count() -> u32 {
        SIGNALS.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn pre_state_key(keyspace: &str, index: usize) -> Vec<u8> {
    format!("pre/{keyspace}/{index:04}").into_bytes()
}

fn pre_state_value(keyspace: &str, index: usize) -> Vec<u8> {
    format!("pre-value/{keyspace}/{index:04}").into_bytes()
}

fn target_key(keyspace: &str, index: usize) -> Vec<u8> {
    format!("target/{keyspace}/{index:04}").into_bytes()
}

/// Deterministic filler. The exact bytes do not matter; that they are identical
/// across all three releases does.
fn target_value(keyspace: &str, index: usize, width: usize) -> Vec<u8> {
    let seed = format!("target-value/{keyspace}/{index:04}/");
    let mut value = Vec::with_capacity(width);
    while value.len() < width {
        let remaining = width - value.len();
        let chunk = seed.as_bytes();
        if chunk.len() <= remaining {
            value.extend_from_slice(chunk);
        } else {
            value.extend_from_slice(&chunk[..remaining]);
        }
    }
    value
}

type Snapshot = BTreeMap<String, BTreeMap<String, String>>;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Exact keys and values for every keyspace -- deliberately not row counts.
fn capture(database: &SingleWriterTxDatabase, keyspaces: &[SingleWriterTxKeyspace]) -> Snapshot {
    let read = database.read_tx();
    let mut snapshot = Snapshot::new();
    for (name, keyspace) in KEYSPACES.iter().zip(keyspaces) {
        let mut rows = BTreeMap::new();
        for guard in read.iter(keyspace) {
            let (key, value) = guard.into_inner().expect("read committed row");
            rows.insert(hex(&key), hex(&value));
        }
        snapshot.insert((*name).to_owned(), rows);
    }
    snapshot
}

fn encode_snapshot(snapshot: &Snapshot) -> String {
    let mut parts = Vec::new();
    for (keyspace, rows) in snapshot {
        for (key, value) in rows {
            parts.push(format!("{keyspace}/{key}={value}"));
        }
    }
    parts.join(",")
}

/// The journal's write offset, discovered as the end of the non-zero prefix.
///
/// Fjall pre-allocates the journal to 64 MiB of zeros via `set_len` and writes
/// forward from offset 0, so the last non-zero byte marks the write position. A
/// record whose final bytes happen to be zero makes this a slight
/// underestimate, which [`FAULT_MARGIN_BYTES`] absorbs.
fn journal_written_len(path: &Path) -> std::io::Result<u64> {
    let bytes = std::fs::read(path)?;
    let window = bytes.len().min(JOURNAL_SCAN_WINDOW as usize);
    let end = bytes[..window]
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |index| index + 1);
    Ok(end as u64)
}

fn open_database(
    path: &Path,
) -> fjall::Result<(SingleWriterTxDatabase, Vec<SingleWriterTxKeyspace>)> {
    let database = SingleWriterTxDatabase::builder(path)
        // One worker and a large memtable keep the fixture from flushing or
        // compacting during the armed window: a background write could
        // otherwise consume the one-shot fault before the journal does.
        .worker_threads(1)
        .open()?;
    let keyspaces = KEYSPACES
        .iter()
        .map(|name| {
            database.keyspace(name, || {
                KeyspaceCreateOptions::default().max_memtable_size(64 * 1_024 * 1_024)
            })
        })
        .collect::<fjall::Result<Vec<_>>>()?;
    Ok((database, keyspaces))
}

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

struct Evidence {
    records: Vec<(String, String)>,
    refusals: Vec<String>,
}

impl Evidence {
    fn new() -> Self {
        Self {
            records: Vec::new(),
            refusals: Vec::new(),
        }
    }

    fn record(&mut self, key: &str, value: impl std::fmt::Display) {
        self.records.push((key.to_owned(), value.to_string()));
    }

    /// A condition the probe itself can see is wrong with the injection. The
    /// harness turns any refusal into a failed run rather than a pass.
    fn refuse(&mut self, reason: impl std::fmt::Display) {
        self.refusals.push(reason.to_string());
    }

    fn emit(&self) -> i32 {
        for (key, value) in &self.records {
            println!("{key}={value}");
        }
        for reason in &self.refusals {
            println!("REFUSE={reason}");
        }
        if self.refusals.is_empty() {
            println!("PROBE_STATUS=ok");
            0
        } else {
            println!("PROBE_STATUS=refused");
            2
        }
    }
}

/// Classifies the commit outcome. The variant matters as much as the fact of an
/// error: a journal `write_batch` failure propagates as `Error::Io`, while a
/// failure in the later `persist` path is converted to `Error::Poisoned`. That
/// distinction is what stops a persistence failure from standing in for the
/// one-shot journal result.
fn classify(result: &fjall::Result<()>) -> (&'static str, &'static str, String, String) {
    match result {
        Ok(()) => ("ok", "none", "none".to_owned(), "none".to_owned()),
        Err(error) => {
            let debug = format!("{error:?}").replace('\n', " ");
            match error {
                fjall::Error::Io(io) => (
                    "err",
                    "io",
                    io.raw_os_error()
                        .map_or_else(|| "none".to_owned(), |code| code.to_string()),
                    debug,
                ),
                fjall::Error::Poisoned => ("err", "poisoned", "none".to_owned(), debug),
                _ => ("err", "other", "none".to_owned(), debug),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// Runs one release probe. `version` is the pinned Fjall version this binary
/// was compiled against.
pub fn run(version: &str) -> ! {
    let mut args = std::env::args().skip(1);
    let mode_arg = args.next().unwrap_or_default();
    let Some(mode) = Mode::parse(&mode_arg) else {
        eprintln!(
            "usage: <probe> <healthy|one-shot|persistent|undersized|misinjected> <directory>"
        );
        std::process::exit(64);
    };
    let Some(directory) = args.next().map(PathBuf::from) else {
        eprintln!("missing probe directory");
        std::process::exit(64);
    };

    let mut evidence = Evidence::new();
    evidence.record("PROBE_VERSION", version);
    evidence.record("PROBE_MODE", mode.as_str());
    evidence.record("PROBE_PLATFORM", std::env::consts::OS);
    evidence.record("JOURNAL_BUFFER_BYTES", JOURNAL_BUFFER_BYTES);

    let database_path = directory.join("db");
    std::fs::create_dir_all(&directory).expect("probe directory");

    let (database, keyspaces) = open_database(&database_path).expect("open fjall database");

    // --- baseline --------------------------------------------------------
    let mut baseline = database.write_tx().durability(Some(PersistMode::SyncAll));
    for (name, keyspace) in KEYSPACES.iter().zip(&keyspaces) {
        for index in 0..PRE_STATE_ROWS {
            baseline.insert(
                keyspace,
                pre_state_key(name, index),
                pre_state_value(name, index),
            );
        }
    }
    baseline.commit().expect("commit baseline");
    database
        .persist(PersistMode::SyncAll)
        .expect("sync baseline");

    let pre_state = capture(&database, &keyspaces);
    evidence.record("STATE_PRE", encode_snapshot(&pre_state));

    // --- arm -------------------------------------------------------------
    let journal_path = database_path.join(ACTIVE_JOURNAL);
    let journal_len_before = journal_written_len(&journal_path).expect("scan journal write offset");
    evidence.record("JOURNAL_PATH", journal_path.display());
    evidence.record("JOURNAL_LEN_BEFORE_ARM", journal_len_before);

    if journal_len_before == 0 || journal_len_before >= JOURNAL_SCAN_WINDOW {
        evidence.refuse(format!(
            "journal write offset {journal_len_before} outside the assumed fixture window"
        ));
    }
    if database_path.join(ROTATED_JOURNAL).exists() {
        evidence.refuse("journal rotated before the fault was armed");
    }

    let soft_limit = journal_len_before + FAULT_MARGIN_BYTES;
    evidence.record(
        "RLIMIT_FSIZE_SOFT",
        if mode.arms_fault() {
            soft_limit.to_string()
        } else {
            "unarmed".to_owned()
        },
    );
    evidence.record(
        "RLIMIT_FSIZE_HARD",
        if mode.arms_fault() {
            HARD_LIMIT_BYTES.to_string()
        } else {
            "unarmed".to_owned()
        },
    );

    if mode.arms_fault() {
        fault::arm(soft_limit, HARD_LIMIT_BYTES, mode.one_shot()).expect("arm RLIMIT_FSIZE fault");
    }

    // Mis-injection control: spend the one-shot fault on a file that is not the
    // journal, so the journal write that follows is healthy.
    if mode == Mode::MisInjected {
        let scratch = directory.join("scratch.bin");
        let filler = vec![b'x'; (soft_limit + 4_096) as usize];
        let scratch_result = std::fs::write(&scratch, &filler);
        evidence.record(
            "MISINJECT_SCRATCH_WRITE",
            match &scratch_result {
                Ok(()) => "ok".to_owned(),
                Err(error) => error
                    .raw_os_error()
                    .map_or_else(|| "err".to_owned(), |code| format!("errno:{code}")),
            },
        );
        evidence.record("MISINJECT_SIGNALS", fault::signal_count());
    }

    // --- target transaction ----------------------------------------------
    let rows = mode.rows_per_keyspace();
    let width = mode.value_bytes();
    let mut target = database.write_tx().durability(Some(PersistMode::SyncAll));
    let mut target_bytes = 0usize;
    for (name, keyspace) in KEYSPACES.iter().zip(&keyspaces) {
        for index in 0..rows {
            let key = target_key(name, index);
            let value = target_value(name, index, width);
            target_bytes += key.len() + value.len();
            target.insert(keyspace, key, value);
        }
    }
    evidence.record("TARGET_BATCH_BYTES", target_bytes);
    evidence.record(
        "TARGET_EXCEEDS_JOURNAL_BUFFER",
        target_bytes > JOURNAL_BUFFER_BYTES,
    );

    let signals_before_commit = fault::signal_count();
    let commit_result = target.commit();
    let signals_after_commit = fault::signal_count();

    if mode.arms_fault() {
        fault::disarm(HARD_LIMIT_BYTES).expect("disarm RLIMIT_FSIZE fault");
    }

    let (outcome, kind, errno, debug) = classify(&commit_result);
    evidence.record("COMMIT_RESULT", outcome);
    evidence.record("COMMIT_ERROR_KIND", kind);
    evidence.record("COMMIT_ERROR_ERRNO", errno);
    evidence.record("COMMIT_ERROR_DEBUG", debug);
    evidence.record("SIGNAL_COUNT_TOTAL", signals_after_commit);
    evidence.record(
        "SIGNAL_COUNT_DURING_COMMIT",
        signals_after_commit - signals_before_commit,
    );

    // --- live (in-process) state -----------------------------------------
    evidence.record(
        "STATE_LIVE",
        encode_snapshot(&capture(&database, &keyspaces)),
    );
    evidence.record(
        "JOURNAL_LEN_AFTER",
        journal_written_len(&journal_path).unwrap_or(0),
    );
    evidence.record(
        "JOURNAL_ROTATED",
        database_path.join(ROTATED_JOURNAL).exists(),
    );

    drop(keyspaces);
    drop(database);

    // --- reopen twice ------------------------------------------------------
    // Two reopens, because "recovers to the exact pre-transaction state" is a
    // claim about a stable state, not about one lucky recovery pass.
    for pass in 1..=2 {
        match open_database(&database_path) {
            Ok((reopened, reopened_keyspaces)) => {
                let snapshot = capture(&reopened, &reopened_keyspaces);
                evidence.record(&format!("STATE_REOPEN{pass}"), encode_snapshot(&snapshot));
                drop(reopened_keyspaces);
                drop(reopened);
            }
            Err(error) => {
                evidence.record(
                    &format!("STATE_REOPEN{pass}"),
                    format!("REOPEN_FAILED:{error:?}").replace('\n', " "),
                );
            }
        }
    }

    // --- probe-visible refusals -------------------------------------------
    if mode.arms_fault() && mode.one_shot() && signals_after_commit > 1 {
        evidence.refuse(format!(
            "one-shot fault injected {signals_after_commit} failures; exactly one is required"
        ));
    }
    if mode == Mode::OneShot && signals_after_commit - signals_before_commit == 0 {
        evidence.refuse("one-shot fault did not fire during the target commit");
    }
    if mode == Mode::MisInjected && signals_after_commit - signals_before_commit > 0 {
        evidence.refuse(
            "mis-injection control leaked a fault into the target commit; the scratch write \
             was supposed to consume it",
        );
    }
    if mode == Mode::MisInjected && signals_before_commit != 1 {
        evidence.refuse(format!(
            "mis-injection control consumed {signals_before_commit} faults on the scratch file; \
             exactly one is required"
        ));
    }
    if database_path.join(ROTATED_JOURNAL).exists() {
        evidence.refuse("journal rotated during the probe; the armed offset is not the target");
    }
    if mode == Mode::Undersized && target_bytes > JOURNAL_BUFFER_BYTES {
        evidence.refuse("undersized control batch is not actually below the journal buffer");
    }

    std::process::exit(evidence.emit());
}
