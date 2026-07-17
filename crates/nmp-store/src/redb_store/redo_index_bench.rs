//! Benchmark-only redo protocol for issue #642.
//!
//! Canonical/provenance facts and a complete derived-index redo payload commit
//! with immediate durability. Derived indexes then apply atomically with
//! `Durability::None` and delete the redo row in that same transaction. Reopen
//! replays any redo row that outlived its non-sync materialization.

use std::path::Path;
use std::time::Instant;

use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
    WriteTransaction,
};
use serde::{Deserialize, Serialize};

use super::schema::{
    BY_AUTHOR, BY_AUTHOR_KIND, BY_CREATED_AT, BY_KIND, BY_TAG, EVENTS, EVENT_IDS,
    EVENT_OBSERVATIONS, INDEX_CARDINALITY, REDB_CACHE_BYTES, RELAYS, RELAY_KEYS, RELAY_REFS,
};
use super::store_bench::{
    StoreBenchPreparedCorpus, StoreBenchPreparedMetrics, StoreBenchPreparedRecord,
    StoreBenchPreparedTable, StoreBenchProcessCounters,
};

const INDEX_REDO: TableDefinition<u64, &[u8]> = TableDefinition::new("index_redo_bench_v1");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedbRedoIndexMetrics {
    pub metrics: StoreBenchPreparedMetrics,
    pub immediate_apply_ns: u64,
    pub immediate_commit_ns: u64,
    pub index_apply_ns: u64,
    pub nonsync_commit_ns: u64,
    pub recovery_ns: u64,
    pub recovery_batches: u64,
    pub redo_bytes: u64,
}

fn duration_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

fn counters_delta(
    after: StoreBenchProcessCounters,
    before: StoreBenchProcessCounters,
) -> StoreBenchProcessCounters {
    StoreBenchProcessCounters {
        cpu_ns: after.cpu_ns.saturating_sub(before.cpu_ns),
        allocation_ops: after.allocation_ops.saturating_sub(before.allocation_ops),
        allocated_bytes: after.allocated_bytes.saturating_sub(before.allocated_bytes),
        current_rss_bytes: before.current_rss_bytes,
        peak_rss_bytes: after.peak_rss_bytes,
        process_write_bytes: after
            .process_write_bytes
            .zip(before.process_write_bytes)
            .map(|(after, before)| after.saturating_sub(before)),
    }
}

fn prepared_array<const N: usize>(bytes: &[u8], field: &str) -> Result<[u8; N], String> {
    bytes
        .try_into()
        .map_err(|_| format!("prepared {field} must be {N} bytes, got {}", bytes.len()))
}

fn is_index_table(table: StoreBenchPreparedTable) -> bool {
    matches!(
        table,
        StoreBenchPreparedTable::ByCreatedAt
            | StoreBenchPreparedTable::ByAuthor
            | StoreBenchPreparedTable::ByKind
            | StoreBenchPreparedTable::ByAuthorKind
            | StoreBenchPreparedTable::ByTag
            | StoreBenchPreparedTable::IndexCardinality
    )
}

fn table_from_byte(value: u8) -> Result<StoreBenchPreparedTable, String> {
    match value {
        6 => Ok(StoreBenchPreparedTable::ByCreatedAt),
        7 => Ok(StoreBenchPreparedTable::ByAuthor),
        8 => Ok(StoreBenchPreparedTable::ByKind),
        9 => Ok(StoreBenchPreparedTable::ByAuthorKind),
        10 => Ok(StoreBenchPreparedTable::ByTag),
        11 => Ok(StoreBenchPreparedTable::IndexCardinality),
        other => Err(format!("redo record has unknown table {other}")),
    }
}

fn encode_redo(records: &[StoreBenchPreparedRecord]) -> Result<Vec<u8>, String> {
    let derived: Vec<_> = records
        .iter()
        .filter(|record| is_index_table(record.table))
        .collect();
    let count = u32::try_from(derived.len()).map_err(|_| "too many redo records".to_owned())?;
    let capacity = derived.iter().try_fold(4usize, |capacity, record| {
        capacity
            .checked_add(9)
            .and_then(|value| value.checked_add(record.key.len()))
            .and_then(|value| value.checked_add(record.value.len()))
            .ok_or_else(|| "redo payload length overflow".to_owned())
    })?;
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(&count.to_be_bytes());
    for record in derived {
        let key_len =
            u32::try_from(record.key.len()).map_err(|_| "redo key too large".to_owned())?;
        let value_len =
            u32::try_from(record.value.len()).map_err(|_| "redo value too large".to_owned())?;
        encoded.push(record.table as u8);
        encoded.extend_from_slice(&key_len.to_be_bytes());
        encoded.extend_from_slice(&value_len.to_be_bytes());
        encoded.extend_from_slice(&record.key);
        encoded.extend_from_slice(&record.value);
    }
    Ok(encoded)
}

