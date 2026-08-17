//! Transactional persistence for immutable packed ordered-postings runs.
//!
//! Packed postings are query-authoritative in schema v8. Every governed event
//! mutation records its packed addition or death in the same Redb transaction.
//! Publication is owned by `GovernedWrite`, so a canonical commit cannot
//! bypass it.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashSet};
use std::sync::Arc;

use redb::ReadableTable;
#[cfg(test)]
use redb::ReadableTableMetadata;

use super::postings::validate_run_metas;
use super::postings::{
    compact_segment, encode_dictionary, encode_run, merge_dead_blocks, shard_for,
    CompactionSegmentSource, DeadKeys, DictionaryView, Family, Membership, PostingCursor, Prefix,
    RunEvent, RunMeta, SegmentView, MAX_DEATH_BLOCKS, SHARD_MASK,
};
use super::query::tag_index_prefix;
#[cfg(test)]
use super::schema::POSTINGS_READY;
use super::schema::{
    persist_err, EventKey, POSTINGS_CATALOG, POSTINGS_NEXT_RUN_ID, POSTINGS_SEGMENTS, STORE_META,
};
use super::{Event, EventCursor, EventId, PersistenceError};

const BASE_RUN_FAN_IN: usize = 8;
const LARGE_RUN_FAN_IN: usize = 6;

/// Process-death seams for the packed publication protocol. The environment
/// variable is set only in the dedicated child-process crash harness, so
/// ordinary parallel unit tests cannot arm a shared in-process failpoint.
#[cfg(test)]
pub(super) fn crash_if_postings(point: &str) {
    if std::env::var("NMP_U5_CRASH_POINT").as_deref() == Ok(point) {
        std::process::abort();
    }
}

#[cfg(test)]
pub(super) fn assert_packed_integrity(
    read_txn: &redb::ReadTransaction,
    canonical: &BTreeMap<EventKey, Event>,
) {
    let catalog = read_txn
        .open_table(POSTINGS_CATALOG)
        .expect("audit packed run catalog");
    let segments = read_txn
        .open_table(POSTINGS_SEGMENTS)
        .expect("audit packed segments");
    let meta = read_txn
        .open_table(STORE_META)
        .expect("audit packed metadata");
    assert_eq!(
        meta.get(POSTINGS_READY)
            .expect("audit packed readiness")
            .expect("packed readiness exists")
            .value(),
        1
    );

    let metas = catalog_run_metas(&catalog).expect("read packed run catalog");
    validate_run_metas(&metas).expect("packed run ranges are valid");
    assert_eq!(
        catalog_column_len(&catalog, CATALOG_BY_MIN).expect("count packed run ranges"),
        metas.len() as u64
    );
    assert_eq!(
        catalog_column_len(&catalog, CATALOG_DICTIONARY).expect("count packed dictionaries"),
        metas.len() as u64
    );

    let mut actual = Vec::new();
    let mut seen_segments = 0u64;
    for run in &metas {
        let range_entry = catalog
            .get(catalog_key(CATALOG_BY_MIN, run.min_event_key).as_slice())
            .expect("audit packed run range")
            .expect("packed run range exists");
        assert_eq!(
            decode_run_id(range_entry.value()).expect("decode packed run range"),
            run.run_id
        );
        let dictionary_bytes = catalog
            .get(catalog_key(CATALOG_DICTIONARY, run.run_id).as_slice())
            .expect("audit packed dictionary")
            .expect("packed dictionary exists");
        let dictionary = DictionaryView::parse(dictionary_bytes.value())
            .and_then(DictionaryView::validate)
            .expect("packed dictionary is valid");
        let mut blocks = Vec::new();
        for level in 0..MAX_DEATH_BLOCKS {
            let key = death_key(run.run_id, level);
            if let Some(value) = catalog.get(key.as_slice()).expect("audit death block") {
                blocks.push(DeadKeys::decode(value.value()).expect("decode death block"));
            }
        }
        let dead = merge_dead_blocks(&blocks).expect("merge death blocks");
        let mut live_keys = BTreeSet::new();
        for family in Family::ALL {
            for shard in 0..=super::postings::SHARD_MASK {
                let key = segment_key(family, shard, run.run_id);
                let Some(value) = segments.get(key.as_slice()).expect("audit packed segment")
                else {
                    continue;
                };
                seen_segments += 1;
                let segment = SegmentView::parse(value.value()).expect("parse packed segment");
                segment
                    .validate(dictionary)
                    .expect("validate packed segment");
                for membership in segment
                    .memberships(dictionary)
                    .expect("decode packed memberships")
                {
                    if dead
                        .as_ref()
                        .is_some_and(|keys| keys.contains(membership.event.event_key))
                    {
                        continue;
                    }
                    live_keys.insert(membership.event.event_key);
                    actual.push(membership_tuple(membership));
                }
            }
        }
        assert_eq!(live_keys.len() as u64, run.live_events);
    }
    assert_eq!(
        seen_segments,
        segments.len().expect("count packed segments"),
        "every packed segment belongs to a published run"
    );

    let mut expected: Vec<_> = memberships_for_events(canonical)
        .into_iter()
        .map(membership_tuple)
        .collect();
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected);
}

#[cfg(test)]
fn membership_tuple(membership: Membership) -> (u8, u8, Vec<u8>, u64, [u8; 32], EventKey) {
    (
        membership.family as u8,
        membership.shard,
        membership.prefix.as_bytes().to_vec(),
        membership.event.created_at,
        membership.event.id,
        membership.event.event_key,
    )
}

/// Physical storage door for the packed-postings write algorithm: run
/// allocation, publication, death application, and compaction. Mirrors
/// `GovernedIngestTxn` (`ingest_txn.rs`) — policy lives in this module's free
/// functions (`publish_run`, `apply_deaths`, `apply_run_deaths`,
/// `compact_overfull_levels`, `compact_cohort`, `rewrite_run_without_dead`,
/// `delete_run`, …); only primitive per-key access is backend-specific.
/// `postings_store` and the LMDB ingest benchmark both implement this, so
/// the run-management algorithm runs identically on both instead of being
/// forked (#1820) — a benchmark that forks the protocol stops measuring the
/// protocol, and the fork is a second place every fix must land.
pub(super) trait PackedPostingsTxn {
    fn dictionary_get(&mut self, run_id: u64) -> Result<Option<Vec<u8>>, PersistenceError>;
    fn dictionary_put(&mut self, run_id: u64, bytes: &[u8]) -> Result<(), PersistenceError>;
    fn dictionary_delete(&mut self, run_id: u64) -> Result<(), PersistenceError>;
    /// Count of every dictionary row, live or orphaned. Used only to prove
    /// no dictionary outlives its run (see `allocate_run_id`).
    fn dictionary_count(&mut self) -> Result<u64, PersistenceError>;

