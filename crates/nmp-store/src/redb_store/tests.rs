use nmp_grammar::RelaySessionKey;
use super::publish_queue_codec::{NEXT_RELAY_ID_KEY, PUBLISH_QUEUE_CODEC_VERSION_KEY};
use super::*;
use crate::{DurabilityOutcome, PersistenceFault};

/// The refcount half of one `relays` row, for falsifiers that assert
/// reference counting rather than URL interning.
fn relay_refs_of(db: &Database, relay_key: RelayKey) -> Option<u64> {
    let read_txn = db.begin_read().expect("read relay row");
    let relays = read_txn.open_table(RELAYS).expect("open relays");
    let row = relays.get(relay_key).expect("get relay row")?;
    let (refs, _url) = decode_relay_row(relay_key, row.value()).expect("decode relay row");
    Some(refs)
}

/// #867: NMP defines ONE current Redb schema epoch and carries no
/// persistent-schema compatibility obligation. Anything that is not exactly
/// that epoch -- a database whose tables predate the schema marker, and a
/// marker that is not exactly `SCHEMA_VERSION` -- must refuse at open, before
/// a store exists and without changing a single durable fact. Which markers
/// and table layouts shipped before is deliberately not written down here:
/// the refusal is "not exactly current", so naming the retired ones would be
/// knowledge of them that this repository does not keep.
///
/// "Durable fact" is the exact table inventory, every table's row count, and
/// the schema marker itself. It is deliberately NOT raw file bytes: redb
/// performs its own allocator bookkeeping whenever a `Database` handle is
/// created, which happens before any NMP code can read the marker. What this
/// proves is the load-bearing claim -- the refusal creates no table, writes no
/// row, and never rewrites the marker it rejected.
fn durable_facts(path: &std::path::Path) -> BTreeMap<String, u64> {
    let db = Database::create(path).expect("reopen fixture for durable-fact snapshot");
    let read_txn = db.begin_read().expect("read fixture");
    let mut facts = BTreeMap::new();
    for handle in read_txn.list_tables().expect("list fixture tables") {
        let name = handle.name().to_owned();
        let len = read_txn
            .open_untyped_table(handle)
            .expect("open fixture table")
            .len()
            .expect("count fixture rows");
        facts.insert(name, len);
    }
    if let Ok(store_meta) = read_txn.open_table(STORE_META) {
        if let Some(version) = store_meta.get(SCHEMA_VERSION_KEY).expect("read marker") {
            facts.insert("::schema-marker".to_owned(), version.value());
        }
    }
    facts
}

#[test]
fn temporary_store_removes_its_redb_directory_on_drop() {
    let store = RedbStore::temporary().expect("temporary Redb store");
    let database_path = store._ownership.target().to_path_buf();
    let directory = database_path
        .parent()
        .expect("temporary Redb path has a parent")
        .to_path_buf();

    assert!(database_path.exists());
    assert!(directory.exists());
    drop(store);
    assert!(
        !directory.exists(),
        "the engine-owned Redb directory must be released with its store"
    );
}

fn assert_refuses_without_mutation(path: &std::path::Path, what: &str) {
    let before = durable_facts(path);
    let error = match RedbStore::open(path) {
        Ok(_) => panic!("{what} must not open as a current-epoch store"),
        Err(error) => error,
    };
    assert!(
        matches!(error, RedbStoreOpenError::UnsupportedSchema { expected, .. } if expected == SCHEMA_VERSION),
        "{what} must produce the one unsupported-schema refusal, got {error:?}"
    );
    assert_eq!(
        durable_facts(path),
        before,
        "{what} refusal must not create, write, or rewrite any durable fact"
    );
}

/// #1017: an operator must learn the full cost from the reachable refusal,
/// before choosing the deliberate discard. Calling this file a cache would
/// hide the accepted-but-unpublished obligations that recreation destroys.
#[test]
fn unsupported_schema_refusal_states_reacquirable_cache_and_permanent_publish_queue_loss() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unsupported-schema-cost.redb");
    drop(RedbStore::open(&path).unwrap());

    let db = Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    {
        let mut store_meta = write_txn.open_table(STORE_META).unwrap();
        store_meta
            .insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION + 1)
            .unwrap();
    }
    write_txn.commit().unwrap();
    drop(db);

    let error = match RedbStore::open(&path) {
        Ok(_) => panic!("a non-current epoch must refuse"),
        Err(error) => error,
    };
    assert!(
        matches!(
            &error,
            RedbStoreOpenError::UnsupportedSchema {
                expected,
                found: Some(found),
                ..
            } if *expected == SCHEMA_VERSION && *found == SCHEMA_VERSION + 1
        ),
        "the discard contract belongs only to the typed schema refusal: {error:?}"
    );

    let rendered = error.to_string();
    for required in [
        "discard and recreate this store to continue",
        "NMP can reacquire the relay-backed read cache",
        "publish queue state",
        "accepted but unpublished writes",
        "receipts",
        "route revisions",
        "attempt evidence",
        "will be permanently lost",
    ] {
        assert!(
            rendered.contains(required),
            "unsupported-schema refusal omitted {required:?}: {rendered}"
        );
    }
}

/// #867 x #489: ownership is acquired BEFORE the epoch is inspected, so a
/// store that would be refused for its schema is never read by a process that
/// does not own it. If the order were reversed, this would surface the schema
/// verdict instead of the ownership one — leaking one process's durable state
/// to a non-owner.
#[test]
fn a_non_owner_is_refused_for_ownership_before_the_schema_epoch_is_inspected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owned-unsupported-epoch.redb");
    let db = Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    write_txn
        .open_table(TableDefinition::<u64, &[u8]>::new(
            "a-table-this-schema-never-writes",
        ))
        .unwrap();
    write_txn.commit().unwrap();
    drop(db);

    let owner = crate::persistent_store_lifetime::acquire_for_open(&path).unwrap();
    assert!(
        matches!(
            RedbStore::open(&path),
            Err(RedbStoreOpenError::StoreAlreadyOpen { .. })
        ),
        "a non-owner must be refused for ownership, never told about the schema"
    );
    drop(owner);

    // Once unowned, the same target reaches the epoch check and produces the
    // one schema refusal.
    assert_refuses_without_mutation(&path, "owned then released non-current epoch");
}

#[test]
fn a_marker_less_database_refuses_at_open_without_mutating_durable_facts() {
    // The refusal reads two things -- the database has at least one table, and
    // none of them is the schema-marker table. It never reads a table NAME, so
    // one table whose name this schema does not write proves it exactly. The
    // names retired layouts actually used are deliberately not enumerated:
    // that list would be knowledge of those layouts, and it decays into a
    // fixture nobody can check the moment the last one is forgotten.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("marker-less.redb");
    let db = Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    write_txn
        .open_table(TableDefinition::<u64, &[u8]>::new(
            "a-table-this-schema-never-writes",
        ))
        .unwrap();
    write_txn.commit().unwrap();
    drop(db);

    assert_refuses_without_mutation(&path, "a database with tables but no marker");
}

#[test]
fn superseded_schema_markers_refuse_at_open_without_mutating_durable_facts() {
    // The invariant is "not EXACTLY the current epoch", in both directions --
    // one below and one above prove it. Both are derived from
    // `SCHEMA_VERSION` rather than written out, deliberately: a hard-coded
    // list of the markers earlier builds wrote is knowledge of retired
    // schemas, and this repository keeps none. Writing them out also rots
    // silently, because such a list has to name a stand-in for a FUTURE epoch
    // -- and the next version bump turns that stand-in into the current one,
    // leaving the test asserting that the current schema is refused.
    for superseded in [SCHEMA_VERSION - 1, SCHEMA_VERSION + 1] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("superseded-marker.redb");
        drop(RedbStore::open(&path).unwrap());

        let db = Database::create(&path).unwrap();
        let write_txn = db.begin_write().unwrap();
        {
            let mut store_meta = write_txn.open_table(STORE_META).unwrap();
            store_meta.insert(SCHEMA_VERSION_KEY, superseded).unwrap();
        }
        write_txn.commit().unwrap();
        drop(db);

        assert_refuses_without_mutation(&path, &format!("schema marker {superseded}"));
    }
}

/// The refusal must never be reachable by damaging the CURRENT epoch: a
/// corrupt current row stays typed corruption, so an operator can never read
/// "unsupported schema" and conclude their data was merely old.
#[test]
fn corrupt_current_schema_rows_fail_as_corruption_not_as_an_old_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt-current.redb");
    drop(RedbStore::open(&path).unwrap());

    let db = Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    {
        let mut publish_queue_meta = write_txn.open_table(PUBLISH_QUEUE_META).unwrap();
        publish_queue_meta
            .insert(PUBLISH_QUEUE_CODEC_VERSION_KEY, [0u8; 3].as_slice())
            .unwrap();
    }
    write_txn.commit().unwrap();
    drop(db);

    assert!(
        matches!(
            RedbStore::open(&path),
            Err(RedbStoreOpenError::Database(redb::Error::Corrupted(_)))
        ),
        "a truncated publish-queue codec marker is current-epoch corruption"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt-current-codec.redb");
    drop(RedbStore::open(&path).unwrap());

    let db = Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    {
        let mut publish_queue_meta = write_txn.open_table(PUBLISH_QUEUE_META).unwrap();
        publish_queue_meta
            .remove(PUBLISH_QUEUE_CODEC_VERSION_KEY)
            .unwrap();
    }
    write_txn.commit().unwrap();
    drop(db);

    assert!(
        matches!(
            RedbStore::open(&path),
            Err(RedbStoreOpenError::Database(redb::Error::Corrupted(_)))
        ),
        "a missing publish-queue codec marker is current-epoch corruption, never an old epoch"
    );
}

#[test]
fn healthy_current_schema_reopen_starts_no_application_write_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("healthy-reopen.redb");

    let fresh = RedbStore::open(&path).unwrap();
    assert_eq!(
        fresh.open_write_transactions(),
        1,
        "fresh schema creation writes once"
    );
    drop(fresh);

    let reopened = RedbStore::open(&path).unwrap();
    assert_eq!(
        reopened.open_write_transactions(),
        0,
        "a healthy schema-marker reopen must remain read-only"
    );
    let read_txn = reopened.raw_database().begin_read().unwrap();
    let store_meta = read_txn.open_table(STORE_META).unwrap();
    assert_eq!(
        store_meta.get(SCHEMA_VERSION_KEY).unwrap().unwrap().value(),
        SCHEMA_VERSION
    );
}

#[test]
fn a_refused_receipt_needs_no_recovery_write_on_reopen() {
    // Deleting the ephemeral mode deleted the only reason `open()` ever
    // wrote on the reopen path. A refused entry is TERMINAL AT BIRTH, so
    // there is no crash-abandoned state to reconcile and a reopen stays a
    // pure read.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refused-reopen.redb");
    let keys = nostr::Keys::generate();
    let frozen_id = EventId::from_byte_array([7; 32]);

    let mut store = RedbStore::open(&path).unwrap();
    let receipt_id = store
        .accept_refused(
            frozen_id,
            keys.public_key(),
            crate::RefuseReason::Tombstoned,
        )
        .unwrap();
    drop(store);

    let recovered = RedbStore::open(&path).unwrap();
    assert_eq!(recovered.open_write_transactions(), 0);
    let receipt = recovered
        .reattach_receipt(receipt_id)
        .unwrap()
        .expect("retained refused receipt");
    assert_eq!(
        receipt.event_state(),
        Some(ReceiptState::Refused(crate::RefuseReason::Tombstoned))
    );
}

#[test]
fn known_signature_returns_stored_sig_for_ingested_id_and_none_otherwise() {
    use nostr::{EventBuilder, Kind};

    let keys = nostr::Keys::generate();
    let event = EventBuilder::new(Kind::TextNote, "verify-dedup")
        .sign_with_keys(&keys)
        .expect("sign fixture");
    let relay = RelayUrl::parse("ws://127.0.0.1:1").unwrap();
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .insert(
            event.clone(),
            RelayObserved::new(relay, Timestamp::from(1u64)),
        )
        .expect("insert fixture");

    let reader = store.share_sig_reader().expect("cut sig reader");
    assert_eq!(reader.known_signature(&event.id), Some(event.sig));
    // Unknown id is not known-good.
    let absent = EventId::from_byte_array([0xff; 32]);
    assert_eq!(reader.known_signature(&absent), None);
}

#[test]
fn known_signature_reader_survives_store_reopen() {
    use nostr::{EventBuilder, Kind};

    // The reader reads through a shared cell the store replaces on reopen,
    // so it must NOT strand on a dead handle after a fault-recovery reopen.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen-sig.redb");
    let keys = nostr::Keys::generate();
    let event = EventBuilder::new(Kind::TextNote, "survives-reopen")
        .custom_created_at(Timestamp::from(1u64))
        .sign_with_keys(&keys)
        .unwrap();
    let relay = RelayUrl::parse("ws://127.0.0.1:2").unwrap();
    let mut store = RedbStore::open(&path).unwrap();
    store
        .insert(
            event.clone(),
            RelayObserved::new(relay, Timestamp::from(2u64)),
        )
        .expect("insert fixture");
    let reader = store.share_sig_reader().expect("cut sig reader");
    assert_eq!(reader.known_signature(&event.id), Some(event.sig));

    store
        .reopen_after_failure()
        .expect("reopen replaces the shared db handle");
    // The committed event survives reopen, and the reader sees it through
    // the NEW handle installed in the shared cell.
    assert_eq!(reader.known_signature(&event.id), Some(event.sig));
}