fn take_u32(bytes: &mut &[u8], field: &str) -> Result<u32, String> {
    if bytes.len() < 4 {
        return Err(format!("truncated redo {field}"));
    }
    let value = u32::from_be_bytes(bytes[..4].try_into().expect("checked length"));
    *bytes = &bytes[4..];
    Ok(value)
}

fn decode_redo(mut bytes: &[u8]) -> Result<Vec<StoreBenchPreparedRecord>, String> {
    let count = take_u32(&mut bytes, "record count")? as usize;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let (&table, rest) = bytes
            .split_first()
            .ok_or_else(|| "truncated redo table".to_owned())?;
        bytes = rest;
        let key_len = take_u32(&mut bytes, "key length")? as usize;
        let value_len = take_u32(&mut bytes, "value length")? as usize;
        let record_len = key_len
            .checked_add(value_len)
            .ok_or_else(|| "redo record length overflow".to_owned())?;
        if bytes.len() < record_len {
            return Err("truncated redo record".to_owned());
        }
        let key = bytes[..key_len].to_vec();
        let value = bytes[key_len..record_len].to_vec();
        bytes = &bytes[record_len..];
        records.push(StoreBenchPreparedRecord {
            table: table_from_byte(table)?,
            key,
            value,
        });
    }
    if !bytes.is_empty() {
        return Err("redo payload has trailing bytes".to_owned());
    }
    Ok(records)
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
    txn.open_table(BY_CREATED_AT)
        .map_err(|error| error.to_string())?;
    txn.open_table(BY_AUTHOR)
        .map_err(|error| error.to_string())?;
    txn.open_table(BY_KIND).map_err(|error| error.to_string())?;
    txn.open_table(BY_AUTHOR_KIND)
        .map_err(|error| error.to_string())?;
    txn.open_table(BY_TAG).map_err(|error| error.to_string())?;
    txn.open_table(INDEX_CARDINALITY)
        .map_err(|error| error.to_string())?;
    txn.open_table(INDEX_REDO)
        .map_err(|error| error.to_string())?;
    txn.commit().map_err(|error| error.to_string())?;
    Ok(db)
}

fn apply_facts(txn: &WriteTransaction, records: &[StoreBenchPreparedRecord]) -> Result<(), String> {
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
    for record in records
        .iter()
        .filter(|record| !is_index_table(record.table))
    {
        match record.table {
            StoreBenchPreparedTable::Events => {
                let key = u64::from_be_bytes(prepared_array(&record.key, "event key")?);
                events
                    .insert(key, record.value.as_slice())
                    .map_err(|error| error.to_string())?;
            }
            StoreBenchPreparedTable::EventIds => {
                let key = prepared_array::<32>(&record.key, "event id")?;
                let value = u64::from_be_bytes(prepared_array(&record.value, "event id value")?);
                event_ids
                    .insert(&key, value)
                    .map_err(|error| error.to_string())?;
            }
            StoreBenchPreparedTable::EventObservations => {
                let key = prepared_array::<12>(&record.key, "observation key")?;
                let value = u64::from_be_bytes(prepared_array(&record.value, "observation value")?);
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
                let value = u64::from_be_bytes(prepared_array(&record.value, "relay ref value")?);
                relay_refs
                    .insert(key, value)
                    .map_err(|error| error.to_string())?;
            }
            table => return Err(format!("unexpected fact table {table:?}")),
        }
    }
    Ok(())
}

