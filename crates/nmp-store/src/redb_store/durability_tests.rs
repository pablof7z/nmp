//! #895 falsifiers for the durability classification carried by
//! [`PersistenceError`].
//!
//! These drive a real [`RedbStore`] over a real `redb::Database` whose
//! storage backend fails on demand, so the classification is proven against
//! redb's own failure machinery — `CheckedBackend`'s latch, `commit_inner`'s
//! ordering, the post-durability `resize` — and not against hand-built
//! `StorageError` values that only assert this module's own opinion.
//!
//! Nothing here reads an error message. That is the point: before this, the
//! only way to tell a dead handle from a failed write was
//! `contains("Previous I/O")`.

use std::collections::BTreeSet;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use nostr::{EventBuilder, Keys, Kind, RelayUrl, Timestamp};
use redb::backends::InMemoryBackend;
use redb::StorageBackend;
use tempfile::TempDir;

use super::store::RedbStore;
use crate::{
    AcceptWrite, DurabilityOutcome, EventStore, IntentId, IntentSigState, PersistenceError,
    PersistenceFault,
};

// ---- fault-injecting storage backend -----------------------------------

/// Every `write` fails: the disk-full sequence from the incident. (`0` is
/// the disarmed default and needs no name — `FaultControl` starts there.)
const FAIL_WRITE: u8 = 1;
/// `write`/`sync_data` succeed and the FIRST `set_len` after a successful
/// `sync_data` fails. That is exactly redb's post-durability `resize`
/// (`page_manager.rs` — `storage.flush()` returns `Ok`, so the transaction
/// IS durable, and the `resize` that follows returns `Err` anyway).
const FAIL_RESIZE_AFTER_SYNC: u8 = 2;

/// Test-side handle on a backend that redb has taken ownership of.
#[derive(Debug, Default)]
struct FaultControl {
    mode: AtomicU8,
    synced: AtomicBool,
    failed_writes: AtomicUsize,
    failed_post_sync_resizes: AtomicUsize,
}

impl FaultControl {
    fn arm(&self, mode: u8) {
        self.synced.store(false, Ordering::SeqCst);
        self.mode.store(mode, Ordering::SeqCst);
    }

    fn failed_writes(&self) -> usize {
        self.failed_writes.load(Ordering::SeqCst)
    }

    fn failed_post_sync_resizes(&self) -> usize {
        self.failed_post_sync_resizes.load(Ordering::SeqCst)
    }
}

/// The bytes are shared, not owned, so a test can stand a second store up
/// over the same storage after dropping the first — an ordinary
/// close-and-reopen, which is what an embedder does by restarting today.
#[derive(Debug)]
struct FaultBackend {
    inner: Arc<InMemoryBackend>,
    control: Arc<FaultControl>,
}

fn disk_full() -> io::Error {
    // The incident's errno. redb wraps it verbatim into `StorageError::Io`.
    io::Error::from_raw_os_error(28)
}

impl StorageBackend for FaultBackend {
    fn len(&self) -> Result<u64, io::Error> {
        self.inner.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), io::Error> {
        self.inner.read(offset, out)
    }

    fn set_len(&self, len: u64) -> Result<(), io::Error> {
        // Two conditions together pin this to `commit_inner`'s post-flush
        // `resize` and nothing else: it follows a successful `sync_data`
        // (the durability point), and it shrinks the file (redb only
        // resizes there when `try_shrink` trimmed the layout). A growth
        // before a flush would be a pre-durability failure — a different,
        // determinate-absent story that must not be labelled this one.
        if self.control.mode.load(Ordering::SeqCst) == FAIL_RESIZE_AFTER_SYNC
            && len < self.inner.len()?
            && self.control.synced.swap(false, Ordering::SeqCst)
        {
            self.control
                .failed_post_sync_resizes
                .fetch_add(1, Ordering::SeqCst);
            return Err(disk_full());
        }
        self.inner.set_len(len)
    }

