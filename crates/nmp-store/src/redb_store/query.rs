use super::canonical::CanonicalWriteTables;
#[cfg(test)]
use super::canonical::{observation_event_key, observation_relay_key};
use super::schema::{persist_err, EventKey, INDEX_CARDINALITY};
#[cfg(test)]
use super::schema::{
    RelayKey, ADDR_INDEX, EVENTS, EVENT_IDS, EVENT_LOCAL, EVENT_OBSERVATIONS, EVENT_STORE_META,
    EXPIRATION_INDEX, INDEX_CARDINALITY_META, INDEX_CARDINALITY_SAMPLE_KEY,
    INDEX_CARDINALITY_SAMPLE_META, INDEX_CARDINALITY_VERSION, INDEX_CARDINALITY_VERSION_KEY,
    NEXT_EVENT_KEY, NEXT_RELAY_KEY, RELAYS, RELAY_KEYS, RELAY_META, RELAY_REFS,
};
#[cfg(test)]
use super::{address_key_for, binary_event, Database, RelayUrl};
use super::{
    decode_hex_32, BTreeMap, BTreeSet, Deserialize, Event, EventId, Filter, IndexedMatch, Kind,
    PersistenceError, PublicKey, Serialize, SingleLetterTag, StoredEventView, Timestamp,
};
use redb::ReadableTable;
#[cfg(test)]
use redb::{ReadableDatabase, ReadableTableMetadata};

/// The `addr_tombstones` table's JSON value.
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

pub(super) const CARDINALITY_GLOBAL: u8 = 0;
pub(super) const CARDINALITY_AUTHOR: u8 = 1;
pub(super) const CARDINALITY_KIND: u8 = 2;
pub(super) const CARDINALITY_TAG: u8 = 4;
pub(super) const CARDINALITY_SAMPLE_MASK: u8 = 0x0f;

#[cfg(feature = "bench-instrumentation")]
static BENCH_EXACT_CARDINALITY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "bench-instrumentation")]
pub fn set_bench_exact_cardinality(enabled: bool) {
    BENCH_EXACT_CARDINALITY.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

pub(super) fn event_is_cardinality_sample(sample_key: &[u8; 32], id: &EventId) -> bool {
    #[cfg(feature = "bench-instrumentation")]
    if BENCH_EXACT_CARDINALITY.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    blake3::keyed_hash(sample_key, id.as_bytes()).as_bytes()[0] & CARDINALITY_SAMPLE_MASK == 0
}

pub(super) fn cardinality_key(namespace: u8, prefix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + prefix.len());
    key.push(namespace);
    key.extend_from_slice(prefix);
    key
}

pub(super) fn global_cardinality_key() -> Vec<u8> {
    cardinality_key(CARDINALITY_GLOBAL, &[])
}

pub(super) fn author_cardinality_key(author: &PublicKey) -> Vec<u8> {
    cardinality_key(CARDINALITY_AUTHOR, author.as_bytes())
}

pub(super) fn kind_cardinality_key(kind: Kind) -> Vec<u8> {
    cardinality_key(CARDINALITY_KIND, &kind.as_u16().to_be_bytes())
}

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

pub(super) fn add_event_cardinalities(
    counts: &mut BTreeMap<Vec<u8>, u64>,
    sample_key: &[u8; 32],
    event: &Event,
) {
    if !event_is_cardinality_sample(sample_key, &event.id) {
        return;
    }
    let mut increment = |key: Vec<u8>| {
        let count = counts.entry(key).or_default();
        *count = count.checked_add(1).expect("event cardinality fits in u64");
    };
    increment(global_cardinality_key());
    increment(author_cardinality_key(&event.pubkey));
    increment(kind_cardinality_key(event.kind));
    let mut tags = BTreeSet::new();
    for tag in event.tags.iter() {
        let (Some(single_letter), Some(value)) = (tag.single_letter_tag(), tag.content()) else {
            continue;
        };
        tags.insert(tag_cardinality_key(single_letter, value));
    }
    for key in tags {
        increment(key);
    }
}

