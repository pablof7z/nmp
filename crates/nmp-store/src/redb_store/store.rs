use super::canonical::{observation_key, observation_range, observation_relay_key};
use super::outbox::{
    is_suppressed_in_txn, reconcile_ephemeral_receipts_in_txn, replace_lane_in_txn,
    OUTBOX_KIND5_CLAIMS, OUTBOX_SUPPRESS_BY_ADDR, OUTBOX_SUPPRESS_BY_ID,
};
use super::query::{
    ordered_fixed_page_range, ordered_index_event_id, ordered_vec_page_range,
    rebuild_index_cardinality, FixedOrderedCursor, OrderedIndex, OrderedPlan, OrderedWindow,
    VariableOrderedCursor,
};
use super::schema::{
    persist_err, EventKey, RelayKey, ADDR_INDEX, ADDR_TOMBSTONES, BY_AUTHOR, BY_CREATED_AT,
    BY_KIND, BY_TAG, COVERAGE, EVENTS, EVENT_IDS, EVENT_LOCAL, EVENT_OBSERVATIONS,
    EVENT_STORE_META, EXPIRATION_INDEX, INDEX_CARDINALITY, INDEX_CARDINALITY_META,
    INDEX_CARDINALITY_SAMPLE_KEY, INDEX_CARDINALITY_SAMPLE_META, INDEX_CARDINALITY_VERSION,
    INDEX_CARDINALITY_VERSION_KEY, LEGACY_BY_AUTHOR_KIND, LEGACY_EVENT_TABLES, OUTBOX_ATTEMPTS,
    OUTBOX_ATTEMPT_DETAILS, OUTBOX_CORRELATIONS, OUTBOX_DEADLINES, OUTBOX_DEADLINES_BY_INTENT,
    OUTBOX_DISPLACED, OUTBOX_INTENTS, OUTBOX_LANES, OUTBOX_META, OUTBOX_RECEIPTS,
    OUTBOX_ROUTE_REVISIONS, PENDING_EPHEMERAL_RECEIPTS_KEY, PREVIOUS_SCHEMA_VERSION,
    REDB_CACHE_BYTES, RELAYS, RELAY_KEYS, RELAY_META, RELAY_REFS, SCHEMA_META, SCHEMA_VERSION,
    SCHEMA_VERSION_KEY, TOMBSTONES,
};
#[cfg(any(test, feature = "bench-instrumentation"))]
use super::AtomicU64;
#[cfg(test)]
use super::AtomicU8;
#[cfg(any(test, feature = "bench-instrumentation"))]
use super::Ordering;
use super::{
    binary_event, open_and_register, reset_store, BTreeMap, BTreeSet, BinaryHeap, CoverageKey,
    Database, EventCursor, EventId, Filter, HashMap, LaneKey, LaneState, OpenStoreRegistration,
    Path, PersistenceError, PreparedFilter, Provenance, RecoveredLane, RegisteredOpen, RelayUrl,
    StoredEvent, StoredEventView, Timestamp,
};
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableHandle};

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
    GcBeforeCommit,
}