    fn sync_data(&self) -> Result<(), io::Error> {
        self.inner.sync_data()?;
        self.control.synced.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), io::Error> {
        if self.control.mode.load(Ordering::SeqCst) == FAIL_WRITE {
            self.control.failed_writes.fetch_add(1, Ordering::SeqCst);
            return Err(disk_full());
        }
        self.inner.write(offset, data)
    }
}

/// A healthy store whose backend can be made to fail afterwards. The store
/// is opened with the fault disarmed so schema creation is a normal, fully
/// durable open — the failure is injected into a live, healthy handle,
/// exactly as a disk filling up would.
fn store_with_injectable_backend(
    dir: &TempDir,
    name: &str,
) -> (RedbStore, Arc<FaultControl>, Arc<InMemoryBackend>) {
    let control = Arc::new(FaultControl::default());
    let bytes = Arc::new(InMemoryBackend::new());
    let backend = FaultBackend {
        inner: Arc::clone(&bytes),
        control: Arc::clone(&control),
    };
    let store = RedbStore::open_with_backend(dir.path().join(name), backend)
        .expect("healthy open over an in-memory backend");
    (store, control, bytes)
}

/// Close a store and open a fresh one over the same storage, with no fault
/// armed. This is a plain reopen — a new `Database`, a new transactional
/// memory, redb's header-driven repair — not an in-place recovery door.
/// `nmp-store` has none and will not grow one: #895 declined that door
/// because dropping the owner and opening again IS the recovery, and it
/// needs no process restart to do it.
fn reopen_over(dir: &TempDir, name: &str, bytes: &Arc<InMemoryBackend>) -> RedbStore {
    let backend = FaultBackend {
        inner: Arc::clone(bytes),
        control: Arc::new(FaultControl::default()),
    };
    RedbStore::open_with_backend(dir.path().join(name), backend).expect("reopen after a failure")
}

fn keys() -> Keys {
    Keys::parse("0000000000000000000000000000000000000000000000000000000000000002")
        .expect("fixed durability-proof key")
}

fn frozen_event(created_at: u64) -> nostr::Event {
    let signed = EventBuilder::new(Kind::TextNote, format!("durability-proof-{created_at}"))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(&keys())
        .expect("sign durability-proof event");
    nostr::Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        crate::sentinel_signature(),
    )
}

/// One durable write attempt through the real acceptance door.
fn attempt_durable_write(store: &mut RedbStore, created_at: u64) -> Result<(), PersistenceError> {
    let keys = keys();
    let frozen = frozen_event(created_at);
    store
        .accept_write(AcceptWrite {
            frozen,
            replaceable_base: None,
            monotonic_stamp: false,
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "durability-proof".into(),
            routing: "durability-proof".into(),
            sig_state: IntentSigState::Pending,
            accepted_at: Timestamp::from(created_at),
            correlation: None,
        })
        .map(|_| ())
}

// ---- falsifiers --------------------------------------------------------