fn apply_indexes(
    txn: &WriteTransaction,
    records: &[StoreBenchPreparedRecord],
) -> Result<(), String> {
    let mut by_created_at = txn
        .open_table(BY_CREATED_AT)
        .map_err(|error| error.to_string())?;
    let mut by_author = txn
        .open_table(BY_AUTHOR)
        .map_err(|error| error.to_string())?;
    let mut by_kind = txn.open_table(BY_KIND).map_err(|error| error.to_string())?;
    let mut by_author_kind = txn
        .open_table(BY_AUTHOR_KIND)
        .map_err(|error| error.to_string())?;
    let mut by_tag = txn.open_table(BY_TAG).map_err(|error| error.to_string())?;
    let mut cardinality = txn
        .open_table(INDEX_CARDINALITY)
        .map_err(|error| error.to_string())?;
    for record in records.iter().filter(|record| is_index_table(record.table)) {
        match record.table {
            StoreBenchPreparedTable::ByCreatedAt => {
                let key = prepared_array::<40>(&record.key, "global index key")?;
                let value =
                    u64::from_be_bytes(prepared_array(&record.value, "global index value")?);
                by_created_at
                    .insert(&key, value)
                    .map_err(|error| error.to_string())?;
            }
            StoreBenchPreparedTable::ByAuthor => {
                let key = prepared_array::<72>(&record.key, "author index key")?;
                let value =
                    u64::from_be_bytes(prepared_array(&record.value, "author index value")?);
                by_author
                    .insert(&key, value)
                    .map_err(|error| error.to_string())?;
            }
            StoreBenchPreparedTable::ByKind => {
                let key = prepared_array::<42>(&record.key, "kind index key")?;
                let value = u64::from_be_bytes(prepared_array(&record.value, "kind index value")?);
                by_kind
                    .insert(&key, value)
                    .map_err(|error| error.to_string())?;
            }
            StoreBenchPreparedTable::ByAuthorKind => {
                let key = prepared_array::<74>(&record.key, "author-kind index key")?;
                let value =
                    u64::from_be_bytes(prepared_array(&record.value, "author-kind index value")?);
                by_author_kind
                    .insert(&key, value)
                    .map_err(|error| error.to_string())?;
            }
            StoreBenchPreparedTable::ByTag => {
                let value = u64::from_be_bytes(prepared_array(&record.value, "tag value")?);
                by_tag
                    .insert(record.key.as_slice(), value)
                    .map_err(|error| error.to_string())?;
            }
            StoreBenchPreparedTable::IndexCardinality => {
                let value = u64::from_be_bytes(prepared_array(&record.value, "cardinality value")?);
                cardinality
                    .insert(record.key.as_slice(), value)
                    .map_err(|error| error.to_string())?;
            }
            table => return Err(format!("unexpected index table {table:?}")),
        }
    }
    Ok(())
}

fn recover_pending(db: &Database) -> Result<u64, String> {
    let txn = db.begin_write().map_err(|error| error.to_string())?;
    let pending = {
        let redo = txn
            .open_table(INDEX_REDO)
            .map_err(|error| error.to_string())?;
        redo.iter()
            .map_err(|error| error.to_string())?
            .map(|entry| {
                let (key, value) = entry.map_err(|error| error.to_string())?;
                Ok((key.value(), value.value().to_vec()))
            })
            .collect::<Result<Vec<_>, String>>()?
    };
    for (_, payload) in &pending {
        apply_indexes(&txn, &decode_redo(payload)?)?;
    }
    {
        let mut redo = txn
            .open_table(INDEX_REDO)
            .map_err(|error| error.to_string())?;
        for (sequence, _) in &pending {
            redo.remove(*sequence).map_err(|error| error.to_string())?;
        }
    }
    txn.commit().map_err(|error| error.to_string())?;
    Ok(pending.len() as u64)
}

fn logical_row_counts(db: &Database) -> Result<(Vec<u64>, u64), String> {
    let txn = db.begin_read().map_err(|error| error.to_string())?;
    let rows = vec![
        txn.open_table(EVENTS).map_err(|e| e.to_string())?.len(),
        txn.open_table(EVENT_IDS).map_err(|e| e.to_string())?.len(),
        txn.open_table(EVENT_OBSERVATIONS)
            .map_err(|e| e.to_string())?
            .len(),
        txn.open_table(RELAYS).map_err(|e| e.to_string())?.len(),
        txn.open_table(RELAY_KEYS).map_err(|e| e.to_string())?.len(),
        txn.open_table(RELAY_REFS).map_err(|e| e.to_string())?.len(),
        txn.open_table(BY_CREATED_AT)
            .map_err(|e| e.to_string())?
            .len(),
        txn.open_table(BY_AUTHOR).map_err(|e| e.to_string())?.len(),
        txn.open_table(BY_KIND).map_err(|e| e.to_string())?.len(),
        txn.open_table(BY_AUTHOR_KIND)
            .map_err(|e| e.to_string())?
            .len(),
        txn.open_table(BY_TAG).map_err(|e| e.to_string())?.len(),
        txn.open_table(INDEX_CARDINALITY)
            .map_err(|e| e.to_string())?
            .len(),
    ]
    .into_iter()
    .map(|rows| rows.map_err(|error| error.to_string()))
    .collect::<Result<Vec<_>, _>>()?;
    let redo_rows = txn
        .open_table(INDEX_REDO)
        .map_err(|e| e.to_string())?
        .len()
        .map_err(|e| e.to_string())?;
    Ok((rows, redo_rows))
}

