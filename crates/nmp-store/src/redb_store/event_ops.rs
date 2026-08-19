use nmp_grammar::RelaySessionKey;
use super::commit::commit_prepared;
use super::ingest::insert_with_tables;
use super::ingest_txn::GovernedWrite;
use super::mutation::remove_row_in_txn;
use super::publish_queue::is_suppressed_in_txn;
use super::query::{expiration_key_timestamp, expiration_key_upper_bound, plan_ordered_query};
use super::schema::PUBLISH_QUEUE_SUPPRESS;
use super::schema::{
    event_local_key, event_row_key, persist_err, EventKey, COVERAGE, EVENTS, EVENT_COL_LOCAL,
    EVENT_COL_ROW, EVENT_IDS, EXPIRATION_INDEX, RELAYS,
};
#[cfg(test)]
use super::store::RedbCrashPoint;
use super::store::RedbStore;
use super::{
    address_key_for, binary_event, compute_coverage_key, merge_interval, shrink_after_eviction,
    BTreeMap, BTreeSet, ContextualAtom, CoverageInterval, CoverageKey, Event, EventCursor, EventId,
    Filter, GcReport, GcRetentionSet, GcVictimIndex, HashMap, IndexedMatch, InsertOutcome,
    LocalOrigin, PersistenceError, PreparedFilter, RelayObserved, RelayUrl, RetractReason,
    SigState, StoredEvent, StoredEventView, Timestamp,
};
use nostr::secp256k1::schnorr::Signature;
use redb::{Database, ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};
#[cfg(any(
    test,
    feature = "bench-instrumentation",
    feature = "test-instrumentation"
))]
use std::sync::atomic::Ordering;

/// The `coverage` table's JSON value: the proven interval and nothing else,
/// stored as raw `u64` seconds (round-tripped through
/// `Timestamp::from`/`as_secs`).
///
/// **No filter is ever stored in the database** (#1849). A row used to
/// carry the window-erased shape it was recorded against — authors, ids,
/// tags and kinds, in full — so `gc` could ask "would this row have matched
/// the event I am evicting". That made the store a durable record of every
/// distinct query a user had ever issued, with no expiry and no delete
/// door, to buy precision `gc` does not need: see [`GcVictimIndex`] for why
/// shrinking on interval overlap alone is the sound rule.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CoverageRowRecord {
    pub(super) from: u64,
    pub(super) through: u64,
}

/// Decode one `COVERAGE` row into its proven interval. Fallible (#790):
/// `record_coverage`/`gc` merge and shrink against this value inside an
/// open write transaction, so a malformed row must refuse the whole
/// mutation rather than silently merge against a defaulted window.
pub(super) fn decode_interval(json: &str) -> Result<CoverageInterval, PersistenceError> {
    let record: CoverageRowRecord = decode_coverage_row(json)?;
    Ok(CoverageInterval::new(
        Timestamp::from(record.from),
        Timestamp::from(record.through),
    ))
}

pub(super) fn decode_coverage_row(json: &str) -> Result<CoverageRowRecord, PersistenceError> {
    serde_json::from_str(json)
        .map_err(|error| PersistenceError::new(format!("decode coverage row: {error}")))
}

pub(super) fn insert(
    store: &mut RedbStore,
    event: Event,
    from: RelayObserved,
) -> Result<InsertOutcome, PersistenceError> {
    let mut write = GovernedWrite::begin(store)?;
    let outcome = write.apply(|tables, _write_txn| insert_with_tables(tables, event, from))?;
    #[cfg(any(test, feature = "test-instrumentation"))]
    if std::mem::take(&mut store.fail_next_observation_before_commit) {
        drop(write);
        return Err(PersistenceError::new(
            "injected observation failed before commit",
        ));
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::ObservationBeforeCommit);
    let outcome = write.commit_prepared(outcome)?;
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::ObservationAfterCommit);
    Ok(outcome)
}

