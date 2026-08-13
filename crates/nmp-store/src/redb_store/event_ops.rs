use super::commit::commit_prepared;
use super::ingest::insert_with_tables;
use super::ingest_txn::GovernedWrite;
use super::mutation::remove_row_in_txn;
use super::publish_queue::is_suppressed_in_txn;
use super::query::{expiration_key_timestamp, expiration_key_upper_bound, plan_ordered_query};
use super::schema::{
    event_local_key, event_row_key, persist_err, EventKey, COVERAGE, EVENTS, EVENT_COL_LOCAL,
    EVENT_COL_ROW, EVENT_IDS, EXPIRATION_INDEX, RELAYS,
};
use super::schema::{PUBLISH_QUEUE_SUPPRESS_BY_ADDR, PUBLISH_QUEUE_SUPPRESS_BY_ID};
#[cfg(test)]
use super::store::RedbCrashPoint;
use super::store::RedbStore;
use super::{
    address_key_for, binary_event, compute_coverage_key, merge_interval, shrink_after_eviction,
    window_erase, BTreeMap, BTreeSet, ConcreteFilter, ContextualAtom, CoverageInterval,
    CoverageKey, Event, EventCursor, EventId, EventStore, Filter, GcReport, GcRetentionSet,
    GcVictimIndex, HashMap, IndexedMatch, InsertOutcome, LocalOrigin, PersistenceError,
    PreparedFilter, RelayObserved, RelayUrl, RetractReason, ShapeRecord, SigState, StoredEvent,
    StoredEventView, Timestamp,
};
use redb::{ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};
#[cfg(any(
    test,
    feature = "bench-instrumentation",
    feature = "test-instrumentation"
))]
use std::sync::atomic::Ordering;

/// The `coverage` table's JSON value: the window-erased shape the row was
/// recorded against (needed so `gc` can test event-shape matches — see
/// `ShapeRecord`'s doc comment) plus the proven interval, stored as raw
/// `u64` seconds (round-tripped through `Timestamp::from`/`as_secs`).
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CoverageRowRecord {
    pub(super) shape: ShapeRecord,
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
        .map_err(|error| PersistenceError::invariant(format!("decode coverage row: {error}")))
}

