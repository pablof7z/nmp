use super::schema::{
    persist_err, EventKey, RelayKey, EVENTS, EVENT_IDS, EVENT_LOCAL, EVENT_OBSERVATIONS,
    EVENT_STORE_META, INDEX_CARDINALITY, NEXT_EVENT_KEY, NEXT_RELAY_KEY, RELAYS, RELAY_KEYS,
    RELAY_META, RELAY_REFS,
};
use super::{
    binary_event, BTreeMap, Event, EventId, HashMap, LocalOrigin, PersistenceError, Provenance,
    RelayUrl, StoredEvent, StoredEventView, Timestamp,
};
use redb::ReadableTable;

/// Owned mutation form of one portable binary event row. Query filtering
/// uses [`StoredEventView`] directly and never constructs this form for a
/// rejected candidate.
#[derive(Debug)]
pub(super) struct StoredEventRecord {
    pub(super) event: Event,
    pub(super) provenance: BTreeMap<RelayUrl, Timestamp>,
    pub(super) local: Option<LocalOrigin>,
}

/// Convert `se` into the record shape used by self-contained displaced rows
/// and governed mutation helpers.
pub(super) fn stored_event_to_record(se: &StoredEvent) -> StoredEventRecord {
    StoredEventRecord {
        event: se.event.clone(),
        provenance: se.provenance.seen.clone(),
        local: se.provenance.local.clone(),
    }
}

/// The read-side counterpart of [`stored_event_to_record`].
pub(super) fn record_to_stored_event(record: &StoredEventRecord) -> StoredEvent {
    StoredEvent {
        event: record.event.clone(),
        provenance: Provenance {
            seen: record.provenance.clone(),
            local: record.local.clone(),
        },
    }
}

/// Encode `se` as a self-contained portable `OUTBOX_DISPLACED` snapshot.
pub(super) fn encode_stored_event(se: &StoredEvent) -> Vec<u8> {
    binary_event::encode(se).expect("redb: encode portable stored event")
}

/// Materialize one self-contained portable `OUTBOX_DISPLACED` value — the
/// read-side counterpart of [`encode_stored_event`].
pub(super) fn decode_stored_event(bytes: &[u8]) -> StoredEvent {
    binary_event::decode(bytes).expect("redb: decode portable stored event")
}

pub(super) fn try_decode_stored_event(bytes: &[u8]) -> Result<StoredEvent, PersistenceError> {
    binary_event::decode(bytes)
        .map_err(|error| PersistenceError(format!("decode portable stored event: {error:?}")))
}

pub(super) fn decode_stored_event_record(bytes: &[u8]) -> StoredEventRecord {
    stored_event_to_record(&decode_stored_event(bytes))
}

pub(super) fn encode_stored_event_record(record: &StoredEventRecord) -> Vec<u8> {
    encode_stored_event(&record_to_stored_event(record))
}

pub(super) fn observation_key(event_key: EventKey, relay_key: RelayKey) -> [u8; 12] {
    let mut key = [0u8; 12];
    key[..8].copy_from_slice(&event_key.to_be_bytes());
    key[8..].copy_from_slice(&relay_key.to_be_bytes());
    key
}

pub(super) fn observation_range(event_key: EventKey) -> ([u8; 12], [u8; 12]) {
    (
        observation_key(event_key, RelayKey::MIN),
        observation_key(event_key, RelayKey::MAX),
    )
}

pub(super) fn observation_relay_key(key: &[u8]) -> RelayKey {
    RelayKey::from_be_bytes(
        key[8..12]
            .try_into()
            .expect("validated observation key is twelve bytes"),
    )
}

#[cfg(test)]
pub(super) fn observation_event_key(key: &[u8]) -> EventKey {
    EventKey::from_be_bytes(
        key[..8]
            .try_into()
            .expect("validated observation key is twelve bytes"),
    )
}

