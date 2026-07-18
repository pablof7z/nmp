use super::{EventId, PersistenceError, PublicKey, TableDefinition};

/// Wrap any `redb` operation error as a [`PersistenceError`] (architecture
/// review correction — see its doc). `accept_write`/`accept_ephemeral`/
/// `promote_signed`/`compensate_write`, and every table-touching helper
/// they call, propagate through this via `?`; the crate's OTHER,
/// pre-existing doors (`insert`/`remove`/`expire_due`/`gc`) still
/// `.expect()` these same `Result`s at their own call sites into the
/// shared helpers below — unchanged behavior for them, just funneled
/// through one typed error type instead of a bespoke panic message each.
pub(super) fn persist_err(e: impl std::fmt::Display) -> PersistenceError {
    PersistenceError(e.to_string())
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
pub(super) const LEGACY_EVENT_TABLES: [&str; 9] = [
    "events",
    "events_v2",
    "events_v3",
    "events_v4",
    "events_v5",
    "outbox_displaced_v2",
    "outbox_displaced_v3",
    "outbox_displaced_v4",
    "outbox_displaced_v5",
];
pub(super) const SCHEMA_META: TableDefinition<&str, u64> = TableDefinition::new("schema_meta_v6");
pub(super) const SCHEMA_VERSION_KEY: &str = "version";
pub(super) const LEGACY_SCHEMA_VERSION: u64 = 6;
pub(super) const PREVIOUS_SCHEMA_VERSION: u64 = 7;
pub(super) const SCHEMA_VERSION: u64 = 8;
/// Bound redb's process-private page cache for mobile/desktop clients.
///
/// redb 4.1 defaults this cache to 1 GiB. A million-event sequential ingest
/// fills most of that default even when NMP's transport queues and live query
/// projections remain bounded, so the database alone can consume more memory
/// than the rest of the process by an order of magnitude.
// Packed runs are large immutable values and the operating system already
// caches their pages. A larger Redb cache retains duplicate hot pages and
// makes the one-million-event working set scale with query traffic.
pub(super) const REDB_CACHE_BYTES: usize = 4 * 1024 * 1024;
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
pub(super) const BY_CREATED_AT: TableDefinition<&[u8; 40], EventKey> =
    TableDefinition::new("by_created_at_v6");
pub(super) const BY_AUTHOR: TableDefinition<&[u8; 72], EventKey> =
    TableDefinition::new("by_author_time_v6");
pub(super) const BY_KIND: TableDefinition<&[u8; 42], EventKey> =
    TableDefinition::new("by_kind_time_v6");
/// Removed from the v7 production query layout. Kept only so the v6 -> v7
/// migration can atomically delete the old table and benchmark-only historical
/// comparisons can name the retired physical shape.
pub(super) const LEGACY_BY_AUTHOR_KIND: TableDefinition<&[u8; 74], EventKey> =
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
pub(super) const BY_TAG: TableDefinition<&[u8], EventKey> = TableDefinition::new("by_tag_v6");
/// Immutable packed ordered-postings artifacts. These tables are dual-written
/// while the v7 row indexes remain query-authoritative; the v8 migration flips
/// authority only after exact differential validation.
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
    TableDefinition::new("index_cardinality_v1");
pub(super) const INDEX_CARDINALITY_META: TableDefinition<&str, u64> =
    TableDefinition::new("index_cardinality_meta_v1");
pub(super) const INDEX_CARDINALITY_VERSION_KEY: &str = "version";
pub(super) const INDEX_CARDINALITY_VERSION: u64 = 3;
pub(super) const INDEX_CARDINALITY_SAMPLE_META: TableDefinition<&str, &[u8]> =
    TableDefinition::new("index_cardinality_sample_meta_v1");
pub(super) const INDEX_CARDINALITY_SAMPLE_KEY: &str = "key";
/// The durable write-outbox journal (crashsafe-accepted-2-3-plan.md §2.2,
/// Fable checkpoint Q2 — APPROVED as co-resident in this same `Database`:
/// redb atomicity is a per-`Database` property, so the one crash-atomic
/// commit #3 requires forces the journal into the store's own transaction
/// boundary). Key: [`intent_key`] (zero-padded decimal `IntentId`, shared
/// verbatim with [`OUTBOX_DISPLACED`]). Value: JSON-encoded
/// `OutboxIntentRecord`. A row exists for exactly as long as its intent is
/// open — `compensate_write` deletes it on pre-signature termination; R8's
/// terminal-deletion-on-full-delivery is a later unit's job (this frame
/// never marks an intent all-lanes-terminal, since dispatch/ack tracking is
/// U3/U4).
pub(super) const OUTBOX_INTENTS: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_intents");
/// The predecessor each open intent displaced, if any (retraction doc
/// §4.2's durable stash). Key: the SAME [`intent_key`] as its
/// `OUTBOX_INTENTS` row. Value is a self-contained `NMPC` binary snapshot
/// (immutable event plus provenance), unlike canonical `EVENTS`'s event-only
/// `NMPE` value. See [`encode_stored_event`]/[`decode_stored_event`]. Deleted
/// durably by `promote_signed` (R6) or `compensate_write`; never by
/// `recover_outbox` (read-only).
pub(super) const OUTBOX_DISPLACED: TableDefinition<&str, &[u8]> =
    TableDefinition::new("outbox_displaced_v6");
/// Per-`(intent, relay, ordinal)` durable attempt evidence
/// (crashsafe-accepted-2-3-plan.md §5) — schema created here so the table
/// exists for the dispatch-time attempt writer to come (U3/U4: "Persist
/// `AttemptStarted` before dispatch"). This unit does not write rows into
/// it (Fable checkpoint R2: folding attempt eligibility into
/// `next_deadline` here is a busy-loop spin hazard — that fold ships with
/// the retry-owner follow-up, not this frame) and `recover_outbox` does not
/// read it — it is created purely for forward schema compatibility.
#[allow(dead_code)]
pub(super) const OUTBOX_ATTEMPTS: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_attempts");
/// Append-only exact resolved-route snapshots, keyed by `(intent, ordinal)`.
pub(super) const OUTBOX_ROUTE_REVISIONS: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_route_revisions");
pub(super) const OUTBOX_LANES: TableDefinition<&str, &str> = TableDefinition::new("outbox_lanes");
pub(super) const OUTBOX_DEADLINES: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_deadlines");
pub(super) const OUTBOX_DEADLINES_BY_INTENT: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_deadlines_by_intent");
pub(super) const OUTBOX_ATTEMPT_DETAILS: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_attempt_details");
/// Store-owned outbox metadata — two rows, `"next_intent_id"` and
/// `"next_receipt_id"`, each the next id of its kind to allocate (decimal
/// `u64`, defaulting to 1 if the row has never been written). Both are
/// bumped inside the SAME transaction as the `OUTBOX_INTENTS`/`EVENTS`
/// writes they accompany, so allocation and the intent/receipt it names
/// commit or roll back together (architecture review correction:
/// allocation of EITHER id is a durable fact the store itself owns — see
/// [`IntentId`]'s doc for the reuse hazard this closes; the identical
/// hazard applies to receipt ids once receipts are durably retained).
pub(super) const OUTBOX_META: TableDefinition<&str, &str> = TableDefinition::new("outbox_meta");
pub(super) const NEXT_INTENT_ID_KEY: &str = "next_intent_id";
pub(super) const NEXT_RECEIPT_ID_KEY: &str = "next_receipt_id";
pub(super) const PENDING_EPHEMERAL_RECEIPTS_KEY: &str = "pending_ephemeral_receipts";
/// Durably-RETAINED receipt records, keyed by `receipt_id` (zero-padded
/// decimal, mirroring [`intent_key`]'s convention) — independent of
/// `OUTBOX_INTENTS`'s open-work rows (architecture review correction: see
/// [`crate::ReceiptState`]'s doc for why this separation exists). Never
/// pruned by this unit.
pub(super) const OUTBOX_RECEIPTS: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_receipts");
/// #591: caller correlation token -> the receipt id it was journaled under,
/// stored as [`receipt_key`]'s SAME zero-padded decimal string (this table
/// is only ever point-looked-up by token, never range-scanned, so the
/// padding is not load-bearing here -- it is simply reusing the existing
/// key format rather than introducing a second one). Written inside the
/// SAME transaction as the
/// `OUTBOX_RECEIPTS` row it names (see `accept_write`'s doc), retained
/// forever like `OUTBOX_RECEIPTS` -- there is no removal door.
pub(super) const OUTBOX_CORRELATIONS: TableDefinition<&str, &str> =
    TableDefinition::new("outbox_correlations");

/// The `tombstones` table's key for one (target id, claiming author) pair —
/// see [`TOMBSTONES`]'s doc for why this is composite, not just the id.
pub(super) fn id_tombstone_key(id: &EventId, author: &PublicKey) -> String {
    format!("{}:{}", id.to_hex(), author.to_hex())
}