    fn segment_get(
        &mut self,
        family: Family,
        shard: u8,
        run_id: u64,
    ) -> Result<Option<Vec<u8>>, PersistenceError>;
    fn segment_put(
        &mut self,
        family: Family,
        shard: u8,
        run_id: u64,
        bytes: &[u8],
    ) -> Result<(), PersistenceError>;
    fn segment_delete(
        &mut self,
        family: Family,
        shard: u8,
        run_id: u64,
    ) -> Result<(), PersistenceError>;

    fn run_meta_get(&mut self, run_id: u64) -> Result<Option<RunMeta>, PersistenceError>;
    fn run_meta_put(&mut self, meta: &RunMeta) -> Result<(), PersistenceError>;
    fn run_meta_delete(&mut self, run_id: u64) -> Result<(), PersistenceError>;
    /// Every live run, proven relationally consistent: unique ids and
    /// non-overlapping event-key ranges (#790 for the redb catalog; the
    /// same check on the LMDB benchmark's separate run-meta table).
    fn list_run_metas(&mut self) -> Result<Vec<RunMeta>, PersistenceError>;

    fn by_min_put(&mut self, min_event_key: u64, run_id: u64) -> Result<(), PersistenceError>;
    fn by_min_delete(&mut self, min_event_key: u64) -> Result<(), PersistenceError>;

    fn death_block_get(
        &mut self,
        run_id: u64,
        level: usize,
    ) -> Result<Option<DeadKeys>, PersistenceError>;
    fn death_block_put(
        &mut self,
        run_id: u64,
        level: usize,
        keys: &DeadKeys,
    ) -> Result<(), PersistenceError>;
    fn death_block_delete(&mut self, run_id: u64, level: usize) -> Result<(), PersistenceError>;

    fn next_run_id_get(&mut self) -> Result<Option<u64>, PersistenceError>;
    fn next_run_id_put(&mut self, value: u64) -> Result<(), PersistenceError>;

    /// Process-death seam for the redb crash-atomicity harness. A no-op on
    /// every other backend.
    fn crash_probe(&self, _point: &'static str) {}

    /// Instrumentation seams. A no-op for the production redb path, which
    /// tracks no such counters; the LMDB benchmark overrides them to
    /// account work it already had to do, not to gate any behavior.
    fn note_run_published(&mut self) {}
    fn note_compaction(&mut self, _input_runs: usize, _live_events: Option<u64>) {}
}

/// The production `PackedPostingsTxn`: primitives backed by the shared
/// [`POSTINGS_CATALOG`] and [`POSTINGS_SEGMENTS`] tables.
pub(super) struct RedbPostingsTxn<'a> {
    write_txn: &'a redb::WriteTransaction,
}

impl<'a> RedbPostingsTxn<'a> {
    pub(super) fn new(write_txn: &'a redb::WriteTransaction) -> Self {
        Self { write_txn }
    }
}