/// #895 falsifier 1: a latched store is distinguishable from the write
/// failure that latched it, through the public `nmp-store` API, with no
/// string matching anywhere.
///
/// The two are genuinely different facts. The first failure is the disk
/// write that went wrong and whose durability nobody can determine; every
/// one after it is redb's `CheckedBackend::check_failure()` refusing ahead
/// of the backend call, so those writes provably never happened. Before
/// this, both arrived as `PersistenceError(String)` and an embedder had to
/// grep for "Previous I/O" to tell them apart.
#[test]
fn a_latched_store_is_distinguishable_from_the_write_failure_that_latched_it() {
    let dir = TempDir::new().expect("tempdir");
    let (mut store, control, _bytes) = store_with_injectable_backend(&dir, "latched.redb");

    // A healthy durable write first, so the store is provably working.
    attempt_durable_write(&mut store, 1_000).expect("healthy durable write");

    control.arm(FAIL_WRITE);
    let first = attempt_durable_write(&mut store, 1_001)
        .expect_err("a failing backend write must surface as Err");
    assert!(
        control.failed_writes() > 0,
        "the fault must have been reached at the backend"
    );
    assert_eq!(
        first.fault(),
        PersistenceFault::Io,
        "the originating failure is redb's `Io`, not the latch"
    );
    assert_eq!(
        first.durability(),
        DurabilityOutcome::Unknown,
        "the first I/O failure never claims the write is absent"
    );
    assert!(first.fault().requires_reopen());

    // Every later write is a latch report. redb answers it before touching
    // the backend, which is why it is determinate-absent.
    let writes_before = control.failed_writes();
    let second =
        attempt_durable_write(&mut store, 1_002).expect_err("a latched store must refuse writes");
    assert_eq!(second.fault(), PersistenceFault::Latched);
    assert_eq!(second.durability(), DurabilityOutcome::Absent);
    assert!(second.fault().requires_reopen());
    assert_eq!(
        control.failed_writes(),
        writes_before,
        "a latch report must not have reached the backend at all"
    );

    assert_ne!(
        first.fault(),
        second.fault(),
        "the two must be distinguishable without reading either message"
    );
}

/// #895 falsifier 2: no path in this crate retries a commit against a
/// `needs_recovery` transactional memory.
///
/// The invariant is asserted directly rather than through the error that
/// `begin_write` happens to return: redb's `commit_inner` opens with
/// `assert!(!self.needs_recovery.load(..))`, so an in-place commit retry
/// aborts the process. Hammering every durable door against the latched
/// handle therefore proves the absence of such a retry — if any of them
/// retried the commit, this test would not fail, it would die. Which error
/// comes back is deliberately not asserted; only that a failure is what
/// comes back, forever, and that it always says reopen.
#[test]
fn no_door_retries_a_commit_against_a_latched_memory() {
    let dir = TempDir::new().expect("tempdir");
    let (mut store, control, _bytes) = store_with_injectable_backend(&dir, "no-retry.redb");

    attempt_durable_write(&mut store, 2_000).expect("healthy durable write");
    control.arm(FAIL_WRITE);
    attempt_durable_write(&mut store, 2_001).expect_err("first failure latches the handle");

    // Several independent commit paths, repeatedly. Each opens its own
    // write transaction against the same latched memory.
    for round in 0..16 {
        let mut faults = vec![attempt_durable_write(&mut store, 2_100 + round).err()];
        faults.push(
            store
                .accept_refused(
                    frozen_event(2_200 + round).id,
                    keys().public_key(),
                    crate::RefuseReason::Tombstoned,
                )
                .err(),
        );
        faults.push(
            store
                .record_route_revision(
                    IntentId(1),
                    BTreeSet::from([
                        RelayUrl::parse("wss://durability-proof.example").expect("fixed relay url")
                    ]),
                )
                .err(),
        );
        for fault in faults {
            let error = fault.expect("a latched handle never starts succeeding on its own");
            assert!(
                error.fault().requires_reopen(),
                "every post-latch failure must tell the embedder to reopen, got {:?}",
                error.fault()
            );
        }
    }

    // The store also exposes no door that could retry: the only recovery is
    // dropping this handle and opening a new one. Prove that is what it
    // takes — a fresh handle over a fresh, healthy backend works, and the
    // latched one is still latched afterwards.
    let (mut reopened, _, _) = store_with_injectable_backend(&dir, "no-retry-reopened.redb");
    attempt_durable_write(&mut reopened, 2_200).expect("a reopened handle writes normally");
    assert!(attempt_durable_write(&mut store, 2_201)
        .expect_err("the latched handle stays latched")
        .fault()
        .requires_reopen());
}

