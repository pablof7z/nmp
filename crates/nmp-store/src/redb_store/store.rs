use nmp_grammar::RelaySessionKey;
use super::canonical::{decode_observed_at, fold_seen_at};
use super::commit::commit_prepared;
use super::postings::Family;
use super::postings_store::{scan_packed, PackedScan};
use super::publish_queue::{is_suppressed_in_txn, replace_lane_in_txn};
use super::publish_queue_codec::{
    codec_error, decode_intent, decode_meta_u64, decode_relay, encode_meta_u64, intent_key,
    relay_key, PublishQueueRelayId, PUBLISH_QUEUE_CODEC_VERSION, PUBLISH_QUEUE_CODEC_VERSION_KEY,
};
use super::query::{OrderedIndex, OrderedPlan};
use super::schema::{
    decode_relay_row, event_local_key, event_row_key, observation_bounds, observation_key,
    observation_relay_key, persist_err, unsupported_schema, EventKey, RelayKey, ADDR_INDEX,
    COVERAGE, EVENTS, EVENT_IDS, EXPIRATION_INDEX, POSTINGS_CATALOG, POSTINGS_READY,
    POSTINGS_SEGMENTS, PUBLISH_QUEUE_ATTEMPTS, PUBLISH_QUEUE_ATTEMPT_DETAILS,
    PUBLISH_QUEUE_DEADLINES, PUBLISH_QUEUE_DISPLACED, PUBLISH_QUEUE_INTENTS,
    PUBLISH_QUEUE_KIND5_CLAIMS, PUBLISH_QUEUE_LANES, PUBLISH_QUEUE_META, PUBLISH_QUEUE_RECEIPTS,
    PUBLISH_QUEUE_RELAYS, PUBLISH_QUEUE_RELAY_IDS, PUBLISH_QUEUE_ROUTE_REVISIONS,
    PUBLISH_QUEUE_SUPPRESS, REDB_CACHE_BYTES, RELAYS, RELAY_IDS, SCHEMA_VERSION,
    SCHEMA_VERSION_KEY, SEMANTIC_MATERIALIZATION_HIGH_WATER, SEMANTIC_OPERATIONS,
    SEMANTIC_RESOURCES, STORE_META, TOMBSTONES,
};
#[cfg(any(
    test,
    feature = "bench-instrumentation",
    feature = "test-instrumentation"
))]
use super::AtomicU64;
#[cfg(test)]
use super::AtomicU8;
#[cfg(any(
    test,
    feature = "bench-instrumentation",
    feature = "test-instrumentation"
))]
use super::Ordering;
use super::{
    acquire_for_open, binary_event, reset_store, BTreeMap, BTreeSet, CoverageKey, Database,
    EventCursor, EventId, Filter, HashMap, Path, PersistenceError, PreparedFilter, Provenance,
    PublishQueueLane, PublishQueueLaneKey, PublishQueueLaneState, RedbStoreOpenError, RelayUrl,
    RequiredLockedFileBackend, StoreOwnership, StoredEvent, StoredEventView, Timestamp,
};
use nostr::secp256k1::schnorr::Signature;
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableHandle};
use std::sync::{Arc, Mutex};

/// NMP's complete durable-store implementation. One Redb database, MVCC, ACID.
/// Persistent stores open a caller-selected path; isolated engines and tests
/// use [`Self::temporary`] and keep the same transactions and indexes.
#[cfg(test)]
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RedbCrashPoint {
    AcceptAfterEventBeforeJournal = 1,
    AcceptBeforeCommit,
    PromoteBeforeCommit,
    CompensateBeforeCommit,
    RouteRevisionBeforeCommit,
    FinishAttemptBeforeCommit,
    LaneBootstrapBeforeCommit,
    LaneTransitionBeforeCommit,
    LaneStartBeforeCommit,
    LaneHandoffBeforeCommit,
    DenyLaneAuthBeforeCommit,
    LaneCloseBeforeCommit,
    TerminalRetentionBeforeCommit,
    ObservationBeforeCommit,
    ObservationAfterCommit,
    SemanticAcceptBeforeCommit,
    SemanticRematerializeBeforeCommit,
    SemanticSourceInstallBeforeCommit,
    SemanticCohortCloseBeforeCommit,
    SemanticPromoteBeforeCommit,
    CoverageBeforeCommit,
    CoverageAfterCommit,
    GcBeforeCommit,
    GcAfterCommit,
}

/// A shared durable reader cut from a [`RedbStore`] for the verify gate's
/// durable dedup-by-id (#1677). Holds a clone of the store's shared
/// `Arc<Database>` cell, so it never borrows the store and survives the
/// engine's exclusive ownership. redb is MVCC, so this reader does not
/// block the writer. On a fault-recovery reopen the store takes the cell
/// (under the same lock) before dropping the old `Database`, so an in-flight
/// verify read cannot keep the old handle alive past the reopen.
///
/// `known_signature` is the one door: the stored known-good signature for
/// an id, or `None` (unknown id, missing row, a still-pending local draft
/// carrying the sentinel signature, or a transiently closed cell during
/// reopen).
pub struct StoreSigReader {
    pub(super) shared: Arc<Mutex<Option<Arc<Database>>>>,
}

impl StoreSigReader {
    /// The known-good signature for an already-ingested event id, if any.
    /// A pending local draft (sentinel signature) returns `None` so the
    /// real signed delivery is still admitted. A closed cell (during
    /// reopen) or a read error returns `None` — the candidate falls through
    /// to schnorr. This door is non-fatal by design.
    pub fn known_signature(&self, id: &EventId) -> Option<Signature> {
        let shared = self.shared.lock().ok()?;
        let db = shared.as_ref()?;
        super::event_ops::known_signature_from_db(db, id)
            .ok()
            .flatten()
    }
}

