//! Store-owned corruption fixtures for tests outside this crate.
//!
//! Callers name a receipt, attempt prefix, route key, or intent. They do not
//! name a Redb table.

use std::path::Path;

use nostr::{EventId, RelayUrl};
use redb::{Database, ReadableTable, TableDefinition};

use super::ingest_txn::GovernedWrite;
use super::schema::{
    event_row_key, persist_err, COVERAGE, EVENTS, EVENT_IDS, PUBLISH_QUEUE_ATTEMPTS,
    PUBLISH_QUEUE_INTENTS, PUBLISH_QUEUE_RECEIPTS, PUBLISH_QUEUE_ROUTE_REVISIONS,
};
use super::RedbStore;
use crate::{AcceptOutcome, CoverageKey, PersistenceError, PersistenceFault};

/// Commit one real event acceptance, close that exact Redb generation, and
/// hide the committed outcome behind a typed I/O failure. This test-owned
/// exit is deliberately outside the production transaction modules so their
/// structural gate continues to require a tail-position commit.
pub(super) fn commit_acceptance_then_return_io(
    store: &mut RedbStore,
    write: GovernedWrite,
    outcome: AcceptOutcome,
) -> Result<AcceptOutcome, PersistenceError> {
    let _committed = write.commit_prepared(outcome)?;
    drop(store.db.take());
    Err(PersistenceError::new(
        PersistenceFault::Io,
        "injected acceptance committed before I/O failure",
    ))
}

fn corrupt_first_row<const N: usize>(
    path: &Path,
    table: TableDefinition<&[u8; N], &[u8]>,
    prefix: &[u8],
) -> Result<(), PersistenceError> {
    let db = Database::open(path).map_err(persist_err)?;
    let tx = db.begin_write().map_err(persist_err)?;
    {
        let mut opened = tx.open_table(table).map_err(persist_err)?;
        let (key, mut value) = opened
            .iter()
            .map_err(persist_err)?
            .map(|row| {
                let (key, value) = row.map_err(persist_err)?;
                Ok((*key.value(), value.value().to_vec()))
            })
            .collect::<Result<Vec<_>, PersistenceError>>()?
            .into_iter()
            .find(|(key, _)| key.as_slice().starts_with(prefix))
            .ok_or_else(|| PersistenceError::invariant("matching publish-queue row"))?;
        if value.len() < 5 {
            return Err(PersistenceError::invariant("versioned delivery envelope"));
        }
        value[4] = 200;
        opened.insert(&key, value.as_slice()).map_err(persist_err)?;
    }
    tx.commit().map_err(persist_err)?;
    Ok(())
}

/// Overwrite one receipt row with undecodable bytes.
pub fn corrupt_publish_queue_receipt(path: &Path, receipt_id: u64) -> Result<(), PersistenceError> {
    corrupt_first_row(path, PUBLISH_QUEUE_RECEIPTS, &receipt_id.to_be_bytes())
}

/// Overwrite the first attempt row whose key starts with `prefix`.
pub fn corrupt_first_publish_queue_attempt(
    path: &Path,
    prefix: &[u8],
) -> Result<(), PersistenceError> {
    corrupt_first_row(path, PUBLISH_QUEUE_ATTEMPTS, prefix)
}

/// Overwrite the first intent row.
pub fn corrupt_first_publish_queue_intent(path: &Path) -> Result<(), PersistenceError> {
    corrupt_first_row(path, PUBLISH_QUEUE_INTENTS, &[])
}

/// Insert one undecodable route-revision row at `key`.
pub fn insert_corrupt_publish_queue_route_revision(
    path: &Path,
    key: &[u8; 16],
) -> Result<(), PersistenceError> {
    let db = Database::open(path).map_err(persist_err)?;
    let tx = db.begin_write().map_err(persist_err)?;
    {
        let mut opened = tx
            .open_table(PUBLISH_QUEUE_ROUTE_REVISIONS)
            .map_err(persist_err)?;
        let value = [b"NMDV".as_slice(), &[200, 0, 0, 0]].concat();
        opened.insert(key, value.as_slice()).map_err(persist_err)?;
    }
    tx.commit().map_err(persist_err)?;
    Ok(())
}

/// Overwrite the canonical row named by `event_id` with undecodable bytes.
pub fn corrupt_canonical_event(path: &Path, event_id: EventId) -> Result<(), PersistenceError> {
    let db = Database::open(path).map_err(persist_err)?;
    let tx = db.begin_write().map_err(persist_err)?;
    let event_key = {
        let event_ids = tx.open_table(EVENT_IDS).map_err(persist_err)?;
        let event_key = event_ids
            .get(event_id.as_bytes())
            .map_err(persist_err)?
            .map(|value| value.value())
            .ok_or_else(|| PersistenceError::invariant("canonical event id fixture"))?;
        event_key
    };
    {
        let mut events = tx.open_table(EVENTS).map_err(persist_err)?;
        events
            .insert(
                event_row_key(event_key).as_slice(),
                b"NMPE-truncated".as_slice(),
            )
            .map_err(persist_err)?;
    }
    tx.commit().map_err(persist_err)?;
    Ok(())
}

/// Overwrite the coverage row named by `key` and `relay` with undecodable JSON.
pub fn corrupt_coverage(
    path: &Path,
    key: CoverageKey,
    relay: &RelayUrl,
) -> Result<(), PersistenceError> {
    let db = Database::open(path).map_err(persist_err)?;
    let tx = db.begin_write().map_err(persist_err)?;
    let row_key = RedbStore::coverage_row_key(key, relay);
    {
        let mut coverage = tx.open_table(COVERAGE).map_err(persist_err)?;
        if coverage
            .get(row_key.as_str())
            .map_err(persist_err)?
            .is_none()
        {
            return Err(PersistenceError::invariant("coverage row fixture"));
        }
        coverage
            .insert(row_key.as_str(), "{ not a coverage row")
            .map_err(persist_err)?;
    }
    tx.commit().map_err(persist_err)?;
    Ok(())
}
