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
use std::panic::{catch_unwind, AssertUnwindSafe};
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
/// Every `read` fails: the store's file has gone away underneath a live
/// handle — the removable volume, the sandbox container reclaimed by the
/// OS, the file another process replaced. Reads alone; a peek opens no
/// write transaction, so this arms exactly the seam #763 is about.
const FAIL_READ: u8 = 3;
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
    failed_reads: AtomicUsize,
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

    fn failed_reads(&self) -> usize {
        self.failed_reads.load(Ordering::SeqCst)
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

/// `EIO`. A read that reaches the device and fails there — the honest shape
/// of a store whose backing file went away under a live handle.
fn disk_gone() -> io::Error {
    io::Error::from_raw_os_error(5)
}

impl StorageBackend for FaultBackend {
    fn len(&self) -> Result<u64, io::Error> {
        self.inner.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), io::Error> {
        if self.control.mode.load(Ordering::SeqCst) == FAIL_READ {
            self.control.failed_reads.fetch_add(1, Ordering::SeqCst);
            return Err(disk_gone());
        }
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
/// memory, redb's header-driven repair. The test-only in-memory backend has
/// no filesystem identity the production #1362 reconstruction door could
/// reopen, so this helper supplies its next backend generation explicitly.
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
            payload: crate::AcceptWritePayload::Event {
                frozen: Box::new(frozen),
                replaceable_base: None,
                monotonic_stamp: false,
                routing: "durability-proof".into(),
                sig_state: IntentSigState::Pending,
            },
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "durability-proof".into(),
            accepted_at: Timestamp::from(created_at),
            correlation: None,
        })
        .map(|_| ())
}

#[test]
fn reopen_replaces_only_the_database_generation_and_preserves_durable_identity() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("engine-owned-reopen.redb");
    let mut store = RedbStore::open(&path).expect("healthy persistent store");
    let keys = keys();
    let frozen = frozen_event(900);
    let outcome = store
        .accept_write(AcceptWrite {
            payload: crate::AcceptWritePayload::Event {
                frozen: Box::new(frozen.clone()),
                replaceable_base: None,
                monotonic_stamp: false,
                routing: "reopen-proof".into(),
                sig_state: IntentSigState::Pending,
            },
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "reopen-proof".into(),
            accepted_at: Timestamp::from(900),
            correlation: Some(
                nmp_grammar::CorrelationToken::try_from("stable-reopen-correlation").unwrap(),
            ),
        })
        .expect("accept before reconstruction");
    let receipt = outcome.journaled_receipt_id().expect("accepted receipt");

    // Model the exact lifecycle edge after a typed requires-reopen fault: the
    // poisoned redb handle is gone, but the NMP ownership fence remains.
    drop(store.db.take());
    assert!(matches!(
        RedbStore::open(&path),
        Err(crate::RedbStoreOpenError::StoreAlreadyOpen { .. })
    ));

    store
        .reopen_after_failure()
        .expect("same owner reconstructs the redb handle");
    assert_eq!(
        store
            .lookup_correlation("stable-reopen-correlation")
            .unwrap(),
        Some(receipt)
    );
    assert_eq!(
        store.reattach_receipt(receipt).unwrap().unwrap().event_id(),
        Some(frozen.id)
    );
}

#[test]
fn reopen_refuses_to_create_a_missing_durable_target() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("missing-during-reopen.redb");
    let mut store = RedbStore::open(&path).expect("healthy persistent store");

    drop(store.db.take());
    std::fs::remove_file(&path).expect("remove target while ownership fence remains");

    let error = store
        .reopen_after_failure()
        .expect_err("reconstruction must not initialize a replacement store");
    assert_eq!(error.fault(), crate::PersistenceFault::Io);
    assert!(!path.exists(), "failed reconstruction recreated the target");
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

    // The #1362 recovery door replaces the whole database generation; it can
    // never retry one of these mutations against this latched transactional
    // memory. This injected in-memory backend has no persistent file to
    // reconstruct, so prove the distinction with a separate healthy store:
    // the new handle works and the old handle remains latched.
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

// ---- #763: the two peeks that used to abort the host ---------------------

/// Seed one expiring row and one coverage row, so both peeks have something
/// real to find and `Ok(None)` cannot pass for a successful read by
/// accident.
fn seed_peekable_state(store: &mut RedbStore) {
    let signed = EventBuilder::new(Kind::TextNote, "peek-fixture")
        .custom_created_at(Timestamp::from(1_000))
        .tag(nostr::Tag::expiration(Timestamp::from(9_000)))
        .sign_with_keys(&keys())
        .expect("sign peek fixture");
    store
        .insert(
            signed,
            crate::RelayObserved::new(peek_relay(), Timestamp::from(1_000)),
        )
        .expect("seed an expiring row");
    store
        .record_coverage(&[(
            peek_atom(),
            peek_relay(),
            crate::CoverageInterval::new(Timestamp::from(10), Timestamp::from(20)),
        )])
        .expect("seed a coverage row");
}

/// A seeded store on its own storage whose backend can be made to fail.
///
/// The healthy readings live on a SEPARATE store deliberately: a peek warms
/// redb's page cache with exactly the pages it needs, so peeking once for
/// reassurance and then arming the fault would prove only that redb can
/// answer out of memory.
fn seeded_peekable_store(dir: &TempDir, name: &str) -> (RedbStore, Arc<FaultControl>) {
    let (mut store, control, _bytes) = store_with_injectable_backend(dir, name);
    seed_peekable_state(&mut store);
    (store, control)
}