pub(super) fn insert_batch(
    store: &mut RedbStore,
    events: Vec<(Event, RelayObserved)>,
) -> Result<Vec<InsertOutcome>, PersistenceError> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    #[cfg(feature = "bench-instrumentation")]
    let transaction_started = std::time::Instant::now();
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::record_batch(events.len());
    #[cfg(feature = "bench-instrumentation")]
    let begin_started = std::time::Instant::now();
    let mut write = GovernedWrite::begin(store)?;
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::begin_write(begin_started.elapsed());
    let mut outcomes = Vec::with_capacity(events.len());
    write.apply(|tables, _write_txn| {
        #[cfg(feature = "bench-instrumentation")]
        let apply_started = std::time::Instant::now();
        for (event, from) in events {
            outcomes.push(insert_with_tables(tables, event, from)?);
        }
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::apply_events(apply_started.elapsed());
        Ok(())
    })?;
    #[cfg(any(test, feature = "test-instrumentation"))]
    if std::mem::take(&mut store.fail_next_observation_before_commit) {
        drop(write);
        return Err(PersistenceError::new(
            "injected observation failed before commit",
        ));
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::ObservationBeforeCommit);
    let outcomes = write.commit_prepared(outcomes)?;
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::ObservationAfterCommit);
    #[cfg(feature = "bench-instrumentation")]
    {
        crate::ingest_attribution::transaction_total(transaction_started.elapsed());
    }
    Ok(outcomes)
}

/// Durable dedup-by-id for the verify gate (#1677): the known-good
/// signature for an already-ingested event id, if any. This is a narrow
/// point read — it decodes only the signature column off the canonical
/// row, not the whole event. A pending local draft (sentinel signature)
/// is NOT known-good and returns `None`, so a relay delivering the real
/// signed version of a still-pending draft is still admitted through to
/// schnorr rather than falsely rejected as a signature mismatch.
pub(super) fn known_signature(
    store: &RedbStore,
    id: &EventId,
) -> Result<Option<Signature>, PersistenceError> {
    known_signature_from_db(store.database()?, id)
}

/// Shared-handle variant: the [`StoreSigReader`] holds its own
/// `Arc<Database>` cut from the store, so it reads durable signatures
/// without borrowing the engine's `RedbStore` and without blocking the
/// writer (redb is MVCC).
pub(super) fn known_signature_from_db(
    db: &Database,
    id: &EventId,
) -> Result<Option<Signature>, PersistenceError> {
    let read_txn = db.begin_read().map_err(persist_err)?;
    let event_ids = read_txn.open_table(EVENT_IDS).map_err(persist_err)?;
    let Some(event_key) = event_ids
        .get(id.as_bytes())
        .map_err(persist_err)?
        .map(|guard| guard.value())
    else {
        return Ok(None);
    };
    let events = read_txn.open_table(EVENTS).map_err(persist_err)?;
    let Some(value) = events
        .get(event_row_key(event_key).as_slice())
        .map_err(persist_err)?
    else {
        // An id map naming a canonical row that is gone is corruption, but
        // for the verify gate a missing row is simply "not known" — the
        // candidate falls through to schnorr. The invariant is caught by
        // other read paths; this door stays non-fatal.
        return Ok(None);
    };
    let view = StoredEventView::from_trusted(value.value()).map_err(|error| {
        PersistenceError::new(format!(
            "decode canonical event view for known_signature {event_key}: {error:?}"
        ))
    })?;
    let sig = Signature::from_slice(view.signature_bytes())
        .expect("decoded canonical row carries a structurally valid 64-byte signature");
    if sig == crate::sentinel_signature() {
        return Ok(None);
    }
    Ok(Some(sig))
}

