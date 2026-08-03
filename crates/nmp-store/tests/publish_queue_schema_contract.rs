//! The name-level durable inventory contract.
//!
//! Two claims live here, and both are falsifiable by a one-line edit to
//! `schema.rs`:
//!
//! 1. A fresh store creates EXACTLY the 40 production tables, named exactly
//!    as listed below. The old spelling of the publish queue (`outbox_*`,
//!    then `delivery_*`) is gone, not aliased.
//! 2. **No table name carries a version suffix.** The single durable epoch
//!    authority is `SCHEMA_VERSION`; a per-table `_v1`/`_v6`/`_v8` marker
//!    advertises a coexistence that has never existed and cannot (#1026).

use std::collections::BTreeSet;

use nmp_store::{RedbStore, RedbStoreOpenError};
use redb::{Database, ReadableDatabase, TableDefinition, TableHandle};
use tempfile::tempdir;

const PUBLISH_QUEUE_META: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("publish_queue_meta");

/// Every table `RedbStore::open` creates, and nothing else.
const PRODUCTION_TABLES: [&str; 40] = [
    "addr_index",
    "addr_tombstones",
    "coverage",
    "event_ids",
    "event_local",
    "event_observations",
    "event_store_meta",
    "events",
    "expiration_index",
    "index_cardinality",
    "index_cardinality_meta",
    "index_cardinality_sample_meta",
    "postings_dead_keys",
    "postings_dictionaries",
    "postings_meta",
    "postings_run_by_min",
    "postings_run_meta",
    "postings_segments",
    "publish_queue_attempt_details",
    "publish_queue_attempts",
    "publish_queue_correlations",
    "publish_queue_deadlines",
    "publish_queue_deadlines_by_intent",
    "publish_queue_displaced",
    "publish_queue_intents",
    "publish_queue_kind5_claims",
    "publish_queue_lanes",
    "publish_queue_meta",
    "publish_queue_receipts",
    "publish_queue_relay_ids",
    "publish_queue_relays",
    "publish_queue_route_revisions",
    "publish_queue_suppress_by_addr",
    "publish_queue_suppress_by_id",
    "relay_keys",
    "relay_meta",
    "relay_refs",
    "relays",
    "schema_meta",
    "tombstones",
];

fn fresh_table_names(path: &std::path::Path) -> BTreeSet<String> {
    let database = Database::open(path).unwrap();
    let read = database.begin_read().unwrap();
    read.list_tables()
        .unwrap()
        .map(|table| table.name().to_owned())
        .collect()
}

#[test]
fn a_fresh_store_creates_exactly_the_named_publish_queue_inventory() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("publish-queue.redb");
    drop(RedbStore::open(&path).unwrap());

    let names = fresh_table_names(&path);
    let expected = PRODUCTION_TABLES
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(names, expected, "durable table inventory drifted");

    assert!(
        names
            .iter()
            .all(|name| !name.starts_with("outbox_") && !name.starts_with("delivery_")),
        "a retired publish-queue namespace survived: {names:?}"
    );

    let database = Database::open(&path).unwrap();
    let read = database.begin_read().unwrap();
    let meta = read.open_table(PUBLISH_QUEUE_META).unwrap();
    assert_eq!(
        meta.get(b"codec_version".as_slice())
            .unwrap()
            .map(|value| value.value().to_vec()),
        Some(1u64.to_be_bytes().to_vec())
    );
}

/// The falsifier for #1026's suffix strip: reintroduce `_v1` (or `_v6`, or
/// `_v8`) on any table and this fails by naming it. A suffix on a
/// one-epoch format claims a coexistence no store has ever had, and nothing
/// reads or branches on it — `SCHEMA_VERSION` is the only authority.
#[test]
fn no_durable_table_name_carries_a_version_suffix() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("publish-queue.redb");
    drop(RedbStore::open(&path).unwrap());

    let suffixed = fresh_table_names(&path)
        .into_iter()
        .filter(|name| {
            name.rsplit_once("_v").is_some_and(|(_, tail)| {
                !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit())
            })
        })
        .collect::<Vec<_>>();
    assert!(
        suffixed.is_empty(),
        "durable tables carry a per-table version suffix, but SCHEMA_VERSION is the one epoch \
         authority: {suffixed:?}"
    );
}

#[test]
fn legacy_outbox_database_is_typed_refusal_and_is_never_initialized() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("legacy-outbox.redb");
    {
        let database = Database::create(&path).unwrap();
        let write = database.begin_write().unwrap();
        {
            let mut legacy = write
                .open_table(TableDefinition::<&str, &str>::new("outbox_intents"))
                .unwrap();
            legacy.insert("00000000000000000001", "{}").unwrap();
        }
        write.commit().unwrap();
    }

    assert!(matches!(
        RedbStore::open(&path),
        Err(RedbStoreOpenError::UnsupportedSchema { found: None, .. })
    ));

    assert_eq!(
        fresh_table_names(&path),
        BTreeSet::from(["outbox_intents".to_owned()])
    );
}

/// A store written by the previous epoch — the one whose publish queue was
/// spelled `delivery_*_v1` — is refused at `open`, unmutated. The wipe is
/// the accepted cutover (#1026); what must never happen is a partial adopt.
#[test]
fn a_previous_epoch_publish_queue_database_is_refused_and_left_untouched() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("previous-epoch.redb");
    {
        let database = Database::create(&path).unwrap();
        let write = database.begin_write().unwrap();
        {
            let mut legacy = write
                .open_table(TableDefinition::<&str, &str>::new("delivery_intents_v1"))
                .unwrap();
            legacy.insert("00000000000000000001", "{}").unwrap();
            let mut meta = write
                .open_table(TableDefinition::<&str, u64>::new("schema_meta_v6"))
                .unwrap();
            meta.insert("version", 12u64).unwrap();
        }
        write.commit().unwrap();
    }

    assert!(matches!(
        RedbStore::open(&path),
        Err(RedbStoreOpenError::UnsupportedSchema { found: None, .. })
    ));

    assert_eq!(
        fresh_table_names(&path),
        BTreeSet::from([
            "delivery_intents_v1".to_owned(),
            "schema_meta_v6".to_owned()
        ])
    );
}