pub struct RedbStore {
    /// The live redb handle. Open for exactly this store's lifetime: it is
    /// installed once at construction and never taken, so no door has to
    /// answer for a handle that went missing.
    pub(super) db: Arc<Database>,
    /// The verify gate's durable-dedup read seam (#1677). A shared cell of the
    /// live `Arc<Database>`, cloned into every `StoreSigReader`. The store
    /// installs the handle here on open; the verifier reads through it under
    /// the lock. On the hot path this Mutex is uncontended (the store touches
    /// it only at open).
    pub(super) shared_db: Arc<Mutex<Option<Arc<Database>>>>,
    // Field order is load-bearing: Rust drops `db` before this ownership
    // token, so no process can open or reset the target until this database
    // handle has finished closing.
    pub(super) _ownership: StoreOwnership,
    // Temporary Redb stores own their directory after the database and
    // ownership fields, so Rust closes the handle and releases both locks
    // before recursive temporary-directory cleanup runs. Persistent stores
    // leave this as `None`.
    temporary_directory: Option<tempfile::TempDir>,
    /// Lazy process-local projection of the publish queue relay dictionary.
    /// Dictionary allocation remains transaction-authoritative; this cache
    /// only prevents reparsing a canonical URL for every row that references
    /// the same four-byte surrogate.
    publish_queue_relays: Mutex<PublishQueueRelayCache>,
    /// Fixed construction-time failures for lane-start rollback tests. No
    /// production build carries or can mutate this set.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) failed_lane_start_relays: BTreeSet<RelayUrl>,
    /// One construction-armed lane-bootstrap refusal consumed at the existing
    /// pre-commit boundary. No production build carries this setting.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fail_next_lane_bootstrap: bool,
    /// Fixed construction-time refusal for route-revision rollback tests. No
    /// production build carries or can mutate this setting.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fail_route_revision_writes: bool,
    /// One construction-armed compensation refusal consumed at the existing
    /// pre-commit boundary. No production build carries this setting.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fail_next_compensation_with_state: bool,
    /// One construction-armed attempt-finish refusal consumed at the existing
    /// pre-commit boundary. No production build carries this setting.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fail_next_lane_attempt_finish: bool,
    /// One construction-armed handoff refusal consumed at the existing
    /// pre-commit boundary. No production build carries this setting.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fail_next_lane_handoff: bool,
    /// One construction-armed event acceptance refusal consumed immediately
    /// before commit. The real prepared Redb transaction is dropped and the
    /// real database handle is closed before the typed I/O error is returned.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fail_next_accept_write_before_commit: bool,
    /// One construction-armed event acceptance refusal consumed immediately
    /// after the real Redb commit. The real database handle is closed before
    /// the typed I/O error is returned, so recovery must read durable identity
    /// from a newly opened Redb generation.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fail_next_accept_write_after_commit: bool,
    /// One construction-armed relay-observation I/O failure consumed at the
    /// staged pre-commit boundary. The prepared Redb transaction and actual
    /// database handle are closed before the typed error is returned.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fail_next_observation_before_commit: bool,
    /// One construction-armed refusal consumed by the next bounded read
    /// behind an exact history cursor. No production build carries this
    /// setting, and no caller can re-arm it after construction.
    #[cfg(any(test, feature = "test-instrumentation"))]
    fail_next_query_newest_before: std::sync::atomic::AtomicBool,
    /// One construction-armed coverage-write refusal consumed only when the
    /// staged batch contains this exact durable row. No production build
    /// carries this setting, and no caller can re-arm it after construction.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fail_next_coverage_write: Option<(CoverageKey, RelayUrl)>,
    /// One construction-armed pause consumed before the shared ordered event
    /// read. The pause controls scheduling only; Redb still supplies every
    /// row and every error.
    #[cfg(any(test, feature = "test-instrumentation"))]
    ordered_event_read_pause: Mutex<Option<OrderedEventReadPauseGate>>,
    /// Armed only by the exact materializer-entry transaction falsifier.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) materializer_entry_probe: Option<Arc<AtomicU64>>,
    /// Application-level write transactions performed by `open`; the
    /// healthy v6 reopen falsifier asserts this stays zero.
    #[cfg(test)]
    pub(super) open_write_transactions: u64,
    #[cfg(test)]
    pub(super) crash_point: AtomicU8,
    /// Owned rows materialized after borrowed filtering.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) examined_rows: AtomicU64,
    /// Ordered index entries consumed, including one prefetched head per OR
    /// range needed to establish global ordering.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) query_index_rows: AtomicU64,
    /// Canonical binary event values dereferenced for borrowed post-filtering.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) query_event_values: AtomicU64,
    /// Coverage-table point reads, kept separate from event projection work
    /// so lifecycle benchmarks can attribute diagnostics cost exactly.
    #[cfg(any(
        test,
        feature = "bench-instrumentation",
        feature = "test-instrumentation"
    ))]
    pub(super) coverage_reads: AtomicU64,
    /// Calls through the concrete publish-queue lane-recovery door. This is
    /// test-only work attribution for reducer scheduling falsifiers, not a
    /// runtime diagnostic.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) publish_queue_lane_recovery_reads: AtomicU64,
    /// Benchmark-only ceiling: governed commits skip persistence barriers.
    /// Ordinary builds cannot construct a store with this set.
    #[cfg(feature = "bench-instrumentation")]
    pub(super) benchmark_durability: BenchmarkDurability,
    /// Number of rows yielded by bounded attempt-table ranges. Tests reset
    /// this to prove work follows the target lane count, not total history.
    #[cfg(test)]
    pub(super) attempt_range_rows: AtomicU64,
    /// Equivalent instrumentation for resolved-route revision ranges.
    #[cfg(test)]
    pub(super) route_revision_range_rows: AtomicU64,
    /// Lane bootstraps that staged no row and therefore committed nothing
    /// (#889). Boot calls bootstrap once per open intent, so this is the
    /// counter that proves recovery over an unchanged lane set spends no
    /// durability barriers at all.
    #[cfg(test)]
    pub(super) unstaged_lane_bootstraps: AtomicU64,
}

#[derive(Default)]
struct PublishQueueRelayCache {
    by_id: HashMap<PublishQueueRelayId, RelayUrl>,
    by_url: HashMap<RelayUrl, PublishQueueRelayId>,
}

#[cfg(any(test, feature = "test-instrumentation"))]
struct OrderedEventReadPauseGate {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

/// Test-only witness for one construction-armed ordered event-read pause.
///
/// This can wait for and release the real Redb read. It cannot choose or
/// manufacture the read's result.
#[cfg(any(test, feature = "test-instrumentation"))]
pub struct OrderedEventReadPause {
    entered: std::sync::mpsc::Receiver<()>,
    release: std::sync::mpsc::SyncSender<()>,
}

#[cfg(any(test, feature = "test-instrumentation"))]
impl OrderedEventReadPause {
    pub fn wait_until_entered(&self) {
        self.entered
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("runtime must reach the ordered event read");
    }

