use super::ingest::insert_with_tables;
use super::ingest_txn::RedbIngestTxn;
use super::mutation::remove_row_in_txn;
use super::outbox::{is_suppressed_in_txn, OUTBOX_SUPPRESS_BY_ADDR, OUTBOX_SUPPRESS_BY_ID};
use super::query::{expiration_key_upper_bound, plan_ordered_query};
use super::schema::{
    persist_err, EventKey, COVERAGE, EVENTS, EVENT_IDS, EVENT_LOCAL, EVENT_OBSERVATIONS,
    EXPIRATION_INDEX, RELAYS,
};
#[cfg(test)]
use super::store::RedbCrashPoint;
use super::store::RedbStore;
use super::{
    address_key_for, binary_event, compute_coverage_key, merge_interval, shrink_after_eviction,
    window_erase, BTreeMap, BTreeSet, ClaimSet, ConcreteFilter, ContextualAtom, CoverageInterval,
    CoverageKey, Event, EventCursor, EventId, EventStore, Filter, GcReport, GcVictimIndex, HashMap,
    IndexedMatch, InsertOutcome, LocalOrigin, PersistenceError, PreparedFilter, RelayObserved,
    RelayUrl, RetractReason, ShapeRecord, SigState, StoredEvent, StoredEventView, Timestamp,
};
use redb::{ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};

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

pub(super) fn decode_interval(json: &str) -> CoverageInterval {
    let record: CoverageRowRecord = serde_json::from_str(json).expect("redb: decode coverage row");
    CoverageInterval::new(
        Timestamp::from(record.from),
        Timestamp::from(record.through),
    )
}

pub(super) fn insert(
    store: &mut RedbStore,
    event: Event,
    from: RelayObserved,
) -> Result<InsertOutcome, PersistenceError> {
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let outcome = {
        let mut tables = RedbIngestTxn::open(&write_txn)?;
        let outcome = insert_with_tables(&mut tables, event, from)?;
        tables.canonical.flush_pending()?;
        outcome
    };
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::ObservationBeforeCommit);
    write_txn.commit().map_err(persist_err)?;
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
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::begin_write(begin_started.elapsed());
    let mut outcomes = Vec::with_capacity(events.len());
    {
        #[cfg(feature = "bench-instrumentation")]
        let open_started = std::time::Instant::now();
        let mut tables = RedbIngestTxn::open(&write_txn)?;
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::open_tables(open_started.elapsed());
        #[cfg(feature = "bench-instrumentation")]
        let apply_started = std::time::Instant::now();
        for (event, from) in events {
            outcomes.push(insert_with_tables(&mut tables, event, from)?);
        }
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::apply_events(apply_started.elapsed());
        #[cfg(feature = "bench-instrumentation")]
        let flush_started = std::time::Instant::now();
        tables.canonical.flush_pending()?;
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::flush(flush_started.elapsed());
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::ObservationBeforeCommit);
    #[cfg(feature = "bench-instrumentation")]
    let commit_started = std::time::Instant::now();
    write_txn.commit().map_err(persist_err)?;
    #[cfg(feature = "bench-instrumentation")]
    {
        crate::ingest_attribution::commit(commit_started.elapsed());
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
    let read_txn = store.db.begin_read().map_err(persist_err)?;
    // Fast path: exact ids resolve through the raw-id -> surrogate-key
    // table, bounded by `|ids|` regardless of table size (issue #17).
    if let Some(ids) = filter.ids.as_ref().filter(|ids| !ids.is_empty()) {
        let events = read_txn.open_table(EVENTS).map_err(persist_err)?;
        let event_ids = read_txn.open_table(EVENT_IDS).map_err(persist_err)?;
        let local = read_txn.open_table(EVENT_LOCAL).map_err(persist_err)?;
        let observations = read_txn
            .open_table(EVENT_OBSERVATIONS)
            .map_err(persist_err)?;
        let relays = read_txn.open_table(RELAYS).map_err(persist_err)?;
        let mut relay_cache = HashMap::new();
        let outbox_suppress_by_id = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ID)
            .map_err(persist_err)?;
        let outbox_suppress_by_addr = read_txn
            .open_table(OUTBOX_SUPPRESS_BY_ADDR)
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
            let value = events
                .get(event_key)
                .map_err(persist_err)?
                .expect("event_ids must always point at a stored event");
            let view = StoredEventView::from_trusted(value.value())
                .expect("redb: decode portable stored event view");
            if !view.matches_prepared_filter_after_index(&prepared_filter, IndexedMatch::None) {
                continue;
            }
            let local_value = local.get(event_key).map_err(persist_err)?;
            let se = store.decode_row(
                event_key,
                view,
                local_value.as_ref().map(|value| value.value()),
                &observations,
                &relays,
                &mut relay_cache,
            )?;
            if !is_suppressed_in_txn(&outbox_suppress_by_id, &outbox_suppress_by_addr, &se.event)? {
                out.push(se);
            }
        }
        return Ok(out);
    }

    let plan = plan_ordered_query(&read_txn, filter)?;
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

    let read_txn = store.db.begin_read().map_err(persist_err)?;

    let plan = plan_ordered_query(&read_txn, filter)?;
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

    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let plan = plan_ordered_query(&read_txn, filter)?;
    store.query_ordered_ids(&read_txn, &plan, filter, limit)
}

