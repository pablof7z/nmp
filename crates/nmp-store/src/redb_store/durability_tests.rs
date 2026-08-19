//! Falsifiers for what a real backend failure costs.
//!
//! These drive a real [`RedbStore`] over a real `redb::Database` whose
//! storage backend fails on demand, so the claims are proven against redb's
//! own failure machinery — `CheckedBackend`'s latch, `commit_inner`'s
//! ordering, the post-durability `resize` — and not against hand-built
//! `StorageError` values that only assert this module's own opinion.
//!
//! Two claims, and no third. Acceptance is atomic, so a failure around it
//! leaves either no write or one whole write for the next generation to find
//! (#1362). And a failing read is a value, not a panic (#763): the peeks run
//! on the embedder's own thread, where an `.expect()` terminates a shipped
//! app rather than degrading NMP.
//!
//! There is deliberately nothing here about classifying the failure. A store
//! write that fails, fails; the operation returns `Err` and the engine
//! carries on.

use nmp_grammar::RelaySessionKey;
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
use crate::{AcceptWrite, IntentSigState, PersistenceError};

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


    fn failed_reads(&self) -> usize {
        self.failed_reads.load(Ordering::SeqCst)
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

fn attempt_write(
    store: &mut RedbStore,
    created_at: u64,
) -> Result<crate::AcceptOutcome, PersistenceError> {
    let keys = keys();
    let frozen = frozen_event(created_at);
    store.accept_write(AcceptWrite {
        payload: crate::AcceptWritePayload::Event {
            frozen: Box::new(frozen),
            routing: "durability-proof".into(),
            sig_state: IntentSigState::Pending,
        },
        expected_pubkey: keys.public_key(),
        signing_identity_ref: "durability-proof".into(),
        accepted_at: Timestamp::from(created_at),
    })
}

#[test]
fn a_precommit_io_failure_leaves_no_durable_receipt_for_a_fresh_open() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("accept-precommit-io.redb");
    let mut store = RedbStore::open_with_accept_write_precommit_io(&path)
        .expect("persistent precommit-I/O store");

    attempt_write(&mut store, 901)
        .expect_err("construction-armed precommit I/O must refuse acceptance");
    drop(store);

    // Acceptance is one transaction. A failure before its commit leaves
    // nothing behind, so the process that reopens the file sees a store that
    // never heard of this write -- and `publish()` never returned `Ok`.
    let mut store = RedbStore::open(&path).expect("fresh generation over the same file");
    assert!(
        store.publish_queue_receipts_after(None, u8::MAX).unwrap().is_empty(),
        "the dropped transaction cannot leave a durable receipt behind"
    );
    assert!(store.recover_publish_queue().unwrap().is_empty());

    let accepted =
        attempt_write(&mut store, 901).expect("the fresh store accepts the exact retry once");
    assert_eq!(
        store.publish_queue_receipts_after(None, u8::MAX).unwrap().len(),
        1,
        "the retry is the only durable receipt"
    );
    assert!(accepted.journaled_receipt_id().is_some());
}

#[test]
fn a_commit_then_io_failure_still_yields_one_receipt_to_a_fresh_open() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("accept-commit-then-io.redb");
    let frozen = frozen_event(902);
    let mut store = RedbStore::open_with_accept_write_commit_then_io(&path)
        .expect("persistent commit-then-I/O store");

    attempt_write(&mut store, 902)
        .expect_err("construction-armed postcommit I/O must hide the committed outcome");
    drop(store);

    // The other side of the same boundary: the transaction did commit, so the
    // receipt, its frozen body and its canonical pending row are all on disk
    // even though the caller was handed an `Err`. A fresh generation reads
    // back exactly one of each -- never zero, never two.
    let store = RedbStore::open(&path).expect("fresh generation over the same file");
    let receipt = store
        .publish_queue_receipts_after(None, u8::MAX)
        .unwrap()
        .into_iter()
        .next()
        .expect("the committed receipt is enumerable from a fresh generation");
    assert_eq!(receipt.event_id(), Some(frozen.id));
    assert_eq!(
        store
            .query(&nostr::Filter::new().id(frozen.id))
            .unwrap()
            .len(),
        1,
        "the committed pending row survives the real Redb generation change"
    );
    assert_eq!(
        store.publish_queue_receipts_after(None, u8::MAX).unwrap().len(),
        1,
        "the ambiguous result owns exactly one durable receipt"
    );
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
            RelaySessionKey::unauthenticated(peek_relay().clone()),
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
        routing: nmp_grammar::ReadRouting::Auto,
        authenticate_as: None,
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
    outcome.expect_err("a failing backend read must surface as Err");
    assert!(
        control.failed_reads() > 0,
        "the fault must have been reached at the backend"
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
            .get_coverage(key.clone(), &RelaySessionKey::unauthenticated(peek_relay().clone()))
            .expect("healthy coverage peek"),
        Some(crate::CoverageInterval::new(
            Timestamp::from(10),
            Timestamp::from(20)
        )),
        "a healthy peek answers the recorded interval"
    );
    assert_eq!(
        healthy.get_coverage(key.clone(), &RelaySessionKey::unauthenticated(absent.clone())).expect("healthy peek"),
        None,
        "an unrecorded row is honest absence"
    );
    drop(healthy);

    let (store, control) = seeded_peekable_store(&dir, "peek-coverage.redb");
    control.arm(FAIL_READ);
    let outcome = catch_unwind(AssertUnwindSafe(|| store.get_coverage(key.clone(), &RelaySessionKey::unauthenticated(peek_relay().clone()))))
        .unwrap_or_else(|_| panic!("the coverage peek aborted the host process on a read error"));
    outcome.expect_err("a failing backend read must surface as Err");
    assert!(
        control.failed_reads() > 0,
        "the fault must have been reached at the backend"
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
    for _ in 0..8 {
        store
            .next_expiration()
            .expect_err("a failing read never answers a deadline");
        store
            .get_coverage(
                key.clone(),
                &RelaySessionKey::unauthenticated(peek_relay().clone()),
            )
            .expect_err("a failing read never answers coverage");
    }
    // The first failure is the originating read; every one after it is redb's
    // own latch refusing ahead of the backend. Both are values, and the
    // process that asked sixteen times is still here to say so.
}
