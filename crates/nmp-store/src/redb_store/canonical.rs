use super::schema::{
    decode_relay_row, encode_relay_row, event_all_columns_bounds, event_local_key, event_row_key,
    observation_bounds, observation_key, observation_relay_key, persist_err, EventKey, RelayKey,
    EVENTS, EVENT_IDS, NEXT_EVENT_KEY, NEXT_RELAY_KEY, RELAYS, RELAY_IDS, STORE_META,
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

/// Encode `se` as a self-contained portable `PUBLISH_QUEUE_DISPLACED` snapshot.
pub(super) fn encode_stored_event(se: &StoredEvent) -> Vec<u8> {
    binary_event::encode(se).expect("redb: encode portable stored event")
}

pub(super) fn encode_stored_event_record(record: &StoredEventRecord) -> Vec<u8> {
    encode_stored_event(&record_to_stored_event(record))
}

/// Decode one observation value. Fallible rather than a `try_into().expect()`:
/// the value now shares a tree with the note row and the local sidecar, so a
/// wrong-width value must refuse the read instead of panicking inside a write
/// transaction.
pub(super) fn decode_observed_at(
    event_key: EventKey,
    relay_key: RelayKey,
    value: &[u8],
) -> Result<u64, PersistenceError> {
    let bytes: [u8; 8] = value.try_into().map_err(|_| {
        PersistenceError::new(format!(
            "observation for event {event_key} relay {relay_key} is {} bytes, expected 8",
            value.len()
        ))
    })?;
    Ok(u64::from_be_bytes(bytes))
}

/// Fold one persisted observation into the typed relay identity exposed to
/// callers. Distinct durable relay keys can outlive URL-normalization changes
/// and parse to the same [`RelayUrl`]; the latest observation remains the
/// strongest truthful seen-at fact.
pub(super) fn fold_seen_at(
    seen: &mut BTreeMap<RelayUrl, Timestamp>,
    relay: RelayUrl,
    at: Timestamp,
) {
    seen.entry(relay)
        .and_modify(|existing| {
            if at > *existing {
                *existing = at;
            }
        })
        .or_insert(at);
}

#[cfg(test)]
pub(super) fn observation_event_key(key: &[u8]) -> EventKey {
    EventKey::from_be_bytes(
        key[..8]
            .try_into()
            .expect("validated observation key is thirteen bytes"),
    )
}

/// Tables that jointly own one canonical event row. Keeping them behind one
/// value makes it hard for a write path to mutate the immutable note without
/// also considering its raw-id mapping, local state, and relay observations.
pub(super) struct CanonicalWriteTables<'txn> {
    /// The one canonical event tree. The note row, the local sidecar and the
    /// relay observations are columns of the same event key, so one handle
    /// serves all three — redb permits a table to be open once per write
    /// transaction.
    events: redb::Table<'txn, &'static [u8], &'static [u8]>,
    pub(super) event_ids: redb::Table<'txn, &'static [u8; 32], EventKey>,
    /// The one durable-scalar tree. Both surrogate allocators live here, so
    /// one handle serves both — redb permits a table to be open once per
    /// write transaction.
    pub(super) store_meta: redb::Table<'txn, &'static str, u64>,
    pub(super) relays: redb::Table<'txn, RelayKey, &'static [u8]>,
    pub(super) relay_ids: redb::Table<'txn, &'static str, RelayKey>,
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
}