pub(super) fn insert(
    store: &mut RedbStore,
    event: Event,
    from: RelayObserved,
) -> Result<InsertOutcome, PersistenceError> {
    let mut write = GovernedWrite::begin(store)?;
    let outcome = write.apply(|tables, _write_txn| insert_with_tables(tables, event, from))?;
    if matches!(&outcome, InsertOutcome::Superseded { .. }) {
        super::publish_queue_ops::maintain_terminal_receipts_in_txn(
            write.transaction(),
            crate::terminal_retention::wall_clock_now(),
            crate::terminal_retention::TerminalRetentionLimits::PRODUCTION,
        )?;
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
    if outcomes
        .iter()
        .any(|outcome| matches!(outcome, InsertOutcome::Superseded { .. }))
    {
        super::publish_queue_ops::maintain_terminal_receipts_in_txn(
            write.transaction(),
            crate::terminal_retention::wall_clock_now(),
            crate::terminal_retention::TerminalRetentionLimits::PRODUCTION,
        )?;
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
        let publish_queue_suppress_by_id = read_txn
            .open_table(PUBLISH_QUEUE_SUPPRESS_BY_ID)
            .map_err(persist_err)?;
        let publish_queue_suppress_by_addr = read_txn
            .open_table(PUBLISH_QUEUE_SUPPRESS_BY_ADDR)
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
                    PersistenceError::invariant(format!(
                        "raw id map points at missing canonical event {event_key}"
                    ))
                })?;
            let view = StoredEventView::from_trusted(value.value()).map_err(|error| {
                PersistenceError::invariant(format!(
                    "decode canonical event view {event_key}: {error:?}"
                ))
            })?;
            if !view.matches_prepared_filter_after_index(&prepared_filter, IndexedMatch::None) {
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
            if !is_suppressed_in_txn(
                &publish_queue_suppress_by_id,
                &publish_queue_suppress_by_addr,
                &se.event,
            )? {
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
    // Exact ids are already a caller-bounded lookup rather than an
    // ordered index range. Preserve that narrow path, then apply the
    // same exact exclusive cursor predicate as the EventStore contract.
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
/// crate misusing its own database (`PersistenceFault::Invariant`), and both
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
    claims: &[(ContextualAtom, RelayUrl, CoverageInterval)],
) -> Result<(), PersistenceError> {
    if claims.is_empty() {
        return Ok(());
    }
    let write_txn = store.database()?.begin_write().map_err(persist_err)?;
    {
        let mut coverage = write_txn.open_table(COVERAGE).map_err(persist_err)?;
        for (atom, relay, proven) in claims {
            let key = compute_coverage_key(atom);
            let shape = window_erase(&atom.filter);
            let row_key = RedbStore::coverage_row_key(key, relay);
            let existing = coverage
                .get(row_key.as_str())
                .map_err(persist_err)?
                .map(|guard| decode_interval(guard.value()))
                .transpose()?;

            let merged = merge_interval(existing, *proven);
            let record = CoverageRowRecord {
                shape: ShapeRecord::from(&shape),
                from: merged.from.as_secs(),
                through: merged.through.as_secs(),
            };
            let encoded = serde_json::to_string(&record).expect("redb: encode coverage row");
            coverage
                .insert(row_key.as_str(), encoded.as_str())
                .map_err(persist_err)?;
        }
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
/// proven", which is a refetch decision made on a false cache miss. The
/// decode failure is a genuine invariant violation (`PersistenceError::
/// invariant`, raised inside [`decode_interval`]) and the transaction/table/
/// row steps are environmental; both are `Err` here, and `PersistenceFault`
/// is what tells them apart at the caller.
pub(super) fn get_coverage(
    store: &RedbStore,
    key: CoverageKey,
    relay: &RelayUrl,
) -> Result<Option<CoverageInterval>, PersistenceError> {
    #[cfg(any(
        test,
        feature = "bench-instrumentation",
        feature = "test-instrumentation"
    ))]
    store.coverage_reads.fetch_add(1, Ordering::Relaxed);
    let row_key = RedbStore::coverage_row_key(key, relay);
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
                    && !is_suppressed_in_txn(
                        &txn.publish_queue_suppress_by_id,
                        &txn.publish_queue_suppress_by_addr,
                        &event,
                    )?
                    && !claims.is_claimed(&event)
                {
                    victims.push(event);
                }
                Ok(())
            };
        for entry in txn.canonical.events.iter().map_err(persist_err)? {
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
                            PersistenceError::invariant(format!(
                                "decode canonical event view {event_key}: {error:?}"
                            ))
                        })?
                        .materialize_event()
                        .map_err(|error| {
                            PersistenceError::invariant(format!(
                                "materialize canonical event {event_key}: {error:?}"
                            ))
                        })?;
                    pending = Some((event_key, event));
                }
                EVENT_COL_LOCAL => {
                    // A local sidecar with no note row before it is a broken
                    // relational invariant, not an event without local state.
                    let Some((pending_key, event)) = pending.take() else {
                        return Err(PersistenceError::invariant(format!(
                            "canonical local state {event_key} has no event row"
                        )));
                    };
                    if pending_key != event_key {
                        return Err(PersistenceError::invariant(format!(
                            "canonical local state {event_key} follows event {pending_key}"
                        )));
                    }
                    let local = binary_event::decode_local(value.value()).map_err(|error| {
                        PersistenceError::invariant(format!(
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
                PersistenceError::invariant(format!(
                    "gc victim {} vanished before its own removal",
                    event.id
                ))
            })?;
            report.events_evicted += 1;
        }

        // Pass 2 (issue #507): a SINGLE pass over coverage rows,
        // using `GcVictimIndex` (shared verbatim with
        // `RedbStore::gc` — see its doc comment for the proof) to
        // find each row's maximum matching victim timestamp directly,
        // instead of re-walking the full victim list per row. Same
        // write transaction as the event removals above — the
        // shrink/delete and the event delete commit atomically
        // together (ruling §5: never leave a watermark claiming
        // coverage of evicted data). `coverage_rows_shrunk`/
        // `coverage_rows_deleted` stay per-ROW, unchanged from
        // before (this was already `RedbStore`'s counting; only
        // `RedbStore`'s per-(victim, row) counting needed
        // unifying).
        let victim_index = GcVictimIndex::new(&victims);
        let mut row_updates: Vec<(String, Option<CoverageRowRecord>)> = Vec::new();
        for entry in coverage.iter().map_err(persist_err)? {
            let (row_key, value) = entry.map_err(persist_err)?;

            let mut record: CoverageRowRecord = decode_coverage_row(value.value())?;
            let shape: ConcreteFilter = (&record.shape).into();
            let interval = CoverageInterval::new(
                Timestamp::from(record.from),
                Timestamp::from(record.through),
            );

            if let Some(m) = victim_index.max_matching_within(&shape, interval) {
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
