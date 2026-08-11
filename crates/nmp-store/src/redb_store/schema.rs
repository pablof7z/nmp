use super::{EventId, Path, PersistenceError, PublicKey, RedbStoreOpenError, TableDefinition};
use crate::PersistenceFault;

/// Wrap any `redb` operation error as a [`PersistenceError`] (architecture
/// review correction — see its doc). `accept_write`/`accept_ephemeral`/
/// `promote_signed`/`compensate_write`, and every table-touching helper
/// they call, propagate through this via `?`; the crate's OTHER,
/// pre-existing doors (`insert`/`remove`/`expire_due`/`gc`) still
/// `.expect()` these same `Result`s at their own call sites into the
/// shared helpers below — unchanged behavior for them, just funneled
/// through one typed error type instead of a bespoke panic message each.
///
/// This is also the one place redb's typed failure survives (#895). redb
/// already distinguishes the latch (`PreviousIo`/`DatabaseClosed`) from the
/// originating I/O failure (`Io`); flattening every error to `to_string()`
/// here was what forced embedders to grep the message. The message is still
/// preserved verbatim — the classification is added alongside it, so the
/// ~450 `map_err(persist_err)` call sites need no change at all.
///
/// The bound is `Into<redb::Error>` rather than `Display` on purpose: it
/// admits exactly redb's own error family (`StorageError`, `TableError`,
/// `TransactionError`, `CommitError`, `DatabaseError`, `SavepointError`,
/// `CompactionError`, `io::Error`, and `redb::Error` itself), so a non-redb
/// failure cannot silently arrive here and be mislabeled as a backend
/// fault. Those go through [`PersistenceError::invariant`] instead.
pub(super) fn persist_err(e: impl Into<redb::Error>) -> PersistenceError {
    let error = e.into();
    PersistenceError::new(classify(&error), error.to_string())
}

/// Map redb's typed error onto the durability classification.
///
/// The load-bearing split is `PreviousIo`/`DatabaseClosed` (raised by
/// `CheckedBackend::check_failure()` *before* the backend op is attempted —
/// so the write was never tried) versus `Io` (the first failure, the one
/// that sets the latch, whose durability is genuinely unknown). See
/// [`PersistenceFault`] for why `Io` cannot honestly be narrowed further.
///
/// Every variant redb 4.1 models above the storage layer — a table type
/// mismatch, a missing table, an upgrade requirement, a savepoint refusal —
/// is enumerated as this crate misusing its own database: `Invariant`.
/// `redb::Error` is non-exhaustive, so the wildcard is reserved for a future
/// backend state and must remain conservative.
fn classify(error: &redb::Error) -> PersistenceFault {
    match error {
        redb::Error::PreviousIo | redb::Error::DatabaseClosed => PersistenceFault::Latched,
        redb::Error::Io(_) => PersistenceFault::Io,
        redb::Error::Corrupted(_) => PersistenceFault::Corrupted,
        redb::Error::ValueTooLarge(_) => PersistenceFault::ValueTooLarge,
        redb::Error::LockPoisoned(_) => PersistenceFault::LockPoisoned,
        redb::Error::DatabaseAlreadyOpen
        | redb::Error::InvalidSavepoint
        | redb::Error::ImmediateDurabilityRequired
        | redb::Error::RepairAborted
        | redb::Error::PersistentSavepointModified
        | redb::Error::PersistentSavepointExists
        | redb::Error::EphemeralSavepointExists
        | redb::Error::TransactionInProgress
        | redb::Error::UpgradeRequired(_)
        | redb::Error::TableTypeMismatch { .. }
        | redb::Error::TableIsMultimap(_)
        | redb::Error::TableIsNotMultimap(_)
        | redb::Error::TypeDefinitionChanged { .. }
        | redb::Error::TableDoesNotExist(_)
        | redb::Error::TableExists(_)
        | redb::Error::TableAlreadyOpen(_, _)
        | redb::Error::ReadTransactionStillInUse(_) => PersistenceFault::Invariant,
        _ => unknown_backend_fault(),
    }
}

