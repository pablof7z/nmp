//! Physical transaction bundle for governed event mutations.
//!
//! Governance lives in `ingest` and `mutation`; this module owns only the
//! Redb tables those decisions mutate together. Keeping the complete bundle
//! behind one value gives the future Fjall adapter one concrete capability
//! boundary without copying policy.

use super::canonical::CanonicalWriteTables;
#[cfg(test)]
use super::postings_store::crash_if_postings;
use super::postings_store::PostingsBatch;
use super::schema::{
    addr_suppress_key, id_suppress_key, persist_err, EventKey, ADDR_INDEX, EXPIRATION_INDEX,
    PUBLISH_QUEUE_DISPLACED, PUBLISH_QUEUE_INTENTS, PUBLISH_QUEUE_KIND5_CLAIMS, PUBLISH_QUEUE_META,
    PUBLISH_QUEUE_RECEIPTS, PUBLISH_QUEUE_SUPPRESS, TOMBSTONES,
};
#[cfg(feature = "bench-instrumentation")]
use super::store::BenchmarkDurability;
use super::store::RedbStore;
use super::{
    Event, EventId, LocalOrigin, PersistenceError, Provenance, RelayUrl, StoredEvent, Timestamp,
};
use redb::ReadableTable;

/// The only commit door for a transaction that mutates canonical event state.
///
/// [`apply`](Self::apply) constructs the complete governed table bundle and
/// always flushes its transaction-local allocators after the mutation closure
/// succeeds. Later packed-postings publication attaches to this same door, so
/// callers cannot commit canonical rows while forgetting derived index work.
pub(super) struct GovernedWrite {
    write_txn: redb::WriteTransaction,
    postings: PostingsBatch,
}

impl GovernedWrite {
    /// Takes `&mut RedbStore` although the body only reads the handle.
    /// `Database::begin_write` takes `&self` and [`RedbStore::database`]
    /// hands out a shared reference, so a `&RedbStore` signature here would
    /// let any read-borrow entry point open the canonical write door. The
    /// exclusive borrow is what makes "a shared borrow of the store cannot
    /// mutate it" a compiler rule rather than a convention.
    pub(super) fn begin(store: &mut RedbStore) -> Result<Self, PersistenceError> {
        let write_txn = store.database()?.begin_write().map_err(persist_err)?;
        #[cfg(feature = "bench-instrumentation")]
        let mut write_txn = write_txn;
        #[cfg(feature = "bench-instrumentation")]
        if store.benchmark_durability == BenchmarkDurability::NoneThenImmediateCheckpoint {
            write_txn
                .set_durability(redb::Durability::None)
                .map_err(persist_err)?;
        }
        Ok(Self {
            write_txn,
            postings: PostingsBatch::default(),
        })
    }

    pub(super) fn apply<T>(
        &mut self,
        mutate: impl FnOnce(
            &mut RedbIngestTxn<'_, '_>,
            &redb::WriteTransaction,
        ) -> Result<T, PersistenceError>,
    ) -> Result<T, PersistenceError> {
        #[cfg(feature = "bench-instrumentation")]
        let open_started = std::time::Instant::now();
        let mut ingest = RedbIngestTxn::open(&self.write_txn, &mut self.postings)?;
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::open_tables(open_started.elapsed());
        let result = mutate(&mut ingest, &self.write_txn)?;
        #[cfg(feature = "bench-instrumentation")]
        let flush_started = std::time::Instant::now();
        ingest.canonical.flush_pending()?;
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::flush(flush_started.elapsed());
        Ok(result)
    }

    pub(super) fn transaction(&self) -> &redb::WriteTransaction {
        &self.write_txn
    }

    /// Flush every derived structure, commit, and return a value that the
    /// caller prepared before this transaction exit.
    pub(super) fn commit_prepared<T>(mut self, prepared: T) -> Result<T, PersistenceError> {
        #[cfg(feature = "bench-instrumentation")]
        let postings_started = std::time::Instant::now();
        self.postings.flush(&self.write_txn)?;
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::postings_flush(postings_started.elapsed());
        #[cfg(test)]
        crash_if_postings("postings-before-commit");
        #[cfg(feature = "bench-instrumentation")]
        let commit_started = std::time::Instant::now();
        self.write_txn.commit().map_err(persist_err)?;
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::commit(commit_started.elapsed());
        #[cfg(test)]
        crash_if_postings("postings-after-commit");
        Ok(prepared)
    }
}