pub(super) fn query_newest_observed_by(
    store: &RedbStore,
    filter: &Filter,
    relays: &BTreeSet<RelayUrl>,
    limit: usize,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    if limit == 0
        || relays.is_empty()
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
            row.provenance
                .seen
                .keys()
                .any(|relay| relays.contains(relay))
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

    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let plan = plan_ordered_query(&read_txn, filter)?;
    store.query_ordered(&read_txn, &plan, filter, None, Some(limit), Some(relays))
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
    // same exact exclusive cursor predicate as the MemoryStore oracle.
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

    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let plan = plan_ordered_query(&read_txn, filter)?;
    store.query_ordered(&read_txn, &plan, filter, Some(before), Some(limit), None)
}

pub(super) fn query_newest_before_observed_by(
    store: &RedbStore,
    filter: &Filter,
    relays: &BTreeSet<RelayUrl>,
    before: EventCursor,
    limit: usize,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    if limit == 0
        || relays.is_empty()
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
                && row
                    .provenance
                    .seen
                    .keys()
                    .any(|relay| relays.contains(relay))
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

    let read_txn = store.db.begin_read().map_err(persist_err)?;
    let plan = plan_ordered_query(&read_txn, filter)?;
    store.query_ordered(
        &read_txn,
        &plan,
        filter,
        Some(before),
        Some(limit),
        Some(relays),
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

pub(super) fn query_newest_before_any_observed_by(
    store: &RedbStore,
    filters: &[Filter],
    relays: &BTreeSet<RelayUrl>,
    before: EventCursor,
    limit: usize,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    if limit == 0 || filters.is_empty() || relays.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_id = BTreeMap::new();
    for filter in filters {
        for row in store.query_newest_before_observed_by(filter, relays, before, limit)? {
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
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let removed = {
        let mut txn = RedbIngestTxn::open(&write_txn)?;
        let removed = remove_row_in_txn(&mut txn, id, |_| true)?;
        txn.canonical.flush_pending()?;
        removed
    };
    write_txn.commit().map_err(persist_err)?;
    Ok(removed)
}

pub(super) fn expire_due(
    store: &mut RedbStore,
    now: Timestamp,
) -> Result<Vec<StoredEvent>, PersistenceError> {
    let write_txn = store.db.begin_write().map_err(persist_err)?;
    let removed = {
        let mut txn = RedbIngestTxn::open(&write_txn)?;

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
            if let Some(row) = remove_row_in_txn(&mut txn, stored.event.id, |_| true)? {
                removed.push(row);
            }
        }
        txn.canonical.flush_pending()?;
        removed
    };
    write_txn.commit().map_err(persist_err)?;
    Ok(removed)
}

pub(super) fn next_expiration(store: &RedbStore) -> Option<Timestamp> {
    let read_txn = store.db.begin_read().expect("redb: begin_read");
    let expiration_index = read_txn
        .open_table(EXPIRATION_INDEX)
        .expect("redb: open expiration_index");
    let (key, _value) = expiration_index
        .first()
        .expect("redb: first expiration_index")?;
    Some(Timestamp::from(u64::from_be_bytes(
        key.value()[..8]
            .try_into()
            .expect("expiration index timestamp is eight bytes"),
    )))
}

pub(super) fn record_coverage(
    store: &mut RedbStore,
    atom: &ContextualAtom,
    relay: &RelayUrl,
    proven: CoverageInterval,
) -> Result<(), PersistenceError> {
    let key = compute_coverage_key(atom);
    let shape = window_erase(&atom.filter);
    let row_key = RedbStore::coverage_row_key(key, relay);

    let write_txn = store.db.begin_write().map_err(persist_err)?;
    {
        let mut coverage = write_txn.open_table(COVERAGE).map_err(persist_err)?;
        let existing = coverage
            .get(row_key.as_str())
            .map_err(persist_err)?
            .map(|guard| decode_interval(guard.value()));

        let merged = merge_interval(existing, proven);
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
    write_txn.commit().map_err(persist_err)?;
    Ok(())
}

pub(super) fn get_coverage(
    store: &RedbStore,
    key: CoverageKey,
    relay: &RelayUrl,
) -> Option<CoverageInterval> {
    let row_key = RedbStore::coverage_row_key(key, relay);
    let read_txn = store.db.begin_read().expect("redb: begin_read");
    let coverage = read_txn.open_table(COVERAGE).expect("redb: open coverage");
    coverage
        .get(row_key.as_str())
        .expect("redb: get coverage row")
        .map(|guard| decode_interval(guard.value()))
}

pub(super) fn gc(store: &mut RedbStore, claims: &ClaimSet) -> Result<GcReport, PersistenceError> {
    let mut report = GcReport::default();

    let write_txn = store.db.begin_write().map_err(persist_err)?;
    {
        let mut txn = RedbIngestTxn::open(&write_txn)?;
        let mut coverage = write_txn.open_table(COVERAGE).map_err(persist_err)?;

        // Pass 1: find victims (regular events matched by no claim, and
        // not an open — unsigned — local intent: Fable checkpoint R5,
        // mirrors `MemoryStore::gc`'s exclusion exactly). A row
        // currently hidden by a still-open kind:5 suppression claim is
        // pinned the same way (architecture review requirement — GC
        // must never evict a target a pending cancel/promote can still
        // act on; NIP-40 expiry may still remove it separately).
        // Collected up front into owned values so the removal pass
        // below never holds a borrow across a mutation.
        let mut victims: Vec<Event> = Vec::new();
        for entry in txn.canonical.events.iter().map_err(persist_err)? {
            let (key, value) = entry.map_err(persist_err)?;
            let event = StoredEventView::from_trusted(value.value())
                .expect("redb: decode canonical event view")
                .materialize_event()
                .expect("redb: materialize canonical event");
            let local = txn
                .canonical
                .local
                .get(key.value())
                .map_err(persist_err)?
                .map(|value| {
                    binary_event::decode_local(value.value())
                        .expect("redb: decode canonical local state")
                });
            if address_key_for(&event).is_none()
                && !matches!(
                    local,
                    Some(LocalOrigin {
                        sig_state: SigState::Pending,
                        ..
                    })
                )
                && !is_suppressed_in_txn(
                    &txn.outbox_suppress_by_id,
                    &txn.outbox_suppress_by_addr,
                    &event,
                )?
                && !claims.is_claimed(&event)
            {
                victims.push(event);
            }
        }

        for event in &victims {
            remove_row_in_txn(&mut txn, event.id, |_| true)?
                .expect("gc victim must remain present until removal");
            report.events_evicted += 1;
        }

        // Pass 2 (issue #507): a SINGLE pass over coverage rows,
        // using `GcVictimIndex` (shared verbatim with
        // `MemoryStore::gc` — see its doc comment for the proof) to
        // find each row's maximum matching victim timestamp directly,
        // instead of re-walking the full victim list per row. Same
        // write transaction as the event removals above — the
        // shrink/delete and the event delete commit atomically
        // together (ruling §5: never leave a watermark claiming
        // coverage of evicted data). `coverage_rows_shrunk`/
        // `coverage_rows_deleted` stay per-ROW, unchanged from
        // before (this was already `RedbStore`'s counting; only
        // `MemoryStore`'s per-(victim, row) counting needed
        // unifying).
        let victim_index = GcVictimIndex::new(&victims);
        let mut row_updates: Vec<(String, Option<CoverageRowRecord>)> = Vec::new();
        let mut legacy_row_keys: Vec<String> = Vec::new();
        for entry in coverage.iter().map_err(persist_err)? {
            let (row_key, value) = entry.map_err(persist_err)?;

            // Legacy-row purge (#106, Fable's C refinement): a row
            // whose key predates the current schema version (no
            // `COVERAGE_ROW_KEY_PREFIX`) is permanently orphaned --
            // nothing will ever compute a matching key for it again
            // (v2 keys fold context + a version tag into the hash
            // itself, so no v1 key can ever collide forward into v2).
            // Delete it outright rather than let it linger forever,
            // tracked separately from `report.coverage_rows_deleted`
            // (which is specifically shrink-emptied current-schema
            // rows).
            if !row_key
                .value()
                .starts_with(RedbStore::COVERAGE_ROW_KEY_PREFIX)
            {
                legacy_row_keys.push(row_key.value().to_string());
                continue;
            }

            let mut record: CoverageRowRecord =
                serde_json::from_str(value.value()).expect("redb: decode coverage row");
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

        for row_key in legacy_row_keys {
            coverage.remove(row_key.as_str()).map_err(persist_err)?;
            report.legacy_coverage_rows_purged += 1;
        }
        txn.canonical.flush_pending()?;
    }
    #[cfg(test)]
    store.crash_if(RedbCrashPoint::GcBeforeCommit);
    write_txn.commit().map_err(persist_err)?;

    Ok(report)
}