/// #895 falsifier 3: a `resize` failure that happens AFTER a successful
/// durability flush is never reported as though the transaction is absent.
///
/// redb's `commit_inner` runs `storage.flush()` — the durability point —
/// and only then `storage.resize(...)`. If the resize fails, the commit
/// returns `Err` on a transaction that is already on disk. Classifying that
/// as absent is precisely how an embedder duplicates a durable write by
/// "retrying" it, so `Io` must resolve to `Unknown`.
///
/// The backend below fails the first `set_len` that follows a successful
/// `sync_data`, which is that post-durability resize and nothing else.
#[test]
fn a_post_flush_resize_failure_is_not_reported_as_absent() {
    let dir = TempDir::new().expect("tempdir");
    let (mut store, control, bytes) = store_with_injectable_backend(&dir, "post-flush-resize.redb");

    attempt_durable_write(&mut store, 3_000).expect("healthy durable write");
    control.arm(FAIL_RESIZE_AFTER_SYNC);

    // Drive writes until the post-durability resize is actually reached.
    // redb only resizes on a commit that shrank the file, so the trigger is
    // workload-dependent; the assertion below refuses to pass vacuously.
    let mut observed = None;
    for round in 0..64 {
        let created_at = 3_100 + round;
        match attempt_durable_write(&mut store, created_at) {
            Ok(()) => {}
            Err(error) => {
                observed = Some((created_at, error));
                break;
            }
        }
    }
    let (created_at, error) = observed.expect("the post-flush resize fault must be reached");
    assert!(
        control.failed_post_sync_resizes() > 0,
        "the failure must be the post-durability resize, not some earlier write"
    );
    assert_eq!(error.fault(), PersistenceFault::Io);
    assert_ne!(
        error.durability(),
        DurabilityOutcome::Absent,
        "a transaction that already flushed must never be reported as absent"
    );
    assert_eq!(error.durability(), DurabilityOutcome::Unknown);

    // And it really was durable. The handle is latched, so the readback
    // needs a reopen — which is exactly the recovery shape #895 describes:
    // catch the error, reopen, read the key back. The row the caller was
    // told had failed is there. "Absent" would not merely have been
    // unprovable here; it would have been false, and an embedder that
    // retried on it would have written this event twice.
    drop(store);
    let reopened = reopen_over(&dir, "post-flush-resize.redb", &bytes);
    let stored = reopened
        .query(&nostr::Filter::new().id(frozen_event(created_at).id))
        .expect("a reopened store reads normally");
    assert_eq!(
        stored.len(),
        1,
        "the transaction the caller saw fail is on disk"
    );
}

/// The classification is legible in whatever an embedder logs (#895 §2).
///
/// In the incident, thirty latch reports and the one indeterminate failure
/// were the same shape of line. `Display` now separates them without the
/// reader knowing anything about redb, while an `Invariant` — which carries
/// no durability question — renders exactly as it always did.
#[test]
fn the_rendered_failure_separates_the_first_io_from_the_latch_reports() {
    let dir = TempDir::new().expect("tempdir");
    let (mut store, control, _bytes) = store_with_injectable_backend(&dir, "logged.redb");

    attempt_durable_write(&mut store, 4_000).expect("healthy durable write");
    control.arm(FAIL_WRITE);

    let first = attempt_durable_write(&mut store, 4_001)
        .expect_err("first failure")
        .to_string();
    let latched = attempt_durable_write(&mut store, 4_002)
        .expect_err("latch report")
        .to_string();

    assert!(
        first.contains("fault=io") && first.contains("durability=unknown"),
        "the originating failure must say so in the log line: {first}"
    );
    assert!(
        latched.contains("fault=latched") && latched.contains("durability=absent"),
        "a latch report must say so in the log line: {latched}"
    );
    assert!(first.contains("reopen=required") && latched.contains("reopen=required"));

    assert_eq!(
        PersistenceError::invariant("decode delivery intent 7").to_string(),
        "durable-store persistence failure: decode delivery intent 7",
        "an invariant carries no durability question and gains no annotation"
    );
}