/// Tables that jointly own one canonical event row. Keeping them behind one
/// value makes it hard for a write path to mutate the immutable note without
/// also considering its raw-id mapping, local state, and relay observations.
pub(super) struct CanonicalWriteTables<'txn> {
    pub(super) events: redb::Table<'txn, EventKey, &'static [u8]>,
    pub(super) event_ids: redb::Table<'txn, &'static [u8; 32], EventKey>,
    pub(super) local: redb::Table<'txn, EventKey, &'static [u8]>,
    pub(super) store_meta: redb::Table<'txn, &'static str, EventKey>,
    pub(super) observations: redb::Table<'txn, &'static [u8; 12], u64>,
    pub(super) relays: redb::Table<'txn, RelayKey, &'static str>,
    pub(super) relay_keys: redb::Table<'txn, &'static str, RelayKey>,
    pub(super) relay_refs: redb::Table<'txn, RelayKey, u64>,
    pub(super) relay_meta: redb::Table<'txn, &'static str, RelayKey>,
    pub(super) cardinality: redb::Table<'txn, &'static [u8], u64>,
    /// Surrogate allocators are loaded once per write transaction and only
    /// flushed if consumed. A large ingest batch therefore writes each hot
    /// metadata row once, in the same atomic commit as its events/indexes.
    pub(super) next_event_key: EventKey,
    pub(super) next_relay_key: RelayKey,
    pub(super) event_allocator_dirty: bool,
    pub(super) relay_allocator_dirty: bool,
    /// Effective counts touched by this transaction. Busy batches commonly
    /// share one relay, so the durable hot row is read and written once.
    pub(super) relay_ref_counts: HashMap<RelayKey, u64>,
    /// Net live-row changes by ordered-index prefix. A governed batch can
    /// touch the same busy room/kind hundreds of times; persisting once per
    /// prefix keeps the single-writer transaction cheap.
    pub(super) cardinality_deltas: HashMap<Vec<u8>, i64>,
}

impl<'txn> CanonicalWriteTables<'txn> {
    pub(super) fn open(write_txn: &'txn redb::WriteTransaction) -> Result<Self, PersistenceError> {
        let store_meta = write_txn
            .open_table(EVENT_STORE_META)
            .map_err(persist_err)?;
        let next_event_key = store_meta
            .get(NEXT_EVENT_KEY)
            .map_err(persist_err)?
            .map(|guard| guard.value())
            .unwrap_or(1);
        let relay_meta = write_txn.open_table(RELAY_META).map_err(persist_err)?;
        let next_relay_key = relay_meta
            .get(NEXT_RELAY_KEY)
            .map_err(persist_err)?
            .map(|guard| guard.value())
            .unwrap_or(1);
        Ok(Self {
            events: write_txn.open_table(EVENTS).map_err(persist_err)?,
            event_ids: write_txn.open_table(EVENT_IDS).map_err(persist_err)?,
            local: write_txn.open_table(EVENT_LOCAL).map_err(persist_err)?,
            store_meta,
            observations: write_txn
                .open_table(EVENT_OBSERVATIONS)
                .map_err(persist_err)?,
            relays: write_txn.open_table(RELAYS).map_err(persist_err)?,
            relay_keys: write_txn.open_table(RELAY_KEYS).map_err(persist_err)?,
            relay_refs: write_txn.open_table(RELAY_REFS).map_err(persist_err)?,
            relay_meta,
            cardinality: write_txn
                .open_table(INDEX_CARDINALITY)
                .map_err(persist_err)?,
            next_event_key,
            next_relay_key,
            event_allocator_dirty: false,
            relay_allocator_dirty: false,
            relay_ref_counts: HashMap::new(),
            cardinality_deltas: HashMap::new(),
        })
    }

    pub(super) fn key_for_id(&self, id: &EventId) -> Result<Option<EventKey>, PersistenceError> {
        Ok(self
            .event_ids
            .get(id.as_bytes())
            .map_err(persist_err)?
            .map(|guard| guard.value()))
    }

    pub(super) fn load_by_key(
        &self,
        key: EventKey,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        let Some(event_bytes) = self.events.get(key).map_err(persist_err)? else {
            return Ok(None);
        };
        let local_bytes = self.local.get(key).map_err(persist_err)?;
        let event = StoredEventView::from_trusted(event_bytes.value())
            .map_err(|error| PersistenceError(format!("decode canonical event view: {error:?}")))?
            .materialize_event()
            .map_err(|error| PersistenceError(format!("materialize canonical event: {error:?}")))?;
        let local = local_bytes
            .map(|bytes| {
                binary_event::decode_local(bytes.value()).map_err(|error| {
                    PersistenceError(format!("decode canonical local state: {error:?}"))
                })
            })
            .transpose()?;
        let provenance = Provenance {
            seen: self.load_seen(key)?,
            local,
        };
        Ok(Some(StoredEvent { event, provenance }))
    }

