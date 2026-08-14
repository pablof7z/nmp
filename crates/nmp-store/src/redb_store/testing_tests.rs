//! Store-owned corruption fixtures for tests outside this crate.
//!
//! Callers name a receipt, attempt, route key, or intent. They do not name a
//! Redb table.

use std::path::Path;

use nostr::{EventId, RelayUrl};
use redb::{Database, ReadableTable, TableDefinition};

use super::ingest_txn::GovernedWrite;
use super::publish_queue_codec::{
    codec_error, deadline_key, decode_deadline, decode_lane, lane_key,
};
use super::schema::{
    event_row_key, persist_err, COVERAGE, EVENTS, EVENT_IDS, PUBLISH_QUEUE_ATTEMPTS,
    PUBLISH_QUEUE_DEADLINES, PUBLISH_QUEUE_INTENTS, PUBLISH_QUEUE_LANES, PUBLISH_QUEUE_RECEIPTS,
    PUBLISH_QUEUE_RELAY_IDS, PUBLISH_QUEUE_ROUTE_REVISIONS,
};
use super::RedbStore;
use crate::{
    AcceptOutcome, CoverageKey, PersistenceError, PersistenceFault, PublishQueueAttempt,
    PublishQueueAttemptOutcome, PublishQueueDeadlineKind, PublishQueueInFlightPhase,
    PublishQueueLaneState,
};

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

/// Overwrite the ordered deadline row owned by one exact live attempt.
///
/// The caller names NMP's typed `(intent, relay, ordinal)` evidence. Relay
/// surrogates, lane keys, deadline keys, and raw table names remain owned by
/// this store fixture. The store must be closed before this offline mutation.
pub fn corrupt_publish_queue_deadline(
    path: &Path,
    attempt: &PublishQueueAttempt,
) -> Result<(), PersistenceError> {
    if attempt.outcome != PublishQueueAttemptOutcome::Started {
        return Err(PersistenceError::invariant(
            "deadline fixture attempt is not live",
        ));
    }

    let db = Database::open(path).map_err(persist_err)?;
    let tx = db.begin_write().map_err(persist_err)?;
    let relay_id = {
        let relay_ids = tx
            .open_table(PUBLISH_QUEUE_RELAY_IDS)
            .map_err(persist_err)?;
        let relay_id = relay_ids
            .get(attempt.relay.as_str().as_bytes())
            .map_err(persist_err)?
            .map(|guard| u32::from_be_bytes(*guard.value()))
            .ok_or_else(|| PersistenceError::invariant("deadline fixture relay is not interned"))?;
        relay_id
    };
    let (key, lane_revision) = {
        let lanes = tx.open_table(PUBLISH_QUEUE_LANES).map_err(persist_err)?;
        let storage_key = lane_key(attempt.intent_id, relay_id);
        let encoded = lanes
            .get(&storage_key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("deadline fixture lane is missing"))?;
        let (event_id, revision, last_ordinal, state) =
            decode_lane(&encoded).map_err(|error| codec_error("deadline fixture lane", error))?;
        if event_id != attempt.event.id || last_ordinal != attempt.ordinal {
            return Err(PersistenceError::invariant(
                "deadline fixture attempt does not own the current lane",
            ));
        }
        let deadline = match state {
            PublishQueueLaneState::InFlight {
                ordinal,
                phase: PublishQueueInFlightPhase::AwaitingAck { deadline },
            } if ordinal == attempt.ordinal => deadline,
            _ => {
                return Err(PersistenceError::invariant(
                    "deadline fixture lane is not awaiting this attempt's acknowledgement",
                ))
            }
        };
        (
            deadline_key(deadline, attempt.intent_id, relay_id),
            revision,
        )
    };
    {
        let mut deadlines = tx
            .open_table(PUBLISH_QUEUE_DEADLINES)
            .map_err(persist_err)?;
        let mut encoded = deadlines
            .get(&key)
            .map_err(persist_err)?
            .map(|guard| guard.value().to_vec())
            .ok_or_else(|| PersistenceError::invariant("deadline fixture row is missing"))?;
        let decoded = decode_deadline(&encoded)
            .map_err(|error| codec_error("deadline fixture row", error))?;
        if decoded != (lane_revision, PublishQueueDeadlineKind::AckTimeout) {
            return Err(PersistenceError::invariant(
                "deadline fixture row does not match the awaiting-ack lane",
            ));
        }
        encoded[4] = 200;
        deadlines
            .insert(&key, encoded.as_slice())
            .map_err(persist_err)?;
    }
    tx.commit().map_err(persist_err)?;
    Ok(())
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
