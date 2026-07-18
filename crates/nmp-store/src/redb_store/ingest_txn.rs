//! Physical transaction bundle for governed event mutations.
//!
//! Governance lives in `ingest` and `mutation`; this module owns only the
//! Redb tables those decisions mutate together. Keeping the complete bundle
//! behind one value gives the future Fjall adapter one concrete capability
//! boundary without copying policy.

use super::canonical::CanonicalWriteTables;
use super::outbox::{OUTBOX_KIND5_CLAIMS, OUTBOX_SUPPRESS_BY_ADDR, OUTBOX_SUPPRESS_BY_ID};
use super::query::{insert_query_index_rows, remove_query_index_rows, QueryIndexWriteTables};
use super::schema::{
    persist_err, EventKey, ADDR_INDEX, ADDR_TOMBSTONES, EXPIRATION_INDEX, OUTBOX_DISPLACED,
    OUTBOX_INTENTS, OUTBOX_RECEIPTS, TOMBSTONES,
};
use super::{
    Event, EventId, LocalOrigin, PersistenceError, Provenance, RelayUrl, StoredEvent, Timestamp,
};
use redb::{Database, ReadableTable};

/// The only commit door for a transaction that mutates canonical event state.
///
/// [`apply`](Self::apply) constructs the complete governed table bundle and
/// always flushes its transaction-local allocators after the mutation closure
/// succeeds. Later packed-postings publication attaches to this same door, so
/// callers cannot commit canonical rows while forgetting derived index work.
pub(super) struct GovernedWrite {
    write_txn: redb::WriteTransaction,
}

impl GovernedWrite {
    pub(super) fn begin(db: &Database) -> Result<Self, PersistenceError> {
        Ok(Self {
            write_txn: db.begin_write().map_err(persist_err)?,
        })
    }

    pub(super) fn apply<T>(
        &mut self,
        mutate: impl FnOnce(
            &mut RedbIngestTxn<'_>,
            &redb::WriteTransaction,
        ) -> Result<T, PersistenceError>,
    ) -> Result<T, PersistenceError> {
        #[cfg(feature = "bench-instrumentation")]
        let open_started = std::time::Instant::now();
        let mut ingest = RedbIngestTxn::open(&self.write_txn)?;
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

    pub(super) fn commit(self) -> Result<(), PersistenceError> {
        self.write_txn.commit().map_err(persist_err)
    }
}

/// String-valued governed keyspaces reachable from relay ingest.
///
/// The enum is internal physical vocabulary, not an app-facing noun. A
/// backend implements these maps while `ingest` and `mutation` retain the
/// only copy of the policy deciding when each map changes.
#[derive(Clone, Copy)]
pub(super) enum GovernedStringMap {
    Tombstones,
    AddrTombstones,
    OutboxIntents,
    OutboxReceipts,
    OutboxKind5Claims,
    OutboxSuppressById,
    OutboxSuppressByAddr,
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

    fn string_get(
        &self,
        map: GovernedStringMap,
        key: &str,
    ) -> Result<Option<String>, PersistenceError>;
    fn string_put(
        &mut self,
        map: GovernedStringMap,
        key: &str,
        value: &str,
    ) -> Result<(), PersistenceError>;
    fn string_remove(
        &mut self,
        map: GovernedStringMap,
        key: &str,
    ) -> Result<Option<String>, PersistenceError>;
    fn displaced_remove(&mut self, key: &str) -> Result<Option<Vec<u8>>, PersistenceError>;
}

pub(super) struct RedbIngestTxn<'txn> {
    pub(super) canonical: CanonicalWriteTables<'txn>,
    pub(super) addr_index: redb::Table<'txn, &'static str, EventKey>,
    pub(super) tombstones: redb::Table<'txn, &'static str, &'static str>,
    pub(super) addr_tombstones: redb::Table<'txn, &'static str, &'static str>,
    pub(super) expiration_index: redb::Table<'txn, &'static [u8; 40], EventKey>,
    pub(super) indexes: QueryIndexWriteTables<'txn>,
    pub(super) outbox_intents: redb::Table<'txn, &'static str, &'static str>,
    pub(super) outbox_receipts: redb::Table<'txn, &'static str, &'static str>,
    pub(super) outbox_displaced: redb::Table<'txn, &'static str, &'static [u8]>,
    pub(super) outbox_kind5_claims: redb::Table<'txn, &'static str, &'static str>,
    pub(super) outbox_suppress_by_id: redb::Table<'txn, &'static str, &'static str>,
    pub(super) outbox_suppress_by_addr: redb::Table<'txn, &'static str, &'static str>,
}