pub(super) fn query(
    store: &RedbStore,
    filter: &Filter,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    if filter
        .since
        .zip(filter.until)
        .is_some_and(|(since, until)| since > until)
        || filter.generic_tags.values().any(BTreeSet::is_empty)
    {
        return Ok(Vec::new());
    }
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    // Fast path: exact ids resolve through the raw-id -> surrogate-key
    // table, bounded by `|ids|` regardless of table size (issue #17).
    if let Some(ids) = filter.ids.as_ref().filter(|ids| !ids.is_empty()) {
        let events = read_txn.open_table(EVENTS).map_err(persist_err)?;
        let event_ids = read_txn.open_table(EVENT_IDS).map_err(persist_err)?;
        let relays = read_txn.open_table(RELAYS).map_err(persist_err)?;
        let mut relay_cache = HashMap::new();
        let publish_queue_suppress = read_txn
            .open_table(PUBLISH_QUEUE_SUPPRESS)
            .map_err(persist_err)?;
        let prepared_filter = PreparedFilter::new(filter);
        let mut out = Vec::new();
        for id in ids {
            let Some(event_key) = event_ids
                .get(id.as_bytes())
                .map_err(persist_err)?
                .map(|guard| guard.value())
            else {
                continue;
            };
            // An id map naming a canonical row that is gone is corruption,
            // not a miss: answering `continue` here would report the event
            // as absent from a store that still claims to hold it.
            let value = events
                .get(event_row_key(event_key).as_slice())
                .map_err(persist_err)?
                .ok_or_else(|| {
                    PersistenceError::new(format!(
                        "raw id map points at missing canonical event {event_key}"
                    ))
                })?;
            let view = StoredEventView::from_trusted(value.value()).map_err(|error| {
                PersistenceError::new(format!(
                    "decode canonical event view {event_key}: {error:?}"
                ))
            })?;
            let matches = view
                .matches_prepared_filter_after_index(&prepared_filter, IndexedMatch::None)
                .map_err(|error| {
                    PersistenceError::new(format!(
                        "match canonical event against filter {event_key}: {error:?}"
                    ))
                })?;
            if !matches {
                continue;
            }
            let local_value = events
                .get(event_local_key(event_key).as_slice())
                .map_err(persist_err)?;
            let se = store.decode_row(
                event_key,
                view,
                local_value.as_ref().map(|value| value.value()),
                &events,
                &relays,
                &mut relay_cache,
            )?;
            if !is_suppressed_in_txn(&publish_queue_suppress, &se.event)? {
                out.push(se);
            }
        }
        return Ok(out);
    }

    let plan = plan_ordered_query(filter);
    store.query_ordered(&read_txn, &plan, filter, None, None, None)
}

pub(super) fn query_newest(
    store: &RedbStore,
    filter: &Filter,
    limit: usize,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    if limit == 0
        || filter
            .since
            .zip(filter.until)
            .is_some_and(|(since, until)| since > until)
        || filter.generic_tags.values().any(BTreeSet::is_empty)
    {
        return Ok(Vec::new());
    }
    // Exact ids are already the narrowest possible lookup. They do not
    // form a time-ordered range, so preserve correctness by sorting this
    // caller-bounded set only; no unrelated row is touched.
    if filter.ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
        let mut rows = store.query(filter)?;
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows.truncate(limit);
        return Ok(rows);
    }

    let read_txn = store.database()?.begin_read().map_err(persist_err)?;

    let plan = plan_ordered_query(filter);
    store.query_ordered(&read_txn, &plan, filter, None, Some(limit), None)
}

pub(super) fn query_newest_ids(
    store: &RedbStore,
    filter: &Filter,
    limit: usize,
) -> Result<Vec<EventId>, PersistenceError> {
    if limit == 0
        || filter
            .since
            .zip(filter.until)
            .is_some_and(|(since, until)| since > until)
        || filter.generic_tags.values().any(BTreeSet::is_empty)
    {
        return Ok(Vec::new());
    }
    if filter.ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
        return Ok(store
            .query_newest(filter, limit)?
            .into_iter()
            .map(|row| row.event.id)
            .collect());
    }

    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let plan = plan_ordered_query(filter);
    store.query_ordered_ids(&read_txn, &plan, filter, limit)
}

pub(super) fn query_newest_under_pin(
    store: &RedbStore,
    filter: &Filter,
    pinned: &BTreeSet<RelayUrl>,
    limit: usize,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    if limit == 0
        || filter
            .since
            .zip(filter.until)
            .is_some_and(|(since, until)| since > until)
        || filter.generic_tags.values().any(BTreeSet::is_empty)
    {
        return Ok(Vec::new());
    }
    if filter.ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
        let mut rows = store.query(filter)?;
        rows.retain(|row| row.provenance.visible_under_pin(pinned));
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows.truncate(limit);
        return Ok(rows);
    }

    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let plan = plan_ordered_query(filter);
    store.query_ordered(&read_txn, &plan, filter, None, Some(limit), Some(pinned))
}