/// Binary publish-queue maps reached from the same governed mutation.
#[derive(Clone, Copy)]
pub(super) enum GovernedPublishQueueMap {
    Intents,
    Receipts,
    Kind5Claims,
    SuppressById,
    SuppressByAddr,
}

/// Backend-neutral physical capabilities required by governed relay ingest.
/// All policy is expressed against this statically-dispatched trait.
pub(super) trait GovernedIngestTxn {
    fn key_for_id(&self, id: &EventId) -> Result<Option<EventKey>, PersistenceError>;
    fn load_by_key(&self, key: EventKey) -> Result<Option<StoredEvent>, PersistenceError>;
    fn load_by_id(&self, id: &EventId)
        -> Result<Option<(EventKey, StoredEvent)>, PersistenceError>;
    fn load_local(&self, key: EventKey) -> Result<Option<LocalOrigin>, PersistenceError>;
    fn merge_observation(
        &mut self,
        key: EventKey,
        relay: &RelayUrl,
        at: Timestamp,
    ) -> Result<bool, PersistenceError>;
    fn replace_event(&mut self, key: EventKey, event: &Event) -> Result<(), PersistenceError>;
    fn replace_local(
        &mut self,
        key: EventKey,
        local: Option<LocalOrigin>,
    ) -> Result<(), PersistenceError>;
    fn insert_new(
        &mut self,
        event: &Event,
        provenance: &Provenance,
    ) -> Result<EventKey, PersistenceError>;
    fn remove_canonical(&mut self, key: EventKey, id: &EventId) -> Result<(), PersistenceError>;
    fn insert_indexes(&mut self, event: &Event, key: EventKey) -> Result<(), PersistenceError>;
    fn remove_indexes(&mut self, event: &Event, key: EventKey) -> Result<(), PersistenceError>;

    fn address_get(&self, key: &str) -> Result<Option<EventKey>, PersistenceError>;
    fn address_put(&mut self, key: &str, value: EventKey) -> Result<(), PersistenceError>;
    fn address_remove(&mut self, key: &str) -> Result<(), PersistenceError>;
    fn expiration_put(&mut self, key: &[u8; 40], value: EventKey) -> Result<(), PersistenceError>;
    fn expiration_remove(&mut self, key: &[u8; 40]) -> Result<(), PersistenceError>;

