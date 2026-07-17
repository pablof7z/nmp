//! Selection benchmark for compact ordered-index event-id suffixes.
//!
//! The production indexes retain all 32 inverted event-id bytes so equal-
//! timestamp rows are both unique and exactly ordered. This experiment keeps
//! only the first 8 bytes. It is deliberately not a production format: an
//! exact collision sidecar would be required before adoption. Its purpose is
//! to measure the maximum Redb benefit available from removing 24 repeated
//! bytes from every ordered/tag index row before building that machinery.

use std::path::Path;
use std::time::Instant;

use redb::{Database, ReadableDatabase, ReadableTableMetadata, TableDefinition};

use super::schema::{
    EventKey, EVENTS, EVENT_IDS, EVENT_OBSERVATIONS, INDEX_CARDINALITY, REDB_CACHE_BYTES, RELAYS,
    RELAY_KEYS, RELAY_REFS,
};
use super::store_bench::{
    duration_ns, nearest_rank, prepared_array, StoreBenchPreparedCorpus, StoreBenchPreparedMetrics,
    StoreBenchPreparedTable, StoreBenchProcessCounters,
};

const COMPACT_BY_CREATED_AT: TableDefinition<&[u8; 16], EventKey> =
    TableDefinition::new("compact_by_created_at_v1");
const COMPACT_BY_AUTHOR: TableDefinition<&[u8; 48], EventKey> =
    TableDefinition::new("compact_by_author_v1");
const COMPACT_BY_KIND: TableDefinition<&[u8; 18], EventKey> =
    TableDefinition::new("compact_by_kind_v1");
const COMPACT_BY_AUTHOR_KIND: TableDefinition<&[u8; 50], EventKey> =
    TableDefinition::new("compact_by_author_kind_v1");
const COMPACT_BY_TAG: TableDefinition<&[u8], EventKey> = TableDefinition::new("compact_by_tag_v1");

fn compact_fixed<const FULL: usize, const COMPACT: usize>(
    key: &[u8],
    field: &str,
) -> Result<[u8; COMPACT], String> {
    if FULL.checked_sub(COMPACT) != Some(24) {
        return Err(format!(
            "{field} compact layout must remove exactly 24 bytes"
        ));
    }
    let full = prepared_array::<FULL>(key, field)?;
    full[..COMPACT]
        .try_into()
        .map_err(|_| format!("compact {field} must be {COMPACT} bytes"))
}

fn compact_variable<'a>(key: &'a [u8], field: &str) -> Result<&'a [u8], String> {
    key.get(
        ..key.len().checked_sub(24).ok_or_else(|| {
            format!("prepared {field} is shorter than the removed event-id suffix")
        })?,
    )
    .ok_or_else(|| format!("cannot compact prepared {field}"))
}

fn init_database(path: &Path) -> Result<Database, String> {
    let db = Database::builder()
        .set_cache_size(REDB_CACHE_BYTES)
        .create(path)
        .map_err(|error| error.to_string())?;
    let txn = db.begin_write().map_err(|error| error.to_string())?;
    txn.open_table(EVENTS).map_err(|error| error.to_string())?;
    txn.open_table(EVENT_IDS)
        .map_err(|error| error.to_string())?;
    txn.open_table(EVENT_OBSERVATIONS)
        .map_err(|error| error.to_string())?;
    txn.open_table(RELAYS).map_err(|error| error.to_string())?;
    txn.open_table(RELAY_KEYS)
        .map_err(|error| error.to_string())?;
    txn.open_table(RELAY_REFS)
        .map_err(|error| error.to_string())?;
    txn.open_table(COMPACT_BY_CREATED_AT)
        .map_err(|error| error.to_string())?;
    txn.open_table(COMPACT_BY_AUTHOR)
        .map_err(|error| error.to_string())?;
    txn.open_table(COMPACT_BY_KIND)
        .map_err(|error| error.to_string())?;
    txn.open_table(COMPACT_BY_AUTHOR_KIND)
        .map_err(|error| error.to_string())?;
    txn.open_table(COMPACT_BY_TAG)
        .map_err(|error| error.to_string())?;
    txn.open_table(INDEX_CARDINALITY)
        .map_err(|error| error.to_string())?;
    txn.commit().map_err(|error| error.to_string())?;
    Ok(db)
}