pub(super) fn query_newest_before(
    store: &RedbStore,
    filter: &Filter,
    before: EventCursor,
    limit: usize,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    if limit == 0
        || filter
            .since
            .zip(filter.until)
            .is_some_and(|(since, until)| since > until)
        || filter.generic_tags.values().any(BTreeSet::is_empty)
    {
        return Ok(Vec::new());
    }
    #[cfg(any(test, feature = "test-instrumentation"))]
    if store.take_query_newest_before_failure() {
        return Err(PersistenceError::new(
            "injected query-newest-before failure",
        ));
    }
    // Exact ids are already a caller-bounded lookup rather than an
    // ordered index range. Preserve that narrow path, then apply the
    // same exact exclusive cursor predicate as the RedbStore contract.
    if filter.ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
        let mut rows = store.query(filter)?;
        rows.retain(|row| {
            row.event.created_at < before.created_at
                || (row.event.created_at == before.created_at && row.event.id > before.event_id)
        });
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows.truncate(limit);
        return Ok(rows);
    }

    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let plan = plan_ordered_query(filter);
    store.query_ordered(&read_txn, &plan, filter, Some(before), Some(limit), None)
}

pub(super) fn query_newest_before_under_pin(
    store: &RedbStore,
    filter: &Filter,
    pinned: &BTreeSet<RelayUrl>,
    before: EventCursor,
    limit: usize,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    if limit == 0
        || filter
            .since
            .zip(filter.until)
            .is_some_and(|(since, until)| since > until)
        || filter.generic_tags.values().any(BTreeSet::is_empty)
    {
        return Ok(Vec::new());
    }
    if filter.ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
        let mut rows = store.query(filter)?;
        rows.retain(|row| {
            (row.event.created_at < before.created_at
                || (row.event.created_at == before.created_at && row.event.id > before.event_id))
                && row.provenance.visible_under_pin(pinned)
        });
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows.truncate(limit);
        return Ok(rows);
    }

    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let plan = plan_ordered_query(filter);
    store.query_ordered(
        &read_txn,
        &plan,
        filter,
        Some(before),
        Some(limit),
        Some(pinned),
    )
}

pub(super) fn query_newest_before_any(
    store: &RedbStore,
    filters: &[Filter],
    before: EventCursor,
    limit: usize,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    if limit == 0 || filters.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_id = BTreeMap::new();
    for filter in filters {
        // The first `limit` rows of the global union can contain no row
        // ranked below `limit` inside every component that matches it.
        // Therefore each component scan stays caller-bounded while this
        // one logical door performs the exact de-duplicated merge.
        for row in store.query_newest_before(filter, before, limit)? {
            by_id.entry(row.event.id).or_insert(row);
        }
    }
    let mut rows: Vec<_> = by_id.into_values().collect();
    rows.sort_by(|a, b| {
        b.event
            .created_at
            .cmp(&a.event.created_at)
            .then_with(|| a.event.id.cmp(&b.event.id))
    });
    rows.truncate(limit);
    Ok(rows)
}

pub(super) fn query_newest_before_any_under_pin(
    store: &RedbStore,
    filters: &[Filter],
    pinned: &BTreeSet<RelayUrl>,
    before: EventCursor,
    limit: usize,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    if limit == 0 || filters.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_id = BTreeMap::new();
    for filter in filters {
        for row in store.query_newest_before_under_pin(filter, pinned, before, limit)? {
            by_id.entry(row.event.id).or_insert(row);
        }
    }
    let mut rows: Vec<_> = by_id.into_values().collect();
    rows.sort_by(|a, b| {
        b.event
            .created_at
            .cmp(&a.event.created_at)
            .then_with(|| a.event.id.cmp(&b.event.id))
    });
    rows.truncate(limit);
    Ok(rows)
}