    /// One permanent deletion fact, keyed by [`super::schema::TOMBSTONES`]'s
    /// discriminated key. Ids and addresses share the key space.
    fn tombstone_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, PersistenceError>;
    fn tombstone_put(&mut self, key: &[u8], value: &[u8]) -> Result<(), PersistenceError>;
    fn publish_queue_get(
        &self,
        map: GovernedPublishQueueMap,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, PersistenceError>;
    fn publish_queue_put(
        &mut self,
        map: GovernedPublishQueueMap,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), PersistenceError>;
    fn publish_queue_remove(
        &mut self,
        map: GovernedPublishQueueMap,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, PersistenceError>;
    fn displaced_remove(&mut self, key: &[u8; 8]) -> Result<Option<Vec<u8>>, PersistenceError>;
}

pub(super) struct RedbIngestTxn<'txn, 'batch> {
    pub(super) canonical: CanonicalWriteTables<'txn>,
    pub(super) addr_index: redb::Table<'txn, &'static str, EventKey>,
    pub(super) tombstones: redb::Table<'txn, &'static [u8], &'static [u8]>,
    pub(super) expiration_index: redb::Table<'txn, &'static [u8; 40], EventKey>,
    pub(super) publish_queue_intents: redb::Table<'txn, &'static [u8; 8], &'static [u8]>,
    pub(super) publish_queue_receipts: redb::Table<'txn, &'static [u8; 8], &'static [u8]>,
    pub(super) publish_queue_meta: redb::Table<'txn, &'static [u8], &'static [u8]>,
    pub(super) publish_queue_displaced: redb::Table<'txn, &'static [u8; 8], &'static [u8]>,
    pub(super) publish_queue_kind5_claims: redb::Table<'txn, &'static [u8; 8], &'static [u8]>,
    pub(super) publish_queue_suppress: redb::Table<'txn, &'static [u8], &'static [u8]>,
    postings: &'batch mut PostingsBatch,
}

impl<'txn, 'batch> RedbIngestTxn<'txn, 'batch> {
    pub(super) fn open(
        write_txn: &'txn redb::WriteTransaction,
        postings: &'batch mut PostingsBatch,
    ) -> Result<Self, PersistenceError> {
        Ok(Self {
            canonical: CanonicalWriteTables::open(write_txn)?,
            addr_index: write_txn.open_table(ADDR_INDEX).map_err(persist_err)?,
            tombstones: write_txn.open_table(TOMBSTONES).map_err(persist_err)?,
            expiration_index: write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?,
            publish_queue_intents: write_txn
                .open_table(PUBLISH_QUEUE_INTENTS)
                .map_err(persist_err)?,
            publish_queue_receipts: write_txn
                .open_table(PUBLISH_QUEUE_RECEIPTS)
                .map_err(persist_err)?,
            publish_queue_meta: write_txn
                .open_table(PUBLISH_QUEUE_META)
                .map_err(persist_err)?,
            publish_queue_displaced: write_txn
                .open_table(PUBLISH_QUEUE_DISPLACED)
                .map_err(persist_err)?,
            publish_queue_kind5_claims: write_txn
                .open_table(PUBLISH_QUEUE_KIND5_CLAIMS)
                .map_err(persist_err)?,
            publish_queue_suppress: write_txn
                .open_table(PUBLISH_QUEUE_SUPPRESS)
                .map_err(persist_err)?,
            postings,
        })
    }
}

impl GovernedIngestTxn for RedbIngestTxn<'_, '_> {
    fn key_for_id(&self, id: &EventId) -> Result<Option<EventKey>, PersistenceError> {
        self.canonical.key_for_id(id)
    }

    fn load_by_key(&self, key: EventKey) -> Result<Option<StoredEvent>, PersistenceError> {
        self.canonical.load_by_key(key)
    }

    fn load_by_id(
        &self,
        id: &EventId,
    ) -> Result<Option<(EventKey, StoredEvent)>, PersistenceError> {
        self.canonical.load_by_id(id)
    }

    fn load_local(&self, key: EventKey) -> Result<Option<LocalOrigin>, PersistenceError> {
        self.canonical.load_local(key)
    }

    fn merge_observation(
        &mut self,
        key: EventKey,
        relay: &RelayUrl,
        at: Timestamp,
    ) -> Result<bool, PersistenceError> {
        self.canonical.merge_observation(key, relay, at)
    }

    fn replace_event(&mut self, key: EventKey, event: &Event) -> Result<(), PersistenceError> {
        self.canonical.replace_event(key, event)
    }

    fn replace_local(
        &mut self,
        key: EventKey,
        local: Option<LocalOrigin>,
    ) -> Result<(), PersistenceError> {
        self.canonical.replace_local(key, local)
    }

    fn insert_new(
        &mut self,
        event: &Event,
        provenance: &Provenance,
    ) -> Result<EventKey, PersistenceError> {
        self.canonical.insert_new(event, provenance)
    }

    fn remove_canonical(&mut self, key: EventKey, id: &EventId) -> Result<(), PersistenceError> {
        self.canonical.remove_by_key(key, id)
    }

    fn insert_indexes(&mut self, event: &Event, key: EventKey) -> Result<(), PersistenceError> {
        self.postings.insert(event, key);
        Ok(())
    }

    fn remove_indexes(&mut self, _event: &Event, key: EventKey) -> Result<(), PersistenceError> {
        self.postings.remove(key);
        Ok(())
    }

    fn address_get(&self, key: &str) -> Result<Option<EventKey>, PersistenceError> {
        Ok(self
            .addr_index
            .get(key)
            .map_err(persist_err)?
            .map(|guard| guard.value()))
    }

    fn address_put(&mut self, key: &str, value: EventKey) -> Result<(), PersistenceError> {
        self.addr_index.insert(key, value).map_err(persist_err)?;
        Ok(())
    }

    fn address_remove(&mut self, key: &str) -> Result<(), PersistenceError> {
        self.addr_index.remove(key).map_err(persist_err)?;
        Ok(())
    }

    fn expiration_put(&mut self, key: &[u8; 40], value: EventKey) -> Result<(), PersistenceError> {
        self.expiration_index
            .insert(key, value)
            .map_err(persist_err)?;
        Ok(())
    }

    fn expiration_remove(&mut self, key: &[u8; 40]) -> Result<(), PersistenceError> {
        self.expiration_index.remove(key).map_err(persist_err)?;
        Ok(())
    }

    fn tombstone_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, PersistenceError> {
        Ok(self
            .tombstones
            .get(key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_owned()))
    }

    fn tombstone_put(&mut self, key: &[u8], value: &[u8]) -> Result<(), PersistenceError> {
        self.tombstones.insert(key, value).map_err(persist_err)?;
        Ok(())
    }

    fn publish_queue_get(
        &self,
        map: GovernedPublishQueueMap,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, PersistenceError> {
        let value = match map {
            GovernedPublishQueueMap::Intents => self
                .publish_queue_intents
                .get(fixed_key::<8>(key, "delivery intent key")?)
                .map_err(persist_err)?,
            GovernedPublishQueueMap::Receipts => self
                .publish_queue_receipts
                .get(fixed_key::<8>(key, "delivery receipt key")?)
                .map_err(persist_err)?,
            GovernedPublishQueueMap::Kind5Claims => self
                .publish_queue_kind5_claims
                .get(fixed_key::<8>(key, "delivery kind:5 key")?)
                .map_err(persist_err)?,
            GovernedPublishQueueMap::SuppressById => self
                .publish_queue_suppress
                .get(
                    id_suppress_key(fixed_key::<64>(key, "delivery id-suppression key")?)
                        .as_slice(),
                )
                .map_err(persist_err)?,
            GovernedPublishQueueMap::SuppressByAddr => self
                .publish_queue_suppress
                .get(addr_suppress_key(key).as_slice())
                .map_err(persist_err)?,
        };
        Ok(value.map(|guard| guard.value().to_vec()))
    }

    fn publish_queue_put(
        &mut self,
        map: GovernedPublishQueueMap,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), PersistenceError> {
        match map {
            GovernedPublishQueueMap::Intents => self
                .publish_queue_intents
                .insert(fixed_key::<8>(key, "delivery intent key")?, value),
            GovernedPublishQueueMap::Receipts => self
                .publish_queue_receipts
                .insert(fixed_key::<8>(key, "delivery receipt key")?, value),
            GovernedPublishQueueMap::Kind5Claims => self
                .publish_queue_kind5_claims
                .insert(fixed_key::<8>(key, "delivery kind:5 key")?, value),
            GovernedPublishQueueMap::SuppressById => self.publish_queue_suppress.insert(
                id_suppress_key(fixed_key::<64>(key, "delivery id-suppression key")?).as_slice(),
                value,
            ),
            GovernedPublishQueueMap::SuppressByAddr => self
                .publish_queue_suppress
                .insert(addr_suppress_key(key).as_slice(), value),
        }
        .map_err(persist_err)?;
        Ok(())
    }

    fn publish_queue_remove(
        &mut self,
        map: GovernedPublishQueueMap,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, PersistenceError> {
        let value = match map {
            GovernedPublishQueueMap::Intents => self
                .publish_queue_intents
                .remove(fixed_key::<8>(key, "delivery intent key")?)
                .map_err(persist_err)?,
            GovernedPublishQueueMap::Receipts => self
                .publish_queue_receipts
                .remove(fixed_key::<8>(key, "delivery receipt key")?)
                .map_err(persist_err)?,
            GovernedPublishQueueMap::Kind5Claims => self
                .publish_queue_kind5_claims
                .remove(fixed_key::<8>(key, "delivery kind:5 key")?)
                .map_err(persist_err)?,
            GovernedPublishQueueMap::SuppressById => self
                .publish_queue_suppress
                .remove(
                    id_suppress_key(fixed_key::<64>(key, "delivery id-suppression key")?)
                        .as_slice(),
                )
                .map_err(persist_err)?,
            GovernedPublishQueueMap::SuppressByAddr => self
                .publish_queue_suppress
                .remove(addr_suppress_key(key).as_slice())
                .map_err(persist_err)?,
        };
        Ok(value.map(|guard| guard.value().to_vec()))
    }

    fn displaced_remove(&mut self, key: &[u8; 8]) -> Result<Option<Vec<u8>>, PersistenceError> {
        Ok(self
            .publish_queue_displaced
            .remove(key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec()))
    }
}

fn fixed_key<'a, const N: usize>(
    key: &'a [u8],
    what: &'static str,
) -> Result<&'a [u8; N], PersistenceError> {
    key.try_into()
        .map_err(|_| PersistenceError::invariant(format!("{what} has wrong width")))
}
