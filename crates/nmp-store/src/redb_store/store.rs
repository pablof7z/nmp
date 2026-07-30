use super::canonical::{fold_seen_at, observation_key, observation_range, observation_relay_key};
use super::commit::commit_prepared;
use super::outbox::{
    is_suppressed_in_txn, reconcile_ephemeral_receipts_in_txn, replace_lane_in_txn,
    OUTBOX_KIND5_CLAIMS, OUTBOX_SUPPRESS_BY_ADDR, OUTBOX_SUPPRESS_BY_ID,
};
use super::postings::Family;
use super::postings_store::{scan_packed, PackedScan};
use super::query::{OrderedIndex, OrderedPlan};
use super::schema::{
    persist_err, unsupported_schema, EventKey, RelayKey, ADDR_INDEX, ADDR_TOMBSTONES, COVERAGE,
    EVENTS, EVENT_IDS, EVENT_LOCAL, EVENT_OBSERVATIONS, EVENT_STORE_META, EXPIRATION_INDEX,
    INDEX_CARDINALITY, INDEX_CARDINALITY_META, INDEX_CARDINALITY_SAMPLE_KEY,
    INDEX_CARDINALITY_SAMPLE_META, INDEX_CARDINALITY_VERSION, INDEX_CARDINALITY_VERSION_KEY,
    OUTBOX_ATTEMPTS, OUTBOX_ATTEMPT_DETAILS, OUTBOX_CORRELATIONS, OUTBOX_DEADLINES,
    OUTBOX_DEADLINES_BY_INTENT, OUTBOX_DISPLACED, OUTBOX_INTENTS, OUTBOX_LANES, OUTBOX_META,
    OUTBOX_RECEIPTS, OUTBOX_ROUTE_REVISIONS, PENDING_EPHEMERAL_RECEIPTS_KEY, POSTINGS_DEAD_KEYS,
    POSTINGS_DICTIONARIES, POSTINGS_META, POSTINGS_READY, POSTINGS_RUN_BY_MIN, POSTINGS_RUN_META,
    POSTINGS_SEGMENTS, REDB_CACHE_BYTES, RELAYS, RELAY_KEYS, RELAY_META, RELAY_REFS, SCHEMA_META,
    SCHEMA_VERSION, SCHEMA_VERSION_KEY, TOMBSTONES,
};
#[cfg(any(test, feature = "bench-instrumentation"))]
use super::AtomicU64;
#[cfg(test)]
use super::AtomicU8;
#[cfg(any(test, feature = "bench-instrumentation"))]
use super::Ordering;
use super::{
    acquire_for_open, binary_event, reset_store, BTreeMap, BTreeSet, CoverageKey, Database,
    EventCursor, EventId, Filter, HashMap, LaneKey, LaneState, Path, PersistenceError,
    PreparedFilter, Provenance, RecoveredLane, RedbStoreOpenError, RelayUrl,
    RequiredLockedFileBackend, StoreOwnership, StoredEvent, StoredEventView, Timestamp,
};
use redb::{ReadableDatabase, ReadableTableMetadata, TableHandle};

/// A persistent, `redb`-backed `EventStore`. One database, MVCC, ACID; the
/// same insert door and coverage/GC contract as [`crate::MemoryStore`], the
/// oracle it is diffed against in `nmp-store/tests/store_contract.rs`.
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
    LaneCloseBeforeCommit,
    ObservationBeforeCommit,
    ObservationAfterCommit,
    CoverageBeforeCommit,
    CoverageAfterCommit,
    GcBeforeCommit,
    GcAfterCommit,
}