pub(super) fn remove(
    store: &mut RedbStore,
    id: EventId,
    _reason: RetractReason,
) -> Result<Option<StoredEvent>, PersistenceError> {
    let mut write = GovernedWrite::begin(store)?;
    let removed = write.apply(|txn, _write_txn| remove_row_in_txn(txn, id, |_| true))?;
    write.commit_prepared(removed)
}

pub(super) fn expire_due(
    store: &mut RedbStore,
    now: Timestamp,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    let mut write = GovernedWrite::begin(store)?;
    let removed = write.apply(|txn, _write_txn| {
        let upper = expiration_key_upper_bound(now);
        // Collect due ids first, propagating any redb read error out of
        // the iterator (a plain `for` accumulate rather than a `.map()`
        // closure so `?` reaches this fn, not the closure).
        let mut due_keys: Vec<EventKey> = Vec::new();
        for entry in txn
            .expiration_index
            .range::<&[u8; 40]>(..=&upper)
            .map_err(persist_err)?
        {
            let (_key, value) = entry.map_err(persist_err)?;
            due_keys.push(value.value());
        }

        let mut removed = Vec::new();
        for event_key in due_keys {
            let Some(stored) = txn.canonical.load_by_key(event_key)? else {
                continue;
            };
            if let Some(row) = remove_row_in_txn(txn, stored.event.id, |_| true)? {
                removed.push(row);
            }
        }
        Ok(removed)
    })?;
    write.commit_prepared(removed)
}

/// Peek the minimum of the expiration index.
///
/// All three backend steps below are ordinary reads, and every one of them
/// can fail for a reason outside this crate's control — the handle latched
/// by an earlier I/O failure, a full or disconnected disk, a poisoned lock,
/// on-disk corruption. `persist_err` is what tells those apart from this
/// crate misusing its own database, and both
/// now leave the host process alive: before #763 every one of them was an
/// `.expect()`, so a read error aborted the embedding iOS/Android app.
///
/// The key decode is the one genuine invariant here, and it is expressed as
/// a total operation rather than an `.expect()`: the index key type is a
/// fixed 40-byte array whose first eight bytes are the big-endian seconds
/// [`expiration_key`] wrote, so the irrefutable array pattern in
/// [`expiration_key_timestamp`] proves the width at compile time and leaves
/// no panic branch to reach.
pub(super) fn next_expiration(store: &RedbStore) -> Result<Option<Timestamp>, PersistenceError> {
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let expiration_index = read_txn.open_table(EXPIRATION_INDEX).map_err(persist_err)?;
    let Some((key, _value)) = expiration_index.first().map_err(persist_err)? else {
        return Ok(None);
    };
    Ok(Some(expiration_key_timestamp(key.value())))
}

pub(super) fn record_coverage(
    store: &mut RedbStore,
    claims: &[(ContextualAtom, RelaySessionKey, CoverageInterval)],
) -> Result<(), PersistenceError> {
    if claims.is_empty() {
        return Ok(());
    }
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    {
        let mut coverage = write_txn.open_table(COVERAGE).map_err(persist_err)?;
        for (atom, session, proven) in claims {
            let key = compute_coverage_key(atom);
            let row_key = RedbStore::coverage_row_key(&key, session);
            let existing = coverage
                .get(row_key.as_str())
                .map_err(persist_err)?
                .map(|guard| decode_interval(guard.value()))
                .transpose()?;

            let merged = merge_interval(existing, *proven);
            let record = CoverageRowRecord {
                from: merged.from.as_secs(),
                through: merged.through.as_secs(),
            };
            let encoded = serde_json::to_string(&record).expect("redb: encode coverage row");
            coverage
                .insert(row_key.as_str(), encoded.as_str())
                .map_err(persist_err)?;
        }
    }
    #[cfg(any(test, feature = "test-instrumentation"))]
    if store
        .fail_next_coverage_write
        .as_ref()
        .is_some_and(|(target_key, target_relay)| {
            claims.iter().any(|(atom, session, _)| {
                compute_coverage_key(atom) == *target_key && session.relay == *target_relay
            })
        })
    {
        store.fail_next_coverage_write = None;
        return Err(PersistenceError::new(
            "injected coverage write failure",
        ));
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::CoverageBeforeCommit);
    commit_prepared(write_txn, ())?;
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::CoverageAfterCommit);
    Ok(())
}