/// The durability axis is a property of the fault, not of the call site, so
/// pin the whole table. The rule that must survive anything added later:
/// `Absent` is a positive claim, so no fault that cannot prove the write
/// did not happen may claim it, and nothing indeterminate may read as safe
/// to retry against the same handle.
#[test]
fn every_fault_that_is_not_determinate_absent_demands_a_reopen() {
    for fault in [
        PersistenceFault::Latched,
        PersistenceFault::Io,
        PersistenceFault::Corrupted,
        PersistenceFault::ValueTooLarge,
        PersistenceFault::LockPoisoned,
        PersistenceFault::UnknownBackend,
        PersistenceFault::Invariant,
    ] {
        if fault.durability() != DurabilityOutcome::Absent {
            assert!(
                fault.requires_reopen(),
                "{fault:?} is not determinate-absent, so it must never read as retryable in place"
            );
        }
    }

    assert_eq!(
        PersistenceFault::Io.durability(),
        DurabilityOutcome::Unknown,
        "`Io` is the conservative union: may be absent, may be durable"
    );
    for indeterminate in [
        PersistenceFault::Corrupted,
        PersistenceFault::LockPoisoned,
        PersistenceFault::UnknownBackend,
    ] {
        assert_eq!(
            indeterminate.durability(),
            DurabilityOutcome::Unknown,
            "{indeterminate:?} can be reported from inside a commit, so it cannot claim absence"
        );
    }
    assert_eq!(
        PersistenceFault::Latched.durability(),
        DurabilityOutcome::Absent,
        "the latch is raised before the backend op, so the write never happened"
    );
    // A local, deterministic refusal leaves a healthy handle. Reopening
    // would be pure cost, and claiming otherwise would train embedders to
    // reopen on a bad argument.
    assert!(!PersistenceFault::ValueTooLarge.requires_reopen());
    assert!(!PersistenceFault::Invariant.requires_reopen());
    assert_eq!(
        PersistenceFault::UnknownBackend.label(),
        "unknown-backend",
        "a future backend state must stay distinguishable from Io and Invariant"
    );
}

/// Reachability: every fault redb 4.1 can produce has a typed input at the
/// `persist_err` funnel. `UnknownBackend` is deliberately the exception: it
/// is reserved for a future non-exhaustive variant and is exercised through
/// the exact fallback helper plus the source-shape gate.
#[test]
fn every_current_fault_variant_has_a_redb_error_that_produces_it() {
    use super::schema::{persist_err, unknown_backend_fault};

    let cases = [
        (
            persist_err(redb::StorageError::PreviousIo),
            PersistenceFault::Latched,
        ),
        (
            persist_err(redb::StorageError::DatabaseClosed),
            PersistenceFault::Latched,
        ),
        (persist_err(disk_full()), PersistenceFault::Io),
        (
            persist_err(redb::StorageError::Corrupted("torn header".to_owned())),
            PersistenceFault::Corrupted,
        ),
        (
            persist_err(redb::StorageError::ValueTooLarge(4 << 30)),
            PersistenceFault::ValueTooLarge,
        ),
        (
            persist_err(redb::StorageError::LockPoisoned(
                std::panic::Location::caller(),
            )),
            PersistenceFault::LockPoisoned,
        ),
        // Above the storage layer: this crate asking redb for a table it
        // never created is a bug in this crate, not a disk event.
        (
            persist_err(redb::TableError::TableDoesNotExist("events_v9".to_owned())),
            PersistenceFault::Invariant,
        ),
    ];

    for (error, expected) in cases {
        assert_eq!(error.fault(), expected, "message was {:?}", error.message());
        assert!(
            !error.message().is_empty(),
            "the display string must survive classification"
        );
    }

    let future = unknown_backend_fault();
    assert_eq!(future, PersistenceFault::UnknownBackend);
    assert_eq!(future.durability(), DurabilityOutcome::Unknown);
    assert!(future.requires_reopen());
}