fn peek_relay() -> RelayUrl {
    RelayUrl::parse("wss://peek.example").expect("peek relay")
}

fn peek_atom() -> nmp_grammar::ContextualAtom {
    nmp_grammar::ContextualAtom {
        filter: nmp_grammar::ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(BTreeSet::from([keys().public_key().to_hex()])),
            ids: None,
            tags: std::collections::BTreeMap::new(),
            since: None,
            until: None,
            limit: None,
        },
        source: nmp_grammar::SourceAuthority::AuthorOutboxes,
        access: nmp_grammar::AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

/// #763 falsifier: a backend read failure on the deadline peek is a value,
/// not a panic.
///
/// This is the whole reason the issue exists. `next_expiration` is called
/// once per engine-loop iteration to arm the wait, on the embedder's own
/// thread; on iOS and Android that thread belongs to the application, so
/// the `.expect("redb: begin_read")` this replaced did not degrade NMP, it
/// terminated a shipped app. The fault is armed on a store that was healthy
/// a moment ago, and nothing about the caller changed — which is exactly
/// why this cannot be a panic.
#[test]
fn a_failing_read_makes_the_deadline_peek_report_rather_than_abort() {
    let dir = TempDir::new().expect("tempdir");

    // A real answer from healthy storage, so the `Err` below is provably
    // distinguishable from a peek that simply found nothing.
    let (healthy, _control) = seeded_peekable_store(&dir, "peek-deadline-healthy.redb");
    assert_eq!(
        healthy.next_expiration().expect("healthy deadline peek"),
        Some(Timestamp::from(9_000)),
        "a healthy peek answers the indexed deadline"
    );
    drop(healthy);

    let (store, control) = seeded_peekable_store(&dir, "peek-deadline.redb");
    control.arm(FAIL_READ);
    let outcome = catch_unwind(AssertUnwindSafe(|| store.next_expiration()))
        .unwrap_or_else(|_| panic!("the deadline peek aborted the host process on a read error"));
    let error = outcome.expect_err("a failing backend read must surface as Err");
    assert!(
        control.failed_reads() > 0,
        "the fault must have been reached at the backend"
    );
    assert_ne!(
        error.fault(),
        PersistenceFault::Invariant,
        "a read that could not reach its bytes is a condition, not this crate misusing its own \
         database: {}",
        error.message()
    );
}

/// #763 falsifier: the same for the coverage peek, and with the stronger
/// requirement that the failure never renders as absent coverage.
///
/// `Ok(None)` here means "this relay has proven nothing for this key",
/// which is a cache-miss decision an engine acts on by refetching. A read
/// that could not answer must not be able to say that.
#[test]
fn a_failing_read_makes_the_coverage_peek_report_rather_than_abort() {
    let dir = TempDir::new().expect("tempdir");
    let key = crate::coverage::coverage_key(&peek_atom());
    let absent = RelayUrl::parse("wss://never-asked.example").expect("absent relay");

    let (healthy, _control) = seeded_peekable_store(&dir, "peek-coverage-healthy.redb");
    assert_eq!(
        healthy
            .get_coverage(key, &peek_relay())
            .expect("healthy coverage peek"),
        Some(crate::CoverageInterval::new(
            Timestamp::from(10),
            Timestamp::from(20)
        )),
        "a healthy peek answers the recorded interval"
    );
    assert_eq!(
        healthy.get_coverage(key, &absent).expect("healthy peek"),
        None,
        "an unrecorded row is honest absence"
    );
    drop(healthy);

    let (store, control) = seeded_peekable_store(&dir, "peek-coverage.redb");
    control.arm(FAIL_READ);
    let outcome = catch_unwind(AssertUnwindSafe(|| store.get_coverage(key, &peek_relay())))
        .unwrap_or_else(|_| panic!("the coverage peek aborted the host process on a read error"));
    let error = outcome.expect_err("a failing backend read must surface as Err");
    assert!(
        control.failed_reads() > 0,
        "the fault must have been reached at the backend"
    );
    assert_ne!(
        error.fault(),
        PersistenceFault::Invariant,
        "a backend read failure is not the caller's bug: {}",
        error.message()
    );
}

/// #763 falsifier: the host survives. A store whose reads all fail is asked
/// for both peeks sixteen times; every answer is a value, and the process
/// that asked is still running to make the assertion.
#[test]
fn both_peeks_leave_the_host_alive_across_repeated_read_failures() {
    let dir = TempDir::new().expect("tempdir");
    let (store, control) = seeded_peekable_store(&dir, "peek-alive.redb");
    let key = crate::coverage::coverage_key(&peek_atom());

    control.arm(FAIL_READ);
    let mut faults: Vec<PersistenceFault> = Vec::new();
    for _ in 0..8 {
        faults.push(
            store
                .next_expiration()
                .expect_err("a failing read never answers a deadline")
                .fault(),
        );
        faults.push(
            store
                .get_coverage(key, &peek_relay())
                .expect_err("a failing read never answers coverage")
                .fault(),
        );
    }
    // The first failure is the originating read; every one after it is
    // redb's own latch refusing ahead of the backend (#895). Both are
    // environmental, both are values, and both leave the host alive.
    assert!(
        faults.contains(&PersistenceFault::Latched),
        "a repeated peek against a latched handle stays a report: {faults:?}"
    );
    assert!(
        !faults.contains(&PersistenceFault::Invariant),
        "no peek blamed the caller for a disk that went away: {faults:?}"
    );
}