/// The mandatory conservative fallback for a future redb error variant.
///
/// Kept as a separately falsifiable mapping because `redb::Error` is
/// non-exhaustive and current Rust cannot construct a future variant in a
/// unit test.
pub(super) fn unknown_backend_fault() -> PersistenceFault {
    PersistenceFault::UnknownBackend
}

/// The ONE refusal for durable bytes that are not the exact current schema
/// epoch (#867).
///
/// Every nonempty store that does not carry exactly [`SCHEMA_VERSION`] ends
/// here — there is no second refusal, no per-epoch decoder to try first, and
/// no migration/adoption/alias/reset door behind it. This is the ONLY
/// construction site of [`RedbStoreOpenError::UnsupportedSchema`], and it is
/// reachable only from [`crate::RedbStore::open`]'s epoch probe, so the
/// refusal is returned before a store handle is exposed and before any byte
/// is written.
///
/// Corruption of the CURRENT epoch is deliberately NOT routed here: it stays
/// `RedbStoreOpenError::Database(redb::Error::Corrupted(..))`, so an operator
/// can never read "unsupported schema" and conclude their current-epoch data
/// was merely old.
pub(super) fn unsupported_schema(target: &Path, found: Option<u64>) -> RedbStoreOpenError {
    RedbStoreOpenError::UnsupportedSchema {
        path: target.to_path_buf(),
        expected: SCHEMA_VERSION,
        found,
    }
}

pub(super) type EventKey = u64;
pub(super) type RelayKey = u32;

/// The canonical event key space: one tree, three columns.
///
/// Key: `[event_key:u64-be | col:u8 | rest]`.
///
/// - [`EVENT_COL_ROW`] — the immutable portable note bytes. One row per event.
/// - [`EVENT_COL_LOCAL`] — this node's local origin sidecar, present iff the
///   row is ours. Zero or one row per event.
/// - [`EVENT_COL_OBSERVATION`] — `rest` is `relay_key:u32-be`; the value is
///   the greatest observation timestamp in seconds. Zero or more per event.
///
/// The local sidecar and the relay observations were separate trees, but both
/// were keyed by (or prefixed with) the event key, so neither was a distinct
/// key space (#1248) — `event_observations` was already a compound key on it.
/// `MemoryStore` does not split them at all: local origin and relay
/// observations live inline on `StoredEvent.provenance`.
///
/// The event key leads, so every column of ONE event is contiguous and
/// "forget event K" is a single range delete instead of four coordinated
/// deletes across three trees — a crash-atomicity simplification, not only a
/// table saving. The cost is that the canonical-row full scan in `gc` visits
/// the sidecars too and filters on the column byte, since no column is
/// contiguous ACROSS events under this ordering. That is the deliberate
/// trade: `gc` is already a full scan, and `remove_by_key` is on the hot
/// governed-mutation path.
pub(super) const EVENTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("events");
pub(super) const EVENT_COL_ROW: u8 = 0;
pub(super) const EVENT_COL_LOCAL: u8 = 1;
pub(super) const EVENT_COL_OBSERVATION: u8 = 2;
pub(super) const EVENT_IDS: TableDefinition<&[u8; 32], EventKey> =
    TableDefinition::new("event_ids");
