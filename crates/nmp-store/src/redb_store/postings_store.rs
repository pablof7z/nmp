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

#[cfg(test)]
use super::postings::validate_run_metas;
use super::postings::{
    compact_segment, encode_dictionary, encode_run, merge_dead_blocks, shard_for,
    CompactionSegmentSource, DeadKeys, DictionaryView, Family, Membership, PostingCursor, Prefix,
    RunEvent, RunMeta, SegmentView, MAX_DEATH_BLOCKS,
};
use super::query::tag_index_prefix;
use super::schema::{
    persist_err, EventKey, EVENTS, POSTINGS_DEAD_KEYS, POSTINGS_DICTIONARIES, POSTINGS_META,
    POSTINGS_NEXT_RUN_ID, POSTINGS_READY, POSTINGS_RUN_BY_MIN, POSTINGS_RUN_META,
    POSTINGS_SEGMENTS,
};
use super::{Event, EventCursor, EventId, PersistenceError, StoredEventView};

const RUN_FAN_IN: usize = 8;
const MIGRATION_RUN_EVENTS: usize = 8_192;

#[cfg(test)]
pub(super) fn assert_packed_integrity(
    read_txn: &redb::ReadTransaction,
    canonical: &BTreeMap<EventKey, Event>,
) {
    let run_meta = read_txn
        .open_table(POSTINGS_RUN_META)
        .expect("audit packed run metadata");
    let dictionaries = read_txn
        .open_table(POSTINGS_DICTIONARIES)
        .expect("audit packed dictionaries");
    let segments = read_txn
        .open_table(POSTINGS_SEGMENTS)
        .expect("audit packed segments");
    let deaths = read_txn
        .open_table(POSTINGS_DEAD_KEYS)
        .expect("audit packed deaths");
    let by_min = read_txn
        .open_table(POSTINGS_RUN_BY_MIN)
        .expect("audit packed run ranges");
    let meta = read_txn
        .open_table(POSTINGS_META)
        .expect("audit packed metadata");
    assert_eq!(
        meta.get(POSTINGS_READY)
            .expect("audit packed readiness")
            .expect("packed readiness exists")
            .value(),
        1
    );

    let metas: Vec<_> = run_meta
        .iter()
        .expect("iterate packed runs")
        .map(|row| {
            let (run_id, value) = row.expect("read packed run");
            let decoded = RunMeta::decode(value.value()).expect("decode packed run metadata");
            assert_eq!(run_id.value(), decoded.run_id);
            decoded
        })
        .collect();
    validate_run_metas(&metas).expect("packed run ranges are valid");
    assert_eq!(
        by_min.len().expect("count packed run ranges"),
        metas.len() as u64
    );
    assert_eq!(
        dictionaries.len().expect("count packed dictionaries"),
        metas.len() as u64
    );

    let mut actual = Vec::new();
    let mut seen_segments = 0u64;
    for run in &metas {
        assert_eq!(
            by_min
                .get(run.min_event_key)
                .expect("audit packed run range")
                .expect("packed run range exists")
                .value(),
            run.run_id
        );
        let dictionary_bytes = dictionaries
            .get(run.run_id)
            .expect("audit packed dictionary")
            .expect("packed dictionary exists");
        let dictionary = DictionaryView::parse(dictionary_bytes.value())
            .and_then(DictionaryView::validate)
            .expect("packed dictionary is valid");
        let mut blocks = Vec::new();
        for level in 0..MAX_DEATH_BLOCKS {
            let key = death_key(run.run_id, level);
            if let Some(value) = deaths.get(key.as_slice()).expect("audit death block") {
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

#[derive(Default)]
pub(super) struct PostingsBatch {
    additions: BTreeMap<EventKey, PendingEvent>,
    deaths: BTreeSet<EventKey>,
}

struct PendingEvent {
    event: Arc<RunEvent>,
    author: [u8; 32],
    kind: [u8; 2],
    tags: Vec<Arc<[u8]>>,
}

impl PendingEvent {
    fn prepare(event: &Event, event_key: EventKey) -> Self {
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
        if !self.deaths.is_empty() {
            apply_deaths(write_txn, &self.deaths)?;
        }
        if !self.additions.is_empty() {
            publish_pending(write_txn, &self.additions)?;
        }
        self.additions.clear();
        self.deaths.clear();
        Ok(())
    }
}

pub(super) fn rebuild_from_canonical(
    write_txn: &redb::WriteTransaction,
) -> Result<(), PersistenceError> {
    write_txn
        .delete_table(POSTINGS_SEGMENTS)
        .map_err(persist_err)?;
    write_txn
        .delete_table(POSTINGS_DICTIONARIES)
        .map_err(persist_err)?;
    write_txn
        .delete_table(POSTINGS_RUN_META)
        .map_err(persist_err)?;
    write_txn
        .delete_table(POSTINGS_RUN_BY_MIN)
        .map_err(persist_err)?;
    write_txn
        .delete_table(POSTINGS_DEAD_KEYS)
        .map_err(persist_err)?;
    write_txn.delete_table(POSTINGS_META).map_err(persist_err)?;

    write_txn
        .open_table(POSTINGS_SEGMENTS)
        .map_err(persist_err)?;
    write_txn
        .open_table(POSTINGS_DICTIONARIES)
        .map_err(persist_err)?;
    write_txn
        .open_table(POSTINGS_RUN_META)
        .map_err(persist_err)?;
    write_txn
        .open_table(POSTINGS_RUN_BY_MIN)
        .map_err(persist_err)?;
    write_txn
        .open_table(POSTINGS_DEAD_KEYS)
        .map_err(persist_err)?;
    write_txn.open_table(POSTINGS_META).map_err(persist_err)?;

    let events = write_txn.open_table(EVENTS).map_err(persist_err)?;
    let mut batch = BTreeMap::new();
    for row in events.iter().map_err(persist_err)? {
        let (event_key, value) = row.map_err(persist_err)?;
        let event = StoredEventView::from_trusted(value.value())
            .map_err(|error| packed_err(format!("decode migration event: {error:?}")))?
            .materialize_event()
            .map_err(|error| packed_err(format!("materialize migration event: {error:?}")))?;
        batch.insert(event_key.value(), event);
        if batch.len() == MIGRATION_RUN_EVENTS {
            publish_events(write_txn, &batch)?;
            batch.clear();
        }
    }
    drop(events);
    if !batch.is_empty() {
        publish_events(write_txn, &batch)?;
    }
    let mut meta = write_txn.open_table(POSTINGS_META).map_err(persist_err)?;
    meta.insert(POSTINGS_READY, 1).map_err(persist_err)?;
    Ok(())
}

fn publish_events(
    write_txn: &redb::WriteTransaction,
    events: &BTreeMap<EventKey, Event>,
) -> Result<(), PersistenceError> {
    let run_id = allocate_run_id(write_txn)?;
    let memberships = memberships_for_events(events);
    let encoded = encode_run(memberships).map_err(packed_err)?;
    let min_event_key = *events.first_key_value().expect("nonempty additions").0;
    let max_event_key = *events.last_key_value().expect("nonempty additions").0;
    let meta = RunMeta {
        run_id,
        level: 0,
        min_event_key,
        max_event_key,
        live_events: encoded.dictionary_entries,
    };
    insert_run(write_txn, meta, encoded)?;
    compact_overfull_levels(write_txn)
}

fn publish_pending(
    write_txn: &redb::WriteTransaction,
    events: &BTreeMap<EventKey, PendingEvent>,
) -> Result<(), PersistenceError> {
    let run_id = allocate_run_id(write_txn)?;
    let encoded = encode_run(memberships_for_pending(events)).map_err(packed_err)?;
    let min_event_key = *events.first_key_value().expect("nonempty additions").0;
    let max_event_key = *events.last_key_value().expect("nonempty additions").0;
    let meta = RunMeta {
        run_id,
        level: 0,
        min_event_key,
        max_event_key,
        live_events: encoded.dictionary_entries,
    };
    insert_run(write_txn, meta, encoded)?;
    compact_overfull_levels(write_txn)
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
    let run_meta = read_txn
        .open_table(POSTINGS_RUN_META)
        .map_err(persist_err)?;
    let dictionaries = read_txn
        .open_table(POSTINGS_DICTIONARIES)
        .map_err(persist_err)?;
    let segments = read_txn
        .open_table(POSTINGS_SEGMENTS)
        .map_err(persist_err)?;
    let deaths = read_txn
        .open_table(POSTINGS_DEAD_KEYS)
        .map_err(persist_err)?;
    let shards: BTreeSet<_> = scan
        .prefixes
        .iter()
        .map(|prefix| shard_for(scan.family, prefix))
        .collect();
    let mut loaded = Vec::new();
    for row in run_meta.iter().map_err(persist_err)? {
        let (run_id, value) = row.map_err(persist_err)?;
        let meta = RunMeta::decode(value.value()).map_err(packed_err)?;
        if meta.run_id != run_id.value() {
            return Err(packed_err("run metadata key disagrees with its value"));
        }
        let mut run_segments = Vec::new();
        for &shard in &shards {
            let key = segment_key(scan.family, shard, meta.run_id);
            let Some(value) = segments.get(key.as_slice()).map_err(persist_err)? else {
                continue;
            };
            let segment = SegmentView::parse(value.value()).map_err(packed_err)?;
            let mut matches = false;
            for prefix in scan.prefixes {
                matches |= segment.prefix(prefix).map_err(packed_err)?.is_some();
            }
            if matches {
                run_segments.push(value);
            }
        }
        if run_segments.is_empty() {
            continue;
        }
        let dictionary = dictionaries
            .get(meta.run_id)
            .map_err(persist_err)?
            .ok_or_else(|| packed_err("run has no dictionary"))?;
        let mut blocks = Vec::new();
        for level in 0..MAX_DEATH_BLOCKS {
            let key = death_key(meta.run_id, level);
            if let Some(value) = deaths.get(key.as_slice()).map_err(persist_err)? {
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
    PersistenceError(format!("packed postings: {error}"))
}

fn allocate_run_id(write_txn: &redb::WriteTransaction) -> Result<u64, PersistenceError> {
    let mut meta = write_txn.open_table(POSTINGS_META).map_err(persist_err)?;
    let run_id = meta
        .get(POSTINGS_NEXT_RUN_ID)
        .map_err(persist_err)?
        .map(|guard| guard.value())
        .unwrap_or(1);
    let next = run_id
        .checked_add(1)
        .ok_or_else(|| packed_err("run id space exhausted"))?;
    meta.insert(POSTINGS_NEXT_RUN_ID, next)
        .map_err(persist_err)?;
    Ok(run_id)
}

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

fn death_key(run_id: u64, level: usize) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[..8].copy_from_slice(&run_id.to_be_bytes());
    key[8] = level as u8;
    key
}

fn insert_run(
    write_txn: &redb::WriteTransaction,
    meta: RunMeta,
    encoded: super::postings::EncodedRun,
) -> Result<(), PersistenceError> {
    let mut dictionaries = write_txn
        .open_table(POSTINGS_DICTIONARIES)
        .map_err(persist_err)?;
    dictionaries
        .insert(meta.run_id, encoded.dictionary.as_slice())
        .map_err(persist_err)?;
    let mut segments = write_txn
        .open_table(POSTINGS_SEGMENTS)
        .map_err(persist_err)?;
    for (family, shard, value) in encoded.segments {
        let key = segment_key(family, shard, meta.run_id);
        segments
            .insert(key.as_slice(), value.as_slice())
            .map_err(persist_err)?;
    }
    insert_run_catalog(write_txn, meta)
}

fn insert_run_catalog(
    write_txn: &redb::WriteTransaction,
    meta: RunMeta,
) -> Result<(), PersistenceError> {
    let encoded_meta = meta.encode().map_err(packed_err)?;
    let mut run_meta = write_txn
        .open_table(POSTINGS_RUN_META)
        .map_err(persist_err)?;
    run_meta
        .insert(meta.run_id, encoded_meta.as_slice())
        .map_err(persist_err)?;
    let mut by_min = write_txn
        .open_table(POSTINGS_RUN_BY_MIN)
        .map_err(persist_err)?;
    by_min
        .insert(meta.min_event_key, meta.run_id)
        .map_err(persist_err)?;
    Ok(())
}

fn apply_deaths(
    write_txn: &redb::WriteTransaction,
    deaths: &BTreeSet<EventKey>,
) -> Result<(), PersistenceError> {
    let mut by_run: BTreeMap<u64, Vec<EventKey>> = BTreeMap::new();
    {
        let by_min = write_txn
            .open_table(POSTINGS_RUN_BY_MIN)
            .map_err(persist_err)?;
        let run_meta = write_txn
            .open_table(POSTINGS_RUN_META)
            .map_err(persist_err)?;
        for &event_key in deaths {
            let Some(candidate) = by_min
                .range(..=event_key)
                .map_err(persist_err)?
                .next_back()
                .transpose()
                .map_err(persist_err)?
            else {
                // A pre-v8 row is rebuilt by the schema migration; packed
                // artifacts are not query-authoritative before that flip.
                continue;
            };
            let run_id = candidate.1.value();
            let Some(value) = run_meta.get(run_id).map_err(persist_err)? else {
                return Err(packed_err("run-range entry has no metadata"));
            };
            let meta = RunMeta::decode(value.value()).map_err(packed_err)?;
            if event_key <= meta.max_event_key {
                by_run.entry(run_id).or_default().push(event_key);
            }
        }
    }
    for (run_id, keys) in by_run {
        apply_run_deaths(write_txn, run_id, keys)?;
    }
    Ok(())
}

fn apply_run_deaths(
    write_txn: &redb::WriteTransaction,
    run_id: u64,
    keys: Vec<EventKey>,
) -> Result<(), PersistenceError> {
    let mut run_meta = write_txn
        .open_table(POSTINGS_RUN_META)
        .map_err(persist_err)?;
    let value = run_meta
        .get(run_id)
        .map_err(persist_err)?
        .ok_or_else(|| packed_err("death target has no run metadata"))?;
    let mut meta = RunMeta::decode(value.value()).map_err(packed_err)?;
    drop(value);

    let mut death_table = write_txn
        .open_table(POSTINGS_DEAD_KEYS)
        .map_err(persist_err)?;
    let mut existing = Vec::new();
    for level in 0..MAX_DEATH_BLOCKS {
        let key = death_key(run_id, level);
        if let Some(value) = death_table.get(key.as_slice()).map_err(persist_err)? {
            existing.push(DeadKeys::decode(value.value()).map_err(packed_err)?);
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
    if fresh_count > meta.live_events {
        return Err(packed_err("death count exceeds run live count"));
    }
    meta.live_events -= fresh_count;
    if meta.live_events == 0 {
        drop(death_table);
        drop(run_meta);
        return delete_run(write_txn, meta);
    }

    let mut carry = DeadKeys::new(fresh).map_err(packed_err)?;
    for level in 0..MAX_DEATH_BLOCKS {
        let key = death_key(run_id, level);
        let prior = death_table
            .get(key.as_slice())
            .map_err(persist_err)?
            .map(|value| DeadKeys::decode(value.value()).map_err(packed_err))
            .transpose()?;
        if let Some(prior) = prior {
            death_table.remove(key.as_slice()).map_err(persist_err)?;
            carry = merge_dead_blocks(&[prior, carry])
                .map_err(packed_err)?
                .expect("two nonempty death blocks");
        } else {
            let encoded = carry.encode().map_err(packed_err)?;
            death_table
                .insert(key.as_slice(), encoded.as_slice())
                .map_err(persist_err)?;
            let encoded_meta = meta.encode().map_err(packed_err)?;
            run_meta
                .insert(run_id, encoded_meta.as_slice())
                .map_err(persist_err)?;
            return Ok(());
        }
    }

    existing.push(carry);
    let all_dead = merge_dead_blocks(&existing)
        .map_err(packed_err)?
        .expect("fresh deaths are nonempty");
    drop(death_table);
    drop(run_meta);
    rewrite_run_without_dead(write_txn, meta, &all_dead)
}

fn stream_compaction_cohort(
    write_txn: &redb::WriteTransaction,
    cohort: &[RunMeta],
    dead: &[Option<DeadKeys>],
) -> Result<Option<(u64, u64, u64, u64)>, PersistenceError> {
    if cohort.len() != dead.len() {
        return Err(packed_err("compaction death-map count mismatch"));
    }
    let dictionaries = write_txn
        .open_table(POSTINGS_DICTIONARIES)
        .map_err(persist_err)?;
    let mut dictionary_entries = Vec::new();
    let mut ordinal_maps = Vec::with_capacity(cohort.len());
    for (source, meta) in cohort.iter().enumerate() {
        let dictionary_bytes = dictionaries
            .get(meta.run_id)
            .map_err(persist_err)?
            .ok_or_else(|| packed_err("run has no dictionary"))?;
        let dictionary = DictionaryView::parse(dictionary_bytes.value())
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
    let output_dictionary = DictionaryView::parse(&dictionary)
        .and_then(DictionaryView::validate)
        .map_err(packed_err)?;
    drop(dictionaries);
    let run_id = allocate_run_id(write_txn)?;
    let mut dictionaries = write_txn
        .open_table(POSTINGS_DICTIONARIES)
        .map_err(persist_err)?;
    dictionaries
        .insert(run_id, dictionary.as_slice())
        .map_err(persist_err)?;
    drop(dictionaries);

    let mut segments = write_txn
        .open_table(POSTINGS_SEGMENTS)
        .map_err(persist_err)?;
    let mut postings = 0u64;
    for family in Family::ALL {
        for shard in 0..=super::postings::SHARD_MASK {
            let mut segment_values = Vec::new();
            for (source, meta) in cohort.iter().enumerate() {
                let key = segment_key(family, shard, meta.run_id);
                let Some(value) = segments.get(key.as_slice()).map_err(persist_err)? else {
                    continue;
                };
                segment_values.push((source, value.value().to_vec()));
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
            let key = segment_key(family, shard, run_id);
            segments
                .insert(key.as_slice(), compacted.value.as_slice())
                .map_err(persist_err)?;
        }
    }
    drop(segments);
    if postings == 0 {
        return Err(packed_err(
            "nonempty compaction dictionary produced no live segments",
        ));
    }
    Ok(Some((run_id, min_event_key, max_event_key, live_events)))
}

fn load_run_deaths(
    write_txn: &redb::WriteTransaction,
    run_id: u64,
) -> Result<Option<DeadKeys>, PersistenceError> {
    let deaths = write_txn
        .open_table(POSTINGS_DEAD_KEYS)
        .map_err(persist_err)?;
    let mut blocks = Vec::new();
    for level in 0..MAX_DEATH_BLOCKS {
        let key = death_key(run_id, level);
        if let Some(value) = deaths.get(key.as_slice()).map_err(persist_err)? {
            blocks.push(DeadKeys::decode(value.value()).map_err(packed_err)?);
        }
    }
    merge_dead_blocks(&blocks).map_err(packed_err)
}

fn compact_overfull_levels(write_txn: &redb::WriteTransaction) -> Result<(), PersistenceError> {
    let mut level = 0u8;
    loop {
        loop {
            let run_meta = write_txn
                .open_table(POSTINGS_RUN_META)
                .map_err(persist_err)?;
            let mut cohort = Vec::new();
            let mut has_higher_level = false;
            for row in run_meta.iter().map_err(persist_err)? {
                let (_run_id, value) = row.map_err(persist_err)?;
                let meta = RunMeta::decode(value.value()).map_err(packed_err)?;
                if meta.level == level {
                    cohort.push(meta);
                } else if meta.level > level {
                    has_higher_level = true;
                }
            }
            drop(run_meta);
            if cohort.len() < RUN_FAN_IN {
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
            cohort.truncate(RUN_FAN_IN);
            compact_cohort(write_txn, level, &cohort)?;
        }
    }
}

fn compact_cohort(
    write_txn: &redb::WriteTransaction,
    level: u8,
    cohort: &[RunMeta],
) -> Result<(), PersistenceError> {
    let output_level = level
        .checked_add(1)
        .ok_or_else(|| packed_err("packed run level space exhausted"))?;
    let dead: Vec<_> = cohort
        .iter()
        .map(|meta| load_run_deaths(write_txn, meta.run_id))
        .collect::<Result<_, _>>()?;
    let output = stream_compaction_cohort(write_txn, cohort, &dead)?;
    for &meta in cohort {
        delete_run(write_txn, meta)?;
    }
    let Some((run_id, min_event_key, max_event_key, live_events)) = output else {
        return Ok(());
    };
    insert_run_catalog(
        write_txn,
        RunMeta {
            run_id,
            level: output_level,
            min_event_key,
            max_event_key,
            live_events,
        },
    )
}

fn rewrite_run_without_dead(
    write_txn: &redb::WriteTransaction,
    old_meta: RunMeta,
    dead: &DeadKeys,
) -> Result<(), PersistenceError> {
    let output = stream_compaction_cohort(write_txn, &[old_meta], &[Some(dead.clone())])?;
    delete_run(write_txn, old_meta)?;
    let Some((run_id, min_event_key, max_event_key, live_events)) = output else {
        return Ok(());
    };
    insert_run_catalog(
        write_txn,
        RunMeta {
            run_id,
            level: old_meta.level,
            min_event_key,
            max_event_key,
            live_events,
        },
    )
}

fn delete_run(write_txn: &redb::WriteTransaction, meta: RunMeta) -> Result<(), PersistenceError> {
    let mut segments = write_txn
        .open_table(POSTINGS_SEGMENTS)
        .map_err(persist_err)?;
    for family in Family::ALL {
        for shard in 0..=super::postings::SHARD_MASK {
            let key = segment_key(family, shard, meta.run_id);
            segments.remove(key.as_slice()).map_err(persist_err)?;
        }
    }
    let mut dictionaries = write_txn
        .open_table(POSTINGS_DICTIONARIES)
        .map_err(persist_err)?;
    dictionaries.remove(meta.run_id).map_err(persist_err)?;
    let mut run_meta = write_txn
        .open_table(POSTINGS_RUN_META)
        .map_err(persist_err)?;
    run_meta.remove(meta.run_id).map_err(persist_err)?;
    let mut by_min = write_txn
        .open_table(POSTINGS_RUN_BY_MIN)
        .map_err(persist_err)?;
    by_min.remove(meta.min_event_key).map_err(persist_err)?;
    let mut deaths = write_txn
        .open_table(POSTINGS_DEAD_KEYS)
        .map_err(persist_err)?;
    for level in 0..MAX_DEATH_BLOCKS {
        let key = death_key(meta.run_id, level);
        deaths.remove(key.as_slice()).map_err(persist_err)?;
    }
    Ok(())
}