/// Read one coverage row.
///
/// Every step is fallible for the same reasons [`next_expiration`] is, and
/// the decode is the reason the door had to widen rather than answer `None`
/// on failure: a corrupt watermark answered as `None` reads as "no coverage
/// proven", which is a refetch decision made on a false cache miss. A decode
/// failure (raised inside [`decode_interval`]) and an environmental
/// transaction/table/row failure are both `Err` here, and neither is ever
/// allowed to render as an absent row.
pub(super) fn get_coverage(
    store: &RedbStore,
    key: CoverageKey,
    session: &RelaySessionKey,
) -> Result<Option<CoverageInterval>, PersistenceError> {
    #[cfg(any(
        test,
        feature = "bench-instrumentation",
        feature = "test-instrumentation"
    ))]
    store.coverage_reads.fetch_add(1, Ordering::Relaxed);
    let row_key = RedbStore::coverage_row_key(&key, session);
    let read_txn = store.database()?.begin_read().map_err(persist_err)?;
    let coverage = read_txn.open_table(COVERAGE).map_err(persist_err)?;
    coverage
        .get(row_key.as_str())
        .map_err(persist_err)?
        .map(|guard| decode_interval(guard.value()))
        .transpose()
}

pub(super) fn gc(
    store: &mut RedbStore,
    claims: &GcRetentionSet,
) -> Result<GcReport, PersistenceError> {
    let mut report = GcReport::default();

    let mut write = GovernedWrite::begin(store)?;
    write.apply(|txn, write_txn| {
        let mut coverage = write_txn.open_table(COVERAGE).map_err(persist_err)?;

        // Pass 1: find victims (regular events matched by no claim, and
        // not an open — unsigned — local intent: Fable checkpoint R5,
        // mirrors `RedbStore::gc`'s exclusion exactly). A row
        // currently hidden by a still-open kind:5 suppression claim is
        // pinned the same way (architecture review requirement — GC
        // must never evict a target a pending cancel/promote can still
        // act on; NIP-40 expiry may still remove it separately).
        // Collected up front into owned values so the removal pass
        // below never holds a borrow across a mutation.
        let mut victims: Vec<Event> = Vec::new();
        // The canonical note rows are one column of the event key space, and
        // the event key LEADS the key, so no column is contiguous across
        // events: this scan visits the local and observation sidecars too.
        // What it exploits instead is that every column of ONE event is
        // adjacent and ordered `row, local, observations…`, so a note row and
        // its own local sidecar arrive back to back. One row of lookahead is
        // therefore enough — no map keyed by event, no second pass, no
        // per-event point lookup. `gc` was already a full scan; the single
        // range delete `remove_by_key` gained is the other half of this trade
        // (#1248).
        let mut pending: Option<(EventKey, Event)> = None;
        let mut consider =
            |event: Event, local: Option<LocalOrigin>| -> Result<(), PersistenceError> {
                if address_key_for(&event).is_none()
                    && !matches!(
                        local,
                        Some(LocalOrigin {
                            sig_state: SigState::Pending,
                            ..
                        })
                    )
                    && !is_suppressed_in_txn(&txn.publish_queue_suppress, &event)?
                    && !claims.is_claimed(&event)
                {
                    victims.push(event);
                }
                Ok(())
            };
        for entry in txn.canonical.scan()? {
            let (key, value) = entry.map_err(persist_err)?;
            let key = key.value();
            let event_key = EventKey::from_be_bytes(
                key[..8]
                    .try_into()
                    .expect("every canonical key leads with an event key"),
            );
            match key[8] {
                EVENT_COL_ROW => {
                    if let Some((_, event)) = pending.take() {
                        consider(event, None)?;
                    }
                    let event = StoredEventView::from_trusted(value.value())
                        .map_err(|error| {
                            PersistenceError::new(format!(
                                "decode canonical event view {event_key}: {error:?}"
                            ))
                        })?
                        .materialize_event()
                        .map_err(|error| {
                            PersistenceError::new(format!(
                                "materialize canonical event {event_key}: {error:?}"
                            ))
                        })?;
                    pending = Some((event_key, event));
                }
                EVENT_COL_LOCAL => {
                    // A local sidecar with no note row before it is a broken
                    // relational invariant, not an event without local state.
                    let Some((pending_key, event)) = pending.take() else {
                        return Err(PersistenceError::new(format!(
                            "canonical local state {event_key} has no event row"
                        )));
                    };
                    if pending_key != event_key {
                        return Err(PersistenceError::new(format!(
                            "canonical local state {event_key} follows event {pending_key}"
                        )));
                    }
                    let local = binary_event::decode_local(value.value()).map_err(|error| {
                        PersistenceError::new(format!(
                            "decode canonical local state {event_key}: {error:?}"
                        ))
                    })?;
                    consider(event, Some(local))?;
                }
                _ => {
                    if let Some((_, event)) = pending.take() {
                        consider(event, None)?;
                    }
                }
            }
        }
        if let Some((_, event)) = pending.take() {
            consider(event, None)?;
        }

        for event in &victims {
            remove_row_in_txn(txn, event.id, |_| true)?.ok_or_else(|| {
                PersistenceError::new(format!(
                    "gc victim {} vanished before its own removal",
                    event.id
                ))
            })?;
            report.events_evicted += 1;
        }

        // Pass 2 (issue #507): a SINGLE pass over coverage rows,
        // using `GcVictimIndex` (see its doc comment for the proof) to
        // find each row's maximum in-range victim timestamp directly,
        // instead of re-walking the full victim list per row. Same
        // write transaction as the event removals above — the
        // shrink/delete and the event delete commit atomically
        // together (ruling §5: never leave a watermark claiming
        // coverage of evicted data). `coverage_rows_shrunk`/
        // `coverage_rows_deleted` stay per-ROW, unchanged from
        // before (this was already `RedbStore`'s counting; only
        // `RedbStore`'s per-(victim, row) counting needed
        // unifying).
        //
        // A row is shrunk on INTERVAL OVERLAP alone: it is never asked
        // whether it would have matched the evicted event, because no
        // filter is stored in the database (#1849). This over-
        // invalidates, and over-invalidation costs a refetch — the
        // opposite error would be a correctness bug.
        let victim_index = GcVictimIndex::new(&victims);
        let mut row_updates: Vec<(String, Option<CoverageRowRecord>)> = Vec::new();
        for entry in coverage.iter().map_err(persist_err)? {
            let (row_key, value) = entry.map_err(persist_err)?;

            let mut record: CoverageRowRecord = decode_coverage_row(value.value())?;
            let interval = CoverageInterval::new(
                Timestamp::from(record.from),
                Timestamp::from(record.through),
            );

            if let Some(m) = victim_index.max_within(interval) {
                match shrink_after_eviction(interval, m) {
                    Some(next) => {
                        record.from = next.from.as_secs();
                        record.through = next.through.as_secs();
                        row_updates.push((row_key.value().to_string(), Some(record)));
                    }
                    None => {
                        row_updates.push((row_key.value().to_string(), None));
                    }
                }
            }
        }

        for (row_key, update) in row_updates {
            match update {
                None => {
                    coverage.remove(row_key.as_str()).map_err(persist_err)?;
                    report.coverage_rows_deleted += 1;
                }
                Some(record) => {
                    let encoded =
                        serde_json::to_string(&record).expect("redb: encode coverage row");
                    coverage
                        .insert(row_key.as_str(), encoded.as_str())
                        .map_err(persist_err)?;
                    report.coverage_rows_shrunk += 1;
                }
            }
        }

        Ok(())
    })?;
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::GcBeforeCommit);
    let report = write.commit_prepared(report)?;
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::GcAfterCommit);

    Ok(report)
}