/// The relay dictionary: surrogate -> (observation refcount, canonical URL).
///
/// The refcount was a second tree keyed by the same surrogate — a column, not
/// a key space (#1248). Folding it in also makes "this URL is interned and N
/// observations reference it" one row rather than two rows that can disagree.
/// Value: `refs:u64-be | url utf8`, via [`encode_relay_row`].
pub(super) const RELAYS: TableDefinition<RelayKey, &[u8]> = TableDefinition::new("relays");
/// The reverse direction, URL -> surrogate. A genuinely distinct key space:
/// it is ordered and looked up by URL, which [`RELAYS`] cannot answer.
pub(super) const RELAY_IDS: TableDefinition<&str, RelayKey> = TableDefinition::new("relay_ids");
pub(super) const STORE_META: TableDefinition<&str, u64> = TableDefinition::new("store_meta");
pub(super) const SCHEMA_VERSION_KEY: &str = "schema_version";
pub(super) const NEXT_EVENT_KEY: &str = "next_event_key";
pub(super) const NEXT_RELAY_KEY: &str = "next_relay_key";
pub(super) const POSTINGS_NEXT_RUN_ID: &str = "postings_next_run_id";
pub(super) const POSTINGS_READY: &str = "postings_query_ready";
/// The ONE exact current schema epoch (#867). NMP carries no persistent-schema
/// compatibility obligation in this architecture cut: there is no pre-current
/// decoder, no migration, no adoption, and no destructive reset door. A
/// nonempty store whose marker is not exactly this value is refused at
/// [`crate::RedbStore::open`] before any table is created or any byte is
/// mutated.
///
/// This value covers the whole durable model together — events, coverage,
/// accepted writes, lanes, attempts, receipts, and route facts — because they
/// share one `redb::Database` transaction boundary and are therefore one
/// epoch, not seven independently-versioned ones.
pub(super) const SCHEMA_VERSION: u64 = 14;
/// Bound redb's process-private page cache for mobile/desktop clients.
///
/// redb 4.1 defaults this cache to 1 GiB. A million-event sequential ingest
/// fills most of that default even when NMP's transport queues and live query
/// projections remain bounded, so the database alone can consume more memory
/// than the rest of the process by an order of magnitude.
// Packed runs are large immutable values and the operating system already
// caches their pages. A larger Redb cache retains duplicate hot pages and
// makes the one-million-event working set scale with query traffic.
pub(super) const REDB_CACHE_BYTES: usize = 12 * 1024 * 1024;
pub(super) const ADDR_INDEX: TableDefinition<&str, EventKey> = TableDefinition::new("addr_index");
pub(super) const COVERAGE: TableDefinition<&str, &str> = TableDefinition::new("coverage");
/// Permanent kind:5 deletion facts, for event ids and for
/// replaceable/addressable addresses alike
/// (retraction-and-negative-deltas.md §2/§7).
///
/// Key: `[kind:u8 | target]`, where `kind` is [`TOMBSTONE_ID`] or
/// [`TOMBSTONE_ADDR`]. Both were separate trees over the same logical thing —
/// a permanent deletion target reached by point `get`/`insert`, never ranged,
/// never iterated, never counted — so neither earned a tree of its own
/// (#1248). The discriminant keeps them contiguous and disjoint.
///
/// An id row is keyed per (target id, CLAIMING AUTHOR) and is never collapsed
/// to one row per id: the target's real author is unknown until it actually
/// arrives, so an unauthorized third party can always name an id someone else
/// has already (or will later) legitimately delete. A single overwritable row
/// per id would let that unauthorized claim silently replace — and so undo —
/// the real author's permanent, authorized deletion. Its value is the
/// deleting kind:5's own raw id (diagnostics only; the key alone decides
/// refusal).
///
/// An address row's value carries the deletion ceiling (highest deleting-event
/// `created_at` seen for that address) — a candidate with
/// `created_at <= ceiling` is tombstoned.
///
/// Never GC-claimed.
pub(super) const TOMBSTONES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tombstones");
pub(super) const TOMBSTONE_ID: u8 = 0;
pub(super) const TOMBSTONE_ADDR: u8 = 1;
/// The persistent NIP-40 expiration index (retraction-and-negative-
/// deltas.md §3.1). Key: `expires_at:u64-be | event_id:[u8;32]`, so ordinary
/// byte ordering matches numeric deadline order without decimal/hex work;
/// value: the canonical event's compact surrogate key.
pub(super) const EXPIRATION_INDEX: TableDefinition<&[u8; 40], EventKey> =
    TableDefinition::new("expiration_index");