pub fn run_prepared_redb_redo_index_bench(
    path: &Path,
    corpus: &StoreBenchPreparedCorpus,
    sample_process: fn() -> StoreBenchProcessCounters,
) -> Result<RedbRedoIndexMetrics, String> {
    if corpus.events == 0 || corpus.batches.is_empty() {
        return Err("redo benchmark corpus must not be empty".to_owned());
    }
    let db = init_database(path)?;
    let process_before = sample_process();
    let started = Instant::now();
    let redo_payloads = corpus
        .batches
        .iter()
        .map(|batch| encode_redo(&batch.records))
        .collect::<Result<Vec<_>, _>>()?;
    let redo_bytes = redo_payloads
        .iter()
        .map(|payload| payload.len() as u64)
        .sum();
    let mut immediate_apply_ns = 0u64;
    let mut immediate_commit_ns = 0u64;
    let mut index_apply_ns = 0u64;
    let mut nonsync_commit_ns = 0u64;

    for (sequence, (batch, redo_payload)) in corpus.batches.iter().zip(&redo_payloads).enumerate() {
        let immediate = db.begin_write().map_err(|error| error.to_string())?;
        let apply_started = Instant::now();
        apply_facts(&immediate, &batch.records)?;
        {
            let mut redo = immediate
                .open_table(INDEX_REDO)
                .map_err(|error| error.to_string())?;
            redo.insert(sequence as u64, redo_payload.as_slice())
                .map_err(|error| error.to_string())?;
        }
        immediate_apply_ns = immediate_apply_ns.saturating_add(duration_ns(apply_started));
        let commit_started = Instant::now();
        immediate.commit().map_err(|error| error.to_string())?;
        immediate_commit_ns = immediate_commit_ns.saturating_add(duration_ns(commit_started));

        let mut indexes = db.begin_write().map_err(|error| error.to_string())?;
        indexes
            .set_durability(Durability::None)
            .map_err(|error| error.to_string())?;
        let apply_started = Instant::now();
        apply_indexes(&indexes, &batch.records)?;
        {
            let mut redo = indexes
                .open_table(INDEX_REDO)
                .map_err(|error| error.to_string())?;
            redo.remove(sequence as u64)
                .map_err(|error| error.to_string())?;
        }
        index_apply_ns = index_apply_ns.saturating_add(duration_ns(apply_started));
        let commit_started = Instant::now();
        indexes.commit().map_err(|error| error.to_string())?;
        nonsync_commit_ns = nonsync_commit_ns.saturating_add(duration_ns(commit_started));
    }
    let wall_ns = duration_ns(started);
    let process = counters_delta(sample_process(), process_before);
    let stats = db.begin_write().map_err(|error| error.to_string())?;
    let database_stored_bytes = stats
        .stats()
        .map_err(|error| error.to_string())?
        .stored_bytes();
    drop(stats);
    drop(db);
    let database_logical_bytes = std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .len();

    let reopen_started = Instant::now();
    let reopened = Database::open(path).map_err(|error| error.to_string())?;
    let recovery_started = Instant::now();
    let recovery_batches = recover_pending(&reopened)?;
    let recovery_ns = duration_ns(recovery_started);
    let (reopened_table_rows, redo_rows) = logical_row_counts(&reopened)?;
    let reopen_ns = duration_ns(reopen_started);
    let reopened_rows = reopened_table_rows[StoreBenchPreparedTable::Events as usize];
    let exact_reopen = reopened_table_rows == corpus.expected_table_rows && redo_rows == 0;

    Ok(RedbRedoIndexMetrics {
        metrics: StoreBenchPreparedMetrics {
            events: corpus.events,
            transactions: (corpus.batches.len() as u64).saturating_mul(2),
            wall_ns,
            commit_ns: immediate_commit_ns.saturating_add(nonsync_commit_ns),
            commit_p50_ns: None,
            commit_p95_ns: None,
            commit_p99_ns: None,
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
            database_logical_bytes,
            database_stored_bytes,
            reopened_rows,
            expected_table_rows: corpus.expected_table_rows.clone(),
            reopened_table_rows,
            exact_reopen,
        },
        immediate_apply_ns,
        immediate_commit_ns,
        index_apply_ns,
        nonsync_commit_ns,
        recovery_ns,
        recovery_batches,
        redo_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redo_codec_round_trips_derived_records_and_rejects_trailing_bytes() {
        let records = vec![
            StoreBenchPreparedRecord {
                table: StoreBenchPreparedTable::Events,
                key: vec![0; 8],
                value: vec![1],
            },
            StoreBenchPreparedRecord {
                table: StoreBenchPreparedTable::ByTag,
                key: vec![2, 3],
                value: vec![0; 8],
            },
        ];
        let encoded = encode_redo(&records).unwrap();
        let decoded = decode_redo(&encoded).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].table, StoreBenchPreparedTable::ByTag);
        assert_eq!(decoded[0].key, [2, 3]);
        let mut malformed = encoded;
        malformed.push(0);
        assert!(decode_redo(&malformed).is_err());
    }
}