impl PackedPostingsTxn for RedbPostingsTxn<'_> {
    fn dictionary_get(&mut self, run_id: u64) -> Result<Option<Vec<u8>>, PersistenceError> {
        let catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        let value = catalog
            .get(catalog_key(CATALOG_DICTIONARY, run_id).as_slice())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec());
        Ok(value)
    }

    fn dictionary_put(&mut self, run_id: u64, bytes: &[u8]) -> Result<(), PersistenceError> {
        let mut catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        catalog
            .insert(catalog_key(CATALOG_DICTIONARY, run_id).as_slice(), bytes)
            .map_err(persist_err)?;
        Ok(())
    }

    fn dictionary_delete(&mut self, run_id: u64) -> Result<(), PersistenceError> {
        let mut catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        catalog
            .remove(catalog_key(CATALOG_DICTIONARY, run_id).as_slice())
            .map_err(persist_err)?;
        Ok(())
    }

    fn dictionary_count(&mut self) -> Result<u64, PersistenceError> {
        let catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        catalog_column_len(&catalog, CATALOG_DICTIONARY)
    }

    fn segment_get(
        &mut self,
        family: Family,
        shard: u8,
        run_id: u64,
    ) -> Result<Option<Vec<u8>>, PersistenceError> {
        let segments = self
            .write_txn
            .open_table(POSTINGS_SEGMENTS)
            .map_err(persist_err)?;
        let value = segments
            .get(segment_key(family, shard, run_id).as_slice())
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec());
        Ok(value)
    }

    fn segment_put(
        &mut self,
        family: Family,
        shard: u8,
        run_id: u64,
        bytes: &[u8],
    ) -> Result<(), PersistenceError> {
        let mut segments = self
            .write_txn
            .open_table(POSTINGS_SEGMENTS)
            .map_err(persist_err)?;
        segments
            .insert(segment_key(family, shard, run_id).as_slice(), bytes)
            .map_err(persist_err)?;
        Ok(())
    }

    fn segment_delete(
        &mut self,
        family: Family,
        shard: u8,
        run_id: u64,
    ) -> Result<(), PersistenceError> {
        let mut segments = self
            .write_txn
            .open_table(POSTINGS_SEGMENTS)
            .map_err(persist_err)?;
        segments
            .remove(segment_key(family, shard, run_id).as_slice())
            .map_err(persist_err)?;
        Ok(())
    }

    fn run_meta_get(&mut self, run_id: u64) -> Result<Option<RunMeta>, PersistenceError> {
        let catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        let meta = catalog
            .get(catalog_key(CATALOG_RUN_META, run_id).as_slice())
            .map_err(persist_err)?
            .map(|value| RunMeta::decode(value.value()).map_err(packed_err))
            .transpose()?;
        Ok(meta)
    }

    fn run_meta_put(&mut self, meta: &RunMeta) -> Result<(), PersistenceError> {
        let encoded = meta.encode().map_err(packed_err)?;
        let mut catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        catalog
            .insert(
                catalog_key(CATALOG_RUN_META, meta.run_id).as_slice(),
                encoded.as_slice(),
            )
            .map_err(persist_err)?;
        Ok(())
    }

    fn run_meta_delete(&mut self, run_id: u64) -> Result<(), PersistenceError> {
        let mut catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        catalog
            .remove(catalog_key(CATALOG_RUN_META, run_id).as_slice())
            .map_err(persist_err)?;
        Ok(())
    }

    fn list_run_metas(&mut self) -> Result<Vec<RunMeta>, PersistenceError> {
        let catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        load_run_catalog(&catalog)
    }

    fn by_min_put(&mut self, min_event_key: u64, run_id: u64) -> Result<(), PersistenceError> {
        let mut catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        catalog
            .insert(
                catalog_key(CATALOG_BY_MIN, min_event_key).as_slice(),
                run_id.to_be_bytes().as_slice(),
            )
            .map_err(persist_err)?;
        Ok(())
    }

    fn by_min_delete(&mut self, min_event_key: u64) -> Result<(), PersistenceError> {
        let mut catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        catalog
            .remove(catalog_key(CATALOG_BY_MIN, min_event_key).as_slice())
            .map_err(persist_err)?;
        Ok(())
    }

    fn death_block_get(
        &mut self,
        run_id: u64,
        level: usize,
    ) -> Result<Option<DeadKeys>, PersistenceError> {
        let catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        let block = catalog
            .get(death_key(run_id, level).as_slice())
            .map_err(persist_err)?
            .map(|value| DeadKeys::decode(value.value()).map_err(packed_err))
            .transpose()?;
        Ok(block)
    }

    fn death_block_put(
        &mut self,
        run_id: u64,
        level: usize,
        keys: &DeadKeys,
    ) -> Result<(), PersistenceError> {
        let encoded = keys.encode().map_err(packed_err)?;
        let mut catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        catalog
            .insert(death_key(run_id, level).as_slice(), encoded.as_slice())
            .map_err(persist_err)?;
        Ok(())
    }

    fn death_block_delete(&mut self, run_id: u64, level: usize) -> Result<(), PersistenceError> {
        let mut catalog = self
            .write_txn
            .open_table(POSTINGS_CATALOG)
            .map_err(persist_err)?;
        catalog
            .remove(death_key(run_id, level).as_slice())
            .map_err(persist_err)?;
        Ok(())
    }

    fn next_run_id_get(&mut self) -> Result<Option<u64>, PersistenceError> {
        let meta = self.write_txn.open_table(STORE_META).map_err(persist_err)?;
        let value = meta
            .get(POSTINGS_NEXT_RUN_ID)
            .map_err(persist_err)?
            .map(|guard| guard.value());
        Ok(value)
    }

    fn next_run_id_put(&mut self, value: u64) -> Result<(), PersistenceError> {
        let mut meta = self.write_txn.open_table(STORE_META).map_err(persist_err)?;
        meta.insert(POSTINGS_NEXT_RUN_ID, value)
            .map_err(persist_err)?;
        Ok(())
    }

    #[cfg(test)]
    fn crash_probe(&self, point: &'static str) {
        crash_if_postings(point);
    }
}

#[derive(Default)]
pub(super) struct PostingsBatch {
    additions: BTreeMap<EventKey, PendingEvent>,
    deaths: BTreeSet<EventKey>,
}

pub(super) struct PendingEvent {
    event: Arc<RunEvent>,
    author: [u8; 32],
    kind: [u8; 2],
    tags: Vec<Arc<[u8]>>,
}

impl PendingEvent {
    pub(super) fn prepare(event: &Event, event_key: EventKey) -> Self {
        let mut tags = BTreeSet::new();
        for tag in event.tags.iter() {
            let (Some(name), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
                continue;
            };
            tags.insert(tag_index_prefix(name, value));
        }
        Self {
            event: Arc::new(RunEvent {
                created_at: event.created_at.as_secs(),
                id: *event.id.as_bytes(),
                event_key,
            }),
            author: *event.pubkey.as_bytes(),
            kind: event.kind.as_u16().to_be_bytes(),
            tags: tags.into_iter().map(Arc::from).collect(),
        }
    }
}

impl PostingsBatch {
    pub(super) fn insert(&mut self, event: &Event, event_key: EventKey) {
        self.deaths.remove(&event_key);
        self.additions
            .insert(event_key, PendingEvent::prepare(event, event_key));
    }

    pub(super) fn remove(&mut self, event_key: EventKey) {
        if self.additions.remove(&event_key).is_none() {
            self.deaths.insert(event_key);
        }
    }

    pub(super) fn flush(
        &mut self,
        write_txn: &redb::WriteTransaction,
    ) -> Result<(), PersistenceError> {
        let mut txn = RedbPostingsTxn::new(write_txn);
        if !self.deaths.is_empty() {
            apply_deaths(&mut txn, &self.deaths)?;
        }
        if !self.additions.is_empty() {
            publish_pending(&mut txn, &self.additions)?;
        }
        self.additions.clear();
        self.deaths.clear();
        Ok(())
    }
}

pub(super) fn publish_pending<T: PackedPostingsTxn>(
    txn: &mut T,
    events: &BTreeMap<EventKey, PendingEvent>,
) -> Result<(), PersistenceError> {
    let min_event_key = *events.first_key_value().expect("nonempty additions").0;
    let max_event_key = *events.last_key_value().expect("nonempty additions").0;
    publish_run(
        txn,
        memberships_for_pending(events),
        min_event_key,
        max_event_key,
    )
}

