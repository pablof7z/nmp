//! Versioned random-access packed ordered-postings format for issue #655.
//!
//! Cryptographic ids are already entropy-dense raw 32-byte values. The format
//! stores each id once per immutable run, then addresses it by a compact ordinal
//! from every index membership. Fixed-width postings preserve exact binary seek
//! for cursors and time bounds without repeating ids or decoding earlier rows.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

const SEGMENT_MAGIC: &[u8; 8] = b"NMPPS\0\x03\0";
const DICTIONARY_MAGIC: &[u8; 8] = b"NMPID\0\x02\0";
const DEAD_KEYS_MAGIC: &[u8; 8] = b"NMPPD\0\x03\0";
const RUN_META_MAGIC: &[u8; 8] = b"NMPRM\0\x01\0";
const SEGMENT_HEADER_LEN: usize = 14;
const DICTIONARY_ENTRY_LEN: usize = 40;
const POSTING_ENTRY_LEN: usize = 12;
pub(super) const SHARD_MASK: u8 = 0x3f;
pub(super) const MAX_DEATH_BLOCKS: usize = 8;
const SHARD_KEY: [u8; 32] = [0x91; 32];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(super) enum Family {
    Global = 0,
    Author = 1,
    Kind = 2,
    Tag = 3,
}

impl Family {
    pub(super) const ALL: [Self; 4] = [Self::Global, Self::Author, Self::Kind, Self::Tag];