/// Binary ordered indexes all end in the same sortable suffix:
/// `created_at:u64-be | !event_id:[u8;32]`. Reverse scans therefore yield
/// `created_at DESC, event_id ASC` and can stop exactly at the visible limit.
///
/// Comparison-only: packed postings own the current query layout, so
/// [`crate::RedbStore`] never creates or reads these row indexes. They survive
/// solely so benchmark variants can measure the alternative physical shape.
#[cfg(feature = "bench-instrumentation")]
pub(super) const BY_CREATED_AT: TableDefinition<&[u8; 40], EventKey> =
    TableDefinition::new("by_created_at");
#[cfg(feature = "bench-instrumentation")]
pub(super) const BY_AUTHOR: TableDefinition<&[u8; 72], EventKey> =
    TableDefinition::new("by_author_time");
#[cfg(feature = "bench-instrumentation")]
pub(super) const BY_KIND: TableDefinition<&[u8; 42], EventKey> =
    TableDefinition::new("by_kind_time");
/// Comparison-only historical index shape used by benchmark variants; never
/// opened by [`crate::RedbStore`] and not part of the current schema epoch.
#[cfg(feature = "bench-instrumentation")]
pub(super) const COMPARISON_BY_AUTHOR_KIND: TableDefinition<&[u8; 74], EventKey> =
    TableDefinition::new("by_author_kind_time");
/// NIP-01 single-letter tag index, borrowing nostrdb's clustered
/// `(tag,value,created_at)` layout. The binary key is:
///
/// `tag:u8 | encoding:u8 | value | created_at:u64-be | !event_id:[u8;32]`
///
/// Big-endian timestamp bytes make redb's ordinary byte ordering usable as a
/// newest-first reverse range scan. The event id suffix both disambiguates
/// equal timestamps. The id bytes are inverted so a reverse scan is
/// `created_at DESC, event_id ASC`, NMP's canonical NIP-01 tie-break, without
/// parsing hex.
/// Values are compact event keys, so a hit dereferences the immutable note
/// directly without rebuilding or hex-encoding its NIP-01 id.
///
/// Comparison-only, exactly like [`BY_CREATED_AT`].
#[cfg(feature = "bench-instrumentation")]
pub(super) const BY_TAG: TableDefinition<&[u8], EventKey> = TableDefinition::new("by_tag");
/// Immutable packed ordered-postings artifacts. Packed postings are the
/// current query-authoritative representation.
pub(super) const POSTINGS_SEGMENTS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("postings_segments");
/// The packed-run catalog: everything that describes a run without being the
/// run's bulk segment bytes.
///
/// Key: `[col:u8 | ...]`, where `col` selects one of four columns of the same
/// catalog — run metadata, run dictionary, the min-event-key range index, and
/// the per-run death blocks. Each was its own tree; none of them was a
/// distinct key space (#1248). redb sorts by raw key bytes, so a
/// discriminant-first key keeps every column contiguous and prefix-scannable
/// exactly as its own tree was, and the heavy dictionary bytes never share a
/// page with the metadata rows the catalog scan reads.
///
/// The one access pattern that needs care is the range index's predecessor
/// search: it must be LOWER-bounded at its own column
/// (`[BY_MIN | 0] ..= [BY_MIN | k]`), or `next_back()` on an unbounded range
/// would walk off the front of the column into the dictionary column.
///
/// The death blocks stay: segments are immutable, deletion removes only the
/// canonical rows, and the query projector hard-errors when a posting
/// resolves to a missing event — so the merged death set is what keeps
/// deleted events out of query merges. Only their tree is gone.
pub(super) const POSTINGS_CATALOG: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("postings_catalog");
/// Sampled live-row counts per ordered-index prefix. Keys are namespaced
/// binary prefixes (global, author, kind, or tag/value); values count sampled
/// physical rows in that bucket.
///
/// Comparison-only, exactly like [`BY_CREATED_AT`]: [`crate::RedbStore`] never
/// creates, opens, or reads this table and is not part of the current schema
/// epoch. The planner it once fed chose between indexes that all return the
/// same rows (`plan.index` only picks a scan order; the post-index mask is
/// derived from the chosen index, never from an estimate), so the durable
/// estimate could only make a query slower, never wrong — and packed postings
/// already carry an exact per-prefix `posting_count` in their segment headers.
/// It survives solely so benchmark variants can measure the alternative
/// physical shape that motivated it.
#[cfg(feature = "bench-instrumentation")]
pub(super) const INDEX_CARDINALITY: TableDefinition<&[u8], u64> =
    TableDefinition::new("index_cardinality");