/// Publish one new level-0 run from `memberships` and fold any now-overfull
/// levels. The one shared entry point for turning pending additions into a
/// published run, regardless of which `PackedPostingsTxn` backs `txn`.
pub(super) fn publish_run<T: PackedPostingsTxn>(
    txn: &mut T,
    memberships: Vec<Membership>,
    min_event_key: u64,
    max_event_key: u64,
) -> Result<(), PersistenceError> {
    let run_id = allocate_run_id(txn)?;
    let encoded = encode_run(memberships).map_err(packed_err)?;
    let meta = RunMeta {
        run_id,
        level: 0,
        min_event_key,
        max_event_key,
        live_events: encoded.dictionary_entries,
    };
    insert_run(txn, meta, encoded)?;
    txn.note_run_published();
    compact_overfull_levels(txn)
}

struct ScanSource<'a> {
    cursor: PostingCursor<'a>,
    dead: Option<&'a DeadKeys>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ScanHead {
    event: RunEvent,
    source: usize,
}

impl Ord for ScanHead {
    fn cmp(&self, other: &Self) -> Ordering {
        self.event
            .created_at
            .cmp(&other.event.created_at)
            .then_with(|| other.event.id.cmp(&self.event.id))
            .then_with(|| other.event.event_key.cmp(&self.event.event_key))
            .then_with(|| other.source.cmp(&self.source))
    }
}

impl PartialOrd for ScanHead {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Merge the selected packed prefix lists and project only canonical-visible
/// rows. Post-filter refusals continue the merge, so `limit` counts visible
/// results rather than raw postings.
pub(super) struct PackedScan<'a> {
    pub(super) family: Family,
    pub(super) prefixes: &'a [Vec<u8>],
    pub(super) since: u64,
    pub(super) until: u64,
    pub(super) before: Option<EventCursor>,
    pub(super) limit: Option<usize>,
}

pub(super) fn scan_packed<T>(
    read_txn: &redb::ReadTransaction,
    scan: PackedScan<'_>,
    mut visited: impl FnMut(),
    mut project: impl FnMut(EventKey, EventId) -> Result<Option<T>, PersistenceError>,
) -> Result<Vec<T>, PersistenceError> {
    if scan.limit == Some(0) {
        return Ok(Vec::new());
    }
    let catalog = read_txn.open_table(POSTINGS_CATALOG).map_err(persist_err)?;
    let segments = read_txn
        .open_table(POSTINGS_SEGMENTS)
        .map_err(persist_err)?;
    let shards: BTreeSet<_> = scan
        .prefixes
        .iter()
        .map(|prefix| shard_for(scan.family, prefix))
        .collect();
    // #790: the bounded run catalog and its range-index column are validated
    // as one relationship before a single prefix directory is consulted. A
    // wrong-id, duplicate-range, or orphan catalog entry is not a run this
    // scan may quietly skip.
    let runs = load_run_catalog(&catalog)?;
    let mut loaded = Vec::new();
    for meta in &runs {
        let mut candidates = Vec::new();
        for &shard in &shards {
            let key = segment_key(scan.family, shard, meta.run_id);
            if let Some(value) = segments.get(key.as_slice()).map_err(persist_err)? {
                candidates.push(value);
            }
        }
        if candidates.is_empty() {
            continue;
        }
        let dictionary = catalog
            .get(catalog_key(CATALOG_DICTIONARY, meta.run_id).as_slice())
            .map_err(persist_err)?
            .ok_or_else(|| packed_err(format!("run {} has no dictionary", meta.run_id)))?;
        // Validate the dictionary once and every candidate segment against
        // it BEFORE `prefix` binary-searches the directory or a cursor
        // binary-searches a posting list. Both searches assume sorted input;
        // on unsorted input they land in the wrong place and answer "no
        // such prefix"/"no such row" — a false miss, not an error.
        let mut run_segments = Vec::new();
        {
            let dictionary = DictionaryView::parse(dictionary.value())
                .and_then(DictionaryView::validate_order)
                .map_err(packed_err)?;
            for value in candidates {
                let segment = SegmentView::parse(value.value()).map_err(packed_err)?;
                segment.validate(dictionary).map_err(packed_err)?;
                let mut matches = false;
                for prefix in scan.prefixes {
                    matches |= segment.prefix(prefix).map_err(packed_err)?.is_some();
                }
                if matches {
                    run_segments.push(value);
                }
            }
        }
        if run_segments.is_empty() {
            continue;
        }
        let mut blocks = Vec::new();
        for level in 0..MAX_DEATH_BLOCKS {
            let key = death_key(meta.run_id, level);
            if let Some(value) = catalog.get(key.as_slice()).map_err(persist_err)? {
                blocks.push(DeadKeys::decode(value.value()).map_err(packed_err)?);
            }
        }
        loaded.push((
            dictionary,
            run_segments,
            merge_dead_blocks(&blocks).map_err(packed_err)?,
        ));
    }
    let before = scan
        .before
        .map(|cursor| (cursor.created_at.as_secs(), *cursor.event_id.as_bytes()));
    let mut sources = Vec::new();
    for (dictionary, segments, dead) in &loaded {
        // Already parsed and validated above; this is the borrow the cursors
        // actually hold, not a second decoded copy of the index.
        let dictionary = DictionaryView::parse(dictionary.value()).map_err(packed_err)?;
        for bytes in segments {
            let segment = SegmentView::parse(bytes.value()).map_err(packed_err)?;
            for prefix in scan.prefixes {
                let Some(list) = segment.prefix(prefix).map_err(packed_err)? else {
                    continue;
                };
                sources.push(ScanSource {
                    cursor: list
                        .cursor(dictionary, before, scan.since, scan.until)
                        .map_err(packed_err)?,
                    dead: dead.as_ref(),
                });
            }
        }
    }

    let mut heap = BinaryHeap::with_capacity(sources.len());
    for (source, run) in sources.iter_mut().enumerate() {
        if let Some(event) = run.cursor.next_live(run.dead).map_err(packed_err)? {
            visited();
            heap.push(ScanHead { event, source });
        }
    }
    let mut seen = HashSet::new();
    let mut output = scan.limit.map_or_else(Vec::new, Vec::with_capacity);
    while let Some(head) = heap.pop() {
        let source = &mut sources[head.source];
        if let Some(event) = source.cursor.next_live(source.dead).map_err(packed_err)? {
            visited();
            heap.push(ScanHead {
                event,
                source: head.source,
            });
        }
        if seen.insert(head.event.event_key) {
            let id = EventId::from_byte_array(head.event.id);
            if let Some(projected) = project(head.event.event_key, id)? {
                output.push(projected);
                if scan.limit.is_some_and(|limit| output.len() == limit) {
                    break;
                }
            }
        }
    }
    Ok(output)
}