impl<'txn> RedbIngestTxn<'txn> {
    pub(super) fn open(write_txn: &'txn redb::WriteTransaction) -> Result<Self, PersistenceError> {
        Ok(Self {
            canonical: CanonicalWriteTables::open(write_txn)?,
            addr_index: write_txn.open_table(ADDR_INDEX).map_err(persist_err)?,
            tombstones: write_txn.open_table(TOMBSTONES).map_err(persist_err)?,
            addr_tombstones: write_txn.open_table(ADDR_TOMBSTONES).map_err(persist_err)?,
            expiration_index: write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?,
            indexes: QueryIndexWriteTables::open(write_txn)?,
            outbox_intents: write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?,
            outbox_receipts: write_txn.open_table(OUTBOX_RECEIPTS).map_err(persist_err)?,
            outbox_displaced: write_txn
                .open_table(OUTBOX_DISPLACED)
                .map_err(persist_err)?,
            outbox_kind5_claims: write_txn
                .open_table(OUTBOX_KIND5_CLAIMS)
                .map_err(persist_err)?,
            outbox_suppress_by_id: write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ID)
                .map_err(persist_err)?,
            outbox_suppress_by_addr: write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ADDR)
                .map_err(persist_err)?,
        })
    }
}

impl GovernedIngestTxn for RedbIngestTxn<'_> {
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
        insert_query_index_rows(&mut self.canonical, &mut self.indexes, event, key)
    }

    fn remove_indexes(&mut self, event: &Event, _key: EventKey) -> Result<(), PersistenceError> {
        remove_query_index_rows(&mut self.canonical, &mut self.indexes, event)
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

    fn string_get(
        &self,
        map: GovernedStringMap,
        key: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let value = match map {
            GovernedStringMap::Tombstones => self.tombstones.get(key).map_err(persist_err)?,
            GovernedStringMap::AddrTombstones => {
                self.addr_tombstones.get(key).map_err(persist_err)?
            }
            GovernedStringMap::OutboxIntents => {
                self.outbox_intents.get(key).map_err(persist_err)?
            }
            GovernedStringMap::OutboxReceipts => {
                self.outbox_receipts.get(key).map_err(persist_err)?
            }
            GovernedStringMap::OutboxKind5Claims => {
                self.outbox_kind5_claims.get(key).map_err(persist_err)?
            }
            GovernedStringMap::OutboxSuppressById => {
                self.outbox_suppress_by_id.get(key).map_err(persist_err)?
            }
            GovernedStringMap::OutboxSuppressByAddr => {
                self.outbox_suppress_by_addr.get(key).map_err(persist_err)?
            }
        };
        Ok(value.map(|guard| guard.value().to_owned()))
    }

    fn string_put(
        &mut self,
        map: GovernedStringMap,
        key: &str,
        value: &str,
    ) -> Result<(), PersistenceError> {
        match map {
            GovernedStringMap::Tombstones => self.tombstones.insert(key, value),
            GovernedStringMap::AddrTombstones => self.addr_tombstones.insert(key, value),
            GovernedStringMap::OutboxIntents => self.outbox_intents.insert(key, value),
            GovernedStringMap::OutboxReceipts => self.outbox_receipts.insert(key, value),
            GovernedStringMap::OutboxKind5Claims => self.outbox_kind5_claims.insert(key, value),
            GovernedStringMap::OutboxSuppressById => self.outbox_suppress_by_id.insert(key, value),
            GovernedStringMap::OutboxSuppressByAddr => {
                self.outbox_suppress_by_addr.insert(key, value)
            }
        }
        .map_err(persist_err)?;
        Ok(())
    }

    fn string_remove(
        &mut self,
        map: GovernedStringMap,
        key: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let value = match map {
            GovernedStringMap::Tombstones => self.tombstones.remove(key).map_err(persist_err)?,
            GovernedStringMap::AddrTombstones => {
                self.addr_tombstones.remove(key).map_err(persist_err)?
            }
            GovernedStringMap::OutboxIntents => {
                self.outbox_intents.remove(key).map_err(persist_err)?
            }
            GovernedStringMap::OutboxReceipts => {
                self.outbox_receipts.remove(key).map_err(persist_err)?
            }
            GovernedStringMap::OutboxKind5Claims => {
                self.outbox_kind5_claims.remove(key).map_err(persist_err)?
            }
            GovernedStringMap::OutboxSuppressById => self
                .outbox_suppress_by_id
                .remove(key)
                .map_err(persist_err)?,
            GovernedStringMap::OutboxSuppressByAddr => self
                .outbox_suppress_by_addr
                .remove(key)
                .map_err(persist_err)?,
        };
        Ok(value.map(|guard| guard.value().to_owned()))
    }

    fn displaced_remove(&mut self, key: &str) -> Result<Option<Vec<u8>>, PersistenceError> {
        Ok(self
            .outbox_displaced
            .remove(key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec()))
    }
}