    pub fn release(self) {
        self.release
            .send(())
            .expect("ordered event read remains paused");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BenchmarkDurability {
    Immediate,
    #[cfg(feature = "bench-instrumentation")]
    NoneThenImmediateCheckpoint,
}

#[cfg(test)]
type RequiredDatabaseInitTestHook = Box<dyn FnMut()>;

#[cfg(test)]
std::thread_local! {
    static REQUIRED_DATABASE_INIT_TEST_HOOK:
        std::cell::RefCell<Option<RequiredDatabaseInitTestHook>> =
            const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
struct ClearRequiredDatabaseInitTestHook;

#[cfg(test)]
impl Drop for ClearRequiredDatabaseInitTestHook {
    fn drop(&mut self) {
        REQUIRED_DATABASE_INIT_TEST_HOOK.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

#[cfg(test)]
pub(crate) fn with_required_database_init_test_hook<T>(
    hook: impl FnMut() + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    REQUIRED_DATABASE_INIT_TEST_HOOK.with(|slot| {
        assert!(
            slot.borrow_mut().replace(Box::new(hook)).is_none(),
            "one open cannot install two database-init hooks"
        );
    });
    let _clear = ClearRequiredDatabaseInitTestHook;
    operation()
}

#[cfg(test)]
fn call_required_database_init_test_hook() {
    REQUIRED_DATABASE_INIT_TEST_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook();
        }
    });
}

#[cfg(feature = "bench-instrumentation")]
impl Drop for RedbStore {
    fn drop(&mut self) {
        if self.benchmark_durability != BenchmarkDurability::NoneThenImmediateCheckpoint {
            return;
        }

        // Redb otherwise performs an implicit Immediate allocator/trim
        // transaction during Database::drop. Make the required durability
        // drain explicit and timed so a no-fsync foreground ceiling cannot
        // hide persistence work after the measurement window.
        let started = std::time::Instant::now();
        let checkpoint = self
            .db
            .begin_write()
            .expect("redb benchmark durability checkpoint begin");
        checkpoint
            .commit()
            .expect("redb benchmark durability checkpoint commit");
        crate::ingest_attribution::durability_checkpoint(started.elapsed());
        self.benchmark_durability = BenchmarkDurability::Immediate;
    }
}

impl RedbStore {
    /// Open an isolated filesystem-backed Redb database and own its directory
    /// for exactly this store's lifetime.
    ///
    /// This is the ephemeral construction path for tests and for an
    /// [`nmp::EngineConfig`](https://docs.rs/nmp) with no store path. It is
    /// still the production Redb implementation: there is no in-memory
    /// store, compatibility alias, or alternate semantic owner.
    pub fn temporary() -> Result<Self, RedbStoreOpenError> {
        let directory = tempfile::tempdir()
            .map_err(|source| RedbStoreOpenError::TemporaryDirectoryFailed { source })?;
        let mut store = Self::open(directory.path().join("nmp.redb"))?;
        store.temporary_directory = Some(directory);
        Ok(store)
    }

    /// Open a real temporary Redb store whose named lane starts fail at the
    /// existing pre-commit boundary.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn temporary_with_failed_lane_starts(
        failed_relays: impl IntoIterator<Item = RelayUrl>,
    ) -> Result<Self, RedbStoreOpenError> {
        let mut store = Self::temporary()?;
        store.failed_lane_start_relays = failed_relays.into_iter().collect();
        Ok(store)
    }

    /// Open a real temporary Redb store whose next staged lane bootstrap
    /// refuses at the existing pre-commit boundary.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn temporary_with_failed_lane_bootstrap() -> Result<Self, RedbStoreOpenError> {
        let mut store = Self::temporary()?;
        store.fail_next_lane_bootstrap = true;
        Ok(store)
    }

    /// Open a real temporary Redb store whose next compensation refuses at
    /// the existing pre-commit boundary.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn temporary_with_failed_compensation_with_state() -> Result<Self, RedbStoreOpenError> {
        let mut store = Self::temporary()?;
        store.fail_next_compensation_with_state = true;
        Ok(store)
    }