pub struct RedbStore {
    pub(super) db: Database,
    // Field order is load-bearing: Rust drops `db` before this ownership
    // token, so no process can open or reset the target until this database
    // handle has finished closing.
    pub(super) _ownership: StoreOwnership,
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
    pub(super) fn persist_lane_state(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        state: LaneState,
    ) -> Result<RecoveredLane, PersistenceError> {
        let write_txn = self.db.begin_write().map_err(persist_err)?;
        let lane = {
            let mut lanes = write_txn.open_table(OUTBOX_LANES).map_err(persist_err)?;
            let mut deadlines = write_txn
                .open_table(OUTBOX_DEADLINES)
                .map_err(persist_err)?;
            let mut deadlines_by_intent = write_txn
                .open_table(OUTBOX_DEADLINES_BY_INTENT)
                .map_err(persist_err)?;
            replace_lane_in_txn(
                &mut lanes,
                &mut deadlines,
                &mut deadlines_by_intent,
                key,
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
    /// durable outbox evidence are permanently lost
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
    /// The database handle itself is unchanged: still a bare `Database`,
    /// still opened exactly once. In-place close/reopen is #895's second
    /// half and is deliberately not here.
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
                has_schema_marker |= table.name() == SCHEMA_META.name();
            }
            (table_count, has_schema_marker)
        };

        let mut _open_write_transactions = 0;
        if has_schema_marker {
            let pending_ephemeral = {
                let read_txn = db.begin_read()?;
                let schema_meta = read_txn.open_table(SCHEMA_META)?;
                let version = schema_meta
                    .get(SCHEMA_VERSION_KEY)?
                    .map(|guard| guard.value());
                // Exactly one accepted epoch. Anything else — older, newer, or
                // absent — is the single refusal; the caller recreates the
                // store deliberately rather than having their bytes silently
                // upgraded, adopted, or reset underneath them.
                if version != Some(SCHEMA_VERSION) {
                    return Err(unsupported_schema(ownership.target(), version));
                }
                let cardinality_meta = read_txn.open_table(INDEX_CARDINALITY_META)?;
                // Inside the current epoch these rows are an invariant, not a
                // compatibility question: the create path writes both in the
                // same transaction as the schema marker. A marker that says
                // "current" while they disagree is corruption of the current
                // epoch, and must stay typed as corruption so it is never
                // mistaken for an old epoch or silently rebuilt.
                if cardinality_meta
                    .get(INDEX_CARDINALITY_VERSION_KEY)?
                    .map(|guard| guard.value())
                    != Some(INDEX_CARDINALITY_VERSION)
                {
                    return Err(redb::Error::Corrupted(
                        "current schema is missing its index-cardinality epoch".to_owned(),
                    )
                    .into());
                }
                let cardinality_sample_meta = read_txn.open_table(INDEX_CARDINALITY_SAMPLE_META)?;
                if cardinality_sample_meta
                    .get(INDEX_CARDINALITY_SAMPLE_KEY)?
                    .map(|value| value.value().len())
                    != Some(32)
                {
                    return Err(redb::Error::Corrupted(
                        "current schema is missing its cardinality sample key".to_owned(),
                    )
                    .into());
                }
                let outbox_meta = read_txn.open_table(OUTBOX_META)?;
                let pending_ephemeral = outbox_meta
                    .get(PENDING_EPHEMERAL_RECEIPTS_KEY)?
                    .map(|guard| guard.value().parse::<u64>())
                    .transpose()
                    .map_err(|err| {
                        redb::Error::Corrupted(format!(
                            "invalid pending ephemeral receipt count: {err}"
                        ))
                    })?
                    .unwrap_or(0);
                pending_ephemeral
            };
            // The one remaining write on the reopen path is CURRENT-epoch
            // crash recovery (crash-abandoned ephemeral receipts), not schema
            // work: it reconciles rows this exact schema wrote and is bounded
            // by a count this exact schema maintains.
            if pending_ephemeral > 0 {
                let write_txn = db.begin_write()?;
                {
                    let mut outbox_receipts = write_txn.open_table(OUTBOX_RECEIPTS)?;
                    let reconciled = reconcile_ephemeral_receipts_in_txn(&mut outbox_receipts)
                        .map_err(|error| redb::Error::Corrupted(error.message().to_owned()))?
                        as u64;
                    if reconciled != pending_ephemeral {
                        return Err(redb::Error::Corrupted(format!(
                            "pending ephemeral receipt count is {pending_ephemeral}, found {reconciled} recoverable rows"
                        ))
                        .into());
                    }
                    let mut outbox_meta = write_txn.open_table(OUTBOX_META)?;
                    outbox_meta.insert(PENDING_EPHEMERAL_RECEIPTS_KEY, "0")?;
                }
                write_txn.commit()?;
                _open_write_transactions += 1;
            }
        } else {
            // A nonempty database without the exact current marker is never
            // treated as fresh and is never mutated: initializing over it
            // would combine unversioned durable outbox/coverage/tombstone
            // facts with an empty canonical epoch.
            if table_count != 0 {
                return Err(unsupported_schema(ownership.target(), None));
            }
            let write_txn = db.begin_write()?;
            {
                write_txn.open_table(EVENTS)?;
                write_txn.open_table(EVENT_IDS)?;
                write_txn.open_table(EVENT_LOCAL)?;
                write_txn.open_table(EVENT_STORE_META)?;
                write_txn.open_table(EVENT_OBSERVATIONS)?;
                write_txn.open_table(RELAYS)?;
                write_txn.open_table(RELAY_KEYS)?;
                write_txn.open_table(RELAY_REFS)?;
                write_txn.open_table(RELAY_META)?;
                write_txn.open_table(ADDR_INDEX)?;
                write_txn.open_table(COVERAGE)?;
                write_txn.open_table(TOMBSTONES)?;
                write_txn.open_table(ADDR_TOMBSTONES)?;
                write_txn.open_table(EXPIRATION_INDEX)?;
                write_txn.open_table(POSTINGS_SEGMENTS)?;
                write_txn.open_table(POSTINGS_DICTIONARIES)?;
                write_txn.open_table(POSTINGS_RUN_META)?;
                write_txn.open_table(POSTINGS_RUN_BY_MIN)?;
                write_txn.open_table(POSTINGS_DEAD_KEYS)?;
                let mut postings_meta = write_txn.open_table(POSTINGS_META)?;
                postings_meta.insert(POSTINGS_READY, 1)?;
                write_txn.open_table(INDEX_CARDINALITY)?;
                let mut cardinality_meta = write_txn.open_table(INDEX_CARDINALITY_META)?;
                cardinality_meta
                    .insert(INDEX_CARDINALITY_VERSION_KEY, INDEX_CARDINALITY_VERSION)?;
                let sample_key = nostr::SecretKey::generate().to_secret_bytes();
                let mut cardinality_sample_meta =
                    write_txn.open_table(INDEX_CARDINALITY_SAMPLE_META)?;
                cardinality_sample_meta
                    .insert(INDEX_CARDINALITY_SAMPLE_KEY, sample_key.as_slice())?;
                write_txn.open_table(OUTBOX_INTENTS)?;
                write_txn.open_table(OUTBOX_DISPLACED)?;
                write_txn.open_table(OUTBOX_ATTEMPTS)?;
                write_txn.open_table(OUTBOX_ROUTE_REVISIONS)?;
                write_txn.open_table(OUTBOX_LANES)?;
                write_txn.open_table(OUTBOX_DEADLINES)?;
                write_txn.open_table(OUTBOX_DEADLINES_BY_INTENT)?;
                write_txn.open_table(OUTBOX_ATTEMPT_DETAILS)?;
                write_txn.open_table(OUTBOX_META)?;
                write_txn.open_table(OUTBOX_KIND5_CLAIMS)?;
                write_txn.open_table(OUTBOX_SUPPRESS_BY_ID)?;
                write_txn.open_table(OUTBOX_SUPPRESS_BY_ADDR)?;
                write_txn.open_table(OUTBOX_RECEIPTS)?;
                write_txn.open_table(OUTBOX_CORRELATIONS)?;
                let mut schema_meta = write_txn.open_table(SCHEMA_META)?;
                schema_meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)?;
            }
            write_txn.commit()?;
            _open_write_transactions += 1;
        }
        Ok(Self {
            db,
            _ownership: ownership,
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
            #[cfg(feature = "bench-instrumentation")]
            benchmark_durability: _benchmark_durability,
            #[cfg(test)]
            attempt_range_rows: AtomicU64::new(0),
            #[cfg(test)]
            route_revision_range_rows: AtomicU64::new(0),
        })
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
    pub(super) fn reset_outbox_range_rows(&self) {
        self.attempt_range_rows.store(0, Ordering::Relaxed);
        self.route_revision_range_rows.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(super) fn outbox_range_rows(&self) -> (u64, u64) {
        (
            self.attempt_range_rows.load(Ordering::Relaxed),
            self.route_revision_range_rows.load(Ordering::Relaxed),
        )
    }

    #[cfg(test)]
    pub(super) fn open_write_transactions(&self) -> u64 {
        self.open_write_transactions
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

    /// The current coverage-key schema prefix, mirroring `CoverageKey`'s own
    /// hash-level version tag (`nmp-store::coverage::COVERAGE_KEY_VERSION`).
    /// It is part of the durable key, not a compatibility discriminator: no
    /// reader tests for its absence, because a row lacking it cannot exist in
    /// the one current epoch.
    pub(super) const COVERAGE_ROW_KEY_PREFIX: &'static str = "d2:";

    pub(super) fn coverage_row_key(key: CoverageKey, relay: &RelayUrl) -> String {
        use std::fmt::Write as _;

        // Full 32-byte BLAKE3 digest, hex-encoded -- NOT truncated to 64
        // bits (see `CoverageKey::as_bytes`'s doc): this is the durable
        // redb watermark key, so the full collision-resistant width must
        // survive into the key, not just exist in memory.
        let mut hex = String::with_capacity(64);
        for byte in key.as_bytes() {
            let _ = write!(hex, "{byte:02x}");
        }
        format!("{}{hex}:{}", Self::COVERAGE_ROW_KEY_PREFIX, relay.as_str())
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
        observations: &redb::ReadOnlyTable<&'static [u8; 12], u64>,
        relays: &redb::ReadOnlyTable<RelayKey, &'static str>,
        relay_cache: &mut HashMap<RelayKey, RelayUrl>,
    ) -> Result<Provenance, PersistenceError> {
        let local = local_bytes
            .map(|bytes| {
                binary_event::decode_local(bytes).map_err(|error| {
                    PersistenceError::invariant(format!(
                        "decode canonical local state {event_key}: {error:?}"
                    ))
                })
            })
            .transpose()?;
        let (lower, upper) = observation_range(event_key);
        let mut seen = BTreeMap::new();
        for entry in observations
            .range::<&[u8; 12]>(&lower..=&upper)
            .map_err(persist_err)?
        {
            let (encoded_key, at) = entry.map_err(persist_err)?;
            let relay_key = observation_relay_key(encoded_key.value());
            let relay = if let Some(relay) = relay_cache.get(&relay_key) {
                relay.clone()
            } else {
                let encoded_relay =
                    relays.get(relay_key).map_err(persist_err)?.ok_or_else(|| {
                        PersistenceError::invariant(format!(
                            "observation points at missing relay {relay_key}"
                        ))
                    })?;
                let relay = RelayUrl::parse(encoded_relay.value()).map_err(|error| {
                    PersistenceError::invariant(format!(
                        "decode interned relay URL {relay_key}: {error}"
                    ))
                })?;
                relay_cache.insert(relay_key, relay.clone());
                relay
            };
            fold_seen_at(&mut seen, relay, Timestamp::from(at.value()));
        }
        Ok(Provenance { seen, local })
    }

    pub(super) fn decode_row(
        &self,
        event_key: EventKey,
        view: StoredEventView<'_>,
        local_bytes: Option<&[u8]>,
        observations: &redb::ReadOnlyTable<&'static [u8; 12], u64>,
        relays: &redb::ReadOnlyTable<RelayKey, &'static str>,
        relay_cache: &mut HashMap<RelayKey, RelayUrl>,
    ) -> Result<StoredEvent, PersistenceError> {
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.examined_rows.fetch_add(1, Ordering::Relaxed);
        Ok(StoredEvent {
            event: view.materialize_event().map_err(|error| {
                PersistenceError::invariant(format!(
                    "materialize canonical event {event_key}: {error:?}"
                ))
            })?,
            provenance: self.read_provenance(
                event_key,
                local_bytes,
                observations,
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
        let outbox_suppress_by_id = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ID)
            .map_err(persist_err)?;
        let outbox_suppress_by_addr = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ADDR)
            .map_err(persist_err)?;
        let suppression_possible = !outbox_suppress_by_id.is_empty().map_err(persist_err)?
            || !outbox_suppress_by_addr.is_empty().map_err(persist_err)?;
        let since = filter.since.map(|ts| ts.as_secs()).unwrap_or(0);
        let until = filter.until.map(|ts| ts.as_secs()).unwrap_or(u64::MAX);
        let prepared_filter = PreparedFilter::new(filter);
        let needs_event_value = prepared_filter.needs_event_value_after_index(plan.index.matched())
            || suppression_possible;
        let mut project_if_visible = |event_key: EventKey,
                                      event_id: EventId|
         -> Result<Option<EventId>, PersistenceError> {
            let canonical_key = event_ids
                .get(event_id.as_bytes())
                .map_err(persist_err)?
                .map(|guard| guard.value());
            if canonical_key != Some(event_key) {
                return Err(PersistenceError::invariant(format!(
                    "ordered index disagrees with canonical id map for {event_id}"
                )));
            }
            if !needs_event_value {
                return Ok(Some(event_id));
            }
            #[cfg(any(test, feature = "bench-instrumentation"))]
            self.query_event_values.fetch_add(1, Ordering::Relaxed);
            let Some(value) = events.get(event_key).map_err(persist_err)? else {
                return Err(PersistenceError::invariant(format!(
                    "ordered index points at missing canonical event {event_key}"
                )));
            };
            let view = StoredEventView::from_trusted(value.value()).map_err(|error| {
                PersistenceError::invariant(format!(
                    "decode canonical event view {event_key}: {error:?}"
                ))
            })?;
            if !view.matches_prepared_filter_after_index(&prepared_filter, plan.index.matched()) {
                return Ok(None);
            }
            if suppression_possible {
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.examined_rows.fetch_add(1, Ordering::Relaxed);
                let event = view.materialize_event().map_err(|error| {
                    PersistenceError::invariant(format!(
                        "materialize canonical event {event_key}: {error:?}"
                    ))
                })?;
                if is_suppressed_in_txn(&outbox_suppress_by_id, &outbox_suppress_by_addr, &event)? {
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
        observed_by: Option<&BTreeSet<RelayUrl>>,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        let events = read_txn.open_table(EVENTS).map_err(persist_err)?;
        let local = read_txn.open_table(EVENT_LOCAL).map_err(persist_err)?;
        let observations = read_txn
            .open_table(EVENT_OBSERVATIONS)
            .map_err(persist_err)?;
        let relays = read_txn.open_table(RELAYS).map_err(persist_err)?;
        let relay_keys = read_txn.open_table(RELAY_KEYS).map_err(persist_err)?;
        let outbox_suppress_by_id = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ID)
            .map_err(persist_err)?;
        let outbox_suppress_by_addr = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ADDR)
            .map_err(persist_err)?;
        let since = filter.since.map(|ts| ts.as_secs()).unwrap_or(0);
        let until = filter.until.map(|ts| ts.as_secs()).unwrap_or(u64::MAX);
        let mut relay_cache = HashMap::new();
        let eligible_relay_keys = if let Some(eligible) = observed_by {
            let mut keys = BTreeSet::new();
            for relay in eligible {
                if let Some(key) = relay_keys.get(relay.as_str()).map_err(persist_err)? {
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
            if let Some(eligible) = &eligible_relay_keys {
                let mut observed = false;
                for relay_key in eligible {
                    let key = observation_key(event_key, *relay_key);
                    if observations.get(&key).map_err(persist_err)?.is_some() {
                        observed = true;
                        break;
                    }
                }
                if !observed {
                    return Ok(None);
                }
            }
            #[cfg(any(test, feature = "bench-instrumentation"))]
            self.query_event_values.fetch_add(1, Ordering::Relaxed);
            let Some(value) = events.get(event_key).map_err(persist_err)? else {
                return Err(PersistenceError::invariant(format!(
                    "ordered index points at missing canonical event {event_key}"
                )));
            };
            let view = StoredEventView::from_trusted(value.value()).map_err(|error| {
                PersistenceError::invariant(format!(
                    "decode canonical event view {event_key}: {error:?}"
                ))
            })?;
            if !view.matches_prepared_filter_after_index(&prepared_filter, plan.index.matched()) {
                return Ok(None);
            }
            let local_value = local.get(event_key).map_err(persist_err)?;
            let stored = self.decode_row(
                event_key,
                view,
                local_value.as_ref().map(|value| value.value()),
                &observations,
                &relays,
                &mut relay_cache,
            )?;
            if is_suppressed_in_txn(
                &outbox_suppress_by_id,
                &outbox_suppress_by_addr,
                &stored.event,
            )? {
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