    pub(super) fn load_local(
        &self,
        key: EventKey,
    ) -> Result<Option<LocalOrigin>, PersistenceError> {
        self.local
            .get(key)
            .map_err(persist_err)?
            .map(|bytes| {
                binary_event::decode_local(bytes.value()).map_err(|error| {
                    PersistenceError(format!("decode canonical local state: {error:?}"))
                })
            })
            .transpose()
    }

    pub(super) fn load_seen(
        &self,
        event_key: EventKey,
    ) -> Result<BTreeMap<RelayUrl, Timestamp>, PersistenceError> {
        let (lower, upper) = observation_range(event_key);
        let mut seen = BTreeMap::new();
        for entry in self
            .observations
            .range::<&[u8; 12]>(&lower..=&upper)
            .map_err(persist_err)?
        {
            let (encoded_key, at) = entry.map_err(persist_err)?;
            let relay_key = observation_relay_key(encoded_key.value());
            let relay = self
                .relays
                .get(relay_key)
                .map_err(persist_err)?
                .expect("redb: observation relay key exists");
            let relay =
                RelayUrl::parse(relay.value()).expect("redb: interned relay URL remains canonical");
            assert!(seen.insert(relay, Timestamp::from(at.value())).is_none());
        }
        Ok(seen)
    }

    pub(super) fn load_by_id(
        &self,
        id: &EventId,
    ) -> Result<Option<(EventKey, StoredEvent)>, PersistenceError> {
        let Some(key) = self.key_for_id(id)? else {
            return Ok(None);
        };
        Ok(self.load_by_key(key)?.map(|stored| (key, stored)))
    }

    pub(super) fn allocate_key(&mut self) -> Result<EventKey, PersistenceError> {
        let next = self.next_event_key;
        self.next_event_key = next
            .checked_add(1)
            .ok_or_else(|| PersistenceError("canonical event key space exhausted".to_owned()))?;
        self.event_allocator_dirty = true;
        Ok(next)
    }

    pub(super) fn allocate_relay_key(&mut self) -> Result<RelayKey, PersistenceError> {
        let next = self.next_relay_key;
        self.next_relay_key = next
            .checked_add(1)
            .ok_or_else(|| PersistenceError("relay key space exhausted".to_owned()))?;
        self.relay_allocator_dirty = true;
        Ok(next)
    }

    pub(super) fn intern_relay(&mut self, relay: &RelayUrl) -> Result<RelayKey, PersistenceError> {
        if let Some(existing) = self.relay_keys.get(relay.as_str()).map_err(persist_err)? {
            return Ok(existing.value());
        }
        let key = self.allocate_relay_key()?;
        self.relays
            .insert(key, relay.as_str())
            .map_err(persist_err)?;
        self.relay_keys
            .insert(relay.as_str(), key)
            .map_err(persist_err)?;
        self.relay_refs.insert(key, 0).map_err(persist_err)?;
        Ok(key)
    }

    pub(super) fn effective_relay_ref(
        &mut self,
        relay_key: RelayKey,
    ) -> Result<u64, PersistenceError> {
        if let Some(current) = self.relay_ref_counts.get(&relay_key) {
            return Ok(*current);
        }
        let current = self
            .relay_refs
            .get(relay_key)
            .map_err(persist_err)?
            .expect("redb: interned relay has refcount")
            .value();
        self.relay_ref_counts.insert(relay_key, current);
        Ok(current)
    }

    pub(super) fn increment_relay_ref(
        &mut self,
        relay_key: RelayKey,
    ) -> Result<(), PersistenceError> {
        let current = self.effective_relay_ref(relay_key)?;
        let next = current
            .checked_add(1)
            .ok_or_else(|| PersistenceError("relay reference count exhausted".to_owned()))?;
        self.relay_ref_counts.insert(relay_key, next);
        Ok(())
    }

