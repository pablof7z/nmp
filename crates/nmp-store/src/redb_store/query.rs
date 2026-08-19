use super::{
    decode_hex_32, Deserialize, EventId, Filter, IndexedMatch, Kind, PublicKey, Serialize,
    SingleLetterTag, Timestamp,
};

/// The address column of the `tombstones` table's JSON value.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct AddrTombstoneRecord {
    pub(super) ceiling: u64,
    pub(super) deleting_event_id: String,
    pub(super) deleting_author: String,
}

/// The `expiration_index` table's fixed binary key. Big-endian seconds make
/// redb's byte ordering numeric; raw id bytes disambiguate equal deadlines.
pub(super) fn expiration_key(ts: Timestamp, id: &EventId) -> [u8; 40] {
    let mut key = [0; 40];
    key[..8].copy_from_slice(&ts.as_secs().to_be_bytes());
    key[8..].copy_from_slice(id.as_bytes());
    key
}

/// The deadline half of an [`expiration_key`], read back.
///
/// Total by construction, and deliberately so: the array pattern is
/// irrefutable against the table's fixed `[u8; 40]` key type, so the width
/// invariant `expiration_key` establishes is proven by the compiler instead
/// of being asserted at runtime by an `.expect()` that a corrupt page could
/// turn into a host-process abort (#763).
pub(super) fn expiration_key_timestamp(key: &[u8; 40]) -> Timestamp {
    let &[s0, s1, s2, s3, s4, s5, s6, s7, ..] = key;
    Timestamp::from(u64::from_be_bytes([s0, s1, s2, s3, s4, s5, s6, s7]))
}

/// The inclusive upper bound of every [`expiration_key`] at or before `ts`.
pub(super) fn expiration_key_upper_bound(ts: Timestamp) -> [u8; 40] {
    let mut key = [u8::MAX; 40];
    key[..8].copy_from_slice(&ts.as_secs().to_be_bytes());
    key
}

pub(super) fn by_author_prefix(author: &PublicKey) -> Vec<u8> {
    author.as_bytes().to_vec()
}

pub(super) fn by_kind_prefix(kind: Kind) -> Vec<u8> {
    kind.as_u16().to_be_bytes().to_vec()
}

pub(super) fn tag_index_prefix(tag: SingleLetterTag, value: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(2 + 4 + value.len());
    key.push(tag.as_char() as u8);
    if let Some(raw_id) = decode_hex_32(value) {
        // nostrdb's packed-id win, kept portable and explicit: e/p/a-like
        // values that are exactly one 32-byte hex identity occupy raw bytes
        // in the index instead of repeating 64 ASCII bytes.
        key.push(1);
        key.extend_from_slice(&raw_id);
    } else {
        key.push(0);
        let value = value.as_bytes();
        let value_len = u32::try_from(value.len()).expect("a Nostr tag value fits in u32");
        key.extend_from_slice(&value_len.to_be_bytes());
        key.extend_from_slice(value);
    }
    key
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OrderedIndex {
    Global,
    Author,
    Kind,
    Tag(SingleLetterTag),
}

impl OrderedIndex {
    pub(super) fn matched(self) -> IndexedMatch {
        match self {
            Self::Global => IndexedMatch::None,
            Self::Author => IndexedMatch::Author,
            Self::Kind => IndexedMatch::Kind,
            Self::Tag(tag) => IndexedMatch::Tag(tag),
        }
    }

    pub(super) fn tie_rank(self) -> u8 {
        match self {
            Self::Author => 0,
            Self::Tag(_) => 1,
            Self::Kind => 2,
            Self::Global => 3,
        }
    }
}

/// One candidate ordered scan: which physical index family to walk, and the
/// prefixes within it that cover the filter.
#[derive(Debug)]
pub(super) struct OrderedPlan {
    pub(super) index: OrderedIndex,
    pub(super) prefixes: Vec<Vec<u8>>,
}

/// Every ordered index that can answer `filter` completely, most selective
/// first by [`OrderedIndex::tie_rank`].
///
/// This is the whole planner input. `Global` is always present and always
/// last, so the list is never empty. Every entry returns the SAME rows: the
/// post-index residual mask is derived from the chosen index
/// ([`OrderedIndex::matched`] feeding
/// `StoredEventView::matches_prepared_filter_after_index`), so whichever
/// candidate is walked, the predicates the index did not enforce are still
/// applied. Index choice is therefore a cost decision only — a worse choice
/// is slower, never wrong. `plan_choice_cannot_change_query_results` in
/// `tests.rs` is the falsifier for that claim.
pub(super) fn candidate_ordered_plans(filter: &Filter) -> Vec<OrderedPlan> {
    let mut plans = Vec::new();
    if let Some(authors) = filter.authors.as_ref().filter(|values| !values.is_empty()) {
        plans.push(OrderedPlan {
            index: OrderedIndex::Author,
            prefixes: authors.iter().map(by_author_prefix).collect(),
        });
    }
    for (tag, values) in &filter.generic_tags {
        if values.is_empty() {
            continue;
        }
        plans.push(OrderedPlan {
            index: OrderedIndex::Tag(*tag),
            prefixes: values
                .iter()
                .map(|value| tag_index_prefix(*tag, value))
                .collect(),
        });
    }
    if let Some(kinds) = filter.kinds.as_ref().filter(|values| !values.is_empty()) {
        plans.push(OrderedPlan {
            index: OrderedIndex::Kind,
            prefixes: kinds.iter().map(|kind| by_kind_prefix(*kind)).collect(),
        });
    }
    plans.push(OrderedPlan {
        index: OrderedIndex::Global,
        prefixes: vec![Vec::new()],
    });
    plans.sort_by_key(|plan| plan.index.tie_rank());
    plans
}

/// Choose the ordered index to scan for `filter`.
///
/// Fixed priority — Author > Tag > Kind > Global — the same choice
/// `RedbStore::plan_ordered_query` makes ("simple and obviously correct
/// beats optimal here"). NMP kept a durable sampled row-count table to rank
/// these instead, and deleted it (#1248): the estimate could not affect
/// correctness, 1/16 sampling quantized every bucket under ~16 rows to zero
/// so this same fixed priority already decided exactly the selective queries
/// the estimate existed to serve, and the query-authoritative structure —
/// packed postings segment headers — already carries an EXACT per-prefix
/// `posting_count`. A smarter planner belongs there, not in durable bytes.
pub(super) fn plan_ordered_query(filter: &Filter) -> OrderedPlan {
    candidate_ordered_plans(filter)
        .into_iter()
        .next()
        .expect("the global ordered plan is always a candidate")
}
