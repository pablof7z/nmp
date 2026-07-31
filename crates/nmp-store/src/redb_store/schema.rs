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

/// Breaking v6 event schema. Compatibility is intentionally not carried:
/// immutable notes, local state, interned relay observations, raw-id lookup,
/// and compact primary keys are independent tables from the first byte.
pub(super) const EVENTS: TableDefinition<EventKey, &[u8]> = TableDefinition::new("events_v6");
pub(super) const EVENT_IDS: TableDefinition<&[u8; 32], EventKey> =
    TableDefinition::new("event_ids_v6");
pub(super) const EVENT_LOCAL: TableDefinition<EventKey, &[u8]> =
    TableDefinition::new("event_local_v6");
pub(super) const EVENT_STORE_META: TableDefinition<&str, EventKey> =
    TableDefinition::new("event_store_meta_v6");
pub(super) const NEXT_EVENT_KEY: &str = "next_event_key";
pub(super) const RELAYS: TableDefinition<RelayKey, &str> = TableDefinition::new("relays_v6");
pub(super) const RELAY_KEYS: TableDefinition<&str, RelayKey> =
    TableDefinition::new("relay_keys_v6");
pub(super) const RELAY_REFS: TableDefinition<RelayKey, u64> = TableDefinition::new("relay_refs_v6");
pub(super) const RELAY_META: TableDefinition<&str, RelayKey> =
    TableDefinition::new("relay_meta_v6");
pub(super) const NEXT_RELAY_KEY: &str = "next_relay_key";
/// Fixed-width key: `event_key:u64-be | relay_key:u32-be`; value is the
/// greatest observation timestamp in seconds.
pub(super) const EVENT_OBSERVATIONS: TableDefinition<&[u8; 12], u64> =
    TableDefinition::new("event_observations_v6");
pub(super) const SCHEMA_META: TableDefinition<&str, u64> = TableDefinition::new("schema_meta_v6");
pub(super) const SCHEMA_VERSION_KEY: &str = "version";
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
pub(super) const SCHEMA_VERSION: u64 = 12;
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
pub(super) const ADDR_INDEX: TableDefinition<&str, EventKey> =
    TableDefinition::new("addr_index_v6");
pub(super) const COVERAGE: TableDefinition<&str, &str> = TableDefinition::new("coverage");
/// Permanent kind:5 tombstones for individual event ids
/// (retraction-and-negative-deltas.md §2/§7). Key: `"{id_hex}:{author_hex}"`
/// -- one row PER CLAIMING AUTHOR, never collapsed to one row per id: the
/// target's real author is unknown until it actually arrives, so an
/// unauthorized third party can always name an id someone else has already
/// (or will later) legitimately delete. A single overwritable row per id
/// would let that unauthorized claim silently replace -- and so undo -- the
/// real author's permanent, authorized deletion. Value: the deleting
/// kind:5's own id hex (diagnostics only; the key alone decides refusal).
/// Never GC-claimed.
pub(super) const TOMBSTONES: TableDefinition<&str, &str> = TableDefinition::new("tombstones");
/// Permanent kind:5 tombstones for replaceable/addressable addresses. Key:
/// [`crate::address_key::AddressKey::to_redb_key`]. Value carries the
/// deletion ceiling (highest deleting-event `created_at` seen for that
/// address) — a candidate with `created_at <= ceiling` is tombstoned.
pub(super) const ADDR_TOMBSTONES: TableDefinition<&str, &str> =
    TableDefinition::new("addr_tombstones");
/// The persistent NIP-40 expiration index (retraction-and-negative-
/// deltas.md §3.1). Key: `expires_at:u64-be | event_id:[u8;32]`, so ordinary
/// byte ordering matches numeric deadline order without decimal/hex work;
/// value: the canonical event's compact surrogate key.
pub(super) const EXPIRATION_INDEX: TableDefinition<&[u8; 40], EventKey> =
    TableDefinition::new("expiration_index_v6");
/// Binary ordered indexes all end in the same sortable suffix:
/// `created_at:u64-be | !event_id:[u8;32]`. Reverse scans therefore yield
/// `created_at DESC, event_id ASC` and can stop exactly at the visible limit.
///
/// Comparison-only: packed postings own the current query layout, so
/// [`crate::RedbStore`] never creates or reads these row indexes. They survive
/// solely so benchmark variants can measure the alternative physical shape.
#[cfg(feature = "bench-instrumentation")]
pub(super) const BY_CREATED_AT: TableDefinition<&[u8; 40], EventKey> =
    TableDefinition::new("by_created_at_v6");
#[cfg(feature = "bench-instrumentation")]
pub(super) const BY_AUTHOR: TableDefinition<&[u8; 72], EventKey> =
    TableDefinition::new("by_author_time_v6");
#[cfg(feature = "bench-instrumentation")]
pub(super) const BY_KIND: TableDefinition<&[u8; 42], EventKey> =
    TableDefinition::new("by_kind_time_v6");
/// Comparison-only historical index shape used by benchmark variants; never
/// opened by [`crate::RedbStore`] and not part of the current schema epoch.
#[cfg(feature = "bench-instrumentation")]
pub(super) const COMPARISON_BY_AUTHOR_KIND: TableDefinition<&[u8; 74], EventKey> =
    TableDefinition::new("by_author_kind_time_v6");
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
pub(super) const BY_TAG: TableDefinition<&[u8], EventKey> = TableDefinition::new("by_tag_v6");
/// Immutable packed ordered-postings artifacts. Packed postings are the
/// current query-authoritative representation.
pub(super) const POSTINGS_SEGMENTS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("postings_segments_v8");
pub(super) const POSTINGS_DICTIONARIES: TableDefinition<u64, &[u8]> =
    TableDefinition::new("postings_dictionaries_v8");