    pub(super) fn decrement_relay_ref(
        &mut self,
        relay_key: RelayKey,
    ) -> Result<(), PersistenceError> {
        let current = self.effective_relay_ref(relay_key)?;
        let next = current
            .checked_sub(1)
            .ok_or_else(|| PersistenceError("relay reference count underflow".to_owned()))?;
        self.relay_ref_counts.insert(relay_key, next);
        Ok(())
    }

    pub(super) fn adjust_cardinality(
        &mut self,
        key: Vec<u8>,
        delta: i64,
    ) -> Result<(), PersistenceError> {
        let current = self.cardinality_deltas.entry(key).or_default();
        *current = current
            .checked_add(delta)
            .ok_or_else(|| PersistenceError("index cardinality delta overflow".to_owned()))?;
        Ok(())
    }

    /// Flush every transaction-local mutation exactly once before the caller
    /// commits: surrogate high-water marks, relay refcounts, and index
    /// cardinalities remain part of the same crash-atomic event transaction.
    pub(super) fn flush_pending(&mut self) -> Result<(), PersistenceError> {
        if self.event_allocator_dirty {
            self.store_meta
                .insert(NEXT_EVENT_KEY, self.next_event_key)
                .map_err(persist_err)?;
            self.event_allocator_dirty = false;
        }
        if self.relay_allocator_dirty {
            self.relay_meta
                .insert(NEXT_RELAY_KEY, self.next_relay_key)
                .map_err(persist_err)?;
            self.relay_allocator_dirty = false;
        }
        for (relay_key, effective) in std::mem::take(&mut self.relay_ref_counts) {
            let persisted = self
                .relay_refs
                .get(relay_key)
                .map_err(persist_err)?
                .expect("redb: interned relay has refcount")
                .value();
            if effective > 0 {
                if effective == persisted {
                    continue;
                }
                self.relay_refs
                    .insert(relay_key, effective)
                    .map_err(persist_err)?;
                continue;
            }
            let relay = self
                .relays
                .get(relay_key)
                .map_err(persist_err)?
                .expect("redb: interned relay exists")
                .value()
                .to_owned();
            self.relay_refs.remove(relay_key).map_err(persist_err)?;
            self.relays.remove(relay_key).map_err(persist_err)?;
            self.relay_keys
                .remove(relay.as_str())
                .map_err(persist_err)?;
        }
        for (key, delta) in std::mem::take(&mut self.cardinality_deltas) {
            if delta == 0 {
                continue;
            }
            let persisted = self
                .cardinality
                .get(key.as_slice())
                .map_err(persist_err)?
                .map(|guard| guard.value())
                .unwrap_or(0);
            let effective = if delta > 0 {
                persisted.checked_add(delta as u64)
            } else {
                persisted.checked_sub(delta.unsigned_abs())
            }
            .ok_or_else(|| {
                PersistenceError(format!(
                    "index cardinality underflow/overflow for prefix {key:?}"
                ))
            })?;
            if effective == 0 {
                self.cardinality
                    .remove(key.as_slice())
                    .map_err(persist_err)?;
            } else {
                self.cardinality
                    .insert(key.as_slice(), effective)
                    .map_err(persist_err)?;
            }
        }
        Ok(())
    }

    pub(super) fn merge_observation(
        &mut self,
        event_key: EventKey,
        relay: &RelayUrl,
        at: Timestamp,
    ) -> Result<bool, PersistenceError> {
        let relay_key = self.intern_relay(relay)?;
        let encoded_key = observation_key(event_key, relay_key);
        let existing = self
            .observations
            .get(&encoded_key)
            .map_err(persist_err)?
            .map(|guard| guard.value());
        if existing.is_some_and(|existing| existing >= at.as_secs()) {
            return Ok(false);
        }
        self.observations
            .insert(&encoded_key, at.as_secs())
            .map_err(persist_err)?;
        if existing.is_none() {
            self.increment_relay_ref(relay_key)?;
        }
        Ok(true)
    }

    pub(super) fn remove_observation(
        &mut self,
        event_key: EventKey,
        relay_key: RelayKey,
    ) -> Result<(), PersistenceError> {
        let encoded_key = observation_key(event_key, relay_key);
        if self
            .observations
            .remove(&encoded_key)
            .map_err(persist_err)?
            .is_some()
        {
            self.decrement_relay_ref(relay_key)?;
        }
        Ok(())
    }