#[test]
fn surrogate_allocators_do_not_touch_hot_metadata_rows_until_one_flush() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("allocator-flush.redb");
    let store = RedbStore::open(&path).unwrap();
    let write_txn = store.raw_database().begin_write().unwrap();
    {
        let mut canonical = CanonicalWriteTables::open(&write_txn).unwrap();
        for expected in 1..=128 {
            assert_eq!(canonical.allocate_key().unwrap(), expected);
        }
        for expected in 1..=16 {
            assert_eq!(canonical.allocate_relay_key().unwrap(), expected);
        }
        assert!(canonical.store_meta.get(NEXT_EVENT_KEY).unwrap().is_none());
        assert!(canonical.store_meta.get(NEXT_RELAY_KEY).unwrap().is_none());

        canonical.flush_pending().unwrap();
        assert_eq!(
            canonical
                .store_meta
                .get(NEXT_EVENT_KEY)
                .unwrap()
                .unwrap()
                .value(),
            129
        );
        assert_eq!(
            canonical
                .store_meta
                .get(NEXT_RELAY_KEY)
                .unwrap()
                .unwrap()
                .value(),
            17
        );
    }
    write_txn.commit().unwrap();
}

/// The verified, intent-bound evidence `promote_signed` takes (#768). Every
/// event promoted below is one this fixture just signed itself, so the
/// verification succeeding is part of the setup, not the property under test.
fn evidence(signed: &Event) -> VerifiedSignature {
    VerifiedSignature::verify(signed).expect("fixture events are validly signed")
}

fn accepted_signed(
    store: &mut RedbStore,
    keys: &nostr::Keys,
    content: &str,
    created_at: u64,
) -> (IntentId, Event) {
    use nostr::EventBuilder;

    let signed = EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign fixture event");
    let frozen = Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        crate::sentinel_signature(),
    );
    let outcome = store
        .accept_write(AcceptWrite {
            payload: crate::AcceptWritePayload::Event {
                frozen: Box::new(frozen),
                routing: "range-proof".into(),
                sig_state: IntentSigState::Pending,
            },
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "range-proof".into(),
            accepted_at: Timestamp::from(created_at),
        })
        .expect("accept fixture intent");
    let intent = outcome.journaled_intent_id().expect("intent id");
    store
        .promote_signed(crate::PromotionTarget::Event(intent), evidence(&signed))
        .expect("promote fixture intent");
    (intent, signed)
}

fn accepted_pending(
    store: &mut RedbStore,
    keys: &nostr::Keys,
    content: &str,
    created_at: u64,
) -> (IntentId, Event) {
    use nostr::EventBuilder;

    let signed = EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign fixture event");
    let frozen = Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        crate::sentinel_signature(),
    );
    let outcome = store
        .accept_write(AcceptWrite {
            payload: crate::AcceptWritePayload::Event {
                frozen: Box::new(frozen),
                routing: "compensation-proof".into(),
                sig_state: IntentSigState::Pending,
            },
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "compensation-proof".into(),
            accepted_at: Timestamp::from(created_at),
        })
        .expect("accept fixture intent");
    (outcome.journaled_intent_id().expect("intent id"), signed)
}

#[test]
fn configured_lane_start_failure_rolls_back_the_precommit_transaction() {
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://blocked-lane-start.example").unwrap();
    let mut store = RedbStore::temporary_with_failed_lane_starts([relay.clone()])
        .expect("temporary Redb failure fixture");
    let (intent, signed) = accepted_signed(&mut store, &keys, "blocked", 1_000);
    store
        .record_route_revision(intent, BTreeSet::from([relay]))
        .unwrap();
    let lane = store
        .bootstrap_publish_queue_lanes(intent)
        .unwrap()
        .remove(0);
    let eligible = store
        .set_lane_eligible(&lane.key, lane.revision, Timestamp::from(1_001u64))
        .unwrap();

    let error = store
        .start_lane_attempt(
            &eligible.key,
            eligible.revision,
            signed,
            Timestamp::from(1_002u64),
        )
        .expect_err("configured relay must fail before commit");
    assert_eq!(
        error.to_string(),
        "durable-store persistence failure: injected attempt start failure"
    );
    assert_eq!(
        store.recover_publish_queue_lanes(intent).unwrap(),
        vec![eligible],
        "the lane cursor remains eligible"
    );
    assert!(store.recover_attempts(intent).unwrap().is_empty());
    assert!(store.recover_attempt_details(intent).unwrap().is_empty());
}

#[test]
fn configured_lane_bootstrap_failure_rolls_back_and_is_consumed_once() {
    let keys = nostr::Keys::generate();
    let first = RelayUrl::parse("wss://blocked-lane-bootstrap-a.example").unwrap();
    let second = RelayUrl::parse("wss://blocked-lane-bootstrap-b.example").unwrap();
    let expected_relays = BTreeSet::from([first, second]);
    let mut store = RedbStore::temporary_with_failed_lane_bootstrap()
        .expect("temporary Redb lane-bootstrap failure fixture");
    let (intent, signed) = accepted_signed(&mut store, &keys, "bootstrap once", 1_000);
    assert!(
        store
            .bootstrap_publish_queue_lanes(intent)
            .expect("route-less lane bootstrap is a healthy no-op")
            .is_empty(),
        "a route-less bootstrap must not stage lanes or consume the configured failure"
    );
    let route = store
        .record_route_revision(intent, expected_relays.clone())
        .expect("route revision commits before lane bootstrap");

    let error = store
        .bootstrap_publish_queue_lanes(intent)
        .expect_err("construction-armed lane bootstrap must fail before commit");
    assert_eq!(
        error.to_string(),
        "durable-store persistence failure: injected lane bootstrap failure"
    );
    assert_eq!(
        store.recover_route_revisions(intent).unwrap(),
        vec![route],
        "the independently committed route survives the refused bootstrap"
    );
    assert!(
        store
            .recover_publish_queue_lanes(intent)
            .unwrap()
            .is_empty(),
        "the refused transaction persists no lane"
    );

    let lanes = store
        .bootstrap_publish_queue_lanes(intent)
        .expect("the same store retries after consuming the one-shot refusal");
    assert_eq!(lanes.len(), expected_relays.len());
    assert_eq!(
        lanes
            .iter()
            .map(|lane| lane.key.relay.clone())
            .collect::<BTreeSet<_>>(),
        expected_relays
    );
    for lane in &lanes {
        assert_eq!(lane.key.intent_id, intent);
        assert_eq!(lane.key.event_id, signed.id);
        assert_eq!(lane.revision, 1);
        assert_eq!(lane.last_ordinal, 0);
        assert_eq!(lane.state, PublishQueueLaneState::WaitingConnection);
    }
    assert_eq!(
        store.recover_publish_queue_lanes(intent).unwrap(),
        lanes,
        "the same-store retry commits exactly the returned lane set"
    );
}

#[test]
fn configured_lane_handoff_failure_rolls_back_and_is_consumed_once() {
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://blocked-lane-handoff.example").unwrap();
    let mut store = RedbStore::temporary_with_failed_lane_handoff()
        .expect("temporary Redb handoff-failure fixture");
    let (intent, signed) = accepted_signed(&mut store, &keys, "blocked handoff", 1_000);
    store
        .record_route_revision(intent, BTreeSet::from([relay]))
        .unwrap();
    let lane = store
        .bootstrap_publish_queue_lanes(intent)
        .unwrap()
        .remove(0);
    let eligible = store
        .set_lane_eligible(&lane.key, lane.revision, Timestamp::from(1_001u64))
        .unwrap();
    let (_, started) = store
        .start_lane_attempt(
            &eligible.key,
            eligible.revision,
            signed,
            Timestamp::from(1_002u64),
        )
        .unwrap();
    let before_details = store.recover_attempt_details(intent).unwrap();
    let handoff = PublishQueueAttemptHandoff {
        at: Timestamp::from(1_003u64),
        result: crate::HandoffEvidence::Written,
    };
    let deadline = Timestamp::from(1_033u64);

    let error = store
        .record_lane_handoff(
            &started.key,
            started.revision,
            started.last_ordinal,
            handoff.clone(),
            PublishQueuePostHandoffState::AwaitingAck { deadline },
        )
        .expect_err("construction-armed handoff must fail before commit");
    assert_eq!(
        error.to_string(),
        "durable-store persistence failure: injected lane handoff failure"
    );
    assert_eq!(
        store.recover_publish_queue_lanes(intent).unwrap(),
        vec![started.clone()],
        "the refused transaction leaves the lane awaiting the same handoff"
    );
    assert_eq!(
        store.recover_attempt_details(intent).unwrap(),
        before_details,
        "the refused transaction persists no handoff evidence"
    );
    assert_eq!(store.next_publish_queue_deadline().unwrap(), None);

    let handed_off = store
        .record_lane_handoff(
            &started.key,
            started.revision,
            started.last_ordinal,
            handoff.clone(),
            PublishQueuePostHandoffState::AwaitingAck { deadline },
        )
        .expect("the same store retries after consuming the one-shot refusal");
    assert!(matches!(
        handed_off.state,
        PublishQueueLaneState::InFlight {
            phase: PublishQueueInFlightPhase::AwaitingAck { deadline: actual },
            ..
        } if actual == deadline
    ));
    assert_eq!(
        store.recover_attempt_details(intent).unwrap()[0].handoff,
        Some(handoff)
    );
    assert_eq!(store.next_publish_queue_deadline().unwrap(), Some(deadline));
}

#[test]
fn corrupt_publish_queue_deadline_targets_exact_awaiting_ack_attempt() {
    let directory = tempfile::tempdir().expect("deadline corruption directory");
    let path = directory.path().join("delivery-deadline-corruption.redb");
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://deadline-corruption.example").unwrap();
    let deadline = Timestamp::from(1_033u64);
    let (intent, attempt, handed_off) = {
        let mut store = RedbStore::open(&path).expect("open persistent Redb fixture");
        let (intent, signed) = accepted_signed(&mut store, &keys, "deadline corruption", 1_000);
        store
            .record_route_revision(intent, BTreeSet::from([relay]))
            .unwrap();
        let lane = store
            .bootstrap_publish_queue_lanes(intent)
            .unwrap()
            .remove(0);
        let eligible = store
            .set_lane_eligible(&lane.key, lane.revision, Timestamp::from(1_001u64))
            .unwrap();
        let (attempt, started) = store
            .start_lane_attempt(
                &eligible.key,
                eligible.revision,
                signed,
                Timestamp::from(1_002u64),
            )
            .unwrap();
        let handed_off = store
            .record_lane_handoff(
                &started.key,
                started.revision,
                started.last_ordinal,
                PublishQueueAttemptHandoff {
                    at: Timestamp::from(1_003u64),
                    result: crate::HandoffEvidence::Written,
                },
                PublishQueuePostHandoffState::AwaitingAck { deadline },
            )
            .unwrap();
        assert_eq!(store.next_expiration().unwrap(), None);
        assert_eq!(store.next_publish_queue_deadline().unwrap(), Some(deadline));
        (intent, attempt, handed_off)
    };

    let mut wrong_attempt = attempt.clone();
    wrong_attempt.ordinal += 1;
    let error = testing::corrupt_publish_queue_deadline(&path, &wrong_attempt)
        .expect_err("a different attempt ordinal must not name the deadline row");
    assert_eq!(error.fault(), PersistenceFault::Invariant);
    {
        let store = RedbStore::open(&path).expect("wrong target leaves store readable");
        assert_eq!(store.next_expiration().unwrap(), None);
        assert_eq!(store.next_publish_queue_deadline().unwrap(), Some(deadline));
    }

    testing::corrupt_publish_queue_deadline(&path, &attempt)
        .expect("corrupt the exact ordered deadline row");
    let store = RedbStore::open(&path).expect("reopen corrupted fixture");
    assert_eq!(store.next_expiration().unwrap(), None);
    assert_eq!(store.recover_attempts(intent).unwrap(), vec![attempt]);
    assert_eq!(
        store.recover_publish_queue_lanes(intent).unwrap(),
        vec![handed_off]
    );
    let error = store
        .next_publish_queue_deadline()
        .expect_err("the corrupt deadline value must not become false absence");
    assert_eq!(error.fault(), PersistenceFault::Invariant);
    assert_eq!(error.durability(), DurabilityOutcome::Absent);
    assert!(
        error.message().contains("decode publish queue deadline"),
        "the exact deadline codec owns the error: {}",
        error.message()
    );
}