impl<'txn> CanonicalWriteTables<'txn> {
    pub(super) fn open(write_txn: &'txn redb::WriteTransaction) -> Result<Self, PersistenceError> {
        let store_meta = write_txn.open_table(STORE_META).map_err(persist_err)?;
        let next_event_key = store_meta
            .get(NEXT_EVENT_KEY)
            .map_err(persist_err)?
            .map(|guard| guard.value())
            .unwrap_or(1);
        let next_relay_key = store_meta
            .get(NEXT_RELAY_KEY)
            .map_err(persist_err)?
            .map(|guard| RelayKey::try_from(guard.value()))
            .transpose()
            .map_err(|_| {
                PersistenceError::new("relay surrogate allocator overflows u32".to_owned())
            })?
            .unwrap_or(1);
        Ok(Self {
            events: write_txn.open_table(EVENTS).map_err(persist_err)?,
            event_ids: write_txn.open_table(EVENT_IDS).map_err(persist_err)?,
            store_meta,
            relays: write_txn.open_table(RELAYS).map_err(persist_err)?,
            relay_ids: write_txn.open_table(RELAY_IDS).map_err(persist_err)?,
            next_event_key,
            next_relay_key,
            event_allocator_dirty: false,
            relay_allocator_dirty: false,
            relay_ref_counts: HashMap::new(),
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
        let Some(event_bytes) = self
            .events
            .get(event_row_key(key).as_slice())
            .map_err(persist_err)?
        else {
            return Ok(None);
        };
        let local_bytes = self
            .events
            .get(event_local_key(key).as_slice())
            .map_err(persist_err)?;
        let event = StoredEventView::from_trusted(event_bytes.value())
            .map_err(|error| {
                PersistenceError::new(format!("decode canonical event view: {error:?}"))
            })?
            .materialize_event()
            .map_err(|error| {
                PersistenceError::new(format!("materialize canonical event: {error:?}"))
            })?;
        let local = local_bytes
            .map(|bytes| {
                binary_event::decode_local(bytes.value()).map_err(|error| {
                    PersistenceError::new(format!("decode canonical local state: {error:?}"))
                })
            })
            .transpose()?;
        let provenance = Provenance {
            seen: self.load_seen(key)?,
            local,
        };
        Ok(Some(StoredEvent { event, provenance }))
    }

    /// Read-only ordered scan of every canonical column row.
    ///
    /// The mutating door stays closed: `redb::Range` cannot insert or remove,
    /// so a full-tree walk (gc's victim pass) is reachable without handing out
    /// the write handle that would let a caller bypass the eight mutators
    /// above.
    pub(super) fn scan(
        &self,
    ) -> Result<redb::Range<'_, &'static [u8], &'static [u8]>, PersistenceError> {
        self.events.iter().map_err(persist_err)
    }

    pub(super) fn load_local(
        &self,
        key: EventKey,
    ) -> Result<Option<LocalOrigin>, PersistenceError> {
        self.events
            .get(event_local_key(key).as_slice())
            .map_err(persist_err)?
            .map(|bytes| {
                binary_event::decode_local(bytes.value()).map_err(|error| {
                    PersistenceError::new(format!("decode canonical local state: {error:?}"))
                })
            })
            .transpose()
    }

    pub(super) fn load_seen(
        &self,
        event_key: EventKey,
    ) -> Result<BTreeMap<RelayUrl, Timestamp>, PersistenceError> {
        let (lower, upper) = observation_bounds(event_key);
        let mut seen = BTreeMap::new();
        for entry in self
            .events
            .range(lower.as_slice()..=upper.as_slice())
            .map_err(persist_err)?
        {
            let (encoded_key, at) = entry.map_err(persist_err)?;
            let relay_key = observation_relay_key(encoded_key.value());
            // An observation naming a relay key the dictionary no longer
            // holds is a broken relational invariant, not "unobserved":
            // dropping it here would silently shrink exact source coverage.
            let row = self
                .relays
                .get(relay_key)
                .map_err(persist_err)?
                .ok_or_else(|| {
                    PersistenceError::new(format!(
                        "observation for event {event_key} points at missing relay {relay_key}"
                    ))
                })?;
            let (_refs, url) = decode_relay_row(relay_key, row.value())?;
            let relay = RelayUrl::parse(url).map_err(|error| {
                PersistenceError::new(format!("decode interned relay URL {relay_key}: {error}"))
            })?;
            let at = decode_observed_at(event_key, relay_key, at.value())?;
            fold_seen_at(&mut seen, relay, Timestamp::from(at));
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
        self.next_event_key = next.checked_add(1).ok_or_else(|| {
            PersistenceError::new("canonical event key space exhausted".to_owned())
        })?;
        self.event_allocator_dirty = true;
        Ok(next)
    }

    pub(super) fn allocate_relay_key(&mut self) -> Result<RelayKey, PersistenceError> {
        let next = self.next_relay_key;
        self.next_relay_key = next
            .checked_add(1)
            .ok_or_else(|| PersistenceError::new("relay key space exhausted".to_owned()))?;
        self.relay_allocator_dirty = true;
        Ok(next)
    }

    pub(super) fn intern_relay(&mut self, relay: &RelayUrl) -> Result<RelayKey, PersistenceError> {
        if let Some(existing) = self.relay_ids.get(relay.as_str()).map_err(persist_err)? {
            return Ok(existing.value());
        }
        let key = self.allocate_relay_key()?;
        self.relays
            .insert(key, encode_relay_row(0, relay.as_str()).as_slice())
            .map_err(persist_err)?;
        self.relay_ids
            .insert(relay.as_str(), key)
            .map_err(persist_err)?;
        Ok(key)
    }

    /// The durable `(refcount, url)` of one interned relay.
    fn relay_row(&self, relay_key: RelayKey) -> Result<(u64, String), PersistenceError> {
        let row = self
            .relays
            .get(relay_key)
            .map_err(persist_err)?
            .ok_or_else(|| {
                PersistenceError::new(format!("interned relay {relay_key} has no row"))
            })?;
        let (refs, url) = decode_relay_row(relay_key, row.value())?;
        Ok((refs, url.to_owned()))
    }

    pub(super) fn effective_relay_ref(
        &mut self,
        relay_key: RelayKey,
    ) -> Result<u64, PersistenceError> {
        if let Some(current) = self.relay_ref_counts.get(&relay_key) {
            return Ok(*current);
        }
        let (current, _url) = self.relay_row(relay_key)?;
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
            .ok_or_else(|| PersistenceError::new("relay reference count exhausted".to_owned()))?;
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
            .ok_or_else(|| PersistenceError::new("relay reference count underflow".to_owned()))?;
        self.relay_ref_counts.insert(relay_key, next);
        Ok(())
    }

    /// Flush every transaction-local mutation exactly once before the caller
    /// commits: surrogate high-water marks and relay refcounts remain part of
    /// the same crash-atomic event transaction.
    pub(super) fn flush_pending(&mut self) -> Result<(), PersistenceError> {
        if self.event_allocator_dirty {
            self.store_meta
                .insert(NEXT_EVENT_KEY, self.next_event_key)
                .map_err(persist_err)?;
            self.event_allocator_dirty = false;
        }
        if self.relay_allocator_dirty {
            self.store_meta
                .insert(NEXT_RELAY_KEY, u64::from(self.next_relay_key))
                .map_err(persist_err)?;
            self.relay_allocator_dirty = false;
        }
        for (relay_key, effective) in std::mem::take(&mut self.relay_ref_counts) {
            let (persisted, url) = self.relay_row(relay_key)?;
            if effective > 0 {
                if effective == persisted {
                    continue;
                }
                self.relays
                    .insert(relay_key, encode_relay_row(effective, &url).as_slice())
                    .map_err(persist_err)?;
                continue;
            }
            self.relays.remove(relay_key).map_err(persist_err)?;
            self.relay_ids.remove(url.as_str()).map_err(persist_err)?;
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
            .events
            .get(encoded_key.as_slice())
            .map_err(persist_err)?
            .map(|guard| decode_observed_at(event_key, relay_key, guard.value()))
            .transpose()?;
        if existing.is_some_and(|existing| existing >= at.as_secs()) {
            return Ok(false);
        }
        self.events
            .insert(
                encoded_key.as_slice(),
                at.as_secs().to_be_bytes().as_slice(),
            )
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
            .events
            .remove(encoded_key.as_slice())
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
        let (lower, upper) = observation_bounds(event_key);
        let relay_keys = self
            .events
            .range(lower.as_slice()..=upper.as_slice())
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
        let event_bytes =
            binary_event::encode_event(event).expect("redb: encode immutable canonical event");
        self.events
            .insert(event_row_key(key).as_slice(), event_bytes.as_slice())
            .map_err(persist_err)?;
        self.event_ids
            .insert(event.id.as_bytes(), key)
            .map_err(persist_err)?;
        if let Some(local) = &provenance.local {
            let encoded =
                binary_event::encode_local(local).expect("redb: encode canonical local state");
            self.events
                .insert(event_local_key(key).as_slice(), encoded.as_slice())
                .map_err(persist_err)?;
        }
        for (relay, at) in &provenance.seen {
            self.merge_observation(key, relay, *at)?;
        }
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
            .insert(event_row_key(key).as_slice(), encoded.as_slice())
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
                    .relay_ids
                    .get(relay.as_str())
                    .map_err(persist_err)?
                    .ok_or_else(|| {
                        PersistenceError::new(format!(
                            "observed relay {relay} is no longer interned"
                        ))
                    })?
                    .value();
                self.remove_observation(key, relay_key)?;
            }
        }
        for (relay, at) in &provenance.seen {
            if existing.get(relay) != Some(at) {
                let relay_key = self.intern_relay(relay)?;
                let encoded_key = observation_key(key, relay_key);
                let was_absent = self
                    .events
                    .get(encoded_key.as_slice())
                    .map_err(persist_err)?
                    .is_none();
                self.events
                    .insert(
                        encoded_key.as_slice(),
                        at.as_secs().to_be_bytes().as_slice(),
                    )
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
            self.events
                .insert(event_local_key(key).as_slice(), encoded.as_slice())
                .map_err(persist_err)?;
        } else {
            self.events
                .remove(event_local_key(key).as_slice())
                .map_err(persist_err)?;
        }
        Ok(())
    }

    /// Forget event `key` entirely.
    ///
    /// Every column of the event -- the note row, the local sidecar, and each
    /// relay observation -- lives under the same `event_key` prefix, so this
    /// is ONE range delete rather than four coordinated deletes across three
    /// trees. The relay refcounts still have to be decremented per observation
    /// before the rows go, since they belong to the relay dictionary rather
    /// than to this event.
    pub(super) fn remove_by_key(
        &mut self,
        key: EventKey,
        id: &EventId,
    ) -> Result<(), PersistenceError> {
        self.remove_all_observations(key)?;
        self.event_ids.remove(id.as_bytes()).map_err(persist_err)?;
        let (lower, upper) = event_all_columns_bounds(key);
        self.events
            .retain_in(lower.as_slice()..=upper.as_slice(), |_key, _value| false)
            .map_err(persist_err)?;
        Ok(())
    }
}