    pub(super) fn from_u8(value: u8) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|family| *family as u8 == value)
            .ok_or_else(|| format!("unknown packed-postings family {value}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RunEvent {
    pub(super) created_at: u64,
    pub(super) id: [u8; 32],
    pub(super) event_key: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Prefix(Arc<[u8]>);

impl Prefix {
    pub(super) fn global() -> Self {
        Self(Arc::from([]))
    }

    pub(super) fn author(value: [u8; 32]) -> Self {
        Self(Arc::from(value))
    }

    pub(super) fn kind(value: [u8; 2]) -> Self {
        Self(Arc::from(value))
    }

    pub(super) fn tag(value: Arc<[u8]>) -> Self {
        Self(value)
    }

    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn from_bytes(family: Family, value: &[u8]) -> Result<Self, String> {
        match family {
            Family::Global if value.is_empty() => Ok(Self::global()),
            Family::Author if value.len() == 32 => Ok(Self(Arc::from(value))),
            Family::Author => Err("author prefix is not 32 bytes".to_owned()),
            Family::Kind if value.len() == 2 => Ok(Self(Arc::from(value))),
            Family::Kind => Err("kind prefix is not 2 bytes".to_owned()),
            Family::Tag => Ok(Self(Arc::from(value))),
            Family::Global => Err("global prefix is not empty".to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Membership {
    pub(super) family: Family,
    pub(super) shard: u8,
    pub(super) prefix: Prefix,
    pub(super) event: Arc<RunEvent>,
}

#[derive(Debug)]
pub(super) struct EncodedRun {
    pub(super) dictionary: Vec<u8>,
    pub(super) segments: Vec<(Family, u8, Vec<u8>)>,
    pub(super) dictionary_entries: u64,
    #[allow(dead_code)]
    pub(super) postings: u64,
    #[allow(dead_code)]
    pub(super) prefixes: u64,
    #[allow(dead_code)]
    pub(super) posting_bytes: u64,
    #[cfg_attr(not(feature = "bench-instrumentation"), allow(dead_code))]
    pub(super) dictionary_build_ns: u64,
    #[cfg_attr(not(feature = "bench-instrumentation"), allow(dead_code))]
    pub(super) membership_sort_ns: u64,
    #[cfg_attr(not(feature = "bench-instrumentation"), allow(dead_code))]
    pub(super) segment_encode_ns: u64,
}

pub(super) fn shard_for(family: Family, prefix: &[u8]) -> u8 {
    if family == Family::Global {
        return 0;
    }
    let mut hasher = blake3::Hasher::new_keyed(&SHARD_KEY);
    hasher.update(&[family as u8]);
    hasher.update(prefix);
    hasher.finalize().as_bytes()[0] & SHARD_MASK
}

pub(super) fn encode_run(mut memberships: Vec<Membership>) -> Result<EncodedRun, String> {
    if memberships.is_empty() {
        return Err("cannot encode an empty postings run".to_owned());
    }
    let dictionary_started = std::time::Instant::now();
    let mut ids_by_key = BTreeMap::new();
    let mut ids = HashSet::with_capacity(memberships.len());
    for membership in &memberships {
        if membership.shard != shard_for(membership.family, membership.prefix.as_bytes()) {
            return Err("membership is assigned to the wrong shard".to_owned());
        }
        match ids_by_key.get(&membership.event.event_key) {
            None => {
                if !ids.insert(membership.event.id) {
                    return Err("one event id maps to multiple event keys".to_owned());
                }
                ids_by_key.insert(membership.event.event_key, membership.event.id);
            }
            Some(id) if id != &membership.event.id => {
                return Err(format!(
                    "event key {} maps to multiple ids",
                    membership.event.event_key
                ));
            }
            Some(_) => {}
        }
    }
    let mut dictionary_entries = Vec::with_capacity(ids.len());
    for (event_key, id) in ids_by_key {
        dictionary_entries.push((event_key, id));
    }
    u32::try_from(dictionary_entries.len()).map_err(|_| "run dictionary exceeds u32".to_owned())?;
    drop(ids);
    let ordinals: HashMap<_, _> = dictionary_entries
        .iter()
        .enumerate()
        .map(|(ordinal, (event_key, _))| (*event_key, ordinal as u32))
        .collect();
    let dictionary_build_ns = elapsed_ns(dictionary_started);

    let sort_started = std::time::Instant::now();
    memberships.sort_unstable_by(|left, right| {
        left.family
            .cmp(&right.family)
            .then_with(|| left.shard.cmp(&right.shard))
            .then_with(|| left.prefix.as_bytes().cmp(right.prefix.as_bytes()))
            .then_with(|| canonical_order(&left.event, &right.event))
    });
    memberships.dedup_by(|right, left| {
        left.family == right.family
            && left.prefix == right.prefix
            && left.event.event_key == right.event.event_key
    });
    let membership_sort_ns = elapsed_ns(sort_started);

    let segment_started = std::time::Instant::now();
    let mut segments = Vec::new();
    let mut prefixes = 0u64;
    let mut start = 0usize;
    while start < memberships.len() {
        let family = memberships[start].family;
        let shard = memberships[start].shard;
        let mut end = start + 1;
        while end < memberships.len()
            && memberships[end].family == family
            && memberships[end].shard == shard
        {
            end += 1;
        }
        let segment_memberships = &memberships[start..end];
        prefixes = prefixes.saturating_add(
            1 + segment_memberships
                .windows(2)
                .filter(|pair| pair[0].prefix != pair[1].prefix)
                .count() as u64,
        );
        segments.push((
            family,
            shard,
            encode_segment(family, shard, segment_memberships, &ordinals)?,
        ));
        start = end;
    }
    let postings = memberships.len() as u64;
    let dictionary = encode_dictionary(&dictionary_entries)?;
    let segment_encode_ns = elapsed_ns(segment_started);
    Ok(EncodedRun {
        dictionary,
        segments,
        dictionary_entries: dictionary_entries.len() as u64,
        postings,
        prefixes,
        posting_bytes: postings.saturating_mul(POSTING_ENTRY_LEN as u64),
        dictionary_build_ns,
        membership_sort_ns,
        segment_encode_ns,
    })
}

fn elapsed_ns(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn canonical_order(left: &RunEvent, right: &RunEvent) -> Ordering {
    right
        .created_at
        .cmp(&left.created_at)
        .then_with(|| left.id.cmp(&right.id))
        .then_with(|| left.event_key.cmp(&right.event_key))
}

pub(super) fn encode_dictionary(entries: &[(u64, [u8; 32])]) -> Result<Vec<u8>, String> {
    let count = u32::try_from(entries.len()).map_err(|_| "dictionary exceeds u32".to_owned())?;
    let mut value = Vec::with_capacity(12 + entries.len().saturating_mul(DICTIONARY_ENTRY_LEN));
    value.extend_from_slice(DICTIONARY_MAGIC);
    value.extend_from_slice(&count.to_be_bytes());
    for (event_key, id) in entries {
        value.extend_from_slice(&event_key.to_be_bytes());
        value.extend_from_slice(id);
    }
    Ok(value)
}

fn encode_segment(
    family: Family,
    shard: u8,
    memberships: &[Membership],
    ordinals: &HashMap<u64, u32>,
) -> Result<Vec<u8>, String> {
    let prefix_count = 1 + memberships
        .windows(2)
        .filter(|pair| pair[0].prefix.as_bytes() != pair[1].prefix.as_bytes())
        .count();
    let prefix_count_u32 =
        u32::try_from(prefix_count).map_err(|_| "segment prefix count exceeds u32".to_owned())?;
    let offsets_start = SEGMENT_HEADER_LEN;
    let records_start = offsets_start
        .checked_add(
            prefix_count
                .checked_mul(4)
                .ok_or_else(|| "segment offset table overflow".to_owned())?,
        )
        .ok_or_else(|| "segment header overflow".to_owned())?;
    let estimated_records = memberships
        .len()
        .checked_mul(POSTING_ENTRY_LEN)
        .and_then(|bytes| bytes.checked_add(prefix_count.saturating_mul(8)))
        .ok_or_else(|| "segment capacity overflow".to_owned())?;
    let mut value = Vec::with_capacity(records_start.saturating_add(estimated_records));
    value.extend_from_slice(SEGMENT_MAGIC);
    value.push(family as u8);
    value.push(shard);
    value.extend_from_slice(&prefix_count_u32.to_be_bytes());
    value.resize(records_start, 0);
    let mut prefix_ordinal = 0usize;
    let mut start = 0usize;
    while start < memberships.len() {
        let prefix = memberships[start].prefix.as_bytes();
        let mut end = start + 1;
        while end < memberships.len() && memberships[end].prefix.as_bytes() == prefix {
            end += 1;
        }
        let offset =
            u32::try_from(value.len()).map_err(|_| "segment offset exceeds u32".to_owned())?;
        let offset_start = offsets_start + prefix_ordinal * 4;
        value[offset_start..offset_start + 4].copy_from_slice(&offset.to_be_bytes());
        encode_prefix_record(&mut value, prefix, &memberships[start..end], ordinals)?;
        prefix_ordinal += 1;
        start = end;
    }
    Ok(value)
}

fn encode_prefix_record(
    value: &mut Vec<u8>,
    prefix: &[u8],
    postings: &[Membership],
    ordinals: &HashMap<u64, u32>,
) -> Result<(), String> {
    let prefix_len = u32::try_from(prefix.len()).map_err(|_| "prefix exceeds u32".to_owned())?;
    let posting_count =
        u32::try_from(postings.len()).map_err(|_| "posting count exceeds u32".to_owned())?;
    value.extend_from_slice(&prefix_len.to_be_bytes());
    value.extend_from_slice(prefix);
    value.extend_from_slice(&posting_count.to_be_bytes());
    for membership in postings {
        value.extend_from_slice(&membership.event.created_at.to_be_bytes());
        let ordinal = ordinals
            .get(&membership.event.event_key)
            .ok_or_else(|| "posting event is absent from run dictionary".to_owned())?;
        value.extend_from_slice(&ordinal.to_be_bytes());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(super) struct DictionaryView<'a> {
    entries: &'a [u8],
    count: usize,
}

impl<'a> DictionaryView<'a> {
    pub(super) fn parse(value: &'a [u8]) -> Result<Self, String> {
        let mut cursor = 0usize;
        if take(value, &mut cursor, DICTIONARY_MAGIC.len())? != DICTIONARY_MAGIC {
            return Err("invalid run dictionary magic".to_owned());
        }
        let count = read_u32(value, &mut cursor)? as usize;
        let expected = count
            .checked_mul(DICTIONARY_ENTRY_LEN)
            .and_then(|bytes| cursor.checked_add(bytes))
            .ok_or_else(|| "dictionary length overflow".to_owned())?;
        if expected != value.len() {
            return Err("run dictionary length mismatch".to_owned());
        }
        Ok(Self {
            entries: &value[cursor..],
            count,
        })
    }

    pub(super) fn validate(self) -> Result<Self, String> {
        let mut previous = None;
        let mut ids = HashSet::with_capacity(self.count);
        for ordinal in 0..self.count {
            let (event_key, id) = dictionary_entry(self.entries, ordinal);
            if previous.is_some_and(|prior| prior >= event_key) {
                return Err("dictionary keys are not strictly ordered".to_owned());
            }
            if !ids.insert(id) {
                return Err("dictionary ids are not unique".to_owned());
            }
            previous = Some(event_key);
        }
        Ok(self)
    }

    #[cfg(test)]
    pub(super) fn id(self, event_key: u64) -> Option<[u8; 32]> {
        let ordinal = binary_search_index(self.count, |ordinal| {
            dictionary_entry(self.entries, ordinal).0.cmp(&event_key)
        })
        .ok()?;
        Some(dictionary_entry(self.entries, ordinal).1)
    }

    fn event(self, ordinal: usize, created_at: u64) -> Result<RunEvent, String> {
        if ordinal >= self.count {
            return Err("posting dictionary ordinal out of bounds".to_owned());
        }
        let (event_key, id) = dictionary_entry(self.entries, ordinal);
        Ok(RunEvent {
            created_at,
            id,
            event_key,
        })
    }

    pub(super) fn entry(self, ordinal: usize) -> Result<(u64, [u8; 32]), String> {
        if ordinal >= self.count {
            return Err("dictionary ordinal out of bounds".to_owned());
        }
        Ok(dictionary_entry(self.entries, ordinal))
    }

    pub(super) fn len(self) -> usize {
        self.count
    }
}

fn dictionary_entry(entries: &[u8], ordinal: usize) -> (u64, [u8; 32]) {
    let start = ordinal * DICTIONARY_ENTRY_LEN;
    (
        u64::from_be_bytes(entries[start..start + 8].try_into().unwrap()),
        entries[start + 8..start + 40].try_into().unwrap(),
    )
}

fn binary_search_index(
    len: usize,
    mut compare: impl FnMut(usize) -> Ordering,
) -> Result<usize, usize> {
    let mut left = 0usize;
    let mut right = len;
    while left < right {
        let middle = left + (right - left) / 2;
        match compare(middle) {
            Ordering::Less => left = middle + 1,
            Ordering::Greater => right = middle,
            Ordering::Equal => return Ok(middle),
        }
    }
    Err(left)
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SegmentView<'a> {
    value: &'a [u8],
    pub(super) family: Family,
    pub(super) shard: u8,
    prefix_count: usize,
}

impl<'a> SegmentView<'a> {
    pub(super) fn parse(value: &'a [u8]) -> Result<Self, String> {
        let mut cursor = 0usize;
        if take(value, &mut cursor, SEGMENT_MAGIC.len())? != SEGMENT_MAGIC {
            return Err("invalid packed-postings magic".to_owned());
        }
        let family = Family::from_u8(take(value, &mut cursor, 1)?[0])?;
        let shard = take(value, &mut cursor, 1)?[0];
        if shard > SHARD_MASK || (family == Family::Global && shard != 0) {
            return Err("invalid packed-postings shard".to_owned());
        }
        let prefix_count = read_u32(value, &mut cursor)? as usize;
        if prefix_count == 0 {
            return Err("segment has no prefixes".to_owned());
        }
        let offsets_end = cursor
            .checked_add(
                prefix_count
                    .checked_mul(4)
                    .ok_or_else(|| "prefix offset overflow".to_owned())?,
            )
            .ok_or_else(|| "prefix offset overflow".to_owned())?;
        if offsets_end > value.len() {
            return Err("truncated prefix offset table".to_owned());
        }
        Ok(Self {
            value,
            family,
            shard,
            prefix_count,
        })
    }

    fn validate_prefix_directory(self) -> Result<(), String> {
        let offsets_end = SEGMENT_HEADER_LEN + self.prefix_count * 4;
        let mut previous_prefix: Option<&[u8]> = None;
        let mut previous_offset = offsets_end;
        for ordinal in 0..self.prefix_count {
            let offset = self.record_offset(ordinal)?;
            if offset < previous_offset {
                return Err("prefix record offsets are not increasing".to_owned());
            }
            let record = self.record(ordinal)?;
            if previous_prefix.is_some_and(|prefix| prefix >= record.prefix) {
                return Err("segment prefixes are not strictly ordered".to_owned());
            }
            if shard_for(self.family, record.prefix) != self.shard {
                return Err("prefix is stored in the wrong shard".to_owned());
            }
            previous_prefix = Some(record.prefix);
            previous_offset = offset;
        }
        Ok(())
    }

    pub(super) fn prefix(self, wanted: &[u8]) -> Result<Option<PostingListView<'a>>, String> {
        let mut left = 0usize;
        let mut right = self.prefix_count;
        while left < right {
            let middle = left + (right - left) / 2;
            match self.record(middle)?.prefix.cmp(wanted) {
                Ordering::Less => left = middle + 1,
                Ordering::Greater => right = middle,
                Ordering::Equal => return Ok(Some(self.record(middle)?.list)),
            }
        }
        Ok(None)
    }

    pub(super) fn validate(self, dictionary: DictionaryView<'_>) -> Result<u64, String> {
        self.validate_prefix_directory()?;
        let mut postings = 0u64;
        for ordinal in 0..self.prefix_count {
            let record = self.record(ordinal)?;
            let mut previous = None;
            for posting in 0..record.list.posting_count {
                let event = record.list.event(dictionary, posting)?;
                if previous.is_some_and(|prior| canonical_order(&prior, &event) != Ordering::Less) {
                    return Err("posting list violates canonical order".to_owned());
                }
                previous = Some(event);
            }
            postings = postings.saturating_add(record.list.posting_count as u64);
        }
        Ok(postings)
    }

    pub(super) fn memberships(
        self,
        dictionary: DictionaryView<'a>,
    ) -> Result<Vec<Membership>, String> {
        let mut events = BTreeMap::new();
        self.memberships_interned(dictionary, &mut events)
    }

    pub(super) fn memberships_interned(
        self,
        dictionary: DictionaryView<'a>,
        events: &mut BTreeMap<u64, Arc<RunEvent>>,
    ) -> Result<Vec<Membership>, String> {
        let mut memberships = Vec::new();
        for ordinal in 0..self.prefix_count {
            let record = self.record(ordinal)?;
            let prefix = Prefix::from_bytes(self.family, record.prefix)?;
            for posting in 0..record.list.posting_count {
                let event = record.list.event(dictionary, posting)?;
                let event = events
                    .entry(event.event_key)
                    .or_insert_with(|| Arc::new(event))
                    .clone();
                memberships.push(Membership {
                    family: self.family,
                    shard: self.shard,
                    prefix: prefix.clone(),
                    event,
                });
            }
        }
        Ok(memberships)
    }

    fn record_offset(self, ordinal: usize) -> Result<usize, String> {
        if ordinal >= self.prefix_count {
            return Err("prefix ordinal out of bounds".to_owned());
        }
        let start = SEGMENT_HEADER_LEN + ordinal * 4;
        Ok(u32::from_be_bytes(self.value[start..start + 4].try_into().unwrap()) as usize)
    }

    fn record(self, ordinal: usize) -> Result<PrefixRecord<'a>, String> {
        let start = self.record_offset(ordinal)?;
        let end = if ordinal + 1 == self.prefix_count {
            self.value.len()
        } else {
            self.record_offset(ordinal + 1)?
        };
        if start >= end || end > self.value.len() {
            return Err("invalid prefix record bounds".to_owned());
        }
        parse_prefix_record(&self.value[start..end])
    }
}

pub(super) struct CompactionSegmentSource<'a> {
    pub(super) segment: SegmentView<'a>,
    pub(super) ordinal_map: &'a [Option<u32>],
}

pub(super) struct CompactedSegment {
    pub(super) value: Vec<u8>,
    pub(super) postings: u64,
    pub(super) prefixes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactionHeapEntry {
    event: RunEvent,
    output_ordinal: u32,
    list: usize,
}

impl Ord for CompactionHeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.event
            .created_at
            .cmp(&other.event.created_at)
            .then_with(|| other.event.id.cmp(&self.event.id))
            .then_with(|| other.event.event_key.cmp(&self.event.event_key))
            .then_with(|| other.list.cmp(&self.list))
    }
}

impl PartialOrd for CompactionHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(super) fn compact_segment(
    family: Family,
    shard: u8,
    sources: &[CompactionSegmentSource<'_>],
    output_dictionary: DictionaryView<'_>,
) -> Result<Option<CompactedSegment>, String> {
    if sources.is_empty() {
        return Ok(None);
    }
    for source in sources {
        if source.segment.family != family || source.segment.shard != shard {
            return Err("compaction segment is assigned to the wrong family or shard".to_owned());
        }
    }

    let prefix_count = count_live_compaction_prefixes(sources)?;
    if prefix_count == 0 {
        return Ok(None);
    }
    let prefix_count_u32 =
        u32::try_from(prefix_count).map_err(|_| "segment prefix count exceeds u32".to_owned())?;
    let records_start = SEGMENT_HEADER_LEN
        .checked_add(
            prefix_count
                .checked_mul(4)
                .ok_or_else(|| "segment offset table overflow".to_owned())?,
        )
        .ok_or_else(|| "segment header overflow".to_owned())?;
    let mut value = Vec::with_capacity(records_start);
    value.extend_from_slice(SEGMENT_MAGIC);
    value.push(family as u8);
    value.push(shard);
    value.extend_from_slice(&prefix_count_u32.to_be_bytes());
    value.resize(records_start, 0);

    let mut records = vec![0usize; sources.len()];
    let mut emitted_prefixes = 0usize;
    let mut postings = 0u64;
    while let Some(prefix) = minimum_compaction_prefix(sources, &records)? {
        let mut matching = Vec::with_capacity(sources.len());
        for (source, candidate) in sources.iter().enumerate() {
            if records[source] >= candidate.segment.prefix_count {
                continue;
            }
            let record = candidate.segment.record(records[source])?;
            if record.prefix == prefix {
                matching.push((source, record.list));
                records[source] += 1;
            }
        }
        if !matching
            .iter()
            .any(|(source, list)| posting_list_has_live(*list, sources[*source].ordinal_map))
        {
            continue;
        }

        let offset =
            u32::try_from(value.len()).map_err(|_| "segment offset exceeds u32".to_owned())?;
        let offset_start = SEGMENT_HEADER_LEN + emitted_prefixes * 4;
        value[offset_start..offset_start + 4].copy_from_slice(&offset.to_be_bytes());
        let prefix_len =
            u32::try_from(prefix.len()).map_err(|_| "prefix exceeds u32".to_owned())?;
        value.extend_from_slice(&prefix_len.to_be_bytes());
        value.extend_from_slice(prefix);
        let count_offset = value.len();
        value.extend_from_slice(&0u32.to_be_bytes());

        let mut next_postings = vec![0usize; matching.len()];
        let mut heap = BinaryHeap::with_capacity(matching.len());
        for list in 0..matching.len() {
            push_next_compaction_posting(
                &mut heap,
                list,
                &matching,
                &mut next_postings,
                sources,
                output_dictionary,
            )?;
        }
        let mut posting_count = 0u32;
        let mut previous_output_ordinal = None;
        while let Some(entry) = heap.pop() {
            push_next_compaction_posting(
                &mut heap,
                entry.list,
                &matching,
                &mut next_postings,
                sources,
                output_dictionary,
            )?;
            if previous_output_ordinal == Some(entry.output_ordinal) {
                continue;
            }
            value.extend_from_slice(&entry.event.created_at.to_be_bytes());
            value.extend_from_slice(&entry.output_ordinal.to_be_bytes());
            posting_count = posting_count
                .checked_add(1)
                .ok_or_else(|| "posting count exceeds u32".to_owned())?;
            previous_output_ordinal = Some(entry.output_ordinal);
        }
        if posting_count == 0 {
            return Err("live compaction prefix produced no postings".to_owned());
        }
        value[count_offset..count_offset + 4].copy_from_slice(&posting_count.to_be_bytes());
        postings = postings.saturating_add(u64::from(posting_count));
        emitted_prefixes += 1;
    }
    if emitted_prefixes != prefix_count {
        return Err("compaction prefix count changed during encoding".to_owned());
    }
    Ok(Some(CompactedSegment {
        value,
        postings,
        prefixes: prefix_count as u64,
    }))
}

fn count_live_compaction_prefixes(
    sources: &[CompactionSegmentSource<'_>],
) -> Result<usize, String> {
    let mut records = vec![0usize; sources.len()];
    let mut count = 0usize;
    while let Some(prefix) = minimum_compaction_prefix(sources, &records)? {
        let mut live = false;
        for (source, candidate) in sources.iter().enumerate() {
            if records[source] >= candidate.segment.prefix_count {
                continue;
            }
            let record = candidate.segment.record(records[source])?;
            if record.prefix == prefix {
                live |= posting_list_has_live(record.list, candidate.ordinal_map);
                records[source] += 1;
            }
        }
        count = count
            .checked_add(usize::from(live))
            .ok_or_else(|| "compaction prefix count overflow".to_owned())?;
    }
    Ok(count)
}

fn minimum_compaction_prefix<'a>(
    sources: &'a [CompactionSegmentSource<'a>],
    records: &[usize],
) -> Result<Option<&'a [u8]>, String> {
    let mut minimum = None;
    for (source, candidate) in sources.iter().enumerate() {
        if records[source] >= candidate.segment.prefix_count {
            continue;
        }
        let prefix = candidate.segment.record(records[source])?.prefix;
        if minimum.is_none_or(|current| prefix < current) {
            minimum = Some(prefix);
        }
    }
    Ok(minimum)
}

fn posting_list_has_live(list: PostingListView<'_>, ordinal_map: &[Option<u32>]) -> bool {
    (0..list.posting_count).any(|posting| {
        let source_ordinal = list.dictionary_ordinal(posting);
        source_ordinal < ordinal_map.len() && ordinal_map[source_ordinal].is_some()
    })
}

fn push_next_compaction_posting(
    heap: &mut BinaryHeap<CompactionHeapEntry>,
    list: usize,
    matching: &[(usize, PostingListView<'_>)],
    next_postings: &mut [usize],
    sources: &[CompactionSegmentSource<'_>],
    output_dictionary: DictionaryView<'_>,
) -> Result<(), String> {
    let (source, postings) = matching[list];
    while next_postings[list] < postings.posting_count {
        let posting = next_postings[list];
        next_postings[list] += 1;
        let created_at = postings.created_at(posting);
        let source_ordinal = postings.dictionary_ordinal(posting);
        let output_ordinal = *sources[source]
            .ordinal_map
            .get(source_ordinal)
            .ok_or_else(|| "compaction posting dictionary ordinal is out of bounds".to_owned())?;
        let Some(output_ordinal) = output_ordinal else {
            continue;
        };
        let event = output_dictionary.event(output_ordinal as usize, created_at)?;
        heap.push(CompactionHeapEntry {
            event,
            output_ordinal,
            list,
        });
        break;
    }
    Ok(())
}

struct PrefixRecord<'a> {
    prefix: &'a [u8],
    list: PostingListView<'a>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PostingListView<'a> {
    posting_count: usize,
    postings: &'a [u8],
}

fn parse_prefix_record(value: &[u8]) -> Result<PrefixRecord<'_>, String> {
    let mut cursor = 0usize;
    let prefix_len = read_u32(value, &mut cursor)? as usize;
    let prefix = take(value, &mut cursor, prefix_len)?;
    let posting_count = read_u32(value, &mut cursor)? as usize;
    if posting_count == 0 {
        return Err("empty posting list".to_owned());
    }
    let posting_bytes = posting_count
        .checked_mul(POSTING_ENTRY_LEN)
        .ok_or_else(|| "posting list length overflow".to_owned())?;
    let postings = take(value, &mut cursor, posting_bytes)?;
    if cursor != value.len() {
        return Err("trailing bytes in prefix record".to_owned());
    }
    Ok(PrefixRecord {
        prefix,
        list: PostingListView {
            posting_count,
            postings,
        },
    })
}

impl<'a> PostingListView<'a> {
    fn created_at(self, ordinal: usize) -> u64 {
        let start = ordinal * POSTING_ENTRY_LEN;
        u64::from_be_bytes(self.postings[start..start + 8].try_into().unwrap())
    }

    fn dictionary_ordinal(self, ordinal: usize) -> usize {
        let start = ordinal * POSTING_ENTRY_LEN;
        u32::from_be_bytes(self.postings[start + 8..start + 12].try_into().unwrap()) as usize
    }

    fn event(self, dictionary: DictionaryView<'_>, ordinal: usize) -> Result<RunEvent, String> {
        if ordinal >= self.posting_count {
            return Err("posting ordinal out of bounds".to_owned());
        }
        let created_at = self.created_at(ordinal);
        let dictionary_ordinal = self.dictionary_ordinal(ordinal);
        dictionary.event(dictionary_ordinal, created_at)
    }

    fn search(
        self,
        dictionary: DictionaryView<'_>,
        target: (u64, [u8; 32]),
    ) -> Result<Result<usize, usize>, String> {
        let mut failed = None;
        let result = binary_search_index(self.posting_count, |ordinal| {
            let event = match self.event(dictionary, ordinal) {
                Ok(event) => event,
                Err(error) => {
                    failed = Some(error);
                    return Ordering::Equal;
                }
            };
            target
                .0
                .cmp(&event.created_at)
                .then_with(|| event.id.cmp(&target.1))
        });
        failed.map_or(Ok(result), Err)
    }

    pub(super) fn cursor(
        self,
        dictionary: DictionaryView<'a>,
        before: Option<(u64, [u8; 32])>,
        since: u64,
        until: u64,
    ) -> Result<PostingCursor<'a>, String> {
        let mut start = 0usize;
        if until != u64::MAX {
            start = match self.search(dictionary, (until, [0u8; 32]))? {
                Ok(index) | Err(index) => index,
            };
        }
        if let Some(before) = before {
            let after = match self.search(dictionary, before)? {
                Ok(index) => index + 1,
                Err(index) => index,
            };
            start = start.max(after);
        }
        Ok(PostingCursor {
            list: self,
            dictionary,
            next_ordinal: start,
            since,
            exhausted: false,
            visited: 0,
        })
    }
}

pub(super) struct PostingCursor<'a> {
    list: PostingListView<'a>,
    dictionary: DictionaryView<'a>,
    next_ordinal: usize,
    since: u64,
    exhausted: bool,
    visited: u64,
}

impl PostingCursor<'_> {
    pub(super) fn next(&mut self) -> Result<Option<RunEvent>, String> {
        if self.exhausted || self.next_ordinal >= self.list.posting_count {
            return Ok(None);
        }
        let event = self.list.event(self.dictionary, self.next_ordinal)?;
        self.next_ordinal += 1;
        self.visited += 1;
        if event.created_at < self.since {
            self.exhausted = true;
            return Ok(None);
        }
        Ok(Some(event))
    }

    #[cfg(test)]
    pub(super) fn visited(&self) -> u64 {
        self.visited
    }

    pub(super) fn next_live(
        &mut self,
        dead: Option<&DeadKeys>,
    ) -> Result<Option<RunEvent>, String> {
        while let Some(event) = self.next()? {
            if dead.is_none_or(|keys| !keys.contains(event.event_key)) {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }
}

#[cfg(any(test, feature = "bench-instrumentation"))]
pub(super) struct MergeSource<'a> {
    pub(super) cursor: PostingCursor<'a>,
    pub(super) dead: Option<&'a DeadKeys>,
}

#[cfg(any(test, feature = "bench-instrumentation"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeapEntry {
    event: RunEvent,
    source: usize,
}

#[cfg(any(test, feature = "bench-instrumentation"))]
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.event
            .created_at
            .cmp(&other.event.created_at)
            .then_with(|| other.event.id.cmp(&self.event.id))
            .then_with(|| other.event.event_key.cmp(&self.event.event_key))
            .then_with(|| other.source.cmp(&self.source))
    }
}