    /// Open a real temporary Redb store whose next lane-attempt finish refuses
    /// at the existing pre-commit boundary.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn temporary_with_failed_lane_attempt_finish() -> Result<Self, RedbStoreOpenError> {
        let mut store = Self::temporary()?;
        store.fail_next_lane_attempt_finish = true;
        Ok(store)
    }

    /// Open a real temporary Redb store whose next lane handoff refuses at
    /// the existing pre-commit boundary.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn temporary_with_failed_lane_handoff() -> Result<Self, RedbStoreOpenError> {
        let mut store = Self::temporary()?;
        store.fail_next_lane_handoff = true;
        Ok(store)
    }

    /// Open a real temporary Redb store whose next nonempty observation
    /// transaction closes the database handle and returns I/O before commit.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn temporary_with_observation_precommit_io() -> Result<Self, RedbStoreOpenError> {
        let mut store = Self::temporary()?;
        store.fail_next_observation_before_commit = true;
        Ok(store)
    }

    /// Open a real temporary Redb store whose next bounded read behind an
    /// exact history cursor refuses once.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn temporary_with_failed_query_newest_before() -> Result<Self, RedbStoreOpenError> {
        let store = Self::temporary()?;
        store
            .fail_next_query_newest_before
            .store(true, Ordering::Relaxed);
        Ok(store)
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(super) fn take_query_newest_before_failure(&self) -> bool {
        self.fail_next_query_newest_before
            .swap(false, Ordering::Relaxed)
    }

    /// Open a persistent Redb store whose named lane starts fail at the
    /// existing pre-commit boundary.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn open_with_failed_lane_starts(
        path: impl AsRef<Path>,
        failed_relays: impl IntoIterator<Item = RelayUrl>,
    ) -> Result<Self, RedbStoreOpenError> {
        let mut store = Self::open(path)?;
        store.failed_lane_start_relays = failed_relays.into_iter().collect();
        Ok(store)
    }

    /// Open a persistent Redb store whose route-revision writes refuse at the
    /// existing pre-commit boundary.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn open_with_route_revision_write_failure(
        path: impl AsRef<Path>,
    ) -> Result<Self, RedbStoreOpenError> {
        let mut store = Self::open(path)?;
        store.fail_route_revision_writes = true;
        Ok(store)
    }

    /// Open a persistent Redb store whose next coverage write containing the
    /// exact durable row refuses at the existing pre-commit boundary.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn open_with_failed_coverage_write(
        path: impl AsRef<Path>,
        key: CoverageKey,
        relay: RelayUrl,
    ) -> Result<Self, RedbStoreOpenError> {
        let mut store = Self::open(path)?;
        store.fail_next_coverage_write = Some((key, relay));
        Ok(store)
    }

    /// Open a persistent Redb store whose next event acceptance closes the
    /// real database handle and returns I/O immediately before commit.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn open_with_accept_write_precommit_io(
        path: impl AsRef<Path>,
    ) -> Result<Self, RedbStoreOpenError> {
        let mut store = Self::open(path)?;
        store.fail_next_accept_write_before_commit = true;
        Ok(store)
    }

    /// Open a persistent Redb store whose next event acceptance commits,
    /// closes the real database handle, and then returns I/O.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn open_with_accept_write_commit_then_io(
        path: impl AsRef<Path>,
    ) -> Result<Self, RedbStoreOpenError> {
        let mut store = Self::open(path)?;
        store.fail_next_accept_write_after_commit = true;
        Ok(store)
    }

    /// Open a persistent Redb store whose first ordered event read pauses
    /// until the returned witness releases it.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn open_with_ordered_event_read_pause(
        path: impl AsRef<Path>,
    ) -> Result<(Self, OrderedEventReadPause), RedbStoreOpenError> {
        let (entered_tx, entered) = std::sync::mpsc::sync_channel(0);
        let (release, release_rx) = std::sync::mpsc::sync_channel(0);
        let mut store = Self::open(path)?;
        store.ordered_event_read_pause = Mutex::new(Some(OrderedEventReadPauseGate {
            entered: entered_tx,
            release: release_rx,
        }));
        Ok((store, OrderedEventReadPause { entered, release }))
    }

    pub(super) fn database(&self) -> &Database {
        &self.db
    }

    /// Share the durable database handle with an out-of-band reader —
    /// the verify gate's durable dedup-by-id (#1677). The returned
    /// [`StoreSigReader`] holds its own `Arc<Database>` and opens its own
    /// read transactions, so it never borrows the store and outlives the
    /// engine's exclusive ownership of the `RedbStore` it was cut from.
    /// redb is MVCC: a concurrent reader on a shared handle does not block
    /// the engine's writer and is never blocked by it.
    pub fn share_sig_reader(&self) -> Result<StoreSigReader, PersistenceError> {
        Ok(StoreSigReader {
            shared: Arc::clone(&self.shared_db),
        })
    }

    /// The unguarded handle, for tests that deliberately corrupt or inspect
    /// the file behind the store's back. `#[cfg(test)]`: no shipping build
    /// compiles it.
    #[cfg(test)]
    pub(super) fn raw_database(&self) -> &Database {
        &self.db
    }

    pub(super) fn publish_queue_relay_id(
        &self,
        relay: &RelayUrl,
    ) -> Result<PublishQueueRelayId, PersistenceError> {
        if let Some(id) = self
            .publish_queue_relays
            .lock()
            .map_err(|_| PersistenceError::new("delivery relay cache poisoned"))?
            .by_url
            .get(relay)
            .copied()
        {
            return Ok(id);
        }
        let read = self.database().begin_read().map_err(persist_err)?;
        let relay_ids = read
            .open_table(PUBLISH_QUEUE_RELAY_IDS)
            .map_err(persist_err)?;
        let relays = read.open_table(PUBLISH_QUEUE_RELAYS).map_err(persist_err)?;
        let raw = relay_ids
            .get(relay.as_str().as_bytes())
            .map_err(persist_err)?
            .map(|guard| *guard.value())
            .ok_or_else(|| PersistenceError::new("delivery relay is not interned"))?;
        let id = u32::from_be_bytes(raw);
        let key = relay_key(id);
        let encoded = relays
            .get(&key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| {
                PersistenceError::new(
                    "delivery relay reverse map points at missing dictionary row",
                )
            })?;
        if decode_relay(&encoded).map_err(|error| codec_error("relay", error))? != *relay {
            return Err(PersistenceError::new(
                "delivery relay dictionary directions disagree",
            ));
        }
        self.cache_publish_queue_relay(id, relay.clone())?;
        Ok(id)
    }

    pub(super) fn publish_queue_relay(
        &self,
        id: PublishQueueRelayId,
    ) -> Result<RelayUrl, PersistenceError> {
        if let Some(relay) = self
            .publish_queue_relays
            .lock()
            .map_err(|_| PersistenceError::new("delivery relay cache poisoned"))?
            .by_id
            .get(&id)
            .cloned()
        {
            return Ok(relay);
        }
        let read = self.database().begin_read().map_err(persist_err)?;
        let relays = read.open_table(PUBLISH_QUEUE_RELAYS).map_err(persist_err)?;
        let relay_ids = read
            .open_table(PUBLISH_QUEUE_RELAY_IDS)
            .map_err(persist_err)?;
        let key = relay_key(id);
        let encoded = relays
            .get(&key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| {
                PersistenceError::new(format!(
                    "delivery row references missing relay surrogate {id}"
                ))
            })?;
        let relay = decode_relay(&encoded).map_err(|error| codec_error("relay", error))?;
        let reverse_id = relay_ids
            .get(relay.as_str().as_bytes())
            .map_err(persist_err)?
            .map(|guard| u32::from_be_bytes(*guard.value()))
            .ok_or_else(|| {
                PersistenceError::new("delivery relay dictionary is missing reverse row")
            })?;
        if reverse_id != id {
            return Err(PersistenceError::new(
                "delivery relay dictionary directions disagree",
            ));
        }
        self.cache_publish_queue_relay(id, relay.clone())?;
        Ok(relay)
    }

    pub(super) fn cache_publish_queue_relay(
        &self,
        id: PublishQueueRelayId,
        relay: RelayUrl,
    ) -> Result<(), PersistenceError> {
        let mut cache = self
            .publish_queue_relays
            .lock()
            .map_err(|_| PersistenceError::new("delivery relay cache poisoned"))?;
        if cache
            .by_id
            .get(&id)
            .is_some_and(|existing| existing != &relay)
            || cache
                .by_url
                .get(&relay)
                .is_some_and(|existing| *existing != id)
        {
            return Err(PersistenceError::new(
                "delivery relay cache disagrees with durable dictionary",
            ));
        }
        cache.by_id.insert(id, relay.clone());
        cache.by_url.insert(relay, id);
        Ok(())
    }

    pub(super) fn persist_lane_state(
        &mut self,
        key: &PublishQueueLaneKey,
        expected_revision: u64,
        state: PublishQueueLaneState,
    ) -> Result<PublishQueueLane, PersistenceError> {
        let relay_id = self.publish_queue_relay_id(&key.relay)?;
        let write_txn = self.database().begin_write().map_err(persist_err)?;
        let lane = {
            let intents = write_txn
                .open_table(PUBLISH_QUEUE_INTENTS)
                .map_err(persist_err)?;
            let intent = intents
                .get(&intent_key(key.intent_id))
                .map_err(persist_err)?
                .map(|value| value.value().to_vec())
                .ok_or_else(|| PersistenceError::new("lane intent is not open"))?;
            let current_event_id = decode_intent(&intent)
                .map_err(|error| codec_error("lane intent", error))?
                .current_event_id()
                .ok_or_else(|| PersistenceError::new("lane intent has no current event"))?;
            if current_event_id != key.event_id {
                return Err(PersistenceError::new(
                    "delivery lane event is not the intent's current event",
                ));
            }
            let mut lanes = write_txn
                .open_table(PUBLISH_QUEUE_LANES)
                .map_err(persist_err)?;
            let mut deadlines = write_txn
                .open_table(PUBLISH_QUEUE_DEADLINES)
                .map_err(persist_err)?;
            replace_lane_in_txn(
                &mut lanes,
                &mut deadlines,
                key,
                relay_id,
                expected_revision,
                state,
            )?
        };
        #[cfg(test)]
        self.crash_if(RedbCrashPoint::LaneTransitionBeforeCommit);
        commit_prepared(write_txn, lane)
    }

    /// Open (creating if absent) a `redb` database file at `path`.
    ///
    /// A healthy current-schema database takes only a read transaction: the
    /// explicit schema marker proves every table exists, and one exact
    /// metadata count tells us whether crash-abandoned ephemeral receipts need
    /// recovery.
    ///
    /// The returned store owns one nonblocking cross-process exclusive
    /// pathname lock plus one required lock on the resolved target inode for
    /// as long as it lives. A second owner — through this path, a relative,
    /// symlink, or hard-link alias, in this process or another — is refused with
    /// [`RedbStoreOpenError::StoreAlreadyOpen`] before a second database
    /// handle exists.
    ///
    /// Any nonempty database that is not exactly the current schema epoch is
    /// refused with [`RedbStoreOpenError::UnsupportedSchema`] before a store
    /// is exposed and before any byte is mutated. There is no migration,
    /// adoption, alias, drain, or destructive-reset path. Continuing requires
    /// deliberate discard and recreation: relay-backed cache rows can be
    /// reacquired, but accepted unpublished writes and the rest of their
    /// publish queue evidence are permanently lost
    /// (`docs/internals/conventions/schema-epoch-discard.md`).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RedbStoreOpenError> {
        Self::open_inner(path, BenchmarkDurability::Immediate, |path| {
            let backend = RequiredLockedFileBackend::open(path)?;
            #[cfg(test)]
            call_required_database_init_test_hook();
            Database::builder()
                .set_cache_size(REDB_CACHE_BYTES)
                .create_with_backend(backend)
                .map_err(|error| RedbStoreOpenError::database(error.into(), path))
        })
    }

    /// Open over a caller-supplied `redb` storage backend (#895 falsifiers).
    ///
    /// Unit-test build only. The durability classification is only worth
    /// having if it is proven against redb's real failure machinery — its
    /// `CheckedBackend` latch, its commit ordering, its post-flush resize —
    /// rather than against hand-constructed `StorageError` values. The only
    /// way to make that machinery fail on demand is to give redb a backend
    /// that fails on demand, so this seam exists purely to let the
    /// durability tests drive a genuine `RedbStore` through a genuine
    /// disk-full sequence.
    ///
    /// The supplied backend belongs only to the first database generation.
    /// Production reconstruction reopens the exact canonical file through
    /// `RequiredLockedFileBackend`; a test-only backend cannot be replayed or
    /// silently substituted after it reports a failure.
    #[cfg(test)]
    pub(super) fn open_with_backend(
        path: impl AsRef<Path>,
        backend: impl redb::StorageBackend,
    ) -> Result<Self, RedbStoreOpenError> {
        let mut backend = Some(backend);
        Self::open_inner(path, BenchmarkDurability::Immediate, move |_| {
            Database::builder()
                .set_cache_size(REDB_CACHE_BYTES)
                .create_with_backend(
                    backend
                        .take()
                        .expect("open_inner calls the database factory exactly once"),
                )
                .map_err(|error| RedbStoreOpenError::Database(error.into()))
        })
    }

    /// Open a benchmark-only diagnostic store whose governed write
    /// transactions use [`redb::Durability::None`].
    ///
    /// This is an upper-bound measurement seam, not a usable persistence
    /// mode: a process or machine crash may roll back every foreground
    /// commit. Drop performs one separately-timed Immediate checkpoint so
    /// maintenance-inclusive evidence cannot hide the deferred durability
    /// work. Schema creation and reopen repair remain immediately durable.
    #[cfg(feature = "bench-instrumentation")]
    pub fn open_benchmark_nondurable(path: impl AsRef<Path>) -> Result<Self, RedbStoreOpenError> {
        Self::open_inner(
            path,
            BenchmarkDurability::NoneThenImmediateCheckpoint,
            |path| {
                let backend = RequiredLockedFileBackend::open(path)?;
                Database::builder()
                    .set_cache_size(REDB_CACHE_BYTES)
                    .create_with_backend(backend)
                    .map_err(|error| RedbStoreOpenError::database(error.into(), path))
            },
        )
    }

    fn open_inner(
        path: impl AsRef<Path>,
        _benchmark_durability: BenchmarkDurability,
        create: impl FnOnce(&Path) -> Result<Database, RedbStoreOpenError>,
    ) -> Result<Self, RedbStoreOpenError> {
        let path = path.as_ref();
        // Ownership first: nothing may create, expose, or mutate the database
        // before this process holds the one exclusive lock for the target.
        // #867 depends on this order — a store refused for its schema epoch is
        // never inspected by a process that does not own it.
        let ownership = acquire_for_open(path)?;
        // Create through the RESOLVED target, never the possibly-retargeted
        // alias the caller supplied — the same canonical identity selected
        // the sidecar lock above.
        let db = create(ownership.target())?;
        // A retarget racing the create fails closed: `db` and `ownership`
        // both drop here, so nothing is exposed and the lock is released.
        ownership.revalidate(path)?;
        // #867: the explicit schema marker is the SOLE schema authority.
        // There is no table-name inventory of older epochs to consult, because
        // there is nothing this store could do with the answer: no pre-current
        // decoder exists, so any nonempty store that is not exactly the
        // current epoch is refused outright — before a `RedbStore` exists and
        // before a single table is created or a single byte mutated.
        let (table_count, has_schema_marker) = {
            let read_txn = db.begin_read()?;
            let mut table_count = 0usize;
            let mut has_schema_marker = false;
            for table in read_txn.list_tables()? {
                table_count += 1;
                has_schema_marker |= table.name() == STORE_META.name();
            }
            (table_count, has_schema_marker)
        };

        let mut _open_write_transactions = 0;
        if has_schema_marker {
            {
                let read_txn = db.begin_read()?;
                let store_meta = read_txn.open_table(STORE_META)?;
                let version = store_meta
                    .get(SCHEMA_VERSION_KEY)?
                    .map(|guard| guard.value());
                // Exactly one accepted epoch. Anything else — older, newer, or
                // absent — is the single refusal; the caller recreates the
                // store deliberately rather than having their bytes silently
                // upgraded, adopted, or reset underneath them.
                if version != Some(SCHEMA_VERSION) {
                    return Err(unsupported_schema(ownership.target(), version));
                }
                let publish_queue_meta = read_txn.open_table(PUBLISH_QUEUE_META)?;
                let codec_version = publish_queue_meta
                    .get(PUBLISH_QUEUE_CODEC_VERSION_KEY)?
                    .map(|guard| decode_meta_u64(guard.value(), "delivery codec version"))
                    .transpose()
                    .map_err(|error| {
                        redb::Error::Corrupted(format!(
                            "invalid publish-queue codec marker: {error}"
                        ))
                    })?;
                if codec_version != Some(PUBLISH_QUEUE_CODEC_VERSION) {
                    return Err(redb::Error::Corrupted(format!(
                        "current schema has publish-queue codec {codec_version:?}, expected {PUBLISH_QUEUE_CODEC_VERSION}"
                    ))
                    .into());
                }
            };
        } else {
            // A nonempty database without the exact current marker is never
            // treated as fresh and is never mutated: initializing over it
            // would combine unversioned publish-queue/coverage/tombstone
            // facts with an empty canonical epoch.
            if table_count != 0 {
                return Err(unsupported_schema(ownership.target(), None));
            }
            let write_txn = db.begin_write()?;
            {
                write_txn.open_table(EVENTS)?;
                write_txn.open_table(EVENT_IDS)?;
                write_txn.open_table(RELAYS)?;
                write_txn.open_table(RELAY_IDS)?;
                write_txn.open_table(ADDR_INDEX)?;
                write_txn.open_table(COVERAGE)?;
                write_txn.open_table(TOMBSTONES)?;
                write_txn.open_table(EXPIRATION_INDEX)?;
                write_txn.open_table(POSTINGS_SEGMENTS)?;
                write_txn.open_table(POSTINGS_CATALOG)?;
                write_txn.open_table(PUBLISH_QUEUE_INTENTS)?;
                write_txn.open_table(PUBLISH_QUEUE_DISPLACED)?;
                write_txn.open_table(PUBLISH_QUEUE_ATTEMPTS)?;
                write_txn.open_table(PUBLISH_QUEUE_ROUTE_REVISIONS)?;
                write_txn.open_table(PUBLISH_QUEUE_LANES)?;
                write_txn.open_table(PUBLISH_QUEUE_DEADLINES)?;
                write_txn.open_table(PUBLISH_QUEUE_ATTEMPT_DETAILS)?;
                let mut publish_queue_meta = write_txn.open_table(PUBLISH_QUEUE_META)?;
                publish_queue_meta.insert(
                    PUBLISH_QUEUE_CODEC_VERSION_KEY,
                    encode_meta_u64(PUBLISH_QUEUE_CODEC_VERSION).as_slice(),
                )?;
                write_txn.open_table(PUBLISH_QUEUE_KIND5_CLAIMS)?;
                write_txn.open_table(PUBLISH_QUEUE_SUPPRESS)?;
                write_txn.open_table(PUBLISH_QUEUE_RECEIPTS)?;
                write_txn.open_table(PUBLISH_QUEUE_RELAYS)?;
                write_txn.open_table(PUBLISH_QUEUE_RELAY_IDS)?;
                write_txn.open_table(SEMANTIC_RESOURCES)?;
                write_txn.open_table(SEMANTIC_OPERATIONS)?;
                write_txn.open_table(SEMANTIC_MATERIALIZATION_HIGH_WATER)?;
                let mut store_meta = write_txn.open_table(STORE_META)?;
                store_meta.insert(POSTINGS_READY, 1)?;
                store_meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)?;
            }
            write_txn.commit()?;
            _open_write_transactions += 1;
        }
        let db = Arc::new(db);
        let mut store = Self {
            db: Arc::clone(&db),
            shared_db: Arc::new(Mutex::new(Some(db))),
            _ownership: ownership,
            temporary_directory: None,
            publish_queue_relays: Mutex::new(PublishQueueRelayCache::default()),
            #[cfg(any(test, feature = "test-instrumentation"))]
            failed_lane_start_relays: BTreeSet::new(),
            #[cfg(any(test, feature = "test-instrumentation"))]
            fail_next_lane_bootstrap: false,
            #[cfg(any(test, feature = "test-instrumentation"))]
            fail_route_revision_writes: false,
            #[cfg(any(test, feature = "test-instrumentation"))]
            fail_next_compensation_with_state: false,
            #[cfg(any(test, feature = "test-instrumentation"))]
            fail_next_lane_attempt_finish: false,
            #[cfg(any(test, feature = "test-instrumentation"))]
            fail_next_lane_handoff: false,
            #[cfg(any(test, feature = "test-instrumentation"))]
            fail_next_accept_write_before_commit: false,
            #[cfg(any(test, feature = "test-instrumentation"))]
            fail_next_accept_write_after_commit: false,
            #[cfg(any(test, feature = "test-instrumentation"))]
            fail_next_observation_before_commit: false,
            #[cfg(any(test, feature = "test-instrumentation"))]
            fail_next_query_newest_before: std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(test, feature = "test-instrumentation"))]
            fail_next_coverage_write: None,
            #[cfg(any(test, feature = "test-instrumentation"))]
            ordered_event_read_pause: Mutex::new(None),
            #[cfg(any(test, feature = "test-instrumentation"))]
            materializer_entry_probe: None,
            #[cfg(test)]
            open_write_transactions: _open_write_transactions,
            #[cfg(test)]
            crash_point: AtomicU8::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            examined_rows: AtomicU64::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            query_index_rows: AtomicU64::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            query_event_values: AtomicU64::new(0),
            #[cfg(any(
                test,
                feature = "bench-instrumentation",
                feature = "test-instrumentation"
            ))]
            coverage_reads: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-instrumentation"))]
            publish_queue_lane_recovery_reads: AtomicU64::new(0),
            #[cfg(feature = "bench-instrumentation")]
            benchmark_durability: _benchmark_durability,
            #[cfg(test)]
            attempt_range_rows: AtomicU64::new(0),
            #[cfg(test)]
            route_revision_range_rows: AtomicU64::new(0),
            #[cfg(test)]
            unstaged_lane_bootstraps: AtomicU64::new(0),
        };
        super::publish_queue_ops::maintain_terminal_receipts_at(
            &mut store,
            crate::terminal_retention::wall_clock_now(),
            crate::terminal_retention::TerminalRetentionLimits::PRODUCTION,
        )
        .map_err(|error| {
            RedbStoreOpenError::Database(redb::Error::Corrupted(format!(
                "terminal receipt maintenance failed during open: {error}"
            )))
        })?;
        Ok(store)
    }

    /// Destructively remove one unowned persistent store target.
    ///
    /// Reset acquires the SAME nonblocking cross-process exclusive ownership
    /// [`RedbStore::open`] does, and holds it through the removal — there is
    /// no check-then-delete window. A live owner in this or any other process
    /// is [`crate::RedbStoreResetError::StoreStillOpen`] and the target is not
    /// touched. Existing and dangling final symlink aliases resolve to the
    /// actual store target; the alias inode is not removed.
    pub fn reset(path: impl AsRef<Path>) -> Result<(), crate::RedbStoreResetError> {
        reset_store(path.as_ref())
    }

    #[cfg(test)]
    pub(super) fn open_with_crash_point(
        path: impl AsRef<Path>,
        crash_point: RedbCrashPoint,
    ) -> Result<Self, RedbStoreOpenError> {
        let store = Self::open(path)?;
        store
            .crash_point
            .store(crash_point as u8, Ordering::Relaxed);
        Ok(store)
    }

    #[cfg(test)]
    pub(super) fn crash_if(&self, point: RedbCrashPoint) {
        if self
            .crash_point
            .compare_exchange(point as u8, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            std::process::abort();
        }
    }

    #[cfg(test)]
    pub(super) fn reset_publish_queue_range_rows(&self) {
        self.attempt_range_rows.store(0, Ordering::Relaxed);
        self.route_revision_range_rows.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(super) fn publish_queue_range_rows(&self) -> (u64, u64) {
        (
            self.attempt_range_rows.load(Ordering::Relaxed),
            self.route_revision_range_rows.load(Ordering::Relaxed),
        )
    }

    #[cfg(test)]
    pub(super) fn open_write_transactions(&self) -> u64 {
        self.open_write_transactions
    }

    /// Lane bootstraps that staged nothing and therefore committed nothing.
    #[cfg(test)]
    pub(super) fn unstaged_lane_bootstraps(&self) -> u64 {
        self.unstaged_lane_bootstraps.load(Ordering::Relaxed)
    }

    /// Current value of [`Self::examined_rows`] — the `query`-indexing
    /// falsifier's read side.
    #[cfg(test)]
    pub(super) fn examined_rows(&self) -> u64 {
        self.examined_rows.load(Ordering::Relaxed)
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub fn reset_query_work(&self) {
        self.examined_rows.store(0, Ordering::Relaxed);
        self.query_index_rows.store(0, Ordering::Relaxed);
        self.query_event_values.store(0, Ordering::Relaxed);
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub fn query_work(&self) -> (u64, u64, u64) {
        (
            self.query_index_rows.load(Ordering::Relaxed),
            self.query_event_values.load(Ordering::Relaxed),
            self.examined_rows.load(Ordering::Relaxed),
        )
    }

    #[cfg(any(
        test,
        feature = "bench-instrumentation",
        feature = "test-instrumentation"
    ))]
    pub fn reset_coverage_reads(&self) {
        self.coverage_reads.store(0, Ordering::Relaxed);
    }

    #[cfg(any(
        test,
        feature = "bench-instrumentation",
        feature = "test-instrumentation"
    ))]
    pub fn coverage_reads(&self) -> u64 {
        self.coverage_reads.load(Ordering::Relaxed)
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn reset_publish_queue_lane_recovery_reads(&self) {
        self.publish_queue_lane_recovery_reads
            .store(0, Ordering::Relaxed);
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn publish_queue_lane_recovery_reads(&self) -> u64 {
        self.publish_queue_lane_recovery_reads
            .load(Ordering::Relaxed)
    }

    /// The current coverage-key schema prefix, mirroring `CoverageKey`'s own
    /// hash-level version tag (`nmp-store::coverage::COVERAGE_KEY_VERSION`).
    /// It is part of the durable key, not a compatibility discriminator: no
    /// reader tests for its absence, because a row lacking it cannot exist in
    /// the one current epoch.
    pub(super) const COVERAGE_ROW_KEY_PREFIX: &'static str = "d3:";

    /// The durable coverage row key: shape digest, then the SOURCE — which
    /// is a relay AND the identity the session reading it was authenticated
    /// as. Both halves of the source belong here for the same reason: what a
    /// row proves was fetched depends on where it was fetched from and on
    /// who asked. A row proven on a connection bound to nobody must never
    /// satisfy a read authenticated as an account, and two accounts must
    /// never share a row — merging either way would claim completeness that
    /// was never earned, silently.
    ///
    /// The identity is DISCOVERED (`RelaySessionKey::authenticate_as`),
    /// never the demand's override: the override says what was asked for,
    /// this says what was actually proven.
    pub(crate) fn coverage_row_key(key: &CoverageKey, session: &RelaySessionKey) -> String {
        use std::fmt::Write as _;

        // The atom's canonical encoding, hex-encoded. NOT a digest: the key
        // is derived from the thing itself, so it can be decoded, compared
        // and reasoned about rather than only equality-checked.
        let encoded = nmp_grammar::canonical_encoding(&key.atom().filter);
        let mut hex = String::with_capacity(encoded.len() * 2);
        for byte in &encoded {
            let _ = write!(hex, "{byte:02x}");
        }
        // Routing is part of coverage identity (#106): the same selection
        // fetched under different routing is a different acquisition.
        let _ = write!(hex, ":{:?}", key.atom().routing);
        // The atom's OWN declared identity is part of coverage identity too
        // (#106): two demands naming different identities are different
        // acquisitions even before a session discovers one.
        if let Some(declared) = key.atom().authenticate_as {
            let _ = write!(hex, ":{}", declared.to_hex());
        }
        let identity = session
            .authenticate_as
            .map(|key| key.to_hex())
            .unwrap_or_default();
        format!(
            "{}{hex}:{}:{identity}",
            Self::COVERAGE_ROW_KEY_PREFIX,
            session.relay.as_str()
        )
    }

    /// Materialize one portable `EVENTS` value into a [`StoredEvent`] —
    /// `query`'s one decode point, so [`Self::examined_rows`] (test-only)
    /// counts every row `query` actually pays the owned-event cost for,
    /// regardless of which of `query`'s three paths (id/indexed/full-scan)
    /// reached it.
    pub(super) fn read_provenance(
        &self,
        event_key: EventKey,
        local_bytes: Option<&[u8]>,
        events: &redb::ReadOnlyTable<&'static [u8], &'static [u8]>,
        relays: &redb::ReadOnlyTable<RelayKey, &'static [u8]>,
        relay_cache: &mut HashMap<RelayKey, RelayUrl>,
    ) -> Result<Provenance, PersistenceError> {
        let local = local_bytes
            .map(|bytes| {
                binary_event::decode_local(bytes).map_err(|error| {
                    PersistenceError::new(format!(
                        "decode canonical local state {event_key}: {error:?}"
                    ))
                })
            })
            .transpose()?;
        let (lower, upper) = observation_bounds(event_key);
        let mut seen = BTreeMap::new();
        for entry in events
            .range(lower.as_slice()..=upper.as_slice())
            .map_err(persist_err)?
        {
            let (encoded_key, at) = entry.map_err(persist_err)?;
            let relay_key = observation_relay_key(encoded_key.value());
            let relay = if let Some(relay) = relay_cache.get(&relay_key) {
                relay.clone()
            } else {
                let row = relays.get(relay_key).map_err(persist_err)?.ok_or_else(|| {
                    PersistenceError::new(format!(
                        "observation points at missing relay {relay_key}"
                    ))
                })?;
                let (_refs, url) = decode_relay_row(relay_key, row.value())?;
                let relay = RelayUrl::parse(url).map_err(|error| {
                    PersistenceError::new(format!(
                        "decode interned relay URL {relay_key}: {error}"
                    ))
                })?;
                relay_cache.insert(relay_key, relay.clone());
                relay
            };
            let at = decode_observed_at(event_key, relay_key, at.value())?;
            fold_seen_at(&mut seen, relay, Timestamp::from(at));
        }
        Ok(Provenance { seen, local })
    }

    pub(super) fn decode_row(
        &self,
        event_key: EventKey,
        view: StoredEventView<'_>,
        local_bytes: Option<&[u8]>,
        events: &redb::ReadOnlyTable<&'static [u8], &'static [u8]>,
        relays: &redb::ReadOnlyTable<RelayKey, &'static [u8]>,
        relay_cache: &mut HashMap<RelayKey, RelayUrl>,
    ) -> Result<StoredEvent, PersistenceError> {
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.examined_rows.fetch_add(1, Ordering::Relaxed);
        Ok(StoredEvent {
            event: view.materialize_event().map_err(|error| {
                PersistenceError::new(format!(
                    "materialize canonical event {event_key}: {error:?}"
                ))
            })?,
            provenance: self.read_provenance(
                event_key,
                local_bytes,
                events,
                relays,
                relay_cache,
            )?,
        })
    }

    /// Merge the planner's chosen packed prefix lists. Once `limit` visible
    /// rows survive the borrowed binary post-filter, no older posting or event
    /// value is touched.
    pub(super) fn query_ordered_ids(
        &self,
        read_txn: &redb::ReadTransaction,
        plan: &OrderedPlan,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<EventId>, PersistenceError> {
        let events = read_txn.open_table(EVENTS).map_err(persist_err)?;
        let event_ids = read_txn.open_table(EVENT_IDS).map_err(persist_err)?;
        let publish_queue_suppress = read_txn
            .open_table(PUBLISH_QUEUE_SUPPRESS)
            .map_err(persist_err)?;
        let suppression_possible = !publish_queue_suppress.is_empty().map_err(persist_err)?;
        let since = filter.since.map(|ts| ts.as_secs()).unwrap_or(0);
        let until = filter.until.map(|ts| ts.as_secs()).unwrap_or(u64::MAX);
        let prepared_filter = PreparedFilter::new(filter);
        let needs_event_value = prepared_filter.needs_event_value_after_index(plan.index.matched())
            || suppression_possible;
        let mut project_if_visible =
            |event_key: EventKey, event_id: EventId| -> Result<Option<EventId>, PersistenceError> {
                let canonical_key = event_ids
                    .get(event_id.as_bytes())
                    .map_err(persist_err)?
                    .map(|guard| guard.value());
                if canonical_key != Some(event_key) {
                    return Err(PersistenceError::new(format!(
                        "ordered index disagrees with canonical id map for {event_id}"
                    )));
                }
                if !needs_event_value {
                    return Ok(Some(event_id));
                }
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.query_event_values.fetch_add(1, Ordering::Relaxed);
                let Some(value) = events
                    .get(event_row_key(event_key).as_slice())
                    .map_err(persist_err)?
                else {
                    return Err(PersistenceError::new(format!(
                        "ordered index points at missing canonical event {event_key}"
                    )));
                };
                let view = StoredEventView::from_trusted(value.value()).map_err(|error| {
                    PersistenceError::new(format!(
                        "decode canonical event view {event_key}: {error:?}"
                    ))
                })?;
                let matches = view
                    .matches_prepared_filter_after_index(&prepared_filter, plan.index.matched())
                    .map_err(|error| {
                        PersistenceError::new(format!(
                            "match canonical event against filter {event_key}: {error:?}"
                        ))
                    })?;
                if !matches {
                    return Ok(None);
                }
                if suppression_possible {
                    #[cfg(any(test, feature = "bench-instrumentation"))]
                    self.examined_rows.fetch_add(1, Ordering::Relaxed);
                    let event = view.materialize_event().map_err(|error| {
                        PersistenceError::new(format!(
                            "materialize canonical event {event_key}: {error:?}"
                        ))
                    })?;
                    if is_suppressed_in_txn(&publish_queue_suppress, &event)? {
                        return Ok(None);
                    }
                }
                Ok(Some(event_id))
            };

        scan_packed(
            read_txn,
            PackedScan {
                family: packed_family(plan.index),
                prefixes: &plan.prefixes,
                since,
                until,
                before: None,
                limit: Some(limit),
            },
            || {
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.query_index_rows.fetch_add(1, Ordering::Relaxed);
            },
            &mut project_if_visible,
        )
    }

    pub(super) fn query_ordered(
        &self,
        read_txn: &redb::ReadTransaction,
        plan: &OrderedPlan,
        filter: &Filter,
        before: Option<EventCursor>,
        limit: Option<usize>,
        pinned: Option<&BTreeSet<RelayUrl>>,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        #[cfg(any(test, feature = "test-instrumentation"))]
        if let Some(pause) = self
            .ordered_event_read_pause
            .lock()
            .expect("ordered event-read pause lock")
            .take()
        {
            pause
                .entered
                .send(())
                .expect("ordered event-read witness remains alive");
            pause
                .release
                .recv()
                .expect("ordered event-read witness releases the pause");
        }
        let events = read_txn.open_table(EVENTS).map_err(persist_err)?;
        let relays = read_txn.open_table(RELAYS).map_err(persist_err)?;
        let relay_ids = read_txn.open_table(RELAY_IDS).map_err(persist_err)?;
        let publish_queue_suppress = read_txn
            .open_table(PUBLISH_QUEUE_SUPPRESS)
            .map_err(persist_err)?;
        let since = filter.since.map(|ts| ts.as_secs()).unwrap_or(0);
        let until = filter.until.map(|ts| ts.as_secs()).unwrap_or(u64::MAX);
        let mut relay_cache = HashMap::new();
        let pinned_relay_keys = if let Some(pinned) = pinned {
            let mut keys = BTreeSet::new();
            for relay in pinned {
                if let Some(key) = relay_ids.get(relay.as_str()).map_err(persist_err)? {
                    keys.insert(key.value());
                }
            }
            Some(keys)
        } else {
            None
        };
        let prepared_filter = PreparedFilter::new(filter);
        let mut materialize_if_visible = |event_key: EventKey,
                                          _event_id: EventId|
         -> Result<Option<StoredEvent>, PersistenceError> {
            // `Provenance::visible_under_pin`, evaluated against the index
            // rather than a decoded row: the whole point of testing here is
            // to skip decoding rows that will be dropped. Both halves of
            // that one rule are asked in cost order — first "did a pinned
            // relay carry it", then, only when nothing pinned did, "is this
            // row OURS" (an [`EVENT_COL_LOCAL`] row exists iff
            // `Provenance.local` is `Some`). A row this node accepted itself
            // is never another host's row, whoever has carried it since.
            let mut local_value = None;
            if let Some(pinned) = &pinned_relay_keys {
                let mut carried_by_pinned = false;
                for relay_key in pinned {
                    let key = observation_key(event_key, *relay_key);
                    if events.get(key.as_slice()).map_err(persist_err)?.is_some() {
                        carried_by_pinned = true;
                        break;
                    }
                }
                if !carried_by_pinned {
                    local_value = events
                        .get(event_local_key(event_key).as_slice())
                        .map_err(persist_err)?;
                    if local_value.is_none() {
                        return Ok(None);
                    }
                }
            }
            #[cfg(any(test, feature = "bench-instrumentation"))]
            self.query_event_values.fetch_add(1, Ordering::Relaxed);
            let Some(value) = events
                .get(event_row_key(event_key).as_slice())
                .map_err(persist_err)?
            else {
                return Err(PersistenceError::new(format!(
                    "ordered index points at missing canonical event {event_key}"
                )));
            };
            let view = StoredEventView::from_trusted(value.value()).map_err(|error| {
                PersistenceError::new(format!(
                    "decode canonical event view {event_key}: {error:?}"
                ))
            })?;
            let matches = view
                .matches_prepared_filter_after_index(&prepared_filter, plan.index.matched())
                .map_err(|error| {
                    PersistenceError::new(format!(
                        "match canonical event against filter {event_key}: {error:?}"
                    ))
                })?;
            if !matches {
                return Ok(None);
            }
            let local_value = match local_value {
                Some(already_read) => Some(already_read),
                None => events
                    .get(event_local_key(event_key).as_slice())
                    .map_err(persist_err)?,
            };
            let stored = self.decode_row(
                event_key,
                view,
                local_value.as_ref().map(|value| value.value()),
                &events,
                &relays,
                &mut relay_cache,
            )?;
            if is_suppressed_in_txn(&publish_queue_suppress, &stored.event)? {
                return Ok(None);
            }
            Ok(Some(stored))
        };

        scan_packed(
            read_txn,
            PackedScan {
                family: packed_family(plan.index),
                prefixes: &plan.prefixes,
                since,
                until,
                before,
                limit,
            },
            || {
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.query_index_rows.fetch_add(1, Ordering::Relaxed);
            },
            &mut materialize_if_visible,
        )
    }
}

fn packed_family(index: OrderedIndex) -> Family {
    match index {
        OrderedIndex::Global => Family::Global,
        OrderedIndex::Author => Family::Author,
        OrderedIndex::Kind => Family::Kind,
        OrderedIndex::Tag(_) => Family::Tag,
    }
}