#[test]
fn configured_query_newest_before_failure_is_consumed_once() {
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://query-newest-before-failure.example").unwrap();
    let events: Vec<_> = (100..103)
        .map(|created_at| {
            nostr::EventBuilder::new(Kind::TextNote, format!("row-{created_at}"))
                .custom_created_at(Timestamp::from(created_at))
                .sign_with_keys(&keys)
                .unwrap()
        })
        .collect();
    let before = EventCursor::new(events[2].created_at, events[2].id);
    let expected = [events[1].id, events[0].id];
    let mut store = RedbStore::temporary_with_failed_query_newest_before()
        .expect("temporary Redb query-newest-before failure fixture");
    store
        .insert_batch(
            events
                .into_iter()
                .map(|event| {
                    (
                        event,
                        RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                    )
                })
                .collect(),
        )
        .unwrap();

    let filter = Filter::new().kind(Kind::TextNote);
    assert!(store
        .query_newest_before(&filter, before, 0)
        .expect("a zero-limit no-op cannot consume the construction arm")
        .is_empty());
    let error = store
        .query_newest_before(&filter, before, 2)
        .expect_err("construction-armed bounded read must refuse once");
    assert_eq!(
        error.to_string(),
        "durable-store persistence failure: injected query-newest-before failure"
    );

    let rows = store
        .query_newest_before(&filter, before, 2)
        .expect("the same store retries after consuming the one-shot refusal");
    assert_eq!(
        rows.iter().map(|row| row.event.id).collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn observation_precommit_io_closes_reopens_and_retries_single_insert() {
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://observation-single-io.example").unwrap();
    let event = nostr::EventBuilder::new(Kind::TextNote, "single observation")
        .custom_created_at(Timestamp::from(1_000u64))
        .sign_with_keys(&keys)
        .unwrap();
    let observed = RelayObserved::new(relay, Timestamp::from(1_001u64));
    let mut store = RedbStore::temporary_with_observation_precommit_io()
        .expect("temporary Redb observation-I/O fixture");

    let error = store
        .insert(event.clone(), observed.clone())
        .expect_err("the construction-armed observation must refuse once");
    assert_eq!(error.fault(), PersistenceFault::Io);
    assert_eq!(error.durability(), DurabilityOutcome::Unknown);
    let latched = store
        .query(&Filter::new().id(event.id))
        .expect_err("the actual failed Redb generation must stay closed");
    assert_eq!(latched.fault(), PersistenceFault::Latched);

    store
        .reopen_after_failure()
        .expect("the existing reconstruction door reopens the same target");
    assert!(store.query(&Filter::new().id(event.id)).unwrap().is_empty());
    assert!(matches!(
        store.insert(event.clone(), observed).unwrap(),
        InsertOutcome::Inserted
    ));
    assert_eq!(store.query(&Filter::new().id(event.id)).unwrap().len(), 1);
}

#[test]
fn observation_precommit_io_preserves_empty_arm_and_rolls_back_whole_batch() {
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://observation-batch-io.example").unwrap();
    let events: Vec<_> = [1_000u64, 1_001]
        .into_iter()
        .map(|created_at| {
            nostr::EventBuilder::new(Kind::TextNote, format!("batch-{created_at}"))
                .custom_created_at(Timestamp::from(created_at))
                .sign_with_keys(&keys)
                .unwrap()
        })
        .collect();
    let ids = events.iter().map(|event| event.id).collect::<Vec<_>>();
    let batch = events
        .into_iter()
        .map(|event| {
            (
                event,
                RelayObserved::new(relay.clone(), Timestamp::from(2_000u64)),
            )
        })
        .collect::<Vec<_>>();
    let mut store = RedbStore::temporary_with_observation_precommit_io()
        .expect("temporary Redb observation-I/O fixture");

    assert!(store.insert_batch(Vec::new()).unwrap().is_empty());
    let error = store
        .insert_batch(batch.clone())
        .expect_err("the first nonempty batch must consume the construction arm");
    assert_eq!(error.fault(), PersistenceFault::Io);
    assert_eq!(error.durability(), DurabilityOutcome::Unknown);
    let latched = store
        .query(&Filter::new().ids(ids.clone()))
        .expect_err("the actual failed Redb generation must stay closed");
    assert_eq!(latched.fault(), PersistenceFault::Latched);

    store
        .reopen_after_failure()
        .expect("the existing reconstruction door reopens the same target");
    assert!(store
        .query(&Filter::new().ids(ids.clone()))
        .unwrap()
        .is_empty());
    assert_eq!(store.insert_batch(batch).unwrap().len(), 2);
    assert_eq!(store.query(&Filter::new().ids(ids)).unwrap().len(), 2);
}

#[test]
fn configured_coverage_write_failure_targets_one_row_rolls_back_and_is_consumed_once() {
    let directory = tempfile::tempdir().expect("coverage failure directory");
    let path = directory.path().join("coverage-write-failure.redb");
    let relay = RelayUrl::parse("wss://coverage-write-failure.example").unwrap();
    let atom = |kind| ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            ..ConcreteFilter::default()
        },
        routing: nmp_grammar::ReadRouting::Auto,
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    };
    let target = atom(1);
    let collateral = atom(2);
    let unrelated = atom(3);
    let target_key = compute_coverage_key(&target);
    let collateral_key = compute_coverage_key(&collateral);
    let unrelated_key = compute_coverage_key(&unrelated);
    let interval = CoverageInterval::new(Timestamp::from(100), Timestamp::from(200));
    let mut store = RedbStore::open_with_failed_coverage_write(&path, target_key, relay.clone())
        .expect("persistent Redb coverage-write failure fixture");

    store
        .record_coverage(&[(unrelated, RelaySessionKey::unauthenticated(relay.clone()), interval)])
        .expect("an unrelated row cannot consume the exact construction arm");

    let batch = [
        (
            target,
            RelaySessionKey::unauthenticated(relay.clone()),
            interval,
        ),
        (
            collateral,
            RelaySessionKey::unauthenticated(relay.clone()),
            interval,
        ),
    ];
    let error = store
        .record_coverage(&batch)
        .expect_err("the exact target row must refuse the staged batch once");
    assert_eq!(
        error.to_string(),
        "durable-store persistence failure: injected coverage write failure"
    );
    assert_eq!(store.get_coverage(target_key, &RelaySessionKey::unauthenticated(relay.clone())).unwrap(), None);
    assert_eq!(store.get_coverage(collateral_key, &RelaySessionKey::unauthenticated(relay.clone())).unwrap(), None);
    assert_eq!(
        store.get_coverage(unrelated_key, &RelaySessionKey::unauthenticated(relay.clone())).unwrap(),
        Some(interval),
        "the independently committed unrelated row survives"
    );

    store
        .record_coverage(&batch)
        .expect("the same store retries after consuming the one-shot refusal");
    assert_eq!(
        store.get_coverage(target_key, &RelaySessionKey::unauthenticated(relay.clone())).unwrap(),
        Some(interval)
    );
    assert_eq!(
        store.get_coverage(collateral_key, &RelaySessionKey::unauthenticated(relay.clone())).unwrap(),
        Some(interval)
    );
}

#[test]
fn configured_compensation_failure_rolls_back_and_is_consumed_once() {
    let keys = nostr::Keys::generate();
    let mut store = RedbStore::temporary_with_failed_compensation_with_state()
        .expect("temporary Redb compensation-failure fixture");
    let (intent, event) = accepted_pending(&mut store, &keys, "compensate once", 1_000);
    let before = store.recover_publish_queue().unwrap();

    let error = super::write_ops::compensate_write_with_state(
        &mut store,
        intent,
        super::write_ops::CompensationReason::Failure,
    )
    .expect_err("construction-armed compensation must fail before commit");
    assert_eq!(
        error.to_string(),
        "durable-store persistence failure: injected compensation failure"
    );
    assert_eq!(store.recover_publish_queue().unwrap(), before);
    assert_eq!(
        store.query(&Filter::new().id(event.id)).unwrap().len(),
        1,
        "the refused transaction leaves the optimistic row live"
    );

    assert!(matches!(
        super::write_ops::compensate_write_with_state(
            &mut store,
            intent,
            super::write_ops::CompensationReason::Failure,
        )
        .expect("the same store retries after consuming the one-shot refusal"),
        CompensateOutcome::Compensated { .. }
    ));
    assert!(store.recover_publish_queue().unwrap().is_empty());
    assert!(store.query(&Filter::new().id(event.id)).unwrap().is_empty());
}

#[test]
fn configured_lane_attempt_finish_failure_rolls_back_and_is_consumed_once() {
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://blocked-lane-finish.example").unwrap();
    let mut store = RedbStore::temporary_with_failed_lane_attempt_finish()
        .expect("temporary Redb lane-finish failure fixture");
    let (intent, signed) = accepted_signed(&mut store, &keys, "finish once", 1_000);
    store
        .record_route_revision(intent, BTreeSet::from([relay]))
        .unwrap();
    let lane = store
        .bootstrap_publish_queue_lanes(intent)
        .unwrap()
        .remove(0);
    let eligible = store
        .set_lane_eligible(&lane.key, lane.revision, Timestamp::from(1_001u64))
        .unwrap();
    let (attempt, in_flight) = store
        .start_lane_attempt(
            &eligible.key,
            eligible.revision,
            signed,
            Timestamp::from(1_002u64),
        )
        .unwrap();

    let error = store
        .finish_lane_attempt(
            &in_flight.key,
            in_flight.revision,
            attempt.ordinal,
            PublishQueueAttemptOutcome::Acked,
            Timestamp::from(1_003u64),
        )
        .expect_err("construction-armed lane finish must fail before commit");
    assert_eq!(
        error.to_string(),
        "durable-store persistence failure: injected attempt finish failure"
    );
    assert_eq!(
        store.recover_publish_queue_lanes(intent).unwrap(),
        vec![in_flight.clone()]
    );
    assert_eq!(
        store.recover_attempts(intent).unwrap(),
        vec![attempt.clone()]
    );
    assert_eq!(
        store.recover_attempt_details(intent).unwrap()[0].terminal,
        None
    );

    let terminal = store
        .finish_lane_attempt(
            &in_flight.key,
            in_flight.revision,
            attempt.ordinal,
            PublishQueueAttemptOutcome::Acked,
            Timestamp::from(1_003u64),
        )
        .expect("the same store retries after consuming the one-shot refusal");
    assert!(matches!(
        terminal.state,
        PublishQueueLaneState::Terminal {
            ordinal: 1,
            outcome: PublishQueueTerminalOutcome::Acked,
        }
    ));
    assert_eq!(
        store.recover_attempt_details(intent).unwrap()[0].terminal,
        Some(PublishQueueAttemptOutcome::Acked)
    );
}

#[test]
fn configured_route_revision_failure_rolls_back_the_precommit_transaction() {
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://blocked-route-revision.example").unwrap();
    let dir = tempfile::tempdir().expect("route rollback tempdir");
    let path = dir.path().join("route-rollback.redb");
    let intent = {
        let mut store = RedbStore::open_with_route_revision_write_failure(&path)
            .expect("persistent Redb route-failure fixture");
        let (intent, _signed) = accepted_signed(&mut store, &keys, "blocked route", 1_000);

        let error = store
            .record_route_revision(intent, BTreeSet::from([relay.clone()]))
            .expect_err("configured route revision must fail before commit");
        assert_eq!(
            error.to_string(),
            "durable-store persistence failure: injected route revision failure"
        );
        assert!(store.recover_route_revisions(intent).unwrap().is_empty());
        let read_txn = store.raw_database().begin_read().unwrap();
        assert_eq!(
            read_txn
                .open_table(PUBLISH_QUEUE_RELAYS)
                .unwrap()
                .len()
                .unwrap(),
            0,
            "relay dictionary allocation is part of the refused transaction"
        );
        assert_eq!(
            read_txn
                .open_table(PUBLISH_QUEUE_RELAY_IDS)
                .unwrap()
                .len()
                .unwrap(),
            0,
            "relay reverse dictionary allocation is part of the refused transaction"
        );
        assert!(
            read_txn
                .open_table(PUBLISH_QUEUE_META)
                .unwrap()
                .get(NEXT_RELAY_ID_KEY)
                .unwrap()
                .is_none(),
            "relay dictionary counter is part of the refused transaction"
        );
        intent
    };

    let mut reopened = RedbStore::open(&path).expect("reopen healthy Redb store");
    assert!(reopened.recover_route_revisions(intent).unwrap().is_empty());
    let revision = reopened
        .record_route_revision(intent, BTreeSet::from([relay.clone()]))
        .expect("healthy reopen records route revision");
    assert_eq!(revision.ordinal, 1);
    assert_eq!(revision.relays, BTreeSet::from([relay]));
}

/// Issue #87's measurable bound: 128 unrelated intents must add zero
/// visited rows to target-intent attempt or route-revision recovery.
/// Relay URLs deliberately share textual prefixes, and intent 1 coexists
/// with prefix-adversarial ids 10/100.
#[test]
fn publish_queue_ranges_visit_only_target_intent_and_exact_relay_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("delivery-ranges.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let keys = nostr::Keys::generate();
    let short = RelayUrl::parse("wss://prefix.example/x").unwrap();
    let extended = RelayUrl::parse("wss://prefix.example/x:443").unwrap();

    let (target, target_event) = accepted_signed(&mut store, &keys, "target", 1_000);
    assert_eq!(target, IntentId(1));
    store
        .record_route_revision(target, BTreeSet::from([short.clone(), extended.clone()]))
        .unwrap();
    store
        .record_route_revision(target, BTreeSet::from([short.clone()]))
        .unwrap();
    let lanes = store.bootstrap_publish_queue_lanes(target).unwrap();
    let short_lane = lanes
        .iter()
        .find(|lane| lane.key.relay == short)
        .unwrap()
        .clone();
    let extended_lane = lanes
        .iter()
        .find(|lane| lane.key.relay == extended)
        .unwrap()
        .clone();
    let short_lane = store
        .set_lane_eligible(
            &short_lane.key,
            short_lane.revision,
            Timestamp::from(1_001u64),
        )
        .unwrap();
    let (_, short_lane) = store
        .start_lane_attempt(
            &short_lane.key,
            short_lane.revision,
            target_event.clone(),
            Timestamp::from(1_002u64),
        )
        .unwrap();
    store
        .finish_lane_attempt(
            &short_lane.key,
            short_lane.revision,
            1,
            PublishQueueAttemptOutcome::GaveUp,
            Timestamp::from(1_003u64),
        )
        .unwrap();
    let extended_lane = store
        .set_lane_eligible(
            &extended_lane.key,
            extended_lane.revision,
            Timestamp::from(1_001u64),
        )
        .unwrap();
    store
        .start_lane_attempt(
            &extended_lane.key,
            extended_lane.revision,
            target_event,
            Timestamp::from(1_002u64),
        )
        .unwrap();

    for index in 0..128u64 {
        let (intent, event) =
            accepted_signed(&mut store, &keys, &format!("noise-{index}"), 2_000 + index);
        let relay = RelayUrl::parse(&format!("wss://noise-{index}.example")).unwrap();
        store
            .record_route_revision(intent, BTreeSet::from([relay.clone()]))
            .unwrap();
        let noise_lane = store
            .bootstrap_publish_queue_lanes(intent)
            .unwrap()
            .remove(0);
        let noise_lane = store
            .set_lane_eligible(
                &noise_lane.key,
                noise_lane.revision,
                Timestamp::from(2_001u64 + index),
            )
            .unwrap();
        store
            .start_lane_attempt(
                &noise_lane.key,
                noise_lane.revision,
                event,
                Timestamp::from(2_002u64 + index),
            )
            .unwrap();
    }

    store.reset_publish_queue_range_rows();
    let attempts = store.recover_attempts(target).unwrap();
    let revisions = store.recover_route_revisions(target).unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(revisions.len(), 2);
    assert_eq!(store.publish_queue_range_rows(), (2, 2));
}

