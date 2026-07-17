//! Benchmark-only reduced writers for issue #618's store-cost decomposition.
//!
//! These variants deliberately do not implement [`crate::EventStore`] and are
//! available only behind `bench-instrumentation`. They reuse the production
//! portable encoder, table definitions, observation layout, ordered-index key
//! builders, tag-key builder, and cardinality-key builders. They are evidence
//! tools, never a semantics-reduced production ingest path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use nostr::{Event, Filter, RelayUrl, Timestamp};
use redb::{Database, ReadableTableMetadata};
use serde::{Deserialize, Serialize};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreBenchVariant {
    EncodeOnly,
    Canonical,
    CanonicalProvenance,
    IndexGlobal,
    IndexAuthor,
    IndexKind,
    IndexAuthorKind,
    AllOrdered,
    AllOrderedTag,
    AllIndexesCardinality,
    FullGoverned,
}

impl StoreBenchVariant {
    fn has_provenance(self) -> bool {
        !matches!(self, Self::EncodeOnly | Self::Canonical)
    }

    fn has_global(self) -> bool {
        matches!(
            self,
            Self::IndexGlobal
                | Self::AllOrdered
                | Self::AllOrderedTag
                | Self::AllIndexesCardinality
        )
    }

    fn has_author(self) -> bool {
        matches!(
            self,
            Self::IndexAuthor
                | Self::AllOrdered
                | Self::AllOrderedTag
                | Self::AllIndexesCardinality
        )
    }

    fn has_kind(self) -> bool {
        matches!(
            self,
            Self::IndexKind | Self::AllOrdered | Self::AllOrderedTag | Self::AllIndexesCardinality
        )
    }

    fn has_author_kind(self) -> bool {
        matches!(
            self,
            Self::IndexAuthorKind
                | Self::AllOrdered
                | Self::AllOrderedTag
                | Self::AllIndexesCardinality
        )
    }

    fn has_tag(self) -> bool {
        matches!(self, Self::AllOrderedTag | Self::AllIndexesCardinality)
    }