/// Fresh publish-queue namespace (#1027). The key widths are the
/// semantic contract, not redb's numeric layout:
///
/// - intent/receipt: `u64-be`;
/// - relay surrogate: `u32-be`;
/// - lane: `intent:u64-be | relay:u32-be`;
/// - attempt: lane key plus `ordinal:u64-be`;
/// - route revision: `intent:u64-be | ordinal:u64-be`;
/// - deadline: `time:u64-be | intent:u64-be | relay:u32-be`.
///
/// Values use the explicit codec in `publish_queue_codec.rs`. No previous
/// execution table is opened, read, transformed, or deleted.
pub(super) const PUBLISH_QUEUE_INTENTS: TableDefinition<&[u8; 8], &[u8]> =
    TableDefinition::new("publish_queue_intents");
pub(super) const PUBLISH_QUEUE_DISPLACED: TableDefinition<&[u8; 8], &[u8]> =
    TableDefinition::new("publish_queue_displaced");
pub(super) const PUBLISH_QUEUE_ATTEMPTS: TableDefinition<&[u8; 20], &[u8]> =
    TableDefinition::new("publish_queue_attempts");
pub(super) const PUBLISH_QUEUE_ROUTE_REVISIONS: TableDefinition<&[u8; 16], &[u8]> =
    TableDefinition::new("publish_queue_route_revisions");
pub(super) const PUBLISH_QUEUE_LANES: TableDefinition<&[u8; 12], &[u8]> =
    TableDefinition::new("publish_queue_lanes");
pub(super) const PUBLISH_QUEUE_DEADLINES: TableDefinition<&[u8; 20], &[u8]> =
    TableDefinition::new("publish_queue_deadlines");
pub(super) const PUBLISH_QUEUE_DEADLINES_BY_INTENT: TableDefinition<&[u8; 20], &[u8]> =
    TableDefinition::new("publish_queue_deadlines_by_intent");
pub(super) const PUBLISH_QUEUE_ATTEMPT_DETAILS: TableDefinition<&[u8; 20], &[u8]> =
    TableDefinition::new("publish_queue_attempt_details");
pub(super) const PUBLISH_QUEUE_META: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("publish_queue_meta");
pub(super) const PUBLISH_QUEUE_RECEIPTS: TableDefinition<&[u8; 8], &[u8]> =
    TableDefinition::new("publish_queue_receipts");
pub(super) const PUBLISH_QUEUE_CORRELATIONS: TableDefinition<&[u8], &[u8; 8]> =
    TableDefinition::new("publish_queue_correlations");
pub(super) const PUBLISH_QUEUE_RELAYS: TableDefinition<&[u8; 4], &[u8]> =
    TableDefinition::new("publish_queue_relays");
pub(super) const PUBLISH_QUEUE_RELAY_IDS: TableDefinition<&[u8], &[u8; 4]> =
    TableDefinition::new("publish_queue_relay_ids");
pub(super) const PUBLISH_QUEUE_KIND5_CLAIMS: TableDefinition<&[u8; 8], &[u8]> =
    TableDefinition::new("publish_queue_kind5_claims");
pub(super) const PUBLISH_QUEUE_SUPPRESS_BY_ID: TableDefinition<&[u8; 64], &[u8]> =
    TableDefinition::new("publish_queue_suppress_by_id");
pub(super) const PUBLISH_QUEUE_SUPPRESS_BY_ADDR: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("publish_queue_suppress_by_addr");

