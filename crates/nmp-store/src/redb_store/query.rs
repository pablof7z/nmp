#[cfg(test)]
use super::canonical::observation_event_key;
#[cfg(any(test, feature = "bench-instrumentation"))]
use super::schema::EventKey;
#[cfg(test)]
use super::schema::{
    decode_relay_row, observation_relay_key, RelayKey, ADDR_INDEX, EVENTS, EVENT_COL_LOCAL,
    EVENT_COL_OBSERVATION, EVENT_COL_ROW, EVENT_IDS, EXPIRATION_INDEX, NEXT_EVENT_KEY,
    NEXT_RELAY_KEY, RELAYS, RELAY_IDS, STORE_META,
};
#[cfg(test)]
use super::BTreeSet;
#[cfg(feature = "bench-instrumentation")]
use super::Event;
#[cfg(test)]
use super::{address_key_for, binary_event, Database, RelayUrl};
use super::{
    decode_hex_32, Deserialize, EventId, Filter, IndexedMatch, Kind, PublicKey, Serialize,
    SingleLetterTag, Timestamp,
};
#[cfg(test)]
use super::{BTreeMap, StoredEventView};
#[cfg(test)]
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};

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

#[cfg(feature = "bench-instrumentation")]
pub(super) fn ordered_vec_key(prefix: &[u8], created_at: Timestamp, id: &EventId) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + 8 + 32);
    key.extend_from_slice(prefix);
    key.extend_from_slice(&created_at.as_secs().to_be_bytes());
    key.extend(id.as_bytes().iter().map(|byte| !byte));
    key
}

#[cfg(feature = "bench-instrumentation")]
pub(super) fn ordered_fixed_key<const N: usize>(
    prefix: &[u8],
    created_at: Timestamp,
    id: &EventId,
) -> [u8; N] {
    assert_eq!(prefix.len() + 40, N);
    let mut key = [0; N];
    key[..prefix.len()].copy_from_slice(prefix);
    key[prefix.len()..prefix.len() + 8].copy_from_slice(&created_at.as_secs().to_be_bytes());
    for (dst, byte) in key[prefix.len() + 8..].iter_mut().zip(id.as_bytes()) {
        *dst = !byte;
    }
    key
}

#[cfg(feature = "bench-instrumentation")]
pub(super) fn created_at_key(event: &Event) -> [u8; 40] {
    ordered_fixed_key(&[], event.created_at, &event.id)
}

#[cfg(feature = "bench-instrumentation")]
pub(super) fn by_author_key(event: &Event) -> [u8; 72] {
    ordered_fixed_key(event.pubkey.as_bytes(), event.created_at, &event.id)
}

pub(super) fn by_author_prefix(author: &PublicKey) -> Vec<u8> {
    author.as_bytes().to_vec()
}

#[cfg(feature = "bench-instrumentation")]
pub(super) fn by_kind_key(event: &Event) -> [u8; 42] {
    ordered_fixed_key(
        &event.kind.as_u16().to_be_bytes(),
        event.created_at,
        &event.id,
    )
}

pub(super) fn by_kind_prefix(kind: Kind) -> Vec<u8> {
    kind.as_u16().to_be_bytes().to_vec()
}

#[cfg(feature = "bench-instrumentation")]
pub(super) fn by_author_kind_key(event: &Event) -> [u8; 74] {
    let mut prefix = [0; 34];
    prefix[..32].copy_from_slice(event.pubkey.as_bytes());
    prefix[32..].copy_from_slice(&event.kind.as_u16().to_be_bytes());
    ordered_fixed_key(&prefix, event.created_at, &event.id)
}

/// Comparison-only cardinality-key builders, retained for the benchmark
/// variants that measure the durable-statistics physical shape (see
/// [`super::schema::INDEX_CARDINALITY`]). Nothing in [`crate::RedbStore`]
/// maintains or reads them.
#[cfg(feature = "bench-instrumentation")]
pub(super) const CARDINALITY_GLOBAL: u8 = 0;
#[cfg(feature = "bench-instrumentation")]
pub(super) const CARDINALITY_AUTHOR: u8 = 1;
#[cfg(feature = "bench-instrumentation")]
pub(super) const CARDINALITY_KIND: u8 = 2;
#[cfg(feature = "bench-instrumentation")]
pub(super) const CARDINALITY_TAG: u8 = 4;
#[cfg(feature = "bench-instrumentation")]
pub(super) const CARDINALITY_SAMPLE_MASK: u8 = 0x0f;