    fn has_cardinality(self) -> bool {
        matches!(self, Self::AllIndexesCardinality)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreBenchAttribution {
    pub transaction_total_ns: u64,
    pub begin_write_ns: u64,
    pub open_tables_ns: u64,
    pub apply_events_ns: u64,
    pub flush_ns: u64,
    pub commit_ns: u64,
    pub encode_event_ns: u64,
    pub canonical_insert_ns: u64,
    pub index_insert_ns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreBenchMetrics {
    pub variant: StoreBenchVariant,
    pub events: u64,
    pub transaction_batch_size: Option<usize>,
    pub transactions: u64,
    pub wall_ns: u64,
    pub commit_ns: u64,
    pub cpu_ns: u64,
    pub allocation_ops: u64,
    pub allocated_bytes: u64,
    pub rss_before_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    pub process_write_bytes: Option<u64>,
    pub encoded_event_bytes: u64,
    pub database_logical_bytes: u64,
    pub database_stored_bytes: u64,
    pub reopened_rows: u64,
    pub exact_reopen: bool,
    pub full_attribution: Option<StoreBenchAttribution>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StoreBenchProcessCounters {
    pub cpu_ns: u64,
    pub allocation_ops: u64,
    pub allocated_bytes: u64,
    pub current_rss_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    pub process_write_bytes: Option<u64>,
}

impl StoreBenchProcessCounters {
    fn delta(self, before: Self) -> Self {
        Self {
            cpu_ns: self.cpu_ns.saturating_sub(before.cpu_ns),
            allocation_ops: self.allocation_ops.saturating_sub(before.allocation_ops),
            allocated_bytes: self.allocated_bytes.saturating_sub(before.allocated_bytes),
            current_rss_bytes: before.current_rss_bytes,
            peak_rss_bytes: self.peak_rss_bytes,
            process_write_bytes: self
                .process_write_bytes
                .zip(before.process_write_bytes)
                .map(|(after, before)| after.saturating_sub(before)),
        }
    }
}

fn duration_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

fn init_reduced_database(path: &Path, variant: StoreBenchVariant) -> Result<Database, String> {
    let db = Database::builder()
        .set_cache_size(REDB_CACHE_BYTES)
        .create(path)
        .map_err(|error| error.to_string())?;
    let write_txn = db.begin_write().map_err(|error| error.to_string())?;
    write_txn
        .open_table(EVENTS)
        .map_err(|error| error.to_string())?;
    write_txn
        .open_table(EVENT_IDS)
        .map_err(|error| error.to_string())?;
    if variant.has_provenance() {
        write_txn
            .open_table(EVENT_OBSERVATIONS)
            .map_err(|error| error.to_string())?;
        write_txn
            .open_table(RELAYS)
            .map_err(|error| error.to_string())?;
        write_txn
            .open_table(RELAY_KEYS)
            .map_err(|error| error.to_string())?;
        write_txn
            .open_table(RELAY_REFS)
            .map_err(|error| error.to_string())?;
    }
    if variant.has_global() {
        write_txn
            .open_table(BY_CREATED_AT)
            .map_err(|error| error.to_string())?;
    }
    if variant.has_author() {
        write_txn
            .open_table(BY_AUTHOR)
            .map_err(|error| error.to_string())?;
    }
    if variant.has_kind() {
        write_txn
            .open_table(BY_KIND)
            .map_err(|error| error.to_string())?;
    }
    if variant.has_author_kind() {
        write_txn
            .open_table(BY_AUTHOR_KIND)
            .map_err(|error| error.to_string())?;
    }
    if variant.has_tag() {
        write_txn
            .open_table(BY_TAG)
            .map_err(|error| error.to_string())?;
    }
    if variant.has_cardinality() {
        write_txn
            .open_table(INDEX_CARDINALITY)
            .map_err(|error| error.to_string())?;
    }
    write_txn.commit().map_err(|error| error.to_string())?;
    Ok(db)
}

fn run_reduced(
    path: &Path,
    events: Vec<Event>,
    batch_size: usize,
    variant: StoreBenchVariant,
    sample_process: fn() -> StoreBenchProcessCounters,
) -> Result<StoreBenchMetrics, String> {
    let event_count = events.len() as u64;
    let db = init_reduced_database(path, variant)?;
    let relay =
        RelayUrl::parse("wss://store-decomposition.invalid").map_err(|error| error.to_string())?;
    let observed_at = events
        .iter()
        .map(|event| event.created_at.as_secs())
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let mut encoded_event_bytes = 0u64;
    let mut commit_ns = 0u64;
    let mut transactions = 0u64;
    let process_before = sample_process();
    let started = Instant::now();

    let mut events = events.into_iter();
    let mut batch_index = 0usize;
    loop {
        let batch: Vec<_> = events.by_ref().take(batch_size).collect();
        if batch.is_empty() {
            break;
        }
        let write_txn = db.begin_write().map_err(|error| error.to_string())?;
        let mut event_rows = write_txn
            .open_table(EVENTS)
            .map_err(|error| error.to_string())?;
        let mut event_ids = write_txn
            .open_table(EVENT_IDS)
            .map_err(|error| error.to_string())?;
        let mut observations = variant
            .has_provenance()
            .then(|| write_txn.open_table(EVENT_OBSERVATIONS))
            .transpose()
            .map_err(|error| error.to_string())?;
        let mut relays = variant
            .has_provenance()
            .then(|| write_txn.open_table(RELAYS))
            .transpose()
            .map_err(|error| error.to_string())?;
        let mut relay_keys = variant
            .has_provenance()
            .then(|| write_txn.open_table(RELAY_KEYS))
            .transpose()
            .map_err(|error| error.to_string())?;
        let mut relay_refs = variant
            .has_provenance()
            .then(|| write_txn.open_table(RELAY_REFS))
            .transpose()
            .map_err(|error| error.to_string())?;
        let mut by_created_at = variant
            .has_global()
            .then(|| write_txn.open_table(BY_CREATED_AT))
            .transpose()
            .map_err(|error| error.to_string())?;
        let mut by_author = variant
            .has_author()
            .then(|| write_txn.open_table(BY_AUTHOR))
            .transpose()
            .map_err(|error| error.to_string())?;
        let mut by_kind = variant
            .has_kind()
            .then(|| write_txn.open_table(BY_KIND))
            .transpose()
            .map_err(|error| error.to_string())?;
        let mut by_author_kind = variant
            .has_author_kind()
            .then(|| write_txn.open_table(BY_AUTHOR_KIND))
            .transpose()
            .map_err(|error| error.to_string())?;
        let mut by_tag = variant
            .has_tag()
            .then(|| write_txn.open_table(BY_TAG))
            .transpose()
            .map_err(|error| error.to_string())?;
        let mut cardinality = variant
            .has_cardinality()
            .then(|| write_txn.open_table(INDEX_CARDINALITY))
            .transpose()
            .map_err(|error| error.to_string())?;

        if variant.has_provenance() && batch_index == 0 {
            relays
                .as_mut()
                .expect("provenance variant has relays")
                .insert(1, relay.as_str())
                .map_err(|error| error.to_string())?;
            relay_keys
                .as_mut()
                .expect("provenance variant has relay keys")
                .insert(relay.as_str(), 1)
                .map_err(|error| error.to_string())?;
        }

        let first_key = batch_index
            .checked_mul(batch_size)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| "event key overflow".to_owned())? as u64;
        let mut cardinality_deltas: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        for (offset, event) in batch.iter().enumerate() {
            let event_key = first_key + offset as u64;
            let encoded = binary_event::encode_event(event)
                .map_err(|error| format!("encode event: {error}"))?;
            encoded_event_bytes = encoded_event_bytes.saturating_add(encoded.len() as u64);
            event_rows
                .insert(event_key, encoded.as_slice())
                .map_err(|error| error.to_string())?;
            event_ids
                .insert(event.id.as_bytes(), event_key)
                .map_err(|error| error.to_string())?;
            if let Some(observations) = observations.as_mut() {
                let key = observation_key(event_key, 1);
                observations
                    .insert(&key, observed_at)
                    .map_err(|error| error.to_string())?;
            }
            if let Some(index) = by_created_at.as_mut() {
                index
                    .insert(&created_at_key(event), event_key)
                    .map_err(|error| error.to_string())?;
            }
            if let Some(index) = by_author.as_mut() {
                index
                    .insert(&by_author_key(event), event_key)
                    .map_err(|error| error.to_string())?;
            }
            if let Some(index) = by_kind.as_mut() {
                index
                    .insert(&by_kind_key(event), event_key)
                    .map_err(|error| error.to_string())?;
            }
            if let Some(index) = by_author_kind.as_mut() {
                index
                    .insert(&by_author_kind_key(event), event_key)
                    .map_err(|error| error.to_string())?;
            }
            if let Some(index) = by_tag.as_mut() {
                insert_tag_index_rows(index, event, event_key)
                    .map_err(|error| error.to_string())?;
            }
            if variant.has_cardinality() {
                let mut increment = |key: Vec<u8>| {
                    *cardinality_deltas.entry(key).or_default() += 1;
                };
                increment(global_cardinality_key());
                increment(author_cardinality_key(&event.pubkey));
                increment(kind_cardinality_key(event.kind));
                increment(author_kind_cardinality_key(&event.pubkey, event.kind));
                let mut tags = BTreeSet::new();
                for tag in event.tags.iter() {
                    let (Some(letter), Some(value)) = (tag.single_letter_tag(), tag.content())
                    else {
                        continue;
                    };
                    tags.insert(tag_cardinality_key(letter, value));
                }
                for key in tags {
                    increment(key);
                }
            }
        }
        if let Some(relay_refs) = relay_refs.as_mut() {
            let through = first_key + batch.len() as u64 - 1;
            relay_refs
                .insert(1, through)
                .map_err(|error| error.to_string())?;
        }
        if let Some(cardinality) = cardinality.as_mut() {
            for (key, delta) in cardinality_deltas {
                let current = cardinality
                    .get(key.as_slice())
                    .map_err(|error| error.to_string())?
                    .map(|guard| guard.value())
                    .unwrap_or(0);
                cardinality
                    .insert(key.as_slice(), current.saturating_add(delta))
                    .map_err(|error| error.to_string())?;
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
        drop(event_rows);
        let commit_started = Instant::now();
        write_txn.commit().map_err(|error| error.to_string())?;
        commit_ns = commit_ns.saturating_add(duration_ns(commit_started));
        transactions += 1;
        batch_index += 1;
    }
    let wall_ns = duration_ns(started);
    let process = sample_process().delta(process_before);
    let stats_txn = db.begin_write().map_err(|error| error.to_string())?;
    let stored_bytes = stats_txn
        .stats()
        .map_err(|error| error.to_string())?
        .stored_bytes();
    drop(stats_txn);
    drop(db);

    let reopened = Database::open(path).map_err(|error| error.to_string())?;
    let read_txn = reopened.begin_read().map_err(|error| error.to_string())?;
    let rows = read_txn
        .open_table(EVENTS)
        .map_err(|error| error.to_string())?
        .len()
        .map_err(|error| error.to_string())?;
    drop(read_txn);
    drop(reopened);

    Ok(StoreBenchMetrics {
        variant,
        events: event_count,
        transaction_batch_size: Some(batch_size),
        transactions,
        wall_ns,
        commit_ns,
        cpu_ns: process.cpu_ns,
        allocation_ops: process.allocation_ops,
        allocated_bytes: process.allocated_bytes,
        rss_before_bytes: process.current_rss_bytes,
        peak_rss_bytes: process.peak_rss_bytes,
        process_write_bytes: process.process_write_bytes,
        encoded_event_bytes,
        database_logical_bytes: std::fs::metadata(path)
            .map_err(|error| error.to_string())?
            .len(),
        database_stored_bytes: stored_bytes,
        reopened_rows: rows,
        exact_reopen: rows == event_count,
        full_attribution: None,
    })
}

fn run_full(
    path: &Path,
    events: Vec<Event>,
    batch_size: usize,
    sample_process: fn() -> StoreBenchProcessCounters,
) -> Result<StoreBenchMetrics, String> {
    let event_count = events.len() as u64;
    let relay =
        RelayUrl::parse("wss://store-decomposition.invalid").map_err(|error| error.to_string())?;
    let observed_at = Timestamp::from(
        events
            .iter()
            .map(|event| event.created_at.as_secs())
            .max()
            .unwrap_or(0)
            .saturating_add(1),
    );
    let mut store = RedbStore::open(path).map_err(|error| error.to_string())?;
    crate::ingest_attribution::reset();
    let process_before = sample_process();
    let started = Instant::now();
    let mut events = events.into_iter();
    loop {
        let rows: Vec<_> = events
            .by_ref()
            .take(batch_size)
            .map(|event| (event, RelayObserved::new(relay.clone(), observed_at)))
            .collect();
        if rows.is_empty() {
            break;
        }
        let outcomes = store
            .insert_batch(rows)
            .map_err(|error| error.to_string())?;
        if outcomes
            .iter()
            .any(|outcome| !matches!(outcome, InsertOutcome::Inserted))
        {
            return Err("full governed benchmark corpus produced a non-insert outcome".to_owned());
        }
    }
    let wall_ns = duration_ns(started);
    let process = sample_process().delta(process_before);
    let snapshot = crate::ingest_attribution::snapshot();
    drop(store);

    let reopened = RedbStore::open(path).map_err(|error| error.to_string())?;
    let rows = reopened
        .query(&Filter::new())
        .map_err(|error| error.to_string())?
        .len() as u64;
    drop(reopened);
    let db = Database::open(path).map_err(|error| error.to_string())?;
    let stats_txn = db.begin_write().map_err(|error| error.to_string())?;
    let stored_bytes = stats_txn
        .stats()
        .map_err(|error| error.to_string())?
        .stored_bytes();
    drop(stats_txn);
    drop(db);

    Ok(StoreBenchMetrics {
        variant: StoreBenchVariant::FullGoverned,
        events: event_count,
        transaction_batch_size: Some(batch_size),
        transactions: snapshot.batches,
        wall_ns,
        commit_ns: snapshot.commit_ns,
        cpu_ns: process.cpu_ns,
        allocation_ops: process.allocation_ops,
        allocated_bytes: process.allocated_bytes,
        rss_before_bytes: process.current_rss_bytes,
        peak_rss_bytes: process.peak_rss_bytes,
        process_write_bytes: process.process_write_bytes,
        encoded_event_bytes: snapshot.encoded_event_bytes,
        database_logical_bytes: std::fs::metadata(path)
            .map_err(|error| error.to_string())?
            .len(),
        database_stored_bytes: stored_bytes,
        reopened_rows: rows,
        exact_reopen: rows == event_count,
        full_attribution: Some(StoreBenchAttribution {
            transaction_total_ns: snapshot.transaction_total_ns,
            begin_write_ns: snapshot.begin_write_ns,
            open_tables_ns: snapshot.open_tables_ns,
            apply_events_ns: snapshot.apply_events_ns,
            flush_ns: snapshot.flush_ns,
            commit_ns: snapshot.commit_ns,
            encode_event_ns: snapshot.encode_event_ns,
            canonical_insert_ns: snapshot.canonical_insert_ns,
            index_insert_ns: snapshot.index_insert_ns,
        }),
    })
}

pub fn run_store_bench_variant(
    path: &Path,
    events: Vec<Event>,
    batch_size: usize,
    variant: StoreBenchVariant,
    sample_process: fn() -> StoreBenchProcessCounters,
) -> Result<StoreBenchMetrics, String> {
    if events.is_empty() {
        return Err("benchmark corpus must not be empty".to_owned());
    }
    if batch_size == 0 {
        return Err("transaction batch size must be nonzero".to_owned());
    }
    if variant == StoreBenchVariant::EncodeOnly {
        let event_count = events.len() as u64;
        let process_before = sample_process();
        let started = Instant::now();
        let mut encoded_event_bytes = 0u64;
        for event in &events {
            encoded_event_bytes = encoded_event_bytes.saturating_add(
                binary_event::encode_event(event)
                    .map_err(|error| format!("encode event: {error}"))?
                    .len() as u64,
            );
        }
        let wall_ns = duration_ns(started);
        let process = sample_process().delta(process_before);
        return Ok(StoreBenchMetrics {
            variant,
            events: event_count,
            transaction_batch_size: None,
            transactions: 0,
            wall_ns,
            commit_ns: 0,
            cpu_ns: process.cpu_ns,
            allocation_ops: process.allocation_ops,
            allocated_bytes: process.allocated_bytes,
            rss_before_bytes: process.current_rss_bytes,
            peak_rss_bytes: process.peak_rss_bytes,
            process_write_bytes: process.process_write_bytes,
            encoded_event_bytes,
            database_logical_bytes: 0,
            database_stored_bytes: 0,
            reopened_rows: event_count,
            exact_reopen: true,
            full_attribution: None,
        });
    }
    if variant == StoreBenchVariant::FullGoverned {
        run_full(path, events, batch_size, sample_process)
    } else {
        run_reduced(path, events, batch_size, variant, sample_process)
    }
}