/// The durable-key falsifier for this fix: `coverage_row_key` must
/// carry the FULL 32-byte BLAKE3 digest (64 hex chars), not a
/// truncated 8-byte (16 hex char) prefix -- truncating back down to
/// 64 bits in the on-disk key would silently undo the whole point of
/// widening `DescriptorHash`/`CoverageKey` (a forged collision only
/// needs to defeat whatever width actually reaches the durable key).
#[test]
fn coverage_row_key_carries_the_full_256_bit_digest() {
    let filter = ConcreteFilter {
        kinds: Some(std::collections::BTreeSet::from([1u16])),
        authors: Some(std::collections::BTreeSet::from(["aa".to_string()])),
        ..ConcreteFilter::default()
    };
    let atom = ContextualAtom {
        filter,
        routing: nmp_grammar::ReadRouting::Auto,
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    };
    let key = compute_coverage_key(&atom);
    let relay = RelayUrl::parse("wss://relay.example").unwrap();
    let row_key = RedbStore::coverage_row_key(key, &nmp_grammar::RelaySessionKey::unauthenticated(relay.clone()));

    // Row key shape is now `<version-prefix><hex>:<relay>` (#106) --
    // skip the version prefix before taking the hex segment.
    let without_prefix = row_key
        .strip_prefix(RedbStore::COVERAGE_ROW_KEY_PREFIX)
        .expect("row key must carry the current schema-version prefix");
    let hex_part = without_prefix
        .split(':')
        .next()
        .expect("row key always has a hex-prefix:relay-url shape");
    assert_eq!(
        hex_part.len(),
        64,
        "expected 64 hex chars (32 bytes) in the durable key, got {} in {row_key:?}",
        hex_part.len()
    );
}

/// The row-count falsifier for issue #17: an author-filtered `query`
/// must decode (JSON-parse) only that author's own rows via
/// `BY_AUTHOR`, never the whole `EVENTS` table -- the documented M5
/// replay jank was `RedbStore::query` doing exactly that unbounded
/// scan+decode on every refresh.
#[test]
fn query_by_author_does_not_scan_all_rows() {
    use nostr::EventBuilder;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let r1 = RelayUrl::parse("wss://r1").expect("relay url");

    let target = nostr::Keys::generate();
    let target_event = EventBuilder::new(Kind::TextNote, "hi")
        .sign_with_keys(&target)
        .expect("sign target event");
    let target_id = target_event.id;
    store
        .insert(
            target_event,
            RelayObserved::new(r1.clone(), Timestamp::from(1u64)),
        )
        .unwrap();

    // A pile of OTHER authors' rows -- large enough that a full-table
    // scan would dwarf the one-row match set below.
    for i in 0..200u64 {
        let noise_author = nostr::Keys::generate();
        let noise = EventBuilder::new(Kind::TextNote, "noise")
            .custom_created_at(Timestamp::from(100 + i))
            .sign_with_keys(&noise_author)
            .expect("sign noise event");
        store
            .insert(
                noise,
                RelayObserved::new(r1.clone(), Timestamp::from(100 + i)),
            )
            .unwrap();
    }

    let before = store.examined_rows();
    let results = store
        .query(&Filter::new().author(target.public_key()))
        .unwrap();
    let examined = store.examined_rows() - before;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].event.id, target_id);
    assert_eq!(
        examined, 1,
        "author-filtered query decoded {examined} row(s) on a 201-row table; \
             expected exactly 1 (the match), not a full-table scan"
    );
}

fn room_event(keys: &nostr::Keys, room: &str, created_at: u64, content: &str) -> Event {
    use nostr::{EventBuilder, Tag};

    EventBuilder::new(Kind::from(9u16), content)
        .tag(Tag::parse(["h", room]).expect("valid h tag"))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign room event")
}

fn raw_canonical_row(store: &RedbStore, id: EventId) -> (EventKey, Vec<u8>, Option<Vec<u8>>) {
    let read_txn = store.raw_database().begin_read().unwrap();
    let event_ids = read_txn.open_table(EVENT_IDS).unwrap();
    let events = read_txn.open_table(EVENTS).unwrap();
    let event_key = event_ids
        .get(id.as_bytes())
        .unwrap()
        .expect("raw id mapping")
        .value();
    let event_bytes = events
        .get(event_row_key(event_key).as_slice())
        .unwrap()
        .expect("raw event row")
        .value()
        .to_vec();
    let local_bytes = events
        .get(event_local_key(event_key).as_slice())
        .unwrap()
        .map(|value| value.value().to_vec());
    (event_key, event_bytes, local_bytes)
}

fn raw_observation_rows(store: &RedbStore, event_key: EventKey) -> Vec<(Vec<u8>, u64)> {
    let read_txn = store.raw_database().begin_read().unwrap();
    let events = read_txn.open_table(EVENTS).unwrap();
    let (lower, upper) = observation_bounds(event_key);
    events
        .range(lower.as_slice()..=upper.as_slice())
        .unwrap()
        .map(|entry| {
            let (key, at) = entry.unwrap();
            let key = key.value().to_vec();
            let relay_key = observation_relay_key(&key);
            (
                key,
                decode_observed_at(event_key, relay_key, at.value()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn tag_index_packs_canonical_hex_ids_without_aliasing_other_strings() {
    let tag = SingleLetterTag::lowercase(nostr::Alphabet::P);
    let canonical = "ab".repeat(32);
    let packed = tag_index_prefix(tag, &canonical);
    assert_eq!(packed.len(), 1 + 1 + 32);
    assert_eq!(packed[1], 1);

    let uppercase = canonical.to_uppercase();
    let ordinary = tag_index_prefix(tag, &uppercase);
    assert_eq!(ordinary[1], 0);
    assert_ne!(ordinary, packed);
}

#[test]
fn duplicate_observation_adds_one_fixed_row_without_rewriting_event_or_local_state() {
    use nostr::EventBuilder;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata-sidecar.redb");
    let mut store = RedbStore::open(&path).unwrap();
    let keys = nostr::Keys::generate();
    let event = EventBuilder::new(Kind::TextNote, "immutable body")
        .custom_created_at(Timestamp::from(10u64))
        .sign_with_keys(&keys)
        .unwrap();
    let first = RelayUrl::parse("wss://first.example").unwrap();
    let second = RelayUrl::parse("wss://second.example").unwrap();
    store
        .insert(
            event.clone(),
            RelayObserved::new(first, Timestamp::from(20u64)),
        )
        .unwrap();
    let (event_key, before_event, before_local) = raw_canonical_row(&store, event.id);
    let before_observations = raw_observation_rows(&store, event_key);

    let outcome = store
        .insert(
            event.clone(),
            RelayObserved::new(second.clone(), Timestamp::from(30u64)),
        )
        .unwrap();
    assert!(matches!(
        outcome,
        InsertOutcome::Duplicate {
            provenance_grew: true,
            ..
        }
    ));
    let (after_key, after_event, after_local) = raw_canonical_row(&store, event.id);
    assert_eq!(after_key, event_key, "surrogate identity is stable");
    assert_eq!(
        after_event, before_event,
        "immutable event bytes were rewritten"
    );
    assert_eq!(after_local, before_local, "local state was rewritten");
    let after_observations = raw_observation_rows(&store, event_key);
    assert_eq!(before_observations.len(), 1);
    assert_eq!(after_observations.len(), 2);
    assert_eq!(
        store.query(&Filter::new().id(event.id)).unwrap()[0]
            .provenance
            .seen
            .get(&second),
        Some(&Timestamp::from(30u64))
    );
}

#[test]
fn equal_or_earlier_redelivery_is_a_true_physical_noop() {
    use nostr::EventBuilder;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata-noop.redb");
    let mut store = RedbStore::open(&path).unwrap();
    let keys = nostr::Keys::generate();
    let event = EventBuilder::new(Kind::TextNote, "no cow churn")
        .custom_created_at(Timestamp::from(10u64))
        .sign_with_keys(&keys)
        .unwrap();
    let relay = RelayUrl::parse("wss://same.example").unwrap();
    store
        .insert(
            event.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(30u64)),
        )
        .unwrap();
    let before = raw_canonical_row(&store, event.id);
    let before_observations = raw_observation_rows(&store, before.0);

    let outcome = store
        .insert(
            event.clone(),
            RelayObserved::new(relay, Timestamp::from(20u64)),
        )
        .unwrap();
    assert!(matches!(
        outcome,
        InsertOutcome::Duplicate {
            provenance_grew: false,
            ..
        }
    ));
    assert_eq!(raw_canonical_row(&store, event.id), before);
    assert_eq!(raw_observation_rows(&store, before.0), before_observations);
}

#[test]
fn relay_dictionary_is_shared_refcounted_reclaimed_and_never_reuses_keys() {
    use nostr::EventBuilder;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-refcounts.redb");
    let mut store = RedbStore::open(&path).unwrap();
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://shared-relay.example").unwrap();
    let make_event = |created_at| {
        EventBuilder::new(Kind::TextNote, format!("event-{created_at}"))
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(&keys)
            .unwrap()
    };
    let first = make_event(1);
    let second = make_event(2);
    store
        .insert(
            first.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(10u64)),
        )
        .unwrap();
    store
        .insert(
            second.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(20u64)),
        )
        .unwrap();

    let first_relay_key = {
        let read_txn = store.raw_database().begin_read().unwrap();
        let relay_ids = read_txn.open_table(RELAY_IDS).unwrap();
        let relay_key = relay_ids.get(relay.as_str()).unwrap().unwrap().value();
        drop(read_txn);
        assert_eq!(relay_refs_of(store.raw_database(), relay_key), Some(2));
        relay_key
    };
    assert_canonical_integrity(store.raw_database());

    store.remove(first.id, RetractReason::Deleted).unwrap();
    assert_eq!(
        relay_refs_of(store.raw_database(), first_relay_key),
        Some(1)
    );
    assert_canonical_integrity(store.raw_database());

    store.remove(second.id, RetractReason::Deleted).unwrap();
    {
        let read_txn = store.raw_database().begin_read().unwrap();
        assert!(read_txn
            .open_table(RELAY_IDS)
            .unwrap()
            .get(relay.as_str())
            .unwrap()
            .is_none());
        assert_eq!(read_txn.open_table(RELAYS).unwrap().len().unwrap(), 0);
        // Every remaining canonical key is a note row or a local sidecar:
        // the observation column is empty.
        assert!(read_txn
            .open_table(EVENTS)
            .unwrap()
            .iter()
            .unwrap()
            .all(|entry| entry.unwrap().0.value()[8] != EVENT_COL_OBSERVATION));
    }
    assert_canonical_integrity(store.raw_database());

    let third = make_event(3);
    store
        .insert(
            third,
            RelayObserved::new(relay.clone(), Timestamp::from(30u64)),
        )
        .unwrap();
    let read_txn = store.raw_database().begin_read().unwrap();
    let new_relay_key = read_txn
        .open_table(RELAY_IDS)
        .unwrap()
        .get(relay.as_str())
        .unwrap()
        .unwrap()
        .value();
    assert!(new_relay_key > first_relay_key);
}

#[test]
fn canonical_relay_aliases_fold_to_latest_timestamp_on_read_and_mutation() {
    use nostr::EventBuilder;

    const CANONICAL_RELAY: &str = "wss://relay-alias.example/";
    const STORED_ALIAS: &str = "wss://relay-alias.example";

    let canonical_relay = RelayUrl::parse(CANONICAL_RELAY).unwrap();
    let parsed_alias = RelayUrl::parse(STORED_ALIAS).unwrap();
    assert_eq!(
        parsed_alias, canonical_relay,
        "fixture spellings must parse to one typed relay identity"
    );
    assert_ne!(
        STORED_ALIAS,
        canonical_relay.as_str(),
        "fixture must persist byte-distinct relay dictionary rows"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("canonical-relay-alias.redb");
    let keys = nostr::Keys::generate();
    let event = EventBuilder::new(Kind::TextNote, "canonical relay alias")
        .custom_created_at(Timestamp::from(1u64))
        .sign_with_keys(&keys)
        .unwrap();

    let mut store = RedbStore::open(&path).unwrap();
    store
        .insert(
            event.clone(),
            RelayObserved::new(canonical_relay.clone(), Timestamp::from(10u64)),
        )
        .unwrap();
    drop(store);

    // Reproduce a durable dictionary created under a different URL
    // normalization spelling: two numeric relay keys, two byte-distinct URL
    // rows, and two observations for one event, but one typed RelayUrl.
    let db = Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    {
        let event_ids = write_txn.open_table(EVENT_IDS).unwrap();
        let event_key = event_ids.get(event.id.as_bytes()).unwrap().unwrap().value();
        let mut relays = write_txn.open_table(RELAYS).unwrap();
        let mut relay_ids = write_txn.open_table(RELAY_IDS).unwrap();
        let canonical_key = relay_ids
            .get(canonical_relay.as_str())
            .unwrap()
            .unwrap()
            .value();
        let alias_key = canonical_key + 1;
        let mut store_meta = write_txn.open_table(STORE_META).unwrap();
        let mut events = write_txn.open_table(EVENTS).unwrap();

        relays
            .insert(alias_key, encode_relay_row(1, STORED_ALIAS).as_slice())
            .unwrap();
        relay_ids.insert(STORED_ALIAS, alias_key).unwrap();
        events
            .insert(
                observation_key(event_key, alias_key).as_slice(),
                20u64.to_be_bytes().as_slice(),
            )
            .unwrap();
        store_meta
            .insert(NEXT_RELAY_KEY, u64::from(alias_key + 1))
            .unwrap();
    }
    write_txn.commit().unwrap();
    drop(db);

    let mut reopened = RedbStore::open(&path).unwrap();

    // The ordinary query path materializes provenance through
    // RedbStore::read_provenance.
    let rows = reopened.query(&Filter::new().id(event.id)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].provenance.seen,
        BTreeMap::from([(canonical_relay.clone(), Timestamp::from(20u64))])
    );

    // Duplicate ingest first loads the canonical row through
    // CanonicalWriteTables::load_seen, then grows the retained seen-at fact.
    let outcome = reopened
        .insert(
            event,
            RelayObserved::new(canonical_relay.clone(), Timestamp::from(30u64)),
        )
        .unwrap();
    assert!(matches!(
        outcome,
        InsertOutcome::Duplicate {
            provenance_grew: true,
            ..
        }
    ));
    let rows = reopened.query(&Filter::new()).unwrap();
    assert_eq!(
        rows[0].provenance.seen,
        BTreeMap::from([(canonical_relay, Timestamp::from(30u64))])
    );
}

#[test]
fn batch_relay_refcounts_flush_once_per_distinct_relay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-refcount-batch.redb");
    let store = RedbStore::open(&path).unwrap();
    let relay = RelayUrl::parse("wss://one-hot-refcount.example").unwrap();
    let write_txn = store.raw_database().begin_write().unwrap();
    {
        let mut canonical = CanonicalWriteTables::open(&write_txn).unwrap();
        let relay_key = canonical.intern_relay(&relay).unwrap();
        for _ in 0..1_114 {
            canonical.increment_relay_ref(relay_key).unwrap();
        }
        assert_eq!(canonical.relay_ref_counts.len(), 1);
        assert_eq!(canonical.relay_ref_counts[&relay_key], 1_114);
        let durable_refs = |canonical: &CanonicalWriteTables<'_>| {
            let row = canonical.relays.get(relay_key).unwrap().unwrap();
            decode_relay_row(relay_key, row.value()).unwrap().0
        };
        assert_eq!(
            durable_refs(&canonical),
            0,
            "the durable hot row stays untouched until the batch flush"
        );
        canonical.flush_pending().unwrap();
        assert!(canonical.relay_ref_counts.is_empty());
        assert_eq!(durable_refs(&canonical), 1_114);
    }
    // This is a white-box write-coalescing proof, not a valid canonical
    // store state, so abort rather than committing the synthetic count.
    write_txn.abort().unwrap();
}