pub(super) const POSTINGS_RUN_META: TableDefinition<u64, &[u8]> =
    TableDefinition::new("postings_run_meta_v8");
pub(super) const POSTINGS_RUN_BY_MIN: TableDefinition<u64, u64> =
    TableDefinition::new("postings_run_by_min_v8");
pub(super) const POSTINGS_DEAD_KEYS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("postings_dead_keys_v8");
pub(super) const POSTINGS_META: TableDefinition<&str, u64> =
    TableDefinition::new("postings_meta_v8");
pub(super) const POSTINGS_NEXT_RUN_ID: &str = "next_run_id";
pub(super) const POSTINGS_READY: &str = "query_ready";
/// Uniform sampled live-row counts for every ordered-index prefix. Keys are
/// namespaced binary prefixes (global, author, kind, or tag/value); values
/// count sampled physical rows in that bucket. Sampling is
/// sufficient for choosing an index and avoids one durable row for nearly
/// every unique author/tag while never changing query correctness.
pub(super) const INDEX_CARDINALITY: TableDefinition<&[u8], u64> =
    TableDefinition::new("index_cardinality");
pub(super) const INDEX_CARDINALITY_META: TableDefinition<&str, u64> =
    TableDefinition::new("index_cardinality_meta");
pub(super) const INDEX_CARDINALITY_VERSION_KEY: &str = "version";
pub(super) const INDEX_CARDINALITY_VERSION: u64 = 3;
pub(super) const INDEX_CARDINALITY_SAMPLE_META: TableDefinition<&str, &[u8]> =
    TableDefinition::new("index_cardinality_sample_meta");
pub(super) const INDEX_CARDINALITY_SAMPLE_KEY: &str = "key";
/// Fresh durable-delivery v1 namespace (#1027). The key widths are the
/// semantic contract, not redb's numeric layout:
///
/// - intent/receipt: `u64-be`;
/// - relay surrogate: `u32-be`;
/// - lane: `intent:u64-be | relay:u32-be`;
/// - attempt: lane key plus `ordinal:u64-be`;
/// - route revision: `intent:u64-be | ordinal:u64-be`;
/// - deadline: `time:u64-be | intent:u64-be | relay:u32-be`.
///
/// Values use the explicit codec in `delivery_codec.rs`. No previous
/// execution table is opened, read, transformed, or deleted.
pub(super) const DELIVERY_INTENTS: TableDefinition<&[u8; 8], &[u8]> =
    TableDefinition::new("delivery_intents_v1");
pub(super) const DELIVERY_DISPLACED: TableDefinition<&[u8; 8], &[u8]> =
    TableDefinition::new("delivery_displaced_v1");
pub(super) const DELIVERY_ATTEMPTS: TableDefinition<&[u8; 20], &[u8]> =
    TableDefinition::new("delivery_attempts_v1");
pub(super) const DELIVERY_ROUTE_REVISIONS: TableDefinition<&[u8; 16], &[u8]> =
    TableDefinition::new("delivery_route_revisions_v1");
pub(super) const DELIVERY_LANES: TableDefinition<&[u8; 12], &[u8]> =
    TableDefinition::new("delivery_lanes_v1");
pub(super) const DELIVERY_DEADLINES: TableDefinition<&[u8; 20], &[u8]> =
    TableDefinition::new("delivery_deadlines_v1");
pub(super) const DELIVERY_DEADLINES_BY_INTENT: TableDefinition<&[u8; 20], &[u8]> =
    TableDefinition::new("delivery_deadlines_by_intent_v1");
pub(super) const DELIVERY_ATTEMPT_DETAILS: TableDefinition<&[u8; 20], &[u8]> =
    TableDefinition::new("delivery_attempt_details_v1");
pub(super) const DELIVERY_META: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("delivery_meta_v1");
pub(super) const DELIVERY_RECEIPTS: TableDefinition<&[u8; 8], &[u8]> =
    TableDefinition::new("delivery_receipts_v1");
pub(super) const DELIVERY_CORRELATIONS: TableDefinition<&[u8], &[u8; 8]> =
    TableDefinition::new("delivery_correlations_v1");
pub(super) const DELIVERY_RELAYS: TableDefinition<&[u8; 4], &[u8]> =
    TableDefinition::new("delivery_relays_v1");
pub(super) const DELIVERY_RELAY_IDS: TableDefinition<&[u8], &[u8; 4]> =
    TableDefinition::new("delivery_relay_ids_v1");
pub(super) const DELIVERY_KIND5_CLAIMS: TableDefinition<&[u8; 8], &[u8]> =
    TableDefinition::new("delivery_kind5_claims_v1");
pub(super) const DELIVERY_SUPPRESS_BY_ID: TableDefinition<&[u8; 64], &[u8]> =
    TableDefinition::new("delivery_suppress_by_id_v1");
pub(super) const DELIVERY_SUPPRESS_BY_ADDR: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("delivery_suppress_by_addr_v1");

/// The `tombstones` table's key for one (target id, claiming author) pair —
/// see [`TOMBSTONES`]'s doc for why this is composite, not just the id.
pub(super) fn id_tombstone_key(id: &EventId, author: &PublicKey) -> String {
    format!("{}:{}", id.to_hex(), author.to_hex())
}