pub(super) fn rebuild_index_cardinality_from_events(
    events: &redb::Table<'_, EventKey, &[u8]>,
    cardinality: &mut redb::Table<'_, &[u8], u64>,
    sample_key: &[u8; 32],
) -> Result<(), redb::StorageError> {
    let mut counts = BTreeMap::new();
    for row in events.iter()? {
        let (_event_key, value) = row?;
        let event = StoredEventView::from_trusted(value.value())
            .expect("redb: canonical event remains valid")
            .materialize_event()
            .expect("redb: canonical event materializes");
        add_event_cardinalities(&mut counts, sample_key, &event);
    }
    let existing: Vec<Vec<u8>> = cardinality
        .iter()?
        .map(|entry| entry.map(|(key, _)| key.value().to_vec()))
        .collect::<Result<_, _>>()?;
    for key in existing {
        cardinality.remove(key.as_slice())?;
    }
    for (key, count) in counts {
        cardinality.insert(key.as_slice(), count)?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn assert_canonical_integrity(db: &Database) {
    let read_txn = db.begin_read().expect("begin canonical integrity audit");
    let events = read_txn.open_table(EVENTS).expect("audit events");
    let event_ids = read_txn.open_table(EVENT_IDS).expect("audit event ids");
    let local = read_txn
        .open_table(EVENT_LOCAL)
        .expect("audit event local metadata");
    let store_meta = read_txn
        .open_table(EVENT_STORE_META)
        .expect("audit event store meta");
    let observations = read_txn
        .open_table(EVENT_OBSERVATIONS)
        .expect("audit event observations");
    let relays = read_txn.open_table(RELAYS).expect("audit relays");
    let relay_keys = read_txn.open_table(RELAY_KEYS).expect("audit relay keys");
    let relay_refs = read_txn.open_table(RELAY_REFS).expect("audit relay refs");
    let relay_meta = read_txn.open_table(RELAY_META).expect("audit relay meta");
    let cardinality = read_txn
        .open_table(INDEX_CARDINALITY)
        .expect("audit index cardinality");
    let cardinality_meta = read_txn
        .open_table(INDEX_CARDINALITY_META)
        .expect("audit index cardinality meta");
    assert_eq!(
        cardinality_meta
            .get(INDEX_CARDINALITY_VERSION_KEY)
            .expect("audit cardinality version")
            .expect("cardinality version exists")
            .value(),
        INDEX_CARDINALITY_VERSION
    );
    let cardinality_sample_meta = read_txn
        .open_table(INDEX_CARDINALITY_SAMPLE_META)
        .expect("audit cardinality sample meta");
    let cardinality_sample_key: [u8; 32] = cardinality_sample_meta
        .get(INDEX_CARDINALITY_SAMPLE_KEY)
        .expect("audit cardinality sample key")
        .expect("cardinality sample key exists")
        .value()
        .try_into()
        .expect("cardinality sample key is 32 bytes");

    let mut canonical = BTreeMap::new();
    for entry in events.iter().expect("iterate audit events") {
        let (key, bytes) = entry.expect("read audit event");
        let key = key.value();
        let view = StoredEventView::parse(bytes.value()).expect("audit event binary value");
        let event = view.materialize_event().expect("audit materialized event");
        assert_eq!(
            event_ids
                .get(event.id.as_bytes())
                .expect("audit id lookup")
                .expect("every event has a raw-id mapping")
                .value(),
            key
        );
        assert!(canonical.insert(key, event).is_none());
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

    for entry in local.iter().expect("iterate audit local metadata") {
        let (event_key, value) = entry.expect("read audit local metadata");
        assert!(canonical.contains_key(&event_key.value()));
        binary_event::decode_local(value.value()).expect("audit local metadata sidecar");
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
    for entry in observations.iter().expect("iterate audit observations") {
        let (encoded_key, _at) = entry.expect("read audit observation");
        let encoded_key = encoded_key.value();
        assert_eq!(encoded_key.len(), 12);
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
        relay_keys.len().expect("count audit relay keys"),
        expected_relay_refs.len() as u64
    );
    assert_eq!(
        relay_refs.len().expect("count audit relay refs"),
        expected_relay_refs.len() as u64
    );
    for entry in relays.iter().expect("iterate audit relays") {
        let (relay_key, encoded_url) = entry.expect("read audit relay");
        let relay_key = relay_key.value();
        RelayUrl::parse(encoded_url.value()).expect("interned relay is canonical");
        assert_eq!(
            relay_keys
                .get(encoded_url.value())
                .expect("audit reverse relay lookup")
                .expect("relay has reverse key")
                .value(),
            relay_key
        );
        assert_eq!(
            relay_refs
                .get(relay_key)
                .expect("audit relay ref lookup")
                .expect("relay has refcount")
                .value(),
            expected_relay_refs[&relay_key]
        );
    }
    for entry in relay_keys.iter().expect("iterate audit reverse relays") {
        let (encoded_url, relay_key) = entry.expect("read audit reverse relay");
        assert_eq!(
            relays
                .get(relay_key.value())
                .expect("audit forward relay lookup")
                .expect("reverse relay has forward row")
                .value(),
            encoded_url.value()
        );
    }
    if let Some(max_key) = expected_relay_refs.keys().next_back() {
        let next = relay_meta
            .get(NEXT_RELAY_KEY)
            .expect("audit next relay key")
            .expect("nonempty relay dictionary has next key")
            .value();
        assert!(next > *max_key, "relay allocator must not reuse keys");
    }

    let mut expected_address = BTreeSet::new();
    let mut expected_expiration = BTreeSet::new();
    let mut expected_cardinality = BTreeMap::new();
    for (&event_key, event) in &canonical {
        add_event_cardinalities(&mut expected_cardinality, &cardinality_sample_key, event);
        if let Some(address) = address_key_for(event) {
            expected_address.insert((address.to_redb_key(), event_key));
        }
        if let Some(timestamp) = event.tags.expiration().copied() {
            expected_expiration.insert((expiration_key(timestamp, &event.id), event_key));
        }
    }
    super::postings_store::assert_packed_integrity(&read_txn, &canonical);
    let actual_cardinality = cardinality
        .iter()
        .expect("iterate audit cardinality")
        .map(|entry| {
            let (key, count) = entry.expect("read audit cardinality");
            (key.value().to_vec(), count.value())
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(actual_cardinality, expected_cardinality);

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

#[derive(Debug)]
pub(super) struct OrderedPlan {
    pub(super) index: OrderedIndex,
    pub(super) prefixes: Vec<Vec<u8>>,
    pub(super) estimated_rows: u64,
}

pub(super) fn cardinality_of(
    table: &redb::ReadOnlyTable<&[u8], u64>,
    key: &[u8],
) -> Result<u64, PersistenceError> {
    Ok(table
        .get(key)
        .map_err(persist_err)?
        .map(|guard| guard.value())
        .unwrap_or(0))
}

pub(super) fn sum_cardinalities<'a>(
    table: &redb::ReadOnlyTable<&[u8], u64>,
    keys: impl IntoIterator<Item = &'a [u8]>,
) -> Result<u64, PersistenceError> {
    let mut total = 0u64;
    for key in keys {
        total = total.saturating_add(cardinality_of(table, key)?);
    }
    Ok(total)
}

pub(super) fn plan_ordered_query(
    read_txn: &redb::ReadTransaction,
    filter: &Filter,
) -> Result<OrderedPlan, PersistenceError> {
    let cardinality = read_txn
        .open_table(INDEX_CARDINALITY)
        .map_err(persist_err)?;
    let global_key = global_cardinality_key();
    let mut plans = vec![OrderedPlan {
        index: OrderedIndex::Global,
        prefixes: vec![Vec::new()],
        estimated_rows: cardinality_of(&cardinality, &global_key)?,
    }];

    let authors = filter.authors.as_ref().filter(|values| !values.is_empty());
    let kinds = filter.kinds.as_ref().filter(|values| !values.is_empty());
    if let Some(authors) = authors {
        let prefixes: Vec<_> = authors.iter().map(by_author_prefix).collect();
        let keys: Vec<_> = authors.iter().map(author_cardinality_key).collect();
        plans.push(OrderedPlan {
            index: OrderedIndex::Author,
            prefixes,
            estimated_rows: sum_cardinalities(&cardinality, keys.iter().map(Vec::as_slice))?,
        });
    }
    if let Some(kinds) = kinds {
        let prefixes: Vec<_> = kinds.iter().map(|kind| by_kind_prefix(*kind)).collect();
        let keys: Vec<_> = kinds
            .iter()
            .map(|kind| kind_cardinality_key(*kind))
            .collect();
        plans.push(OrderedPlan {
            index: OrderedIndex::Kind,
            prefixes,
            estimated_rows: sum_cardinalities(&cardinality, keys.iter().map(Vec::as_slice))?,
        });
    }
    for (tag, values) in &filter.generic_tags {
        let prefixes: Vec<_> = values
            .iter()
            .map(|value| tag_index_prefix(*tag, value))
            .collect();
        let keys: Vec<_> = values
            .iter()
            .map(|value| tag_cardinality_key(*tag, value))
            .collect();
        plans.push(OrderedPlan {
            index: OrderedIndex::Tag(*tag),
            prefixes,
            estimated_rows: sum_cardinalities(&cardinality, keys.iter().map(Vec::as_slice))?,
        });
    }

    Ok(plans
        .into_iter()
        .min_by_key(|plan| (plan.estimated_rows, plan.index.tie_rank()))
        .expect("global ordered query plan always exists"))
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

pub(super) fn insert_query_cardinalities(
    canonical: &mut CanonicalWriteTables<'_>,
    event: &Event,
) -> Result<(), PersistenceError> {
    #[cfg(feature = "bench-instrumentation")]
    let started = std::time::Instant::now();
    if event_is_cardinality_sample(&canonical.cardinality_sample_key, &event.id) {
        canonical.adjust_cardinality(global_cardinality_key(), 1)?;
        canonical.adjust_cardinality(author_cardinality_key(&event.pubkey), 1)?;
        canonical.adjust_cardinality(kind_cardinality_key(event.kind), 1)?;
        let mut tags = BTreeSet::new();
        for tag in event.tags.iter() {
            let (Some(single_letter), Some(value)) = (tag.single_letter_tag(), tag.content())
            else {
                continue;
            };
            tags.insert(tag_cardinality_key(single_letter, value));
        }
        for key in tags {
            canonical.adjust_cardinality(key, 1)?;
        }
    }
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::index_insert(started.elapsed());
    Ok(())
}

pub(super) fn remove_query_cardinalities(
    canonical: &mut CanonicalWriteTables<'_>,
    event: &Event,
) -> Result<(), PersistenceError> {
    if event_is_cardinality_sample(&canonical.cardinality_sample_key, &event.id) {
        canonical.adjust_cardinality(global_cardinality_key(), -1)?;
        canonical.adjust_cardinality(author_cardinality_key(&event.pubkey), -1)?;
        canonical.adjust_cardinality(kind_cardinality_key(event.kind), -1)?;
        let mut tags = BTreeSet::new();
        for tag in event.tags.iter() {
            let (Some(single_letter), Some(value)) = (tag.single_letter_tag(), tag.content())
            else {
                continue;
            };
            tags.insert(tag_cardinality_key(single_letter, value));
        }
        for key in tags {
            canonical.adjust_cardinality(key, -1)?;
        }
    }
    Ok(())
}