/// Apply the equivalent prepared corpus with 8-byte ordered event-id
/// prefixes. Any compact-key collision fails the run instead of silently
/// overwriting a row. The result is a selection ceiling, not a production
/// correctness claim; adoption requires an exact collision sidecar.
pub fn run_prepared_redb_compact_index_bench(
    path: &Path,
    corpus: &StoreBenchPreparedCorpus,
    sample_process: fn() -> StoreBenchProcessCounters,
) -> Result<StoreBenchPreparedMetrics, String> {
    if corpus.events == 0 || corpus.batches.is_empty() {
        return Err("prepared benchmark corpus must not be empty".to_owned());
    }
    let db = init_database(path)?;
    let process_before = sample_process();
    let started = Instant::now();
    let mut commit_ns = 0u64;
    let mut commit_latencies = Vec::with_capacity(corpus.batches.len());

    for batch in &corpus.batches {
        let txn = db.begin_write().map_err(|error| error.to_string())?;
        let mut events = txn.open_table(EVENTS).map_err(|error| error.to_string())?;
        let mut event_ids = txn
            .open_table(EVENT_IDS)
            .map_err(|error| error.to_string())?;
        let mut observations = txn
            .open_table(EVENT_OBSERVATIONS)
            .map_err(|error| error.to_string())?;
        let mut relays = txn.open_table(RELAYS).map_err(|error| error.to_string())?;
        let mut relay_keys = txn
            .open_table(RELAY_KEYS)
            .map_err(|error| error.to_string())?;
        let mut relay_refs = txn
            .open_table(RELAY_REFS)
            .map_err(|error| error.to_string())?;
        let mut by_created_at = txn
            .open_table(COMPACT_BY_CREATED_AT)
            .map_err(|error| error.to_string())?;
        let mut by_author = txn
            .open_table(COMPACT_BY_AUTHOR)
            .map_err(|error| error.to_string())?;
        let mut by_kind = txn
            .open_table(COMPACT_BY_KIND)
            .map_err(|error| error.to_string())?;
        let mut by_author_kind = txn
            .open_table(COMPACT_BY_AUTHOR_KIND)
            .map_err(|error| error.to_string())?;
        let mut by_tag = txn
            .open_table(COMPACT_BY_TAG)
            .map_err(|error| error.to_string())?;
        let mut cardinality = txn
            .open_table(INDEX_CARDINALITY)
            .map_err(|error| error.to_string())?;

        for record in &batch.records {
            match record.table {
                StoreBenchPreparedTable::Events => {
                    let key = u64::from_be_bytes(prepared_array(&record.key, "event key")?);
                    events
                        .insert(key, record.value.as_slice())
                        .map_err(|error| error.to_string())?;
                }
                StoreBenchPreparedTable::EventIds => {
                    let key = prepared_array::<32>(&record.key, "event id")?;
                    let value =
                        u64::from_be_bytes(prepared_array(&record.value, "event id value")?);
                    event_ids
                        .insert(&key, value)
                        .map_err(|error| error.to_string())?;
                }
                StoreBenchPreparedTable::EventObservations => {
                    let key = prepared_array::<12>(&record.key, "observation key")?;
                    let value =
                        u64::from_be_bytes(prepared_array(&record.value, "observation value")?);
                    observations
                        .insert(&key, value)
                        .map_err(|error| error.to_string())?;
                }
                StoreBenchPreparedTable::Relays => {
                    let key = u32::from_be_bytes(prepared_array(&record.key, "relay key")?);
                    let value = std::str::from_utf8(&record.value).map_err(|e| e.to_string())?;
                    relays
                        .insert(key, value)
                        .map_err(|error| error.to_string())?;
                }
                StoreBenchPreparedTable::RelayKeys => {
                    let key = std::str::from_utf8(&record.key).map_err(|e| e.to_string())?;
                    let value = u32::from_be_bytes(prepared_array(&record.value, "relay id")?);
                    relay_keys
                        .insert(key, value)
                        .map_err(|error| error.to_string())?;
                }
                StoreBenchPreparedTable::RelayRefs => {
                    let key = u32::from_be_bytes(prepared_array(&record.key, "relay ref key")?);
                    let value =
                        u64::from_be_bytes(prepared_array(&record.value, "relay ref value")?);
                    relay_refs
                        .insert(key, value)
                        .map_err(|error| error.to_string())?;
                }
                StoreBenchPreparedTable::ByCreatedAt => {
                    let key = compact_fixed::<40, 16>(&record.key, "global index key")?;
                    let value =
                        u64::from_be_bytes(prepared_array(&record.value, "global index value")?);
                    if by_created_at
                        .insert(&key, value)
                        .map_err(|error| error.to_string())?
                        .is_some_and(|existing| existing.value() != value)
                    {
                        return Err("compact global index key collision".to_owned());
                    }
                }
                StoreBenchPreparedTable::ByAuthor => {
                    let key = compact_fixed::<72, 48>(&record.key, "author index key")?;
                    let value =
                        u64::from_be_bytes(prepared_array(&record.value, "author index value")?);
                    if by_author
                        .insert(&key, value)
                        .map_err(|error| error.to_string())?
                        .is_some_and(|existing| existing.value() != value)
                    {
                        return Err("compact author index key collision".to_owned());
                    }
                }
                StoreBenchPreparedTable::ByKind => {
                    let key = compact_fixed::<42, 18>(&record.key, "kind index key")?;
                    let value =
                        u64::from_be_bytes(prepared_array(&record.value, "kind index value")?);
                    if by_kind
                        .insert(&key, value)
                        .map_err(|error| error.to_string())?
                        .is_some_and(|existing| existing.value() != value)
                    {
                        return Err("compact kind index key collision".to_owned());
                    }
                }
                StoreBenchPreparedTable::ByAuthorKind => {
                    let key = compact_fixed::<74, 50>(&record.key, "author-kind index key")?;
                    let value = u64::from_be_bytes(prepared_array(
                        &record.value,
                        "author-kind index value",
                    )?);
                    if by_author_kind
                        .insert(&key, value)
                        .map_err(|error| error.to_string())?
                        .is_some_and(|existing| existing.value() != value)
                    {
                        return Err("compact author-kind index key collision".to_owned());
                    }
                }
                StoreBenchPreparedTable::ByTag => {
                    let key = compact_variable(&record.key, "tag index key")?;
                    let value =
                        u64::from_be_bytes(prepared_array(&record.value, "tag index value")?);
                    if by_tag
                        .insert(key, value)
                        .map_err(|error| error.to_string())?
                        .is_some_and(|existing| existing.value() != value)
                    {
                        return Err("compact tag index key collision".to_owned());
                    }
                }
                StoreBenchPreparedTable::IndexCardinality => {
                    let value =
                        u64::from_be_bytes(prepared_array(&record.value, "cardinality value")?);
                    cardinality
                        .insert(record.key.as_slice(), value)
                        .map_err(|error| error.to_string())?;
                }
            }
        }

        drop(cardinality);
        drop(by_tag);
        drop(by_author_kind);
        drop(by_kind);
        drop(by_author);
        drop(by_created_at);
        drop(relay_refs);
        drop(relay_keys);
        drop(relays);
        drop(observations);
        drop(event_ids);
        drop(events);
        let commit_started = Instant::now();
        txn.commit().map_err(|error| error.to_string())?;
        let latency = duration_ns(commit_started);
        commit_ns = commit_ns.saturating_add(latency);
        commit_latencies.push(latency);
    }

    let wall_ns = duration_ns(started);
    let process = sample_process().delta(process_before);
    let stats_txn = db.begin_write().map_err(|error| error.to_string())?;
    let database_stored_bytes = stats_txn
        .stats()
        .map_err(|error| error.to_string())?
        .stored_bytes();
    drop(stats_txn);
    drop(db);

    let reopen_started = Instant::now();
    let reopened = Database::open(path).map_err(|error| error.to_string())?;
    let read = reopened.begin_read().map_err(|error| error.to_string())?;
    let reopened_table_rows = vec![
        read.open_table(EVENTS).map_err(|e| e.to_string())?.len(),
        read.open_table(EVENT_IDS).map_err(|e| e.to_string())?.len(),
        read.open_table(EVENT_OBSERVATIONS)
            .map_err(|e| e.to_string())?
            .len(),
        read.open_table(RELAYS).map_err(|e| e.to_string())?.len(),
        read.open_table(RELAY_KEYS)
            .map_err(|e| e.to_string())?
            .len(),
        read.open_table(RELAY_REFS)
            .map_err(|e| e.to_string())?
            .len(),
        read.open_table(COMPACT_BY_CREATED_AT)
            .map_err(|e| e.to_string())?
            .len(),
        read.open_table(COMPACT_BY_AUTHOR)
            .map_err(|e| e.to_string())?
            .len(),
        read.open_table(COMPACT_BY_KIND)
            .map_err(|e| e.to_string())?
            .len(),
        read.open_table(COMPACT_BY_AUTHOR_KIND)
            .map_err(|e| e.to_string())?
            .len(),
        read.open_table(COMPACT_BY_TAG)
            .map_err(|e| e.to_string())?
            .len(),
        read.open_table(INDEX_CARDINALITY)
            .map_err(|e| e.to_string())?
            .len(),
    ]
    .into_iter()
    .map(|rows| rows.map_err(|error| error.to_string()))
    .collect::<Result<Vec<_>, _>>()?;
    let reopened_rows = reopened_table_rows[StoreBenchPreparedTable::Events as usize];
    drop(read);
    drop(reopened);
    let reopen_ns = duration_ns(reopen_started);

    Ok(StoreBenchPreparedMetrics {
        events: corpus.events,
        transactions: corpus.batches.len() as u64,
        wall_ns,
        commit_ns,
        commit_p50_ns: nearest_rank(&commit_latencies, 50),
        commit_p95_ns: nearest_rank(&commit_latencies, 95),
        commit_p99_ns: nearest_rank(&commit_latencies, 99),
        maintenance_ns: None,
        ending_write_buffer_bytes: None,
        l0_tables: None,
        sst_tables: None,
        reopen_ns,
        cpu_ns: process.cpu_ns,
        allocation_ops: process.allocation_ops,
        allocated_bytes: process.allocated_bytes,
        rss_before_bytes: process.current_rss_bytes,
        peak_rss_bytes: process.peak_rss_bytes,
        process_write_bytes: process.process_write_bytes,
        database_logical_bytes: std::fs::metadata(path)
            .map_err(|error| error.to_string())?
            .len(),
        database_stored_bytes,
        reopened_rows,
        expected_table_rows: corpus.expected_table_rows.clone(),
        exact_reopen: reopened_table_rows == corpus.expected_table_rows,
        reopened_table_rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_key_keeps_prefix_timestamp_and_first_eight_id_bytes() {
        let mut full = [0u8; 40];
        for (index, byte) in full.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let compact = compact_fixed::<40, 16>(&full, "test key").unwrap();
        assert_eq!(compact, full[..16]);
    }

    #[test]
    fn compact_variable_rejects_keys_without_a_full_id_suffix() {
        assert!(compact_variable(&[0; 23], "test key").is_err());
        assert_eq!(compact_variable(&[7; 40], "test key").unwrap(), &[7; 16]);
    }
}