    pub(super) fn remove_all_observations(
        &mut self,
        event_key: EventKey,
    ) -> Result<(), PersistenceError> {
        let (lower, upper) = observation_range(event_key);
        let relay_keys = self
            .observations
            .range::<&[u8; 12]>(&lower..=&upper)
            .map_err(persist_err)?
            .map(|entry| {
                entry
                    .map(|(key, _)| observation_relay_key(key.value()))
                    .map_err(persist_err)
            })
            .collect::<Result<Vec<_>, _>>()?;
        for relay_key in relay_keys {
            self.remove_observation(event_key, relay_key)?;
        }
        Ok(())
    }

    pub(super) fn insert_new(
        &mut self,
        event: &Event,
        provenance: &Provenance,
    ) -> Result<EventKey, PersistenceError> {
        debug_assert!(self.key_for_id(&event.id)?.is_none());
        let key = self.allocate_key()?;
        #[cfg(feature = "bench-instrumentation")]
        let encode_started = std::time::Instant::now();
        let event_bytes =
            binary_event::encode_event(event).expect("redb: encode immutable canonical event");
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::encode_event(encode_started.elapsed(), event_bytes.len());
        #[cfg(feature = "bench-instrumentation")]
        let insert_started = std::time::Instant::now();
        self.events
            .insert(key, event_bytes.as_slice())
            .map_err(persist_err)?;
        self.event_ids
            .insert(event.id.as_bytes(), key)
            .map_err(persist_err)?;
        if let Some(local) = &provenance.local {
            let encoded =
                binary_event::encode_local(local).expect("redb: encode canonical local state");
            self.local
                .insert(key, encoded.as_slice())
                .map_err(persist_err)?;
        }
        for (relay, at) in &provenance.seen {
            self.merge_observation(key, relay, *at)?;
        }
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::canonical_insert(insert_started.elapsed());
        Ok(key)
    }

    pub(super) fn replace_event(
        &mut self,
        key: EventKey,
        event: &Event,
    ) -> Result<(), PersistenceError> {
        let encoded =
            binary_event::encode_event(event).expect("redb: encode immutable canonical event");
        self.events
            .insert(key, encoded.as_slice())
            .map_err(persist_err)?;
        Ok(())
    }

    pub(super) fn replace_provenance(
        &mut self,
        key: EventKey,
        provenance: &Provenance,
    ) -> Result<(), PersistenceError> {
        let existing = self.load_seen(key)?;
        for relay in existing.keys() {
            if !provenance.seen.contains_key(relay) {
                let relay_key = self
                    .relay_keys
                    .get(relay.as_str())
                    .map_err(persist_err)?
                    .expect("redb: observed relay remains interned")
                    .value();
                self.remove_observation(key, relay_key)?;
            }
        }
        for (relay, at) in &provenance.seen {
            if existing.get(relay) != Some(at) {
                let relay_key = self.intern_relay(relay)?;
                let encoded_key = observation_key(key, relay_key);
                let was_absent = self
                    .observations
                    .get(&encoded_key)
                    .map_err(persist_err)?
                    .is_none();
                self.observations
                    .insert(&encoded_key, at.as_secs())
                    .map_err(persist_err)?;
                if was_absent {
                    self.increment_relay_ref(relay_key)?;
                }
            }
        }
        self.replace_local(key, provenance.local.clone())
    }

    pub(super) fn replace_local(
        &mut self,
        key: EventKey,
        local: Option<LocalOrigin>,
    ) -> Result<(), PersistenceError> {
        if let Some(local) = local {
            let encoded =
                binary_event::encode_local(&local).expect("redb: encode canonical local state");
            self.local
                .insert(key, encoded.as_slice())
                .map_err(persist_err)?;
        } else {
            self.local.remove(key).map_err(persist_err)?;
        }
        Ok(())
    }

    pub(super) fn remove_by_key(
        &mut self,
        key: EventKey,
        id: &EventId,
    ) -> Result<(), PersistenceError> {
        self.events.remove(key).map_err(persist_err)?;
        self.event_ids.remove(id.as_bytes()).map_err(persist_err)?;
        self.local.remove(key).map_err(persist_err)?;
        self.remove_all_observations(key)?;
        Ok(())
    }
}