#[test]
fn batch_net_zero_observation_reclaims_new_relay_dictionary_row() {
    use nostr::EventBuilder;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-refcount-net-zero.redb");
    let mut store = RedbStore::open(&path).unwrap();
    let keys = nostr::Keys::generate();
    let old = EventBuilder::new(Kind::ContactList, "old")
        .custom_created_at(Timestamp::from(1u64))
        .sign_with_keys(&keys)
        .unwrap();
    let new = EventBuilder::new(Kind::ContactList, "new")
        .custom_created_at(Timestamp::from(2u64))
        .sign_with_keys(&keys)
        .unwrap();
    let old_relay = RelayUrl::parse("wss://superseded-in-batch.example").unwrap();
    let new_relay = RelayUrl::parse("wss://winner-in-batch.example").unwrap();

    let outcomes = store
        .insert_batch(vec![
            (
                old,
                RelayObserved::new(old_relay.clone(), Timestamp::from(1u64)),
            ),
            (
                new,
                RelayObserved::new(new_relay.clone(), Timestamp::from(2u64)),
            ),
        ])
        .unwrap();
    assert!(matches!(outcomes[0], InsertOutcome::Inserted));
    assert!(matches!(outcomes[1], InsertOutcome::Superseded { .. }));
    assert_canonical_integrity(store.raw_database());

    let read_txn = store.raw_database().begin_read().unwrap();
    let relay_ids = read_txn.open_table(RELAY_IDS).unwrap();
    assert!(relay_ids.get(old_relay.as_str()).unwrap().is_none());
    let winner_key = relay_ids.get(new_relay.as_str()).unwrap().unwrap().value();
    drop(read_txn);
    assert_eq!(relay_refs_of(store.raw_database(), winner_key), Some(1));
}

#[test]
fn later_same_relay_updates_only_one_timestamp_value() {
    use nostr::EventBuilder;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-timestamp.redb");
    let mut store = RedbStore::open(&path).unwrap();
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://timestamp-relay.example").unwrap();
    let event = EventBuilder::new(Kind::TextNote, "timestamp")
        .custom_created_at(Timestamp::from(1u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(
            event.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(10u64)),
        )
        .unwrap();
    let canonical_before = raw_canonical_row(&store, event.id);
    let before = raw_observation_rows(&store, canonical_before.0);

    let outcome = store
        .insert(
            event.clone(),
            RelayObserved::new(relay, Timestamp::from(20u64)),
        )
        .unwrap();
    assert!(matches!(
        outcome,
        InsertOutcome::Duplicate {
            provenance_grew: true,
            ..
        }
    ));
    assert_eq!(raw_canonical_row(&store, event.id), canonical_before);
    let after = raw_observation_rows(&store, canonical_before.0);
    assert_eq!(before.len(), 1);
    assert_eq!(after.len(), 1);
    assert_eq!(before[0].0, after[0].0);
    assert_eq!(before[0].1, 10);
    assert_eq!(after[0].1, 20);
    let read_txn = store.raw_database().begin_read().unwrap();
    let relays = read_txn.open_table(RELAYS).unwrap();
    let (relay_key, row) = {
        let entry = relays.iter().unwrap().next().unwrap().unwrap();
        (entry.0.value(), entry.1.value().to_vec())
    };
    assert_eq!(decode_relay_row(relay_key, &row).unwrap().0, 1);
}

#[test]
fn surrogate_keys_are_monotonic_and_never_reused_after_remove_or_reopen() {
    use nostr::EventBuilder;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("surrogate-keys.redb");
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://surrogates.example").unwrap();
    let make_event = |created_at| {
        EventBuilder::new(Kind::TextNote, format!("event-{created_at}"))
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(&keys)
            .unwrap()
    };

    let first = make_event(1);
    let second = make_event(2);
    let third = make_event(3);
    let mut store = RedbStore::open(&path).unwrap();
    store
        .insert(
            first.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(10u64)),
        )
        .unwrap();
    let first_key = raw_canonical_row(&store, first.id).0;
    store.remove(first.id, RetractReason::Deleted).unwrap();
    store
        .insert(
            second.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(20u64)),
        )
        .unwrap();
    let second_key = raw_canonical_row(&store, second.id).0;
    assert!(second_key > first_key);

    drop(store);
    let mut reopened = RedbStore::open(&path).unwrap();
    reopened
        .insert(
            third.clone(),
            RelayObserved::new(relay, Timestamp::from(30u64)),
        )
        .unwrap();
    let third_key = raw_canonical_row(&reopened, third.id).0;
    assert!(third_key > second_key);
    assert_canonical_integrity(reopened.raw_database());
}

#[test]
fn canonical_integrity_survives_every_governed_event_mutation_class() {
    use nostr::{EventBuilder, Tag};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("governed-integrity.redb");
    let mut store = RedbStore::open(&path).unwrap();
    let keys = nostr::Keys::generate();
    let relay1 = RelayUrl::parse("wss://integrity-one.example").unwrap();
    let relay2 = RelayUrl::parse("wss://integrity-two.example").unwrap();
    let observed = |relay: RelayUrl, at| RelayObserved::new(relay, Timestamp::from(at));

    let target = EventBuilder::new(Kind::TextNote, "target")
        .custom_created_at(Timestamp::from(10u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(target.clone(), observed(relay1.clone(), 10))
        .unwrap();
    store
        .insert(target.clone(), observed(relay2.clone(), 11))
        .unwrap();
    assert_canonical_integrity(store.raw_database());

    let replaceable_old = EventBuilder::new(Kind::ContactList, "old")
        .custom_created_at(Timestamp::from(20u64))
        .sign_with_keys(&keys)
        .unwrap();
    let replaceable_new = EventBuilder::new(Kind::ContactList, "new")
        .custom_created_at(Timestamp::from(30u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(replaceable_old, observed(relay1.clone(), 20))
        .unwrap();
    store
        .insert(replaceable_new, observed(relay1.clone(), 30))
        .unwrap();
    assert_canonical_integrity(store.raw_database());

    let deletion = EventBuilder::new(Kind::EventDeletion, "")
        .tag(Tag::event(target.id))
        .custom_created_at(Timestamp::from(40u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(deletion, observed(relay1.clone(), 40))
        .unwrap();
    assert_canonical_integrity(store.raw_database());

    let expiring = EventBuilder::new(Kind::TextNote, "expiring")
        .tag(Tag::expiration(Timestamp::from(60u64)))
        .custom_created_at(Timestamp::from(50u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(expiring, observed(relay1.clone(), 50))
        .unwrap();
    store.expire_due(Timestamp::from(60u64)).unwrap();
    assert_canonical_integrity(store.raw_database());

    let gc_candidate = EventBuilder::new(Kind::TextNote, "gc")
        .custom_created_at(Timestamp::from(70u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(gc_candidate, observed(relay1.clone(), 70))
        .unwrap();
    store.gc(&GcRetentionSet::new(Vec::new())).unwrap();
    assert_canonical_integrity(store.raw_database());

    let signed = EventBuilder::new(Kind::TextNote, "pending")
        .custom_created_at(Timestamp::from(80u64))
        .sign_with_keys(&keys)
        .unwrap();
    let frozen = Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        crate::sentinel_signature(),
    );
    let accepted = store
        .accept_write(AcceptWrite {
            payload: crate::AcceptWritePayload::Event {
                frozen: Box::new(frozen),
                routing: "integrity".into(),
                sig_state: IntentSigState::Pending,
            },
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "integrity".into(),
            accepted_at: Timestamp::from(80u64),
        })
        .unwrap();
    assert_canonical_integrity(store.raw_database());
    store
        .compensate_write(accepted.journaled_intent_id().unwrap())
        .unwrap();
    assert_canonical_integrity(store.raw_database());
}

#[test]
fn query_by_single_letter_tag_decodes_only_that_tag_bucket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tag-index.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://groups.example").unwrap();

    for i in 0..12u64 {
        store
            .insert(
                room_event(&keys, "target", 1_000 + i, &format!("target-{i}")),
                RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
            )
            .unwrap();
    }
    for i in 0..200u64 {
        store
            .insert(
                room_event(&keys, "noise", 3_000 + i, &format!("noise-{i}")),
                RelayObserved::new(relay.clone(), Timestamp::from(4_000 + i)),
            )
            .unwrap();
    }

    let filter = Filter::new()
        .kind(Kind::from(9u16))
        .custom_tag(SingleLetterTag::lowercase(nostr::Alphabet::H), "target");
    let before = store.examined_rows();
    let rows = store.query(&filter).unwrap();
    let examined = store.examined_rows() - before;
    assert_eq!(rows.len(), 12);
    assert_eq!(examined, 12, "noise-room rows must never be decoded");
}

#[test]
fn query_newest_tag_scan_stops_before_decoding_past_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tag-limit.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://groups.example").unwrap();

    for i in 0..240u64 {
        store
            .insert(
                room_event(&keys, "target", 1_000 + i, &format!("target-{i}")),
                RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
            )
            .unwrap();
    }

    let filter = Filter::new()
        .kind(Kind::from(9u16))
        .custom_tag(SingleLetterTag::lowercase(nostr::Alphabet::H), "target");
    let before = store.examined_rows();
    let rows = store.query_newest(&filter, 25).unwrap();
    let examined = store.examined_rows() - before;

    assert_eq!(rows.len(), 25);
    assert_eq!(examined, 25, "rows past the top-N must not be decoded");
    assert!(rows
        .windows(2)
        .all(|pair| pair[0].event.created_at >= pair[1].event.created_at));
    assert_eq!(rows[0].event.created_at, Timestamp::from(1_239u64));
    assert_eq!(rows[24].event.created_at, Timestamp::from(1_215u64));
}

#[test]
fn query_newest_postfilters_binary_views_before_event_materialization() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("binary-postfilter.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let wanted = nostr::Keys::generate();
    let noise = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://groups.example").unwrap();

    store
        .insert(
            room_event(&wanted, "target", 1_000, "wanted"),
            RelayObserved::new(relay.clone(), Timestamp::from(2_000u64)),
        )
        .unwrap();
    for i in 0..200u64 {
        store
            .insert(
                room_event(&noise, "target", 2_000 + i, &format!("noise-{i}")),
                RelayObserved::new(relay.clone(), Timestamp::from(3_000 + i)),
            )
            .unwrap();
    }

    let filter = Filter::new().kind(Kind::from(9u16)).search("wanted");
    let before = store.examined_rows();
    let rows = store.query_newest(&filter, 1).unwrap();
    let materialized = store.examined_rows() - before;

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event.pubkey, wanted.public_key());
    assert_eq!(
            materialized, 1,
            "200 newer kind-index candidates rejected by search must stay borrowed binary views; only the returned row becomes an owned Event"
        );
}

#[test]
fn query_newest_kind_and_global_scans_stop_at_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ordered-limit.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://groups.example").unwrap();

    for i in 0..240u64 {
        store
            .insert(
                room_event(&keys, "target", 1_000 + i, &format!("event-{i}")),
                RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
            )
            .unwrap();
    }

    let before = store.examined_rows();
    let kind_rows = store
        .query_newest(&Filter::new().kind(Kind::from(9u16)), 25)
        .unwrap();
    assert_eq!(kind_rows.len(), 25);
    assert_eq!(store.examined_rows() - before, 25);
    assert_eq!(kind_rows[0].event.created_at, Timestamp::from(1_239u64));

    let before = store.examined_rows();
    let global_rows = store.query_newest(&Filter::new(), 17).unwrap();
    assert_eq!(global_rows.len(), 17);
    assert_eq!(store.examined_rows() - before, 17);
    assert_eq!(global_rows[0].event.created_at, Timestamp::from(1_239u64));
}

#[test]
fn query_newest_ids_projects_covered_filters_without_event_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = RedbStore::open(dir.path().join("projected-ids.redb")).unwrap();
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://projected.example").unwrap();

    for i in 0..40u64 {
        store
            .insert(
                room_event(&keys, "target", 1_000 + i, &"x".repeat(64 * 1024)),
                RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
            )
            .unwrap();
    }

    let filter = Filter::new().kind(Kind::from(9u16));
    let expected: Vec<_> = store
        .query_newest(&filter, 25)
        .unwrap()
        .into_iter()
        .map(|row| row.event.id)
        .collect();
    store.reset_query_work();
    let projected = store.query_newest_ids(&filter, 25).unwrap();

    assert_eq!(projected, expected);
    let (index_rows, event_values, materialized) = store.query_work();
    assert_eq!(
        (event_values, materialized),
        (0, 0),
        "an index-covered ID projection must not read or own 64 KiB event values"
    );
    assert!(
        index_rows <= 32,
        "packed merge may decode only the bounded active-run heads plus the requested rows"
    );
}

#[test]
fn query_newest_ids_postfilters_borrowed_values_without_materializing_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = RedbStore::open(dir.path().join("projected-postfilter.redb")).unwrap();
    let wanted = nostr::Keys::generate();
    let noise = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://projected.example").unwrap();

    let wanted = room_event(&wanted, "target", 1_000, "wanted");
    store
        .insert(
            wanted.clone(),
            RelayObserved::new(relay.clone(), Timestamp::from(2_000u64)),
        )
        .unwrap();
    for i in 0..20u64 {
        store
            .insert(
                room_event(&noise, "target", 2_000 + i, &format!("noise-{i}")),
                RelayObserved::new(relay.clone(), Timestamp::from(3_000 + i)),
            )
            .unwrap();
    }

    store.reset_query_work();
    let ids = store
        .query_newest_ids(&Filter::new().kind(Kind::from(9u16)).search("wanted"), 1)
        .unwrap();

    assert_eq!(ids, vec![wanted.id]);
    let (index_rows, event_values, materialized) = store.query_work();
    assert_eq!(index_rows, 21);
    assert_eq!(event_values, 21);
    assert_eq!(materialized, 0);
}

#[test]
fn query_newest_ids_preserves_provisional_suppression() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = RedbStore::open(dir.path().join("projected-suppression.redb")).unwrap();
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://projected.example").unwrap();
    let visible = room_event(&keys, "target", 1_000, "visible");
    let hidden = room_event(&keys, "target", 2_000, "hidden");
    for event in [visible.clone(), hidden.clone()] {
        store
            .insert(
                event,
                RelayObserved::new(relay.clone(), Timestamp::from(3_000u64)),
            )
            .unwrap();
    }
    let claim_key = publish_queue_codec::id_claim_key(&hidden.id, &hidden.pubkey);
    let write_txn = store.raw_database().begin_write().unwrap();
    {
        let mut claims = write_txn.open_table(PUBLISH_QUEUE_SUPPRESS).unwrap();
        add_claimant_in_txn(&mut claims, &claim_key, IntentId(1)).unwrap();
    }
    write_txn.commit().unwrap();

    let filter = Filter::new().kind(Kind::from(9u16));
    let expected: Vec<_> = store
        .query_newest(&filter, 2)
        .unwrap()
        .into_iter()
        .map(|row| row.event.id)
        .collect();
    store.reset_query_work();
    let projected = store.query_newest_ids(&filter, 2).unwrap();

    assert_eq!(expected, vec![visible.id]);
    assert_eq!(projected, expected);
    assert_eq!(store.query_work(), (2, 2, 2));
}

/// #1248: `publish_queue_suppress_by_id` and `publish_queue_suppress_by_addr`
/// folded into one `PUBLISH_QUEUE_SUPPRESS` tree, discriminant-tagged
/// (`PUBLISH_QUEUE_SUPPRESS_ID`/`PUBLISH_QUEUE_SUPPRESS_ADDR`) the same way
/// [`TOMBSTONES`] already folded its own id/addr pair. This proves the
/// merged representation round-trips exactly what the two former tables
/// held: an id claim and an addr claim staged together, in the SAME tree,
/// survive a REAL `redb` reopen (not just the same open handle) with no
/// cross-namespace interference, and the raw tree holds exactly the two rows
/// under their own tags -- not four, not one merged row.
#[test]
fn merged_suppress_table_round_trips_both_former_tables_across_a_real_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("merged-suppress.redb");
    let id_keys = nostr::Keys::generate();
    let addr_keys = nostr::Keys::generate();
    let id_target = room_event(&id_keys, "id-target", 1_000, "id claim target");
    let id_claim_key = publish_queue_codec::id_claim_key(&id_target.id, &id_keys.public_key());
    let addr_key = crate::address_key::AddressKey::Addressable(
        addr_keys.public_key(),
        Kind::from(30_023u16),
        "d".to_string(),
    );
    let addr_key_bytes = addr_key.to_redb_key().into_bytes();

    {
        let store = RedbStore::open(&path).unwrap();
        let write_txn = store.raw_database().begin_write().unwrap();
        {
            let mut suppress = write_txn.open_table(PUBLISH_QUEUE_SUPPRESS).unwrap();
            add_claimant_in_txn(&mut suppress, &id_claim_key, IntentId(11)).unwrap();
            add_addr_claimant_in_txn(
                &mut suppress,
                &addr_key_bytes,
                IntentId(22),
                Timestamp::from(5_000u64),
            )
            .unwrap();
        }
        write_txn.commit().unwrap();
    }
    // A real reopen, not the same handle -- proves durable bytes, not
    // in-process state.
    let store = RedbStore::open(&path).unwrap();

    let read_txn = store.raw_database().begin_read().unwrap();
    let suppress = read_txn.open_table(PUBLISH_QUEUE_SUPPRESS).unwrap();

    // Both claims are independently readable under their own tag.
    assert!(
        has_claimants_in_txn(&suppress, &id_claim_key).unwrap(),
        "the id claim must survive the merge and the reopen"
    );
    assert!(
        addr_has_covering_claimant_in_txn(&suppress, &addr_key_bytes, Timestamp::from(5_000u64))
            .unwrap(),
        "the addr claim must survive the merge and the reopen"
    );
    // An id-tagged lookup must never see the addr row and vice versa -- the
    // discriminant, not accident of key width, is what separates them.
    assert!(
        !addr_has_covering_claimant_in_txn(
            &suppress,
            &id_target.id.as_bytes()[..],
            Timestamp::from(5_000u64)
        )
        .unwrap(),
        "an id-shaped key must not resolve under the addr tag"
    );

    // The raw tree holds EXACTLY the two rows staged, under the expected
    // tags -- not the four a naive concatenation could produce, not one row
    // colliding the two claims together.
    let mut rows: Vec<(u8, usize)> = suppress
        .iter()
        .unwrap()
        .map(|entry| {
            let (key, _) = entry.unwrap();
            let key = key.value().to_vec();
            (key[0], key.len() - 1)
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            (PUBLISH_QUEUE_SUPPRESS_ID, 64),
            (PUBLISH_QUEUE_SUPPRESS_ADDR, addr_key_bytes.len()),
        ],
        "the merged tree must hold exactly the id row and the addr row, each under its own tag"
    );
}

#[test]
fn query_newest_ids_fails_closed_on_stale_ordered_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = RedbStore::open(dir.path().join("projected-corruption.redb")).unwrap();
    let keys = nostr::Keys::generate();
    let event = room_event(&keys, "target", 1_000, "event");
    store
        .insert(
            event.clone(),
            RelayObserved::new(
                RelayUrl::parse("wss://projected.example").unwrap(),
                Timestamp::from(2_000u64),
            ),
        )
        .unwrap();
    let write_txn = store.raw_database().begin_write().unwrap();
    {
        let mut event_ids = write_txn.open_table(EVENT_IDS).unwrap();
        event_ids.remove(event.id.as_bytes()).unwrap();
    }
    write_txn.commit().unwrap();

    let error = store
        .query_newest_ids(&Filter::new().kind(Kind::from(9u16)), 1)
        .unwrap_err();
    assert!(error.message().contains("canonical id map"));
}