fn packed_err(error: impl std::fmt::Display) -> PersistenceError {
    PersistenceError::invariant(format!("packed postings: {error}"))
}

/// Decode the whole packed run catalog and prove its relational invariants
/// before anything acts on it (#790).
///
/// Three separate facts, none of which any individual row can carry: each
/// run-metadata value decodes and agrees with its own key; the runs have
/// unique ids and non-overlapping event-key ranges ([`validate_run_metas`]);
/// and the range-index column is an exact bijection with them — no missing
/// entry, no duplicate range, no wrong id, no orphan. Bounded by the live run
/// count, which levelled compaction keeps small.
fn load_run_catalog(
    catalog: &impl ReadableTable<&'static [u8], &'static [u8]>,
) -> Result<Vec<RunMeta>, PersistenceError> {
    let metas = catalog_run_metas(catalog)?;
    validate_run_metas(&metas).map_err(packed_err)?;
    if catalog_column_len(catalog, CATALOG_BY_MIN)? != metas.len() as u64 {
        return Err(packed_err(
            "packed run-range index does not match the run catalog",
        ));
    }
    for meta in &metas {
        let mapped = catalog
            .get(catalog_key(CATALOG_BY_MIN, meta.min_event_key).as_slice())
            .map_err(persist_err)?
            .map(|guard| decode_run_id(guard.value()))
            .transpose()?
            .ok_or_else(|| packed_err(format!("run {} has no run-range entry", meta.run_id)))?;
        if mapped != meta.run_id {
            return Err(packed_err(format!(
                "run-range entry for {} names run {mapped}",
                meta.min_event_key
            )));
        }
    }
    Ok(metas)
}

/// Allocate the next packed run id, proving first that doing so cannot
/// overwrite live packed state (#790).
///
/// Every byte involved is a well-typed `u64`, so decoder-level validation
/// cannot see this class at all: a missing or rewound `POSTINGS_NEXT_RUN_ID`
/// in a non-empty catalog hands back an id a live run already owns, and the
/// dictionary/segment/catalog inserts that follow silently overwrite it
/// inside an otherwise valid transaction. The allocator and the catalog are
/// therefore checked as one relational invariant, before any run-owned row
/// is written:
///
/// - a missing allocator is legal only against an empty catalog, where the
///   canonical initial next id is `1`;
/// - against a non-empty catalog it must be present, non-zero, and strictly
///   greater than every live run id;
/// - `u64::MAX` stays typed exhaustion — never wrap, never reuse.
fn allocate_run_id<T: PackedPostingsTxn>(txn: &mut T) -> Result<u64, PersistenceError> {
    let runs = txn.list_run_metas()?;
    // One dictionary per live run, and no dictionary that outlives its run:
    // an orphan here is a run id the allocator could hand out again while
    // its bytes are still on disk.
    if txn.dictionary_count()? != runs.len() as u64 {
        return Err(packed_err(
            "packed dictionaries do not match the run catalog",
        ));
    }
    for meta in &runs {
        if txn.dictionary_get(meta.run_id)?.is_none() {
            return Err(packed_err(format!("run {} has no dictionary", meta.run_id)));
        }
    }
    let highest_live = runs.iter().map(|meta| meta.run_id).max();
    let stored = txn.next_run_id_get()?;
    let run_id = match (stored, highest_live) {
        (None, None) => 1,
        (None, Some(highest)) => {
            return Err(packed_err(format!(
                "packed run allocator is missing while run {highest} is live"
            )));
        }
        (Some(0), _) => return Err(packed_err("packed run allocator is zero")),
        (Some(next), None) => next,
        (Some(next), Some(highest)) if next > highest => next,
        (Some(next), Some(highest)) => {
            return Err(packed_err(format!(
                "packed run allocator {next} would reuse live run id {highest}"
            )));
        }
    };
    let next = run_id
        .checked_add(1)
        .ok_or_else(|| packed_err("run id space exhausted"))?;
    txn.next_run_id_put(next)?;
    Ok(run_id)
}

#[cfg(test)]
fn memberships_for_events(events: &BTreeMap<EventKey, Event>) -> Vec<Membership> {
    let mut memberships = Vec::new();
    let global = Prefix::global();
    for (&event_key, event) in events {
        let run_event = Arc::new(RunEvent {
            created_at: event.created_at.as_secs(),
            id: *event.id.as_bytes(),
            event_key,
        });
        push_membership(&mut memberships, Family::Global, global.clone(), &run_event);
        push_membership(
            &mut memberships,
            Family::Author,
            Prefix::author(*event.pubkey.as_bytes()),
            &run_event,
        );
        push_membership(
            &mut memberships,
            Family::Kind,
            Prefix::kind(event.kind.as_u16().to_be_bytes()),
            &run_event,
        );
        let mut tags = BTreeSet::new();
        for tag in event.tags.iter() {
            let (Some(name), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
                continue;
            };
            tags.insert(tag_index_prefix(name, value));
        }
        for prefix in tags {
            push_membership(
                &mut memberships,
                Family::Tag,
                Prefix::tag(prefix.into()),
                &run_event,
            );
        }
    }
    memberships
}

fn memberships_for_pending(events: &BTreeMap<EventKey, PendingEvent>) -> Vec<Membership> {
    let mut memberships = Vec::new();
    let global = Prefix::global();
    for event in events.values() {
        push_membership(
            &mut memberships,
            Family::Global,
            global.clone(),
            &event.event,
        );
        push_membership(
            &mut memberships,
            Family::Author,
            Prefix::author(event.author),
            &event.event,
        );
        push_membership(
            &mut memberships,
            Family::Kind,
            Prefix::kind(event.kind),
            &event.event,
        );
        for prefix in &event.tags {
            push_membership(
                &mut memberships,
                Family::Tag,
                Prefix::tag(prefix.clone()),
                &event.event,
            );
        }
    }
    memberships
}