#[cfg(any(test, feature = "bench-instrumentation"))]
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(any(test, feature = "bench-instrumentation"))]
pub(super) fn merge_posting_cursors(
    mut sources: Vec<MergeSource<'_>>,
    limit: usize,
) -> Result<Vec<RunEvent>, String> {
    let mut heap = BinaryHeap::with_capacity(sources.len());
    for (source, run) in sources.iter_mut().enumerate() {
        if let Some(event) = run.cursor.next_live(run.dead)? {
            heap.push(HeapEntry { event, source });
        }
    }
    let mut seen = HashSet::with_capacity(limit);
    let mut output = Vec::with_capacity(limit);
    while output.len() < limit {
        let Some(entry) = heap.pop() else {
            break;
        };
        let source = &mut sources[entry.source];
        if let Some(event) = source.cursor.next_live(source.dead)? {
            heap.push(HeapEntry {
                event,
                source: entry.source,
            });
        }
        if seen.insert(entry.event.event_key) {
            output.push(entry.event);
        }
    }
    Ok(output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DeadKeys {
    keys: Vec<u64>,
}

impl DeadKeys {
    pub(super) fn new(keys: Vec<u64>) -> Result<Self, String> {
        if keys.is_empty() {
            return Err("dead-key block is empty".to_owned());
        }
        if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("dead keys are not strictly ordered".to_owned());
        }
        Ok(Self { keys })
    }

    pub(super) fn contains(&self, event_key: u64) -> bool {
        self.keys.binary_search(&event_key).is_ok()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.keys.iter().copied()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.keys.len()
    }

    pub(super) fn encode(&self) -> Result<Vec<u8>, String> {
        let mut value = Vec::new();
        value.extend_from_slice(DEAD_KEYS_MAGIC);
        put_varint(&mut value, self.keys.len() as u64);
        let mut previous = 0u64;
        for (index, &key) in self.keys.iter().enumerate() {
            put_varint(&mut value, if index == 0 { key } else { key - previous });
            previous = key;
        }
        Ok(value)
    }

    pub(super) fn decode(value: &[u8]) -> Result<Self, String> {
        let mut cursor = 0usize;
        if take(value, &mut cursor, DEAD_KEYS_MAGIC.len())? != DEAD_KEYS_MAGIC {
            return Err("invalid dead-key block magic".to_owned());
        }
        let count = read_varint(value, &mut cursor)?;
        if count == 0 {
            return Err("empty dead-key block".to_owned());
        }
        let mut keys = Vec::with_capacity(count.min(usize::MAX as u64) as usize);
        let mut previous = 0u64;
        for index in 0..count {
            let encoded = read_varint(value, &mut cursor)?;
            let key = if index == 0 {
                encoded
            } else {
                previous
                    .checked_add(encoded)
                    .ok_or_else(|| "dead-key delta overflow".to_owned())?
            };
            if index > 0 && key <= previous {
                return Err("dead keys are not strictly ordered".to_owned());
            }
            keys.push(key);
            previous = key;
        }
        if cursor != value.len() {
            return Err("trailing bytes in dead-key block".to_owned());
        }
        Ok(Self { keys })
    }
}

pub(super) fn merge_dead_blocks(blocks: &[DeadKeys]) -> Result<Option<DeadKeys>, String> {
    if blocks.len() > MAX_DEATH_BLOCKS {
        return Err("dead-key block fan-in exceeds the hard bound".to_owned());
    }
    let mut keys: Vec<_> = blocks.iter().flat_map(DeadKeys::iter).collect();
    if keys.is_empty() {
        return Ok(None);
    }
    keys.sort_unstable();
    keys.dedup();
    Ok(Some(DeadKeys::new(keys)?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RunMeta {
    pub(super) run_id: u64,
    pub(super) level: u8,
    pub(super) min_event_key: u64,
    pub(super) max_event_key: u64,
    pub(super) live_events: u64,
}

impl RunMeta {
    pub(super) fn encode(self) -> Result<[u8; 41], String> {
        if self.min_event_key == 0
            || self.min_event_key > self.max_event_key
            || self.live_events == 0
            || self.live_events > self.max_event_key - self.min_event_key + 1
        {
            return Err("invalid packed run metadata".to_owned());
        }
        let mut value = [0u8; 41];
        value[..8].copy_from_slice(RUN_META_MAGIC);
        value[8..16].copy_from_slice(&self.run_id.to_be_bytes());
        value[16] = self.level;
        value[17..25].copy_from_slice(&self.min_event_key.to_be_bytes());
        value[25..33].copy_from_slice(&self.max_event_key.to_be_bytes());
        value[33..41].copy_from_slice(&self.live_events.to_be_bytes());
        Ok(value)
    }

    pub(super) fn decode(value: &[u8]) -> Result<Self, String> {
        if value.len() != 41 || &value[..8] != RUN_META_MAGIC {
            return Err("invalid packed run metadata bytes".to_owned());
        }
        let meta = Self {
            run_id: u64::from_be_bytes(value[8..16].try_into().unwrap()),
            level: value[16],
            min_event_key: u64::from_be_bytes(value[17..25].try_into().unwrap()),
            max_event_key: u64::from_be_bytes(value[25..33].try_into().unwrap()),
            live_events: u64::from_be_bytes(value[33..41].try_into().unwrap()),
        };
        meta.encode()?;
        Ok(meta)
    }

    #[cfg(test)]
    pub(super) fn contains(self, event_key: u64) -> bool {
        (self.min_event_key..=self.max_event_key).contains(&event_key)
    }
}

#[allow(dead_code)]
pub(super) fn validate_run_metas(metas: &[RunMeta]) -> Result<(), String> {
    let mut ordered = metas.to_vec();
    ordered.sort_unstable_by_key(|meta| (meta.min_event_key, meta.max_event_key, meta.run_id));
    let mut run_ids = HashSet::with_capacity(ordered.len());
    let mut previous = None;
    for meta in ordered {
        meta.encode()?;
        if !run_ids.insert(meta.run_id) {
            return Err("duplicate packed run id".to_owned());
        }
        if previous.is_some_and(|maximum| meta.min_event_key <= maximum) {
            return Err("packed run event-key ranges overlap".to_owned());
        }
        previous = Some(meta.max_event_key);
    }
    Ok(())
}

fn put_varint(target: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        target.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    target.push(value as u8);
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..=63).step_by(7) {
        let byte = take(bytes, cursor, 1)?[0];
        if shift == 63 && byte > 1 {
            return Err("varint overflow".to_owned());
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("unterminated varint".to_owned())
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, String> {
    Ok(u32::from_be_bytes(
        take(bytes, cursor, 4)?.try_into().unwrap(),
    ))
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8], String> {
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| "packed-postings cursor overflow".to_owned())?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| "truncated packed-postings bytes".to_owned())?;
    *cursor = end;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u64) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[24..].copy_from_slice(&value.to_be_bytes());
        id
    }

    fn global_memberships(count: usize, created_at: u64) -> Vec<Membership> {
        (0..count)
            .map(|ordinal| Membership {
                family: Family::Global,
                shard: 0,
                prefix: Prefix::global(),
                event: RunEvent {
                    created_at,
                    id: id(ordinal as u64),
                    event_key: (count - ordinal) as u64,
                }
                .into(),
            })
            .collect()
    }

    #[test]
    fn dictionary_projects_ids_without_event_values() {
        let encoded = encode_run(global_memberships(300, 10)).unwrap();
        let dictionary = DictionaryView::parse(&encoded.dictionary).unwrap();
        assert_eq!(dictionary.len(), 300);
        assert_eq!(dictionary.id(300), Some(id(0)));
        assert_eq!(dictionary.id(1), Some(id(299)));
        assert_eq!(dictionary.id(301), None);
        assert_eq!(encoded.dictionary_entries, 300);
        assert_eq!(encoded.postings, 300);
        assert_eq!(encoded.prefixes, 1);
        assert_eq!(encoded.posting_bytes, 3_600);
    }

    #[test]
    fn exact_cursor_seeks_without_visiting_equal_timestamp_predecessors() {
        let encoded = encode_run(global_memberships(300, 10)).unwrap();
        let dictionary = DictionaryView::parse(&encoded.dictionary).unwrap();
        let (_, _, value) = &encoded.segments[0];
        let segment = SegmentView::parse(value).unwrap();
        assert_eq!(segment.validate(dictionary).unwrap(), 300);
        let list = segment.prefix(&[]).unwrap().unwrap();
        let mut cursor = list
            .cursor(dictionary, Some((10, id(119))), 0, u64::MAX)
            .unwrap();
        let mut rows = Vec::new();
        for _ in 0..10 {
            rows.push(cursor.next().unwrap().unwrap().id);
        }
        assert_eq!(rows, (120..130).map(id).collect::<Vec<_>>());
        assert_eq!(cursor.visited(), 10);
    }

    #[test]
    fn until_seek_is_exact_and_does_not_visit_newer_postings() {
        let memberships = (0..300)
            .map(|ordinal| Membership {
                family: Family::Global,
                shard: 0,
                prefix: Prefix::global(),
                event: RunEvent {
                    created_at: 1_000 - ordinal as u64,
                    id: id(ordinal as u64),
                    event_key: ordinal as u64 + 1,
                }
                .into(),
            })
            .collect();
        let encoded = encode_run(memberships).unwrap();
        let dictionary = DictionaryView::parse(&encoded.dictionary).unwrap();
        let segment = SegmentView::parse(&encoded.segments[0].2).unwrap();
        let mut cursor = segment
            .prefix(&[])
            .unwrap()
            .unwrap()
            .cursor(dictionary, None, 0, 800)
            .unwrap();
        assert_eq!(cursor.next().unwrap().unwrap().created_at, 800);
        assert_eq!(cursor.visited(), 1);
    }

    #[test]
    fn prefix_directory_finds_exact_prefix_without_scanning_prior_records() {
        let events: Vec<_> = (0..256)
            .map(|ordinal| Membership {
                family: Family::Author,
                shard: shard_for(Family::Author, &id(ordinal as u64)),
                prefix: Prefix::author(id(ordinal as u64)),
                event: RunEvent {
                    created_at: ordinal as u64,
                    id: id(ordinal as u64),
                    event_key: ordinal as u64 + 1,
                }
                .into(),
            })
            .collect();
        let encoded = encode_run(events).unwrap();
        let wanted = id(200).to_vec();
        let shard = shard_for(Family::Author, &wanted);
        let (_, _, value) = encoded
            .segments
            .iter()
            .find(|(family, candidate_shard, _)| {
                *family == Family::Author && *candidate_shard == shard
            })
            .unwrap();
        let segment = SegmentView::parse(value).unwrap();
        assert!(segment.prefix(&wanted).unwrap().is_some());
        assert!(segment.prefix(b"absent").unwrap().is_none());
    }

    #[test]
    fn segment_and_dictionary_corruption_fail_closed() {
        let encoded = encode_run(global_memberships(3, 10)).unwrap();
        let mut dictionary = encoded.dictionary.clone();
        dictionary.push(0);
        assert!(DictionaryView::parse(&dictionary).is_err());
        let mut segment = encoded.segments[0].2.clone();
        segment.push(0);
        let dictionary = DictionaryView::parse(&encoded.dictionary)
            .unwrap()
            .validate()
            .unwrap();
        assert!(SegmentView::parse(&segment)
            .and_then(|view| view.validate(dictionary))
            .is_err());

        let mut missing = encoded.dictionary.clone();
        missing.truncate(missing.len() - DICTIONARY_ENTRY_LEN);
        missing[8..12].copy_from_slice(&2u32.to_be_bytes());
        let dictionary = DictionaryView::parse(&missing).unwrap().validate().unwrap();
        let segment = SegmentView::parse(&encoded.segments[0].2).unwrap();
        assert!(segment.validate(dictionary).is_err());
    }

    #[test]
    fn duplicate_memberships_are_encoded_once() {
        let mut memberships = global_memberships(1, 10);
        memberships.push(memberships[0].clone());
        let encoded = encode_run(memberships).unwrap();
        assert_eq!(encoded.postings, 1);
        let dictionary = DictionaryView::parse(&encoded.dictionary).unwrap();
        assert_eq!(
            SegmentView::parse(&encoded.segments[0].2)
                .unwrap()
                .validate(dictionary)
                .unwrap(),
            1
        );
    }

    #[test]
    fn dead_keys_and_run_metadata_round_trip_exactly() {
        let dead = DeadKeys::new(vec![8, 16, 24, 4_096]).unwrap();
        let decoded = DeadKeys::decode(&dead.encode().unwrap()).unwrap();
        assert!(decoded.contains(24));
        assert!(!decoded.contains(25));
        let merged = merge_dead_blocks(&[
            DeadKeys::new(vec![8, 16]).unwrap(),
            DeadKeys::new(vec![16, 24]).unwrap(),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(merged.iter().collect::<Vec<_>>(), vec![8, 16, 24]);
        assert!(merge_dead_blocks(&vec![DeadKeys::new(vec![8]).unwrap(); 9]).is_err());
        let meta = RunMeta {
            run_id: 9,
            level: 2,
            min_event_key: 8,
            max_event_key: 4_096,
            live_events: 3_500,
        };
        assert_eq!(RunMeta::decode(&meta.encode().unwrap()).unwrap(), meta);
        assert!(meta.contains(24));
        assert!(!meta.contains(4_097));
        assert!(validate_run_metas(&[
            meta,
            RunMeta {
                run_id: 10,
                level: 0,
                min_event_key: 4_097,
                max_event_key: 8_192,
                live_events: 4_096,
            },
        ])
        .is_ok());
        assert!(validate_run_metas(&[
            meta,
            RunMeta {
                run_id: 10,
                level: 0,
                min_event_key: 4_096,
                max_event_key: 8_192,
                live_events: 4_096,
            },
        ])
        .is_err());
    }

    #[test]
    fn multi_run_multi_prefix_merge_is_exact_deduplicated_and_dead_filtered() {
        let author = id(900);
        let run_one_events = [
            RunEvent {
                created_at: 12,
                id: id(1),
                event_key: 1,
            },
            RunEvent {
                created_at: 10,
                id: id(3),
                event_key: 2,
            },
        ];
        let run_two_events = [
            RunEvent {
                created_at: 11,
                id: id(2),
                event_key: 3,
            },
            RunEvent {
                created_at: 10,
                id: id(4),
                event_key: 4,
            },
        ];
        let memberships = |events: &[RunEvent]| {
            events
                .iter()
                .flat_map(|event| {
                    [
                        Membership {
                            family: Family::Global,
                            shard: 0,
                            prefix: Prefix::global(),
                            event: (*event).into(),
                        },
                        Membership {
                            family: Family::Author,
                            shard: shard_for(Family::Author, &author),
                            prefix: Prefix::author(author),
                            event: (*event).into(),
                        },
                    ]
                })
                .collect()
        };
        let run_one = encode_run(memberships(&run_one_events)).unwrap();
        let run_two = encode_run(memberships(&run_two_events)).unwrap();
        let dictionary_one = DictionaryView::parse(&run_one.dictionary).unwrap();
        let dictionary_two = DictionaryView::parse(&run_two.dictionary).unwrap();
        let dead_one = DeadKeys::new(vec![2]).unwrap();
        let mut sources = Vec::new();
        for (run, dictionary, dead) in [
            (&run_one, dictionary_one, Some(&dead_one)),
            (&run_two, dictionary_two, None),
        ] {
            for (family, prefix) in [(Family::Global, &[][..]), (Family::Author, &author)] {
                let shard = shard_for(family, prefix);
                let (_, _, bytes) = run
                    .segments
                    .iter()
                    .find(|(candidate_family, candidate_shard, _)| {
                        *candidate_family == family && *candidate_shard == shard
                    })
                    .unwrap();
                let segment = SegmentView::parse(bytes).unwrap();
                assert_eq!(segment.family, family);
                assert_eq!(segment.shard, shard);
                assert_eq!(segment.memberships(dictionary).unwrap().len(), 2);
                let cursor = segment
                    .prefix(prefix)
                    .unwrap()
                    .unwrap()
                    .cursor(dictionary, None, 0, u64::MAX)
                    .unwrap();
                sources.push(MergeSource { cursor, dead });
            }
        }
        let rows = merge_posting_cursors(sources, 10).unwrap();
        assert_eq!(
            rows.iter().map(|event| event.event_key).collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
        assert_eq!(dead_one.iter().collect::<Vec<_>>(), vec![2]);
        assert_eq!(dead_one.len(), 1);
    }

    #[test]
    fn compaction_streams_live_postings_into_one_exact_segment() {
        let first = encode_run(global_memberships(2, 50)).expect("encode first run");
        let second_memberships = (2..4)
            .map(|ordinal| Membership {
                family: Family::Global,
                shard: 0,
                prefix: Prefix::global(),
                event: Arc::new(RunEvent {
                    created_at: 50,
                    id: id(ordinal),
                    event_key: ordinal + 1,
                }),
            })
            .collect();
        let second = encode_run(second_memberships).expect("encode second run");
        let first_segment = SegmentView::parse(&first.segments[0].2).expect("parse first segment");
        let second_segment =
            SegmentView::parse(&second.segments[0].2).expect("parse second segment");
        let output_entries = vec![(1, id(0)), (3, id(2)), (4, id(3))];
        let output_bytes = encode_dictionary(&output_entries).expect("encode output dictionary");
        let output_dictionary = DictionaryView::parse(&output_bytes)
            .and_then(DictionaryView::validate)
            .expect("validate output dictionary");
        let first_map = [Some(0), None];
        let second_map = [Some(1), Some(2)];
        let compacted = compact_segment(
            Family::Global,
            0,
            &[
                CompactionSegmentSource {
                    segment: first_segment,
                    ordinal_map: &first_map,
                },
                CompactionSegmentSource {
                    segment: second_segment,
                    ordinal_map: &second_map,
                },
            ],
            output_dictionary,
        )
        .expect("compact segments")
        .expect("live compacted segment");
        assert_eq!(compacted.prefixes, 1);
        assert_eq!(compacted.postings, 3);
        let output = SegmentView::parse(&compacted.value).expect("parse compacted segment");
        output
            .validate(output_dictionary)
            .expect("validate compacted segment");
        let keys: Vec<_> = output
            .memberships(output_dictionary)
            .expect("decode compacted memberships")
            .into_iter()
            .map(|membership| membership.event.event_key)
            .collect();
        assert_eq!(keys, vec![1, 3, 4]);
    }
}