#[test]
fn query_newest_merges_multiple_tag_values_in_global_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tag-merge.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://groups.example").unwrap();

    for (room, created_at) in [("a", 100), ("b", 104), ("a", 103), ("b", 101)] {
        store
            .insert(
                room_event(&keys, room, created_at, room),
                RelayObserved::new(relay.clone(), Timestamp::from(created_at + 1)),
            )
            .unwrap();
    }

    let filter =
        Filter::new().custom_tags(SingleLetterTag::lowercase(nostr::Alphabet::H), ["a", "b"]);
    let rows = store.query_newest(&filter, 3).unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.event.created_at.as_secs())
            .collect::<Vec<_>>(),
        vec![104, 103, 101]
    );
}

#[test]
fn query_newest_tag_scan_uses_id_ascending_tie_break() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tag-tie-break.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://groups.example").unwrap();
    let mut expected = Vec::new();

    for i in 0..8u64 {
        let event = room_event(&keys, "target", 1_000, &format!("target-{i}"));
        expected.push(event.id);
        store
            .insert(
                event,
                RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
            )
            .unwrap();
    }
    expected.sort();

    let filter = Filter::new().custom_tag(SingleLetterTag::lowercase(nostr::Alphabet::H), "target");
    let rows = store.query_newest(&filter, 3).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.event.id).collect::<Vec<_>>(),
        expected[..3]
    );
}

#[test]
fn query_newest_before_starts_after_exact_same_second_key_in_page_work() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tag-cursor-work.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://groups.example").unwrap();
    let created_at = Timestamp::from(1_000u64);
    let mut expected = Vec::new();

    for i in 0..240u64 {
        let event = room_event(
            &keys,
            "target",
            created_at.as_secs(),
            &format!("target-{i}"),
        );
        expected.push(event.id);
        store
            .insert(
                event,
                RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
            )
            .unwrap();
    }
    expected.sort();

    let filter = Filter::new().custom_tag(SingleLetterTag::lowercase(nostr::Alphabet::H), "target");
    let before = EventCursor::new(created_at, expected[119]);
    store.reset_query_work();
    let rows = store.query_newest_before(&filter, before, 10).unwrap();

    assert_eq!(
        rows.iter().map(|row| row.event.id).collect::<Vec<_>>(),
        expected[120..130]
    );
    let (index_rows, event_values, materialized) = store.query_work();
    assert_eq!((event_values, materialized), (10, 10));
    assert!(
        index_rows <= 20,
        concat!(
            "each packed run must seek directly after the exact cursor; only bounded ",
            "active-run heads may be decoded in addition to the requested rows"
        )
    );
}

#[test]
fn union_replacement_page_work_is_bounded_per_root_and_deduplicated_globally() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("union-replacement-work.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let author_a = nostr::Keys::generate();
    let author_b = nostr::Keys::generate();
    let noise = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://union-work.example").unwrap();
    let mut newest_a = Vec::new();

    for index in 0..64u64 {
        let event = room_event(&author_a, "a", 3_000 - index, &format!("author-a-{index}"));
        newest_a.push(event.id);
        store
            .insert(
                event,
                RelayObserved::new(relay.clone(), Timestamp::from(10_000 + index)),
            )
            .unwrap();
        store
            .insert(
                room_event(&author_b, "b", 2_000 - index, &format!("author-b-{index}")),
                RelayObserved::new(relay.clone(), Timestamp::from(11_000 + index)),
            )
            .unwrap();
    }
    for index in 0..256u64 {
        store
            .insert(
                room_event(&noise, "noise", 1_000 - index, &format!("noise-{index}")),
                RelayObserved::new(relay.clone(), Timestamp::from(12_000 + index)),
            )
            .unwrap();
    }

    let room_kind = Kind::from(9u16);
    let filters = vec![
        Filter::new().kind(room_kind).author(author_a.public_key()),
        Filter::new().kind(room_kind).author(author_b.public_key()),
        // This root overlaps both author roots and the large noise set.
        // Its first three rows are the same rows returned by author A.
        Filter::new().kind(room_kind),
    ];
    let before = EventCursor::new(Timestamp::from(4_000u64), newest_a[0]);
    store.reset_query_work();
    let rows = store.query_newest_before_any(&filters, before, 3).unwrap();

    assert_eq!(
        rows.iter().map(|row| row.event.id).collect::<Vec<_>>(),
        newest_a[..3],
        "overlapping roots must merge into one canonical de-duplicated page"
    );
    let (index_rows, event_values, materialized) = store.query_work();
    assert_eq!((event_values, materialized), (9, 9));
    assert!(
        index_rows <= 24,
        concat!(
            "three packed roots may decode bounded active-run heads plus nine rows; ",
            "the 384-row store cannot turn union replacement into a full scan"
        )
    );
}

#[test]
fn strict_ordered_scan_stops_after_requested_eligible_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("strict-provenance-work.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let keys = nostr::Keys::generate();
    let wanted = RelayUrl::parse("wss://wanted.example").unwrap();
    let other = RelayUrl::parse("wss://other.example").unwrap();

    for index in 0..20u64 {
        store
            .insert(
                room_event(
                    &keys,
                    "target",
                    2_000 - index,
                    &format!("ineligible-{index}"),
                ),
                RelayObserved::new(other.clone(), Timestamp::from(3_000 + index)),
            )
            .unwrap();
    }
    for index in 0..3u64 {
        store
            .insert(
                room_event(&keys, "target", 1_000 - index, &format!("eligible-{index}")),
                RelayObserved::new(wanted.clone(), Timestamp::from(4_000 + index)),
            )
            .unwrap();
    }

    let filter = Filter::new().custom_tag(SingleLetterTag::lowercase(nostr::Alphabet::H), "target");
    let eligible = BTreeSet::from([wanted]);
    store.reset_query_work();
    let rows = store.query_newest_under_pin(&filter, &eligible, 3).unwrap();

    assert_eq!(
        rows.iter()
            .map(|row| row.event.created_at.as_secs())
            .collect::<Vec<_>>(),
        vec![1_000, 999, 998]
    );
    assert_eq!(
        store.query_work(),
        (23, 3, 3),
        concat!(
            "the ordered index may inspect the twenty newer ineligible keys, ",
            "but event decoding/provenance materialization stops at the three eligible rows"
        )
    );
}