pub struct RedbStore {
    pub(super) db: Database,
    // Field order is load-bearing: Rust drops `db` before this registration,
    // so reset cannot proceed until the database handle is fully closed.
    pub(super) _open_registration: OpenStoreRegistration,
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
    /// Number of rows yielded by bounded attempt-table ranges. Tests reset
    /// this to prove work follows the target lane count, not total history.
    #[cfg(test)]
    pub(super) attempt_range_rows: AtomicU64,
    /// Equivalent instrumentation for resolved-route revision ranges.
    #[cfg(test)]
    pub(super) route_revision_range_rows: AtomicU64,
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
        write_txn.commit().map_err(persist_err)?;
        Ok(lane)
    }

    /// Open (creating if absent) a `redb` database file at `path`.
    ///
    /// A healthy v7 database takes only a read transaction: the explicit
    /// schema marker proves every table exists, and one exact metadata count
    /// tells us whether crash-abandoned ephemeral receipts need recovery.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, redb::Error> {
        let registered = open_and_register(path.as_ref(), |path| {
            Database::builder()
                .set_cache_size(REDB_CACHE_BYTES)
                .create(path)
        })?;
        let db = &registered.value;
        // Schema v6 deliberately carried no event-row migration. Refuse any
        // older NMP event epoch before creating a single v7 table: otherwise
        // canonical events would appear empty while unversioned durable
        // outbox/coverage/tombstone facts from the old epoch remained live.
        // A caller opting into this breaking release must recreate the whole
        // database, never unknowingly run a split-brain mixture.
        let (table_count, has_schema_marker, has_legacy_epoch) = {
            let read_txn = db.begin_read()?;
            let mut table_count = 0usize;
            let mut has_schema_marker = false;
            let mut has_legacy_epoch = false;
            for table in read_txn.list_tables()? {
                table_count += 1;
                let name = table.name();
                has_schema_marker |= name == SCHEMA_META.name();
                has_legacy_epoch |= LEGACY_EVENT_TABLES.contains(&name);
            }
            (table_count, has_schema_marker, has_legacy_epoch)
        };
        if has_legacy_epoch {
            return Err(redb::Error::UpgradeRequired(SCHEMA_VERSION as u8));
        }

        let mut _open_write_transactions = 0;
        if has_schema_marker {
            let (needs_schema_migration, needs_cardinality_rebuild, pending_ephemeral) = {
                let read_txn = db.begin_read()?;
                let schema_meta = read_txn.open_table(SCHEMA_META)?;
                let version = schema_meta
                    .get(SCHEMA_VERSION_KEY)?
                    .map(|guard| guard.value());
                if version != Some(SCHEMA_VERSION) && version != Some(PREVIOUS_SCHEMA_VERSION) {
                    return Err(redb::Error::UpgradeRequired(SCHEMA_VERSION as u8));
                }
                let needs_schema_migration = version == Some(PREVIOUS_SCHEMA_VERSION);
                let cardinality_meta = read_txn.open_table(INDEX_CARDINALITY_META)?;
                let cardinality_version = cardinality_meta
                    .get(INDEX_CARDINALITY_VERSION_KEY)?
                    .map(|guard| guard.value());
                let cardinality_sample_meta = read_txn.open_table(INDEX_CARDINALITY_SAMPLE_META)?;
                let cardinality_sample_key_len = cardinality_sample_meta
                    .get(INDEX_CARDINALITY_SAMPLE_KEY)?
                    .map(|value| value.value().len());
                if cardinality_sample_key_len.is_some_and(|len| len != 32) {
                    return Err(redb::Error::Corrupted(
                        "invalid cardinality sample key length".to_owned(),
                    ));
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
                (
                    needs_schema_migration,
                    needs_schema_migration
                        || cardinality_version != Some(INDEX_CARDINALITY_VERSION)
                        || cardinality_sample_key_len.is_none(),
                    pending_ephemeral,
                )
            };
            if needs_schema_migration || needs_cardinality_rebuild || pending_ephemeral > 0 {
                let write_txn = db.begin_write()?;
                {
                    if needs_schema_migration {
                        write_txn.delete_table(LEGACY_BY_AUTHOR_KIND)?;
                        let mut schema_meta = write_txn.open_table(SCHEMA_META)?;
                        schema_meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)?;
                    }
                    if needs_cardinality_rebuild {
                        let mut cardinality_sample_meta =
                            write_txn.open_table(INDEX_CARDINALITY_SAMPLE_META)?;
                        let existing_sample_key = cardinality_sample_meta
                            .get(INDEX_CARDINALITY_SAMPLE_KEY)?
                            .map(|value| value.value().to_vec());
                        let sample_key = match existing_sample_key {
                            Some(value) => value.as_slice().try_into().map_err(|_| {
                                redb::Error::Corrupted(
                                    "invalid cardinality sample key length".to_owned(),
                                )
                            })?,
                            None => {
                                let key = nostr::SecretKey::generate().to_secret_bytes();
                                cardinality_sample_meta
                                    .insert(INDEX_CARDINALITY_SAMPLE_KEY, key.as_slice())?;
                                key
                            }
                        };
                        drop(cardinality_sample_meta);
                        let by_created_at = write_txn.open_table(BY_CREATED_AT)?;
                        let by_author = write_txn.open_table(BY_AUTHOR)?;
                        let by_kind = write_txn.open_table(BY_KIND)?;
                        let by_tag = write_txn.open_table(BY_TAG)?;
                        let mut cardinality = write_txn.open_table(INDEX_CARDINALITY)?;
                        rebuild_index_cardinality(
                            &by_created_at,
                            &by_author,
                            &by_kind,
                            &by_tag,
                            &mut cardinality,
                            &sample_key,
                        )?;
                        let mut cardinality_meta = write_txn.open_table(INDEX_CARDINALITY_META)?;
                        cardinality_meta
                            .insert(INDEX_CARDINALITY_VERSION_KEY, INDEX_CARDINALITY_VERSION)?;
                    }
                    if pending_ephemeral > 0 {
                        let mut outbox_receipts = write_txn.open_table(OUTBOX_RECEIPTS)?;
                        let reconciled =
                            reconcile_ephemeral_receipts_in_txn(&mut outbox_receipts) as u64;
                        if reconciled != pending_ephemeral {
                            return Err(redb::Error::Corrupted(format!(
                                "pending ephemeral receipt count is {pending_ephemeral}, found {reconciled} recoverable rows"
                            )));
                        }
                        let mut outbox_meta = write_txn.open_table(OUTBOX_META)?;
                        outbox_meta.insert(PENDING_EPHEMERAL_RECEIPTS_KEY, "0")?;
                    }
                }
                write_txn.commit()?;
                _open_write_transactions += 1;
            }
        } else {
            // A non-empty database without the v6 marker is never treated as
            // fresh: doing so could combine old unversioned governed facts
            // with an empty canonical epoch.
            if table_count != 0 {
                return Err(redb::Error::UpgradeRequired(SCHEMA_VERSION as u8));
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
                write_txn.open_table(BY_CREATED_AT)?;
                write_txn.open_table(BY_AUTHOR)?;
                write_txn.open_table(BY_KIND)?;
                write_txn.open_table(BY_TAG)?;
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
        let RegisteredOpen {
            value: db,
            registration,
        } = registered;
        Ok(Self {
            db,
            _open_registration: registration,
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
            #[cfg(test)]
            attempt_range_rows: AtomicU64::new(0),
            #[cfg(test)]
            route_revision_range_rows: AtomicU64::new(0),
        })
    }

    /// Destructively remove one closed persistent store target. The same
    /// process-global mutex serializes this operation with every
    /// [`RedbStore::open`] path, including stores later moved through raw
    /// engine construction. Existing and dangling final symlink aliases
    /// resolve to the actual store target; the alias inode is not removed.
    /// A live target is a typed refusal. This is deliberately process-local:
    /// arbitrary external retargeting and cross-process reset require a
    /// separate advisory-lock contract.
    pub fn reset(path: impl AsRef<Path>) -> Result<(), crate::RedbStoreResetError> {
        reset_store(path.as_ref())
    }

    #[cfg(test)]
    pub(super) fn open_with_crash_point(
        path: impl AsRef<Path>,
        crash_point: RedbCrashPoint,
    ) -> Result<Self, redb::Error> {
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

    /// The current schema-version row-key PREFIX (#106, Fable's C
    /// refinement): distinguishes a v2 (context-aware `ContextualAtom`)
    /// row from a legacy v1 (bare `ConcreteFilter`, pre-#106) row by a
    /// cheap string check, independent of `CoverageKey`'s own hash-level
    /// version tag (`nmp-store::coverage::COVERAGE_KEY_VERSION`) -- `gc`'s
    /// legacy-purge pass greps for the ABSENCE of this exact prefix.
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
        let local = local_bytes.map(|bytes| {
            binary_event::decode_local(bytes).expect("redb: decode canonical local state")
        });
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
                        PersistenceError(format!("observation points at missing relay {relay_key}"))
                    })?;
                let relay = RelayUrl::parse(encoded_relay.value())
                    .expect("redb: interned relay URL remains canonical");
                relay_cache.insert(relay_key, relay.clone());
                relay
            };
            assert!(seen.insert(relay, Timestamp::from(at.value())).is_none());
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
            event: view
                .materialize_event()
                .expect("redb: materialize validated portable event"),
            provenance: self.read_provenance(
                event_key,
                local_bytes,
                observations,
                relays,
                relay_cache,
            )?,
        })
    }

    pub(super) fn scan_fixed_ordered<const N: usize, T>(
        &self,
        index: &redb::ReadOnlyTable<&[u8; N], EventKey>,
        prefixes: &[Vec<u8>],
        window: OrderedWindow,
        limit: Option<usize>,
        project_if_visible: &mut impl FnMut(EventKey, EventId) -> Result<Option<T>, PersistenceError>,
    ) -> Result<Vec<T>, PersistenceError> {
        if let [prefix] = prefixes {
            let Some((lower, upper, exclusive_upper)) =
                ordered_fixed_page_range(prefix, window.since, window.until, window.before)
            else {
                return Ok(Vec::new());
            };
            let entries = if exclusive_upper {
                index
                    .range::<&[u8; N]>(&lower..&upper)
                    .map_err(persist_err)?
            } else {
                index
                    .range::<&[u8; N]>(&lower..=&upper)
                    .map_err(persist_err)?
            };
            let mut out = limit.map_or_else(Vec::new, Vec::with_capacity);
            for entry in entries.rev() {
                let (key, value) = entry.map_err(persist_err)?;
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.query_index_rows.fetch_add(1, Ordering::Relaxed);
                if let Some(projected) =
                    project_if_visible(value.value(), ordered_index_event_id(key.value()))?
                {
                    out.push(projected);
                    if limit.is_some_and(|limit| out.len() == limit) {
                        break;
                    }
                }
            }
            return Ok(out);
        }

        let mut cursors: Vec<_> = prefixes
            .iter()
            .map(|prefix| {
                FixedOrderedCursor::new(index, prefix, window.since, window.until, window.before)
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        let mut heap = BinaryHeap::new();
        for (cursor_index, cursor) in cursors.iter_mut().enumerate() {
            if let Some(head) = cursor.next_head(cursor_index)? {
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.query_index_rows.fetch_add(1, Ordering::Relaxed);
                heap.push(head);
            }
        }

        let mut out = limit.map_or_else(Vec::new, Vec::with_capacity);
        let mut last_event_key = None;
        while let Some(head) = heap.pop() {
            let is_new = last_event_key.replace(head.event_key) != Some(head.event_key);
            if is_new {
                if let Some(projected) = project_if_visible(head.event_key, head.id)? {
                    out.push(projected);
                    if limit.is_some_and(|limit| out.len() == limit) {
                        break;
                    }
                }
            }
            if let Some(next) = cursors[head.cursor].next_head(head.cursor)? {
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.query_index_rows.fetch_add(1, Ordering::Relaxed);
                heap.push(next);
            }
        }
        Ok(out)
    }

    pub(super) fn scan_variable_ordered<T>(
        &self,
        index: &redb::ReadOnlyTable<&[u8], EventKey>,
        prefixes: &[Vec<u8>],
        window: OrderedWindow,
        limit: Option<usize>,
        project_if_visible: &mut impl FnMut(EventKey, EventId) -> Result<Option<T>, PersistenceError>,
    ) -> Result<Vec<T>, PersistenceError> {
        if let [prefix] = prefixes {
            let Some((lower, upper, exclusive_upper)) =
                ordered_vec_page_range(prefix, window.since, window.until, window.before)
            else {
                return Ok(Vec::new());
            };
            let entries = if exclusive_upper {
                index
                    .range(lower.as_slice()..upper.as_slice())
                    .map_err(persist_err)?
            } else {
                index
                    .range(lower.as_slice()..=upper.as_slice())
                    .map_err(persist_err)?
            };
            let mut out = limit.map_or_else(Vec::new, Vec::with_capacity);
            for entry in entries.rev() {
                let (key, value) = entry.map_err(persist_err)?;
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.query_index_rows.fetch_add(1, Ordering::Relaxed);
                if let Some(projected) =
                    project_if_visible(value.value(), ordered_index_event_id(key.value()))?
                {
                    out.push(projected);
                    if limit.is_some_and(|limit| out.len() == limit) {
                        break;
                    }
                }
            }
            return Ok(out);
        }

        let mut cursors: Vec<_> = prefixes
            .iter()
            .map(|prefix| {
                VariableOrderedCursor::new(index, prefix, window.since, window.until, window.before)
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        let mut heap = BinaryHeap::new();
        for (cursor_index, cursor) in cursors.iter_mut().enumerate() {
            if let Some(head) = cursor.next_head(cursor_index)? {
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.query_index_rows.fetch_add(1, Ordering::Relaxed);
                heap.push(head);
            }
        }

        let mut out = limit.map_or_else(Vec::new, Vec::with_capacity);
        let mut last_event_key = None;
        while let Some(head) = heap.pop() {
            let is_new = last_event_key.replace(head.event_key) != Some(head.event_key);
            if is_new {
                if let Some(projected) = project_if_visible(head.event_key, head.id)? {
                    out.push(projected);
                    if limit.is_some_and(|limit| out.len() == limit) {
                        break;
                    }
                }
            }
            if let Some(next) = cursors[head.cursor].next_head(head.cursor)? {
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.query_index_rows.fetch_add(1, Ordering::Relaxed);
                heap.push(next);
            }
        }
        Ok(out)
    }

    /// Reverse-merge one or more ranges from the planner's chosen index.
    /// Each cursor asks redb for exactly its next key; once `limit` visible
    /// rows have survived the borrowed binary post-filter, no older key or
    /// event value is touched.
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
        let window = OrderedWindow {
            since: filter.since.map(|ts| ts.as_secs()).unwrap_or(0),
            until: filter.until.map(|ts| ts.as_secs()).unwrap_or(u64::MAX),
            before: None,
        };
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
                return Err(PersistenceError(format!(
                    "ordered index disagrees with canonical id map for {event_id}"
                )));
            }
            if !needs_event_value {
                return Ok(Some(event_id));
            }
            #[cfg(any(test, feature = "bench-instrumentation"))]
            self.query_event_values.fetch_add(1, Ordering::Relaxed);
            let Some(value) = events.get(event_key).map_err(persist_err)? else {
                return Err(PersistenceError(format!(
                    "ordered index points at missing canonical event {event_key}"
                )));
            };
            let view = StoredEventView::from_trusted(value.value())
                .expect("redb: decode portable stored event view");
            if !view.matches_prepared_filter_after_index(&prepared_filter, plan.index.matched()) {
                return Ok(None);
            }
            if suppression_possible {
                #[cfg(any(test, feature = "bench-instrumentation"))]
                self.examined_rows.fetch_add(1, Ordering::Relaxed);
                let event = view
                    .materialize_event()
                    .expect("redb: materialize validated portable event");
                if is_suppressed_in_txn(&outbox_suppress_by_id, &outbox_suppress_by_addr, &event)? {
                    return Ok(None);
                }
            }
            Ok(Some(event_id))
        };

        match plan.index {
            OrderedIndex::Global => {
                let index = read_txn.open_table(BY_CREATED_AT).map_err(persist_err)?;
                self.scan_fixed_ordered(
                    &index,
                    &plan.prefixes,
                    window,
                    Some(limit),
                    &mut project_if_visible,
                )
            }
            OrderedIndex::Author => {
                let index = read_txn.open_table(BY_AUTHOR).map_err(persist_err)?;
                self.scan_fixed_ordered(
                    &index,
                    &plan.prefixes,
                    window,
                    Some(limit),
                    &mut project_if_visible,
                )
            }
            OrderedIndex::Kind => {
                let index = read_txn.open_table(BY_KIND).map_err(persist_err)?;
                self.scan_fixed_ordered(
                    &index,
                    &plan.prefixes,
                    window,
                    Some(limit),
                    &mut project_if_visible,
                )
            }
            OrderedIndex::Tag(_) => {
                let index = read_txn.open_table(BY_TAG).map_err(persist_err)?;
                self.scan_variable_ordered(
                    &index,
                    &plan.prefixes,
                    window,
                    Some(limit),
                    &mut project_if_visible,
                )
            }
        }
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
        let window = OrderedWindow {
            since,
            until,
            before,
        };
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
                return Err(PersistenceError(format!(
                    "ordered index points at missing canonical event {event_key}"
                )));
            };
            let view = StoredEventView::from_trusted(value.value())
                .expect("redb: decode portable stored event view");
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

        match plan.index {
            OrderedIndex::Global => {
                let index = read_txn.open_table(BY_CREATED_AT).map_err(persist_err)?;
                self.scan_fixed_ordered(
                    &index,
                    &plan.prefixes,
                    window,
                    limit,
                    &mut materialize_if_visible,
                )
            }
            OrderedIndex::Author => {
                let index = read_txn.open_table(BY_AUTHOR).map_err(persist_err)?;
                self.scan_fixed_ordered(
                    &index,
                    &plan.prefixes,
                    window,
                    limit,
                    &mut materialize_if_visible,
                )
            }
            OrderedIndex::Kind => {
                let index = read_txn.open_table(BY_KIND).map_err(persist_err)?;
                self.scan_fixed_ordered(
                    &index,
                    &plan.prefixes,
                    window,
                    limit,
                    &mut materialize_if_visible,
                )
            }
            OrderedIndex::Tag(_) => {
                let index = read_txn.open_table(BY_TAG).map_err(persist_err)?;
                self.scan_variable_ordered(
                    &index,
                    &plan.prefixes,
                    window,
                    limit,
                    &mut materialize_if_visible,
                )
            }
        }
    }
}