#[cfg(feature = "bench-instrumentation")]
pub(super) fn event_is_cardinality_sample(sample_key: &[u8; 32], id: &EventId) -> bool {
    blake3::keyed_hash(sample_key, id.as_bytes()).as_bytes()[0] & CARDINALITY_SAMPLE_MASK == 0
}

#[cfg(feature = "bench-instrumentation")]
pub(super) fn cardinality_key(namespace: u8, prefix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + prefix.len());
    key.push(namespace);
    key.extend_from_slice(prefix);
    key
}

#[cfg(feature = "bench-instrumentation")]
pub(super) fn global_cardinality_key() -> Vec<u8> {
    cardinality_key(CARDINALITY_GLOBAL, &[])
}

#[cfg(feature = "bench-instrumentation")]
pub(super) fn author_cardinality_key(author: &PublicKey) -> Vec<u8> {
    cardinality_key(CARDINALITY_AUTHOR, author.as_bytes())
}

#[cfg(feature = "bench-instrumentation")]
pub(super) fn kind_cardinality_key(kind: Kind) -> Vec<u8> {
    cardinality_key(CARDINALITY_KIND, &kind.as_u16().to_be_bytes())
}

#[cfg(feature = "bench-instrumentation")]
pub(super) fn tag_cardinality_key(tag: SingleLetterTag, value: &str) -> Vec<u8> {
    cardinality_key(CARDINALITY_TAG, &tag_index_prefix(tag, value))
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

#[cfg(feature = "bench-instrumentation")]
pub(super) fn tag_index_key(
    tag: SingleLetterTag,
    value: &str,
    created_at: Timestamp,
    id: &EventId,
) -> Vec<u8> {
    ordered_vec_key(&tag_index_prefix(tag, value), created_at, id)
}

#[cfg(test)]
pub(super) fn assert_canonical_integrity(db: &Database) {
    let read_txn = db.begin_read().expect("begin canonical integrity audit");
    let events = read_txn.open_table(EVENTS).expect("audit events");
    let event_ids = read_txn.open_table(EVENT_IDS).expect("audit event ids");
    let store_meta = read_txn
        .open_table(STORE_META)
        .expect("audit event store meta");
    let relays = read_txn.open_table(RELAYS).expect("audit relays");
    let relay_ids = read_txn.open_table(RELAY_IDS).expect("audit relay ids");

    // One pass over the folded event key space, split back into its three
    // columns. Nothing else may live in this tree: an unknown column byte is
    // a durable row the schema does not define.
    let mut canonical = BTreeMap::new();
    let mut local_rows = BTreeMap::new();
    let mut observation_rows = Vec::new();
    for entry in events.iter().expect("iterate audit events") {
        let (key, bytes) = entry.expect("read audit event");
        let key = key.value();
        let event_key =
            EventKey::from_be_bytes(key[..8].try_into().expect("canonical key leads with a key"));
        match key[8] {
            EVENT_COL_ROW => {
                assert_eq!(key.len(), 9, "an event row key carries no suffix");
                let view = StoredEventView::parse(bytes.value()).expect("audit event binary value");
                let event = view.materialize_event().expect("audit materialized event");
                assert_eq!(
                    event_ids
                        .get(event.id.as_bytes())
                        .expect("audit id lookup")
                        .expect("every event has a raw-id mapping")
                        .value(),
                    event_key
                );
                assert!(canonical.insert(event_key, event).is_none());
            }
            EVENT_COL_LOCAL => {
                assert_eq!(key.len(), 9, "a local sidecar key carries no suffix");
                assert!(local_rows
                    .insert(event_key, bytes.value().to_vec())
                    .is_none());
            }
            EVENT_COL_OBSERVATION => {
                assert_eq!(key.len(), 13, "an observation key names one relay");
                observation_rows.push(key.to_vec());
            }
            other => panic!("unknown canonical event column {other}"),
        }
    }

    assert_eq!(
        event_ids.len().expect("count audit event ids"),
        canonical.len() as u64
    );
    for entry in event_ids.iter().expect("iterate audit event ids") {
        let (raw_id, event_key) = entry.expect("read audit event id");
        let event_key = event_key.value();
        let event = canonical
            .get(&event_key)
            .expect("raw id mapping points at a live event");
        assert_eq!(raw_id.value(), event.id.as_bytes());
    }

    for (event_key, value) in &local_rows {
        assert!(canonical.contains_key(event_key));
        binary_event::decode_local(value).expect("audit local metadata sidecar");
    }

    if let Some(max_key) = canonical.keys().next_back() {
        let next = store_meta
            .get(NEXT_EVENT_KEY)
            .expect("audit next event key")
            .expect("nonempty canonical store has next event key")
            .value();
        assert!(next > *max_key, "surrogate allocator must not reuse keys");
    }

    let mut expected_relay_refs = BTreeMap::<RelayKey, u64>::new();
    for encoded_key in &observation_rows {
        let event_key = observation_event_key(encoded_key);
        let relay_key = observation_relay_key(encoded_key);
        assert!(
            canonical.contains_key(&event_key),
            "observation points at live event"
        );
        assert!(
            relays.get(relay_key).expect("audit relay lookup").is_some(),
            "observation points at interned relay"
        );
        *expected_relay_refs.entry(relay_key).or_default() += 1;
    }
    assert_eq!(
        relays.len().expect("count audit relays"),
        expected_relay_refs.len() as u64
    );
    assert_eq!(
        relay_ids.len().expect("count audit relay ids"),
        expected_relay_refs.len() as u64
    );
    for entry in relays.iter().expect("iterate audit relays") {
        let (relay_key, row) = entry.expect("read audit relay");
        let relay_key = relay_key.value();
        let (refs, url) = decode_relay_row(relay_key, row.value()).expect("audit relay row");
        RelayUrl::parse(url).expect("interned relay is canonical");
        assert_eq!(
            relay_ids
                .get(url)
                .expect("audit reverse relay lookup")
                .expect("relay has reverse key")
                .value(),
            relay_key
        );
        assert_eq!(refs, expected_relay_refs[&relay_key]);
    }
    for entry in relay_ids.iter().expect("iterate audit reverse relays") {
        let (encoded_url, relay_key) = entry.expect("read audit reverse relay");
        let relay_key = relay_key.value();
        let row = relays
            .get(relay_key)
            .expect("audit forward relay lookup")
            .expect("reverse relay has forward row");
        let (_refs, url) = decode_relay_row(relay_key, row.value()).expect("audit relay row");
        assert_eq!(url, encoded_url.value());
    }
    if let Some(max_key) = expected_relay_refs.keys().next_back() {
        let next = store_meta
            .get(NEXT_RELAY_KEY)
            .expect("audit next relay key")
            .expect("nonempty relay dictionary has next key")
            .value();
        assert!(
            next > u64::from(*max_key),
            "relay allocator must not reuse keys"
        );
    }

    let mut expected_address = BTreeSet::new();
    let mut expected_expiration = BTreeSet::new();
    for (&event_key, event) in &canonical {
        if let Some(address) = address_key_for(event) {
            expected_address.insert((address.to_redb_key(), event_key));
        }
        if let Some(timestamp) = event.tags.expiration().copied() {
            expected_expiration.insert((expiration_key(timestamp, &event.id), event_key));
        }
    }
    super::postings_store::assert_packed_integrity(&read_txn, &canonical);

    let address = read_txn
        .open_table(ADDR_INDEX)
        .expect("audit address index");
    let actual_address = address
        .iter()
        .expect("iterate audit address index")
        .map(|entry| {
            let (encoded_address, event_key) = entry.expect("read audit address index");
            (encoded_address.value().to_owned(), event_key.value())
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_address, expected_address);

    let expiration = read_txn
        .open_table(EXPIRATION_INDEX)
        .expect("audit expiration index");
    let actual_expiration = expiration
        .iter()
        .expect("iterate audit expiration index")
        .map(|entry| {
            let (encoded_expiration, event_key) = entry.expect("read audit expiration index");
            (encoded_expiration.value().to_owned(), event_key.value())
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_expiration, expected_expiration);
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

#[cfg(feature = "bench-instrumentation")]
pub(super) fn insert_tag_index_rows(
    by_tag: &mut redb::Table<'_, &[u8], EventKey>,
    event: &Event,
    event_key: EventKey,
) -> Result<(), redb::StorageError> {
    for tag in event.tags.iter() {
        let (Some(single_letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
            continue;
        };
        let key = tag_index_key(single_letter, value, event.created_at, &event.id);
        by_tag.insert(key.as_slice(), event_key)?;
    }
    Ok(())
}