#[test]
fn packed_postings_use_inclusive_equal_time_ranges_and_id_ascending_ties() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fixed-index-tie-break.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://groups.example").unwrap();
    let created_at = Timestamp::from(1_000u64);
    let kind = Kind::from(9u16);
    let mut expected = Vec::new();

    for i in 0..8u64 {
        let event = room_event(&keys, "target", created_at.as_secs(), &format!("event-{i}"));
        expected.push(event.id);
        store
            .insert(
                event,
                RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
            )
            .unwrap();
    }
    expected.sort();

    let filters = [
        (
            Filter::new().since(created_at).until(created_at),
            OrderedIndex::Global,
        ),
        (
            Filter::new()
                .author(keys.public_key())
                .since(created_at)
                .until(created_at),
            OrderedIndex::Author,
        ),
        (
            Filter::new().kind(kind).since(created_at).until(created_at),
            OrderedIndex::Kind,
        ),
        (
            Filter::new()
                .author(keys.public_key())
                .kind(kind)
                .since(created_at)
                .until(created_at),
            OrderedIndex::Author,
        ),
    ];

    for (filter, expected_index) in filters {
        let read_txn = store.raw_database().begin_read().unwrap();
        let plan = plan_ordered_query(&filter);
        assert_eq!(plan.index, expected_index);
        drop(read_txn);

        let rows = store.query_newest(&filter, expected.len()).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.event.id).collect::<Vec<_>>(),
            expected,
            "{expected_index:?} did not preserve canonical equal-time ordering"
        );
    }
}