/// The [`EVENT_COL_ROW`] key of one event.
pub(super) fn event_row_key(event_key: EventKey) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[..8].copy_from_slice(&event_key.to_be_bytes());
    key[8] = EVENT_COL_ROW;
    key
}

/// The [`EVENT_COL_LOCAL`] key of one event.
pub(super) fn event_local_key(event_key: EventKey) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[..8].copy_from_slice(&event_key.to_be_bytes());
    key[8] = EVENT_COL_LOCAL;
    key
}

/// The inclusive bounds covering EVERY column of one event — what makes
/// "forget event K" one range delete.
pub(super) fn event_all_columns_bounds(event_key: EventKey) -> ([u8; 9], [u8; 13]) {
    let mut lower = [0u8; 9];
    lower[..8].copy_from_slice(&event_key.to_be_bytes());
    let mut upper = [u8::MAX; 13];
    upper[..8].copy_from_slice(&event_key.to_be_bytes());
    (lower, upper)
}

/// The [`EVENTS`] key of one relay observation.
pub(super) fn observation_key(event_key: EventKey, relay_key: RelayKey) -> [u8; 13] {
    let mut key = [0u8; 13];
    key[..8].copy_from_slice(&event_key.to_be_bytes());
    key[8] = EVENT_COL_OBSERVATION;
    key[9..].copy_from_slice(&relay_key.to_be_bytes());
    key
}

/// The inclusive bounds of one event's observation column.
pub(super) fn observation_bounds(event_key: EventKey) -> ([u8; 13], [u8; 13]) {
    (
        observation_key(event_key, RelayKey::MIN),
        observation_key(event_key, RelayKey::MAX),
    )
}

/// The relay surrogate of a validated [`observation_key`].
pub(super) fn observation_relay_key(key: &[u8]) -> RelayKey {
    RelayKey::from_be_bytes(
        key[9..13]
            .try_into()
            .expect("validated observation key is thirteen bytes"),
    )
}

/// The `tombstones` key for one (target id, claiming author) pair — see
/// [`TOMBSTONES`]'s doc for why this is composite, not just the id.
pub(super) fn id_tombstone_key(id: &EventId, author: &PublicKey) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 32 + 32);
    key.push(TOMBSTONE_ID);
    key.extend_from_slice(id.as_bytes());
    key.extend_from_slice(author.as_bytes());
    key
}

/// The `tombstones` key for one replaceable/addressable address, from
/// [`crate::address_key::AddressKey::to_redb_key`].
/// Encode one [`RELAYS`] row. The refcount leads so it can be read without
/// scanning the URL.
pub(super) fn encode_relay_row(refs: u64, url: &str) -> Vec<u8> {
    let mut value = Vec::with_capacity(8 + url.len());
    value.extend_from_slice(&refs.to_be_bytes());
    value.extend_from_slice(url.as_bytes());
    value
}

/// Decode one [`RELAYS`] row. Fallible rather than panicking: this runs
/// inside open write transactions and on the query read path, so a malformed
/// row must refuse the operation, not defeat a `debug_assert`.
pub(super) fn decode_relay_row(
    relay_key: RelayKey,
    value: &[u8],
) -> Result<(u64, &str), PersistenceError> {
    if value.len() < 8 {
        return Err(PersistenceError::invariant(format!(
            "interned relay {relay_key} row is {} bytes, expected at least 8",
            value.len()
        )));
    }
    let (refs, url) = value.split_at(8);
    let refs = u64::from_be_bytes(refs.try_into().expect("split_at yields eight bytes"));
    let url = std::str::from_utf8(url).map_err(|error| {
        PersistenceError::invariant(format!(
            "interned relay {relay_key} URL is not UTF-8: {error}"
        ))
    })?;
    Ok((refs, url))
}

pub(super) fn addr_tombstone_key(address: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + address.len());
    key.push(TOMBSTONE_ADDR);
    key.extend_from_slice(address.as_bytes());
    key
}