fn push_membership(
    memberships: &mut Vec<Membership>,
    family: Family,
    prefix: Prefix,
    event: &Arc<RunEvent>,
) {
    memberships.push(Membership {
        family,
        shard: shard_for(family, prefix.as_bytes()),
        prefix,
        event: event.clone(),
    });
}

fn segment_key(family: Family, shard: u8, run_id: u64) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = family as u8;
    key[1] = shard;
    key[2..].copy_from_slice(&run_id.to_be_bytes());
    key
}

/// Columns of [`POSTINGS_CATALOG`]. The discriminant leads the key, so each
/// column is contiguous and prefix-scannable exactly as its own tree was.
pub(super) const CATALOG_RUN_META: u8 = 0;
pub(super) const CATALOG_DICTIONARY: u8 = 1;
pub(super) const CATALOG_BY_MIN: u8 = 2;
pub(super) const CATALOG_DEATHS: u8 = 3;

pub(super) fn catalog_key(column: u8, id: u64) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[0] = column;
    key[1..].copy_from_slice(&id.to_be_bytes());
    key
}

/// The inclusive bounds of one whole `u64`-keyed catalog column.
pub(super) fn catalog_column_bounds(column: u8) -> ([u8; 9], [u8; 9]) {
    (catalog_key(column, 0), catalog_key(column, u64::MAX))
}

fn death_key(run_id: u64, level: usize) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[..9].copy_from_slice(&catalog_key(CATALOG_DEATHS, run_id));
    key[9] = level as u8;
    key
}

/// Read one catalog column's `u64` payload — the range index's only value
/// shape.
fn decode_run_id(value: &[u8]) -> Result<u64, PersistenceError> {
    let bytes: [u8; 8] = value
        .try_into()
        .map_err(|_| packed_err("packed run-range entry is not eight bytes"))?;
    Ok(u64::from_be_bytes(bytes))
}

/// Every live [`RunMeta`], in run-id order, without the relational checks
/// [`load_run_catalog`] adds.
pub(super) fn catalog_run_metas(
    catalog: &impl ReadableTable<&'static [u8], &'static [u8]>,
) -> Result<Vec<RunMeta>, PersistenceError> {
    let (lower, upper) = catalog_column_bounds(CATALOG_RUN_META);
    let mut metas = Vec::new();
    for row in catalog
        .range(lower.as_slice()..=upper.as_slice())
        .map_err(persist_err)?
    {
        let (key, value) = row.map_err(persist_err)?;
        let meta = RunMeta::decode(value.value()).map_err(packed_err)?;
        let run_id = decode_run_id(&key.value()[1..])?;
        if meta.run_id != run_id {
            return Err(packed_err("run metadata key disagrees with its value"));
        }
        metas.push(meta);
    }
    Ok(metas)
}

/// The number of rows in one `u64`-keyed catalog column. Replaces the O(1)
/// `len()` the separate trees offered; the catalog is bounded by the live run
/// count, which levelled compaction keeps small.
fn catalog_column_len(
    catalog: &impl ReadableTable<&'static [u8], &'static [u8]>,
    column: u8,
) -> Result<u64, PersistenceError> {
    let (lower, upper) = catalog_column_bounds(column);
    let mut count = 0u64;
    for row in catalog
        .range(lower.as_slice()..=upper.as_slice())
        .map_err(persist_err)?
    {
        row.map_err(persist_err)?;
        count += 1;
    }
    Ok(count)
}

fn insert_run<T: PackedPostingsTxn>(
    txn: &mut T,
    meta: RunMeta,
    encoded: super::postings::EncodedRun,
) -> Result<(), PersistenceError> {
    txn.crash_probe("postings-before-segments");
    txn.dictionary_put(meta.run_id, &encoded.dictionary)?;
    for (family, shard, value) in encoded.segments {
        txn.segment_put(family, shard, meta.run_id, &value)?;
    }
    txn.crash_probe("postings-after-segments");
    insert_run_catalog(txn, meta)
}

fn insert_run_catalog<T: PackedPostingsTxn>(
    txn: &mut T,
    meta: RunMeta,
) -> Result<(), PersistenceError> {
    txn.crash_probe("postings-before-catalog");
    txn.run_meta_put(&meta)?;
    txn.by_min_put(meta.min_event_key, meta.run_id)?;
    txn.crash_probe("postings-after-catalog");
    Ok(())
}

pub(super) fn apply_deaths<T: PackedPostingsTxn>(
    txn: &mut T,
    deaths: &BTreeSet<EventKey>,
) -> Result<(), PersistenceError> {
    // Runs have unique, non-overlapping ranges (`list_run_metas` proves it),
    // and the live run count stays small under levelled compaction, so
    // resolving every death against one loaded list costs one round trip
    // total rather than one per key.
    let runs = txn.list_run_metas()?;
    let mut by_run: BTreeMap<u64, Vec<EventKey>> = BTreeMap::new();
    for &event_key in deaths {
        if let Some(meta) = runs
            .iter()
            .find(|meta| (meta.min_event_key..=meta.max_event_key).contains(&event_key))
        {
            by_run.entry(meta.run_id).or_default().push(event_key);
        }
    }
    for (run_id, keys) in by_run {
        apply_run_deaths(txn, run_id, keys)?;
    }
    Ok(())
}