/// Choosing a different ordered index cannot change which rows a query
/// returns — only how fast it gets to them.
///
/// This is the behavioural claim NMP relies on after deleting the durable
/// `index_cardinality` estimate (#1248): the planner now picks by fixed
/// priority instead of by a sampled row count, so the *plan* changed and the
/// *results* must not have. It holds structurally because the post-index
/// residual mask is derived from the chosen index
/// (`plan.index.matched()` feeding `matches_prepared_filter_after_index`),
/// never from any estimate — so every predicate the walked index did not
/// enforce is still applied afterwards.
///
/// The fixture is deliberately adversarial for the mask: a 100-row `#h`
/// bucket, a 5-row `#p` subset inside it, and one event carrying the rare
/// `#p` with the WRONG `#h`. Scanning by `#p` must still reject that event
/// on `#h`, and scanning by `#h` must still reject the 95 events without
/// `#p`.
#[test]
fn plan_choice_cannot_change_query_results() {
    use nostr::{Alphabet, EventBuilder, Tag};

    let dir = tempfile::tempdir().unwrap();
    let mut store = RedbStore::open(dir.path().join("plan-independence.redb")).unwrap();
    let keys = nostr::Keys::new(nostr::SecretKey::from_slice(&[1; 32]).unwrap());
    let member = nostr::Keys::new(nostr::SecretKey::from_slice(&[2; 32]).unwrap())
        .public_key()
        .to_hex();
    let relay = RelayUrl::parse("wss://plan-independence.example").unwrap();
    let h = SingleLetterTag::lowercase(Alphabet::H);
    let p = SingleLetterTag::lowercase(Alphabet::P);

    for i in 0..100u64 {
        let mut builder = EventBuilder::new(Kind::from(9u16), format!("room-{i}"))
            .tag(Tag::parse(["h", "busy-room"]).unwrap());
        if i < 5 {
            builder = builder.tag(Tag::parse(["p", member.as_str()]).unwrap());
        }
        let event = builder
            .custom_created_at(Timestamp::from(1_000 + i))
            .sign_with_keys(&keys)
            .unwrap();
        store
            .insert(
                event,
                RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i)),
            )
            .unwrap();
    }
    // Same rare #p but the wrong #h: proves the chosen-tag matched mask
    // skips only the chosen tag, not every tag predicate.
    let wrong_room = EventBuilder::new(Kind::from(9u16), "wrong-room")
        .tags([
            Tag::parse(["h", "other-room"]).unwrap(),
            Tag::parse(["p", member.as_str()]).unwrap(),
        ])
        .custom_created_at(Timestamp::from(2_000u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(
            wrong_room,
            RelayObserved::new(relay, Timestamp::from(3_000u64)),
        )
        .unwrap();

    let filter = Filter::new()
        .kind(Kind::from(9u16))
        .custom_tag(h, "busy-room")
        .custom_tag(p, member);

    // Fixed priority: tags outrank kinds, and `#h` sorts before `#p`.
    assert_eq!(plan_ordered_query(&filter).index, OrderedIndex::Tag(h));

    let candidates = candidate_ordered_plans(&filter);
    assert_eq!(
        candidates.iter().map(|plan| plan.index).collect::<Vec<_>>(),
        vec![
            OrderedIndex::Tag(h),
            OrderedIndex::Tag(p),
            OrderedIndex::Kind,
            OrderedIndex::Global,
        ],
        "every index that can answer this filter is a candidate"
    );

    let read_txn = store.raw_database().begin_read().unwrap();
    for plan in &candidates {
        let complete: BTreeSet<_> = store
            .query_ordered(&read_txn, plan, &filter, None, None, None)
            .unwrap()
            .into_iter()
            .map(|row| row.event.id)
            .collect();
        assert_eq!(
            complete.len(),
            5,
            "{:?} changed the complete result",
            plan.index
        );
        let bounded: Vec<_> = store
            .query_ordered(&read_txn, plan, &filter, None, Some(3), None)
            .unwrap()
            .into_iter()
            .map(|row| row.event.id)
            .collect();
        let projected = store
            .query_ordered_ids(&read_txn, plan, &filter, 3)
            .unwrap();
        assert_eq!(bounded, projected, "{:?} projected differently", plan.index);
        if plan.index == candidates[0].index {
            continue;
        }
        let first_complete: BTreeSet<_> = store
            .query_ordered(&read_txn, &candidates[0], &filter, None, None, None)
            .unwrap()
            .into_iter()
            .map(|row| row.event.id)
            .collect();
        let first_bounded: Vec<_> = store
            .query_ordered(&read_txn, &candidates[0], &filter, None, Some(3), None)
            .unwrap()
            .into_iter()
            .map(|row| row.event.id)
            .collect();
        assert_eq!(complete, first_complete, "{:?} changed results", plan.index);
        assert_eq!(bounded, first_bounded, "{:?} changed order", plan.index);
    }
    drop(read_txn);
    assert_canonical_integrity(store.raw_database());
}

#[test]
fn ordered_plan_uses_one_dimension_for_author_kind_products() {
    let dir = tempfile::tempdir().unwrap();
    let store = RedbStore::open(dir.path().join("bounded-composite-plan.redb")).unwrap();
    let authors: BTreeSet<_> = (0..65)
        .map(|_| nostr::Keys::generate().public_key())
        .collect();
    let kinds: BTreeSet<_> = (0..65u16).map(Kind::from).collect();
    let filter = Filter::new().authors(authors).kinds(kinds);
    let plan = plan_ordered_query(&filter);
    assert_eq!(plan.index, OrderedIndex::Author);
    assert_eq!(plan.prefixes.len(), 65);
    drop(store);
}

#[test]
fn empty_filter_sets_and_reversed_windows_match_nostr_semantics() {
    use nostr::{Alphabet, EventBuilder};

    let dir = tempfile::tempdir().unwrap();
    let mut store = RedbStore::open(dir.path().join("empty-filter-sets.redb")).unwrap();
    let keys = nostr::Keys::generate();
    let event = EventBuilder::new(Kind::TextNote, "one")
        .custom_created_at(Timestamp::from(10u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(
            event,
            RelayObserved::new(
                RelayUrl::parse("wss://empty-sets.example").unwrap(),
                Timestamp::from(10u64),
            ),
        )
        .unwrap();

    for filter in [
        Filter {
            ids: Some(BTreeSet::new()),
            ..Filter::new()
        },
        Filter {
            authors: Some(BTreeSet::new()),
            ..Filter::new()
        },
        Filter {
            kinds: Some(BTreeSet::new()),
            ..Filter::new()
        },
    ] {
        assert_eq!(store.query(&filter).unwrap().len(), 1);
        assert_eq!(store.query_newest(&filter, 10).unwrap().len(), 1);
    }

    let mut impossible_tag = Filter::new();
    impossible_tag
        .generic_tags
        .insert(SingleLetterTag::lowercase(Alphabet::H), BTreeSet::new());
    assert!(store.query(&impossible_tag).unwrap().is_empty());
    assert!(store.query_newest(&impossible_tag, 10).unwrap().is_empty());

    let reversed = Filter::new()
        .since(Timestamp::from(11u64))
        .until(Timestamp::from(10u64));
    assert!(store.query(&reversed).unwrap().is_empty());
    assert!(store.query_newest(&reversed, 10).unwrap().is_empty());
}

#[test]
fn multi_value_tag_merge_deduplicates_one_event_without_candidate_set() {
    use nostr::{Alphabet, EventBuilder, Tag};

    let dir = tempfile::tempdir().unwrap();
    let mut store = RedbStore::open(dir.path().join("tag-overlap.redb")).unwrap();
    let keys = nostr::Keys::generate();
    let event = EventBuilder::new(Kind::from(9u16), "both")
        .tags([
            Tag::parse(["h", "a"]).unwrap(),
            Tag::parse(["h", "b"]).unwrap(),
        ])
        .custom_created_at(Timestamp::from(100u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(
            event.clone(),
            RelayObserved::new(
                RelayUrl::parse("wss://tag-overlap.example").unwrap(),
                Timestamp::from(100u64),
            ),
        )
        .unwrap();
    let filter = Filter::new().custom_tags(SingleLetterTag::lowercase(Alphabet::H), ["a", "b"]);
    store.reset_query_work();
    let rows = store.query(&filter).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event.id, event.id);
    let (_index_rows, event_values, materialized) = store.query_work();
    assert_eq!(event_values, 1);
    assert_eq!(materialized, 1);
    assert_canonical_integrity(store.raw_database());
}

#[test]
fn ordered_planner_matches_fixture_derived_expectations_over_mixed_filters() {
    use nostr::{Alphabet, EventBuilder, Tag};

    fn next(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state
    }

    let dir = tempfile::tempdir().unwrap();
    let mut redb = RedbStore::open(dir.path().join("planner-expected-results.redb")).unwrap();
    let authors: Vec<_> = (0..8).map(|_| nostr::Keys::generate()).collect();
    let relay = RelayUrl::parse("wss://planner-differential.example").unwrap();
    let mut events = Vec::new();
    for i in 0..120u64 {
        let kind = Kind::from([1u16, 9, 42][(i as usize) % 3]);
        let content = if i % 9 == 0 {
            format!("needle-{i}")
        } else {
            format!("ordinary-{i}")
        };
        let mut tags = vec![
            Tag::parse(vec!["h".to_owned(), format!("room-{}", i % 7)]).unwrap(),
            Tag::parse(vec!["p".to_owned(), format!("member-{}", i % 11)]).unwrap(),
        ];
        if i % 10 == 0 {
            tags.push(Tag::parse(vec!["h".to_owned(), format!("room-{}", (i + 1) % 7)]).unwrap());
        }
        let event = EventBuilder::new(kind, content)
            .tags(tags)
            .custom_created_at(Timestamp::from(1_000 + (i * 17) % 97))
            .sign_with_keys(&authors[(i as usize) % authors.len()])
            .unwrap();
        let observed = RelayObserved::new(relay.clone(), Timestamp::from(2_000 + i));
        redb.insert(event.clone(), observed).unwrap();
        events.push(event);
    }

    let h = SingleLetterTag::lowercase(Alphabet::H);
    let p = SingleLetterTag::lowercase(Alphabet::P);
    let mut state = 0x169_cafe_f00d_u64;
    for round in 0..100u64 {
        let random = next(&mut state);
        let mut filter = Filter::new();
        if round % 5 == 0 {
            filter.ids = Some(if round % 20 == 0 {
                BTreeSet::new()
            } else {
                BTreeSet::from([
                    events[(random as usize) % events.len()].id,
                    events[((random >> 8) as usize) % events.len()].id,
                ])
            });
        }
        if round % 3 == 0 {
            filter.authors = Some(if round % 21 == 0 {
                BTreeSet::new()
            } else {
                BTreeSet::from([
                    authors[(random as usize) % authors.len()].public_key(),
                    authors[((random >> 5) as usize) % authors.len()].public_key(),
                ])
            });
        }
        if round % 4 == 0 {
            filter.kinds = Some(if round % 28 == 0 {
                BTreeSet::new()
            } else {
                BTreeSet::from([Kind::from([1u16, 9, 42][((random >> 11) as usize) % 3])])
            });
        }
        if round % 2 == 0 {
            filter.generic_tags.insert(
                h,
                if round % 22 == 0 {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([
                        format!("room-{}", (random >> 17) % 7),
                        format!("room-{}", (random >> 23) % 7),
                    ])
                },
            );
        }
        if round % 6 == 0 {
            filter.generic_tags.insert(
                p,
                BTreeSet::from([format!("member-{}", (random >> 29) % 11)]),
            );
        }
        if round % 7 == 0 {
            filter.search = Some("needle".to_owned());
        }
        if round % 8 == 0 {
            filter.since = Some(Timestamp::from(1_020 + (random % 30)));
            filter.until = Some(Timestamp::from(1_050 + ((random >> 7) % 30)));
        }
        if round % 31 == 0 {
            filter.since = Some(Timestamp::from(1_100u64));
            filter.until = Some(Timestamp::from(1_000u64));
        }

        let redb_complete: BTreeSet<_> = redb
            .query(&filter)
            .unwrap()
            .into_iter()
            .map(|row| row.event.id)
            .collect();
        let expected_complete: BTreeSet<_> = events
            .iter()
            .filter(|event| filter.match_event(event, nostr::filter::MatchEventOptions::new()))
            .map(|event| event.id)
            .collect();
        assert_eq!(redb_complete, expected_complete, "complete round {round}");

        let limit = 1 + (random as usize % 12);
        let redb_newest: Vec<_> = redb
            .query_newest(&filter, limit)
            .unwrap()
            .into_iter()
            .map(|row| row.event.id)
            .collect();
        let mut expected_newest: Vec<_> = events
            .iter()
            .filter(|event| filter.match_event(event, nostr::filter::MatchEventOptions::new()))
            .collect();
        expected_newest.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let expected_newest: Vec<_> = expected_newest
            .into_iter()
            .take(limit)
            .map(|event| event.id)
            .collect();
        assert_eq!(redb_newest, expected_newest, "bounded round {round}");
        assert_eq!(
            redb.query_newest_ids(&filter, limit).unwrap(),
            expected_newest,
            "projected bounded round {round}"
        );

        // Same filter, every ordered index that could answer it. The planner
        // picks one; this asserts the other choices would have returned the
        // same rows in the same order, which is why deleting the durable
        // `index_cardinality` estimate could not change any answer (#1248).
        let plannable = filter.ids.as_ref().is_none_or(BTreeSet::is_empty)
            && !filter.generic_tags.values().any(BTreeSet::is_empty)
            && !filter
                .since
                .zip(filter.until)
                .is_some_and(|(since, until)| since > until);
        if plannable {
            let read_txn = redb.raw_database().begin_read().unwrap();
            for plan in candidate_ordered_plans(&filter) {
                let complete: BTreeSet<_> = redb
                    .query_ordered(&read_txn, &plan, &filter, None, None, None)
                    .unwrap()
                    .into_iter()
                    .map(|row| row.event.id)
                    .collect();
                assert_eq!(
                    complete, expected_complete,
                    "round {round} complete under {:?}",
                    plan.index
                );
                let bounded: Vec<_> = redb
                    .query_ordered(&read_txn, &plan, &filter, None, Some(limit), None)
                    .unwrap()
                    .into_iter()
                    .map(|row| row.event.id)
                    .collect();
                assert_eq!(
                    bounded, expected_newest,
                    "round {round} bounded under {:?}",
                    plan.index
                );
                assert_eq!(
                    redb.query_ordered_ids(&read_txn, &plan, &filter, limit)
                        .unwrap(),
                    expected_newest,
                    "round {round} projected under {:?}",
                    plan.index
                );
            }
        }
    }
    assert_canonical_integrity(redb.raw_database());
}

/// #889's store half: a lane bootstrap that stages no row commits nothing.
///
/// Bootstrap runs once per open intent on every boot and is idempotent by
/// design, so on a store carrying a large open queue the overwhelmingly common
/// call is the one that finds every lane already there. Committing that call
/// spent a durability barrier per intent to leave the database byte-identical,
/// which is the amplification behind the 15,311-intent laptop incident: the
/// engine thread runs recovery to completion before it reads its first
/// command, so the app's first call waited for all of them.
///
/// The counter proves the barrier is gone; re-reading the lane set proves
/// aborting the unstaged transaction returned the same answer committing it
/// did, and that a genuinely new route still commits.
#[test]
fn a_lane_bootstrap_that_stages_no_row_commits_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("unstaged-lane-bootstrap.redb");
    let mut store = RedbStore::open(&path).expect("open redb store");
    let keys = nostr::Keys::generate();
    let first = RelayUrl::parse("wss://first.example").unwrap();
    let second = RelayUrl::parse("wss://second.example").unwrap();

    let (intent, _) = accepted_signed(&mut store, &keys, "unstaged", 1_000);
    store
        .record_route_revision(intent, BTreeSet::from([first.clone()]))
        .unwrap();

    let created = store.bootstrap_publish_queue_lanes(intent).unwrap();
    assert_eq!(created.len(), 1, "the first bootstrap mints the lane");
    assert_eq!(
        store.unstaged_lane_bootstraps(),
        0,
        "minting a lane is a real mutation and must commit"
    );

    for _ in 0..8 {
        assert_eq!(
            store.bootstrap_publish_queue_lanes(intent).unwrap(),
            created,
            "a complete lane set bootstraps to the identical answer"
        );
    }
    assert_eq!(
        store.unstaged_lane_bootstraps(),
        8,
        "every bootstrap over an unchanged lane set must skip its commit"
    );

    store
        .record_route_revision(intent, BTreeSet::from([first, second]))
        .unwrap();
    let widened = store.bootstrap_publish_queue_lanes(intent).unwrap();
    assert_eq!(widened.len(), 2, "a new route still mints its lane");
    assert_eq!(
        store.unstaged_lane_bootstraps(),
        8,
        "the bootstrap that staged a lane committed"
    );

    drop(store);
    let reopened = RedbStore::open(&path).expect("reopen redb store");
    assert_eq!(
        reopened.recover_publish_queue_lanes(intent).unwrap(),
        widened,
        "the committed lane set survives, and the aborted ones changed nothing"
    );
}

/// #1799: `insert`/`insert_batch` used to call terminal-receipt retention
/// maintenance -- hardcoded to `wall_clock_now()` and
/// `TerminalRetentionLimits::PRODUCTION` -- whenever the write produced
/// `InsertOutcome::Superseded`. An event write completely unrelated to any
/// receipt could therefore silently destroy old terminal receipts. Prove
/// the opposite is now true: a receipt old enough that `PRODUCTION`
/// retention would evict it survives an ordinary ingest that produces
/// `Superseded`, and only an explicit retention call still reclaims it.
#[test]
fn ingest_supersession_no_longer_evicts_terminal_receipts() {
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    let owner = nostr::Keys::generate();

    // Fabricate one terminal receipt directly, bypassing every public
    // acceptance door, with `terminal_at` pinned to the epoch -- far more
    // than `TERMINAL_RECEIPT_MAX_AGE_SECS` (24h) in the past relative to
    // any real wall-clock reading this test can observe.
    let receipt_id = {
        let write_txn = store.database().unwrap().begin_write().unwrap();
        let receipt_id = {
            let mut meta = write_txn.open_table(PUBLISH_QUEUE_META).unwrap();
            let mut receipts = write_txn.open_table(PUBLISH_QUEUE_RECEIPTS).unwrap();
            let receipt_id = alloc_receipt_id_in_txn(&mut meta).unwrap();
            let record = PublishQueueReceiptRecord {
                intent_id: None,
                expected_pubkey: owner.public_key(),
                accepted_at: None,
                payload: crate::PublishQueueReceiptPayload::Event {
                    event_id: EventId::all_zeros(),
                    state: ReceiptState::Cancelled,
                },
                terminal_sequence: None,
                terminal_at: None,
                terminal_bytes: None,
            };
            let encoded = super::publish_queue_codec::encode_receipt(&record);
            receipts
                .insert(
                    &super::publish_queue_codec::receipt_key(receipt_id),
                    encoded.as_slice(),
                )
                .unwrap();
            mark_terminal_receipt(&mut receipts, &mut meta, receipt_id, Timestamp::from(1), 0)
                .unwrap();
            receipt_id
        };
        write_txn.commit().unwrap();
        receipt_id
    };
    assert!(
        store.reattach_receipt(receipt_id).unwrap().is_some(),
        "fixture setup must actually retain the fabricated receipt"
    );

    // An ordinary relay-ingest write that produces `Superseded`: two
    // replaceable events for the same author, the second replacing the
    // first.
    let older = nostr::EventBuilder::new(Kind::ContactList, "")
        .custom_created_at(Timestamp::from(100))
        .sign_with_keys(&owner)
        .unwrap();
    let newer = nostr::EventBuilder::new(Kind::ContactList, "")
        .custom_created_at(Timestamp::from(200))
        .sign_with_keys(&owner)
        .unwrap();
    let relay = RelayUrl::parse("wss://ingest.example").unwrap();
    store
        .insert(older, RelayObserved::new(relay.clone(), Timestamp::from(1)))
        .unwrap();
    let outcome = store
        .insert(newer, RelayObserved::new(relay, Timestamp::from(2)))
        .unwrap();
    assert!(
        matches!(outcome, InsertOutcome::Superseded { .. }),
        "fixture ingest must actually supersede, or this proves nothing"
    );

    assert!(
        store.reattach_receipt(receipt_id).unwrap().is_some(),
        "an ordinary ingest write must never evict terminal receipts as a side effect"
    );

    // Confirm the receipt really was eviction-eligible under real
    // production policy: an explicit retention call (the door #1799 leaves
    // open for the engine/app to invoke deliberately) still reclaims it.
    let evicted = super::publish_queue_ops::maintain_terminal_receipts_at(
        &mut store,
        crate::terminal_retention::wall_clock_now(),
        crate::terminal_retention::TerminalRetentionLimits::PRODUCTION,
    )
    .unwrap();
    assert_eq!(evicted, vec![receipt_id]);
    assert!(store.reattach_receipt(receipt_id).unwrap().is_none());
}

/// `apply_run_deaths` keeps dead keys in a binary counter of at most
/// `MAX_DEATH_BLOCKS` (8) levels. When a death batch carries through every
/// occupied level, the counter overflows and the run must be rewritten
/// without its dead events.
///
/// That rewrite was unreachable *precisely* when it became necessary.
/// Falling out of the carry loop means all 8 levels were occupied and
/// `carry` already held their union; the code then pushed `carry` onto the
/// 8 blocks it had read at the top of the function and re-merged all 9 —
/// which `merge_dead_blocks` rejects, because its hard bound is 8. So the
/// single case the rewrite path existed to handle was the one case that
/// failed the entire write transaction instead.
///
/// This test drives the counter to overflow through the public door. It is
/// deliberately slow-ish (one committed transaction per death) because the
/// carry is a property of *separate* flushes: batching the removes would
/// coalesce them into one death block and never fill a level.
#[test]
fn a_death_batch_that_overflows_the_counter_rewrites_the_run() {
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    let owner = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://death.example").unwrap();

    // One run, large enough that 256 deaths cannot empty it -- an emptied run
    // takes the `delete_run` early return at the top and never reaches the
    // carry at all.
    let events: Vec<_> = (0..320u64)
        .map(|index| {
            nostr::EventBuilder::new(Kind::TextNote, format!("death-fanin-{index}"))
                .custom_created_at(Timestamp::from(2_000 + index))
                .sign_with_keys(&owner)
                .expect("sign death fan-in event")
        })
        .collect();
    let ids: Vec<_> = events.iter().map(|event| event.id).collect();
    store
        .insert_batch(
            events
                .into_iter()
                .map(|event| {
                    (
                        event,
                        RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                    )
                })
                .collect(),
        )
        .expect("seed one run");

    // Levels occupied after `n` batches are the set bits of `n`. After 255 all
    // eight are occupied; the 256th carries through every one of them and is
    // the batch that reaches the rewrite. Before the fix it returned
    // "packed postings: dead-key block fan-in exceeds the hard bound" and
    // failed the write.
    for (batch, id) in ids.iter().take(256).enumerate() {
        store
            .remove(*id, RetractReason::Deleted)
            .unwrap_or_else(|error| {
                panic!("death batch {batch} must not fail the write transaction: {error}")
            });
    }

    // The rewrite really did happen rather than the deaths silently no-oping:
    // every removed event is gone, and every untouched one survives it.
    let dead: Vec<_> = ids.iter().take(256).copied().collect();
    let alive: Vec<_> = ids.iter().skip(256).copied().collect();
    assert!(
        store.query(&Filter::new().ids(dead)).unwrap().is_empty(),
        "no removed event may survive the run rewrite"
    );
    assert_eq!(
        store
            .query(&Filter::new().ids(alive.clone()))
            .unwrap()
            .len(),
        alive.len(),
        "the rewrite must preserve every event that was not dead"
    );
}

/// **No filter is ever stored in the database** (#1849). A coverage row is
/// `key` + `from` + `through` and nothing else: the durable value carries no
/// author, no id, no tag and no kind from the filter it was proven against.
///
/// Before this, every row retained the full window-erased shape, so the store
/// was a permanent record of every distinct query a user had ever issued --
/// which authors they read, which tags they followed -- with no expiry and no
/// delete door. It existed only so `gc` could ask "would this row have matched
/// the event I am evicting"; `gc` now shrinks on interval overlap alone.
///
/// The test looks at the raw bytes of BOTH halves of the row, not at a decoded
/// struct: a field reinstated on `CoverageRowRecord` would still decode fine
/// into whatever the reader asked for, and only the bytes can say what is
/// actually on disk.
#[test]
fn a_persisted_coverage_row_carries_no_filter_derived_bytes() {
    let mut store = RedbStore::temporary().expect("temporary Redb store");

    let author = nostr::Keys::generate().public_key().to_hex();
    let id = nostr::Keys::generate().public_key().to_hex();
    let tag_value = "distinctive-tag-value-for-1849".to_string();
    let kind = 30_042u16;

    let filter = ConcreteFilter {
        kinds: Some(BTreeSet::from([kind])),
        authors: Some(BTreeSet::from([author.clone()])),
        ids: Some(BTreeSet::from([id.clone()])),
        tags: BTreeMap::from([(
            nmp_grammar::IndexedTagName::new('d').expect("'d' is an indexed tag name"),
            BTreeSet::from([tag_value.clone()]),
        )]),
        since: Some(1_700_000_042),
        until: None,
        limit: None,
    };
    let atom = ContextualAtom {
        filter,
        routing: nmp_grammar::ReadRouting::Auto,
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    };
    let relay = RelayUrl::parse("wss://r1.example").expect("relay url");

    store
        .record_coverage(&[(
            atom,
            RelaySessionKey::unauthenticated(relay.clone()),
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
        )])
        .expect("record coverage");

    let rows: Vec<(String, String)> = {
        let read_txn = store
            .database()
            .expect("database handle")
            .begin_read()
            .expect("read txn");
        let coverage = read_txn.open_table(COVERAGE).expect("open coverage");
        coverage
            .iter()
            .expect("iterate coverage")
            .map(|entry| {
                let (key, value) = entry.expect("coverage row");
                (key.value().to_string(), value.value().to_string())
            })
            .collect()
    };

    assert_eq!(rows.len(), 1, "one (atom, relay) claim is one row");
    let (row_key, row_value) = &rows[0];

    for (what, needle) in [
        ("the author pubkey", author.as_str()),
        ("the event id", id.as_str()),
        ("the tag value", tag_value.as_str()),
        ("the kind", "30042"),
        ("the `since` bound", "1700000042"),
    ] {
        assert!(
            !row_key.contains(needle) && !row_value.contains(needle),
            "{what} is filter-derived and must appear nowhere on disk; \
             found it in coverage row {row_key:?} => {row_value:?}"
        );
    }

    assert_eq!(
        row_value, r#"{"from":0,"through":100}"#,
        "a coverage row's value is the proven interval and nothing else"
    );
}
