use std::collections::BTreeSet;

use nmp_store::{RedbStore, RedbStoreOpenError};
use redb::{Database, ReadableDatabase, TableDefinition, TableHandle};
use tempfile::tempdir;

const DELIVERY_META: TableDefinition<&[u8], &[u8]> = TableDefinition::new("delivery_meta_v1");

#[test]
fn fresh_store_uses_only_the_versioned_delivery_namespace() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("delivery.redb");
    drop(RedbStore::open(&path).unwrap());

    let database = Database::open(&path).unwrap();
    let read = database.begin_read().unwrap();
    let names = read
        .list_tables()
        .unwrap()
        .map(|table| table.name().to_owned())
        .collect::<BTreeSet<_>>();

    let expected = [
        "delivery_attempt_details_v1",
        "delivery_attempts_v1",
        "delivery_correlations_v1",
        "delivery_deadlines_by_intent_v1",
        "delivery_deadlines_v1",
        "delivery_displaced_v1",
        "delivery_intents_v1",
        "delivery_kind5_claims_v1",
        "delivery_lanes_v1",
        "delivery_meta_v1",
        "delivery_receipts_v1",
        "delivery_relay_ids_v1",
        "delivery_relays_v1",
        "delivery_route_revisions_v1",
        "delivery_suppress_by_addr_v1",
        "delivery_suppress_by_id_v1",
    ];
    for table in expected {
        assert!(
            names.contains(table),
            "missing fresh delivery table {table}"
        );
    }
    assert!(
        names.iter().all(|name| !name.starts_with("outbox_")),
        "retired execution namespace survived: {names:?}"
    );

    let meta = read.open_table(DELIVERY_META).unwrap();
    assert_eq!(
        meta.get(b"codec_version".as_slice())
            .unwrap()
            .map(|value| value.value().to_vec()),
        Some(1u64.to_be_bytes().to_vec())
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

    let database = Database::open(&path).unwrap();
    let read = database.begin_read().unwrap();
    let names = read
        .list_tables()
        .unwrap()
        .map(|table| table.name().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(names, BTreeSet::from(["outbox_intents".to_owned()]));
}