pub(super) fn apply_run_deaths<T: PackedPostingsTxn>(
    txn: &mut T,
    run_id: u64,
    keys: Vec<EventKey>,
) -> Result<(), PersistenceError> {
    let mut meta = txn
        .run_meta_get(run_id)?
        .ok_or_else(|| packed_err("death target has no run metadata"))?;

    // Ground truth for how many events this run ever held. A run's
    // dictionary is written once at publication and never mutated again, so
    // its length -- not the stored, arithmetically-updated `live_events`
    // counter -- is what `live_events` must be computed *from* below (#1817).
    // `DictionaryView::parse` only reads the header; this costs nothing more
    // than the lookup already required.
    let dictionary_bytes = txn
        .dictionary_get(run_id)?
        .ok_or_else(|| packed_err(format!("run {run_id} has no dictionary")))?;
    let total_events = DictionaryView::parse(&dictionary_bytes)
        .map_err(packed_err)?
        .len() as u64;
    drop(dictionary_bytes);

    let mut existing = Vec::new();
    for level in 0..MAX_DEATH_BLOCKS {
        if let Some(value) = txn.death_block_get(run_id, level)? {
            existing.push(value);
        }
    }
    let existing_union = merge_dead_blocks(&existing).map_err(packed_err)?;
    let mut fresh: Vec<_> = keys
        .into_iter()
        .filter(|key| {
            existing_union
                .as_ref()
                .is_none_or(|dead| !dead.contains(*key))
        })
        .collect();
    fresh.sort_unstable();
    fresh.dedup();
    if fresh.is_empty() {
        return Ok(());
    }
    let fresh_count = fresh.len() as u64;
    // Derived, not decremented: `live_events` is recomputed from the run's
    // total event count and the full merged dead set every time a death is
    // applied, so a previously stored value -- however it got there -- is
    // never trusted, only overwritten. This is what makes the counter's
    // drift unrepresentable rather than merely reconciled (#1817): there is
    // no arithmetic step left that could compound an earlier mistake.
    let existing_dead_count = existing_union.as_ref().map_or(0, |dead| dead.len() as u64);
    let total_dead = existing_dead_count
        .checked_add(fresh_count)
        .ok_or_else(|| packed_err("run dead-key count overflow"))?;
    meta.live_events = total_events.checked_sub(total_dead).ok_or_else(|| {
        packed_err(format!(
            "run {run_id} has {total_dead} dead events but only {total_events} in its dictionary"
        ))
    })?;
    if meta.live_events == 0 {
        return delete_run(txn, meta);
    }

    let mut carry = DeadKeys::new(fresh).map_err(packed_err)?;
    for level in 0..MAX_DEATH_BLOCKS {
        let prior = txn.death_block_get(run_id, level)?;
        if let Some(prior) = prior {
            txn.death_block_delete(run_id, level)?;
            carry = merge_dead_blocks(&[prior, carry])
                .map_err(packed_err)?
                .expect("two nonempty death blocks");
        } else {
            txn.crash_probe("postings-before-death");
            txn.death_block_put(run_id, level, &carry)?;
            txn.run_meta_put(&meta)?;
            txn.crash_probe("postings-after-death");
            return Ok(());
        }
    }

    // Falling out of the loop means every level was occupied, so `carry` is
    // already `fresh` merged with all `MAX_DEATH_BLOCKS` prior blocks — the
    // carry removed each one as it consumed it. Re-merging it against
    // `existing` would hand `merge_dead_blocks` that same union plus the 8
    // blocks a second time, exceeding the fan-in bound it enforces and
    // failing the whole write transaction. That made this rewrite path
    // unreachable precisely when it became necessary: the only way to arrive
    // here is a full counter, which is exactly the case the re-merge
    // rejected.
    rewrite_run_without_dead(txn, meta, &carry)
}

fn stream_compaction_cohort<T: PackedPostingsTxn>(
    txn: &mut T,
    cohort: &[RunMeta],
    dead: &[Option<DeadKeys>],
) -> Result<Option<(u64, u64, u64, u64)>, PersistenceError> {
    if cohort.len() != dead.len() {
        return Err(packed_err("compaction death-map count mismatch"));
    }
    let mut dictionary_entries = Vec::new();
    let mut ordinal_maps = Vec::with_capacity(cohort.len());
    for (source, meta) in cohort.iter().enumerate() {
        let dictionary_bytes = txn
            .dictionary_get(meta.run_id)?
            .ok_or_else(|| packed_err("run has no dictionary"))?;
        let dictionary = DictionaryView::parse(&dictionary_bytes)
            .and_then(DictionaryView::validate)
            .map_err(packed_err)?;
        let mut ordinal_map = Vec::with_capacity(dictionary.len());
        for ordinal in 0..dictionary.len() {
            let (event_key, id) = dictionary.entry(ordinal).map_err(packed_err)?;
            if dead[source]
                .as_ref()
                .is_some_and(|keys| keys.contains(event_key))
            {
                ordinal_map.push(None);
                continue;
            }
            if dictionary_entries
                .last()
                .is_some_and(|(prior, _)| *prior >= event_key)
            {
                return Err(packed_err(
                    "compaction cohort dictionaries are not range ordered",
                ));
            }
            let output_ordinal = u32::try_from(dictionary_entries.len())
                .map_err(|_| packed_err("compaction dictionary exceeds u32"))?;
            dictionary_entries.push((event_key, id));
            ordinal_map.push(Some(output_ordinal));
        }
        ordinal_maps.push(ordinal_map);
    }
    if dictionary_entries.is_empty() {
        return Ok(None);
    }
    let min_event_key = dictionary_entries
        .first()
        .expect("nonempty compaction dictionary")
        .0;
    let max_event_key = dictionary_entries
        .last()
        .expect("nonempty compaction dictionary")
        .0;
    let live_events = dictionary_entries.len() as u64;
    let dictionary = encode_dictionary(&dictionary_entries).map_err(packed_err)?;
    drop(dictionary_entries);
    let run_id = allocate_run_id(txn)?;
    txn.crash_probe("postings-before-compaction-output");
    txn.dictionary_put(run_id, &dictionary)?;
    drop(dictionary);
    // Re-read what was actually persisted rather than reusing the in-memory
    // buffer: every source dictionary was validated above, source event-key
    // ranges are disjoint, and the canonical EVENT_IDS invariant forbids one
    // id from reaching two event keys, but this still proves the bytes that
    // will be read back on reopen round-trip, not just the ones computed in
    // memory (#1820: the LMDB benchmark used to skip this round-trip check).
    let output_dictionary_bytes = txn
        .dictionary_get(run_id)?
        .ok_or_else(|| packed_err("new compacted run has no dictionary"))?;
    let output_dictionary =
        DictionaryView::parse(&output_dictionary_bytes).map_err(packed_err)?;

    let mut postings = 0u64;
    for family in Family::ALL {
        for shard in 0..=SHARD_MASK {
            let mut segment_values = Vec::new();
            for (source, meta) in cohort.iter().enumerate() {
                let Some(value) = txn.segment_get(family, shard, meta.run_id)? else {
                    continue;
                };
                segment_values.push((source, value));
            }
            if segment_values.is_empty() {
                continue;
            }
            let mut segment_views = Vec::with_capacity(segment_values.len());
            for (source, value) in &segment_values {
                let segment = SegmentView::parse(value).map_err(packed_err)?;
                segment_views.push((*source, segment));
            }
            let sources: Vec<_> = segment_views
                .iter()
                .map(|(source, segment)| CompactionSegmentSource {
                    segment: *segment,
                    ordinal_map: &ordinal_maps[*source],
                })
                .collect();
            let Some(compacted) =
                compact_segment(family, shard, &sources, output_dictionary).map_err(packed_err)?
            else {
                continue;
            };
            postings = postings.saturating_add(compacted.postings);
            txn.segment_put(family, shard, run_id, &compacted.value)?;
        }
    }
    if postings == 0 {
        return Err(packed_err(
            "nonempty compaction dictionary produced no live segments",
        ));
    }
    txn.crash_probe("postings-after-compaction-output");
    Ok(Some((run_id, min_event_key, max_event_key, live_events)))
}

fn load_run_deaths<T: PackedPostingsTxn>(
    txn: &mut T,
    run_id: u64,
) -> Result<Option<DeadKeys>, PersistenceError> {
    let mut blocks = Vec::new();
    for level in 0..MAX_DEATH_BLOCKS {
        if let Some(value) = txn.death_block_get(run_id, level)? {
            blocks.push(value);
        }
    }
    merge_dead_blocks(&blocks).map_err(packed_err)
}

fn compact_overfull_levels<T: PackedPostingsTxn>(txn: &mut T) -> Result<(), PersistenceError> {
    let mut level = 0u8;
    loop {
        loop {
            let fan_in = if level == 0 {
                BASE_RUN_FAN_IN
            } else {
                LARGE_RUN_FAN_IN
            };
            let mut cohort = Vec::new();
            let mut has_higher_level = false;
            for meta in txn.list_run_metas()? {
                if meta.level == level {
                    cohort.push(meta);
                } else if meta.level > level {
                    has_higher_level = true;
                }
            }
            if cohort.len() < fan_in {
                if has_higher_level {
                    level = level
                        .checked_add(1)
                        .ok_or_else(|| packed_err("packed run level space exhausted"))?;
                } else {
                    return Ok(());
                }
                break;
            }
            cohort.sort_unstable_by_key(|meta| meta.min_event_key);
            cohort.truncate(fan_in);
            compact_cohort(txn, level, &cohort)?;
        }
    }
}

/// A whole compaction cohort dying is legitimate here -- level-fan-in
/// grouping says nothing about liveness, so every run it merges may turn out
/// to have no survivors -- which is why, unlike [`rewrite_run_without_dead`],
/// this deletes its inputs unconditionally once `stream_compaction_cohort`
/// returns and treats `None` as "nothing to publish," not an error.
fn compact_cohort<T: PackedPostingsTxn>(
    txn: &mut T,
    level: u8,
    cohort: &[RunMeta],
) -> Result<(), PersistenceError> {
    let output_level = level
        .checked_add(1)
        .ok_or_else(|| packed_err("packed run level space exhausted"))?;
    let dead: Vec<_> = cohort
        .iter()
        .map(|meta| load_run_deaths(txn, meta.run_id))
        .collect::<Result<_, _>>()?;
    let output = stream_compaction_cohort(txn, cohort, &dead)?;
    for &meta in cohort {
        delete_run(txn, meta)?;
    }
    txn.note_compaction(cohort.len(), output.map(|(_, _, _, live_events)| live_events));
    let Some((run_id, min_event_key, max_event_key, live_events)) = output else {
        return Ok(());
    };
    insert_run_catalog(
        txn,
        RunMeta {
            run_id,
            level: output_level,
            min_event_key,
            max_event_key,
            live_events,
        },
    )
}

/// Unlike [`compact_cohort`], whose input cohort can legitimately be dead in
/// full (every run it merges may have no survivors), this function's only
/// caller ([`apply_run_deaths`]) reaches it with an `old_meta.live_events`
/// already proven positive -- the zero case takes the `delete_run` branch
/// above it and never gets here. A `None` result therefore means the death
/// set handed to it does not agree with that proof, and the correct response
/// is to refuse, not to delete `old_meta` and quietly replace it with
/// nothing (#1817): sharing `compact_cohort`'s tolerance for `None` is
/// exactly how that silent destruction became expressible. The check runs,
/// and `old_meta` stays untouched, before `delete_run` is ever called.
pub(super) fn rewrite_run_without_dead<T: PackedPostingsTxn>(
    txn: &mut T,
    old_meta: RunMeta,
    dead: &DeadKeys,
) -> Result<(), PersistenceError> {
    let output = stream_compaction_cohort(txn, &[old_meta], &[Some(dead.clone())])?;
    let (run_id, min_event_key, max_event_key, live_events) = output.ok_or_else(|| {
        packed_err(format!(
            "run {} has live_events={} but rewriting its death set produced no surviving \
             events; refusing to delete it in favor of nothing",
            old_meta.run_id, old_meta.live_events
        ))
    })?;
    txn.note_compaction(1, Some(live_events));
    delete_run(txn, old_meta)?;
    insert_run_catalog(
        txn,
        RunMeta {
            run_id,
            level: old_meta.level,
            min_event_key,
            max_event_key,
            live_events,
        },
    )
}

fn delete_run<T: PackedPostingsTxn>(txn: &mut T, meta: RunMeta) -> Result<(), PersistenceError> {
    for family in Family::ALL {
        for shard in 0..=SHARD_MASK {
            txn.segment_delete(family, shard, meta.run_id)?;
        }
    }
    // One logical delete's worth of rows, wherever the backend keeps them:
    // dictionary, metadata, range-index entry and every death block.
    txn.dictionary_delete(meta.run_id)?;
    txn.run_meta_delete(meta.run_id)?;
    txn.by_min_delete(meta.min_event_key)?;
    for level in 0..MAX_DEATH_BLOCKS {
        txn.death_block_delete(meta.run_id, level)?;
    }
    Ok(())
}
