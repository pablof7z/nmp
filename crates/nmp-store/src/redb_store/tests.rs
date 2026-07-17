use super::*;

#[test]
fn v5_event_epoch_is_rejected_before_any_v6_table_is_created() {
    const LEGACY_EVENTS_V5: TableDefinition<u64, &[u8]> = TableDefinition::new("events_v5");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-epoch.redb");
    let db = Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    write_txn.open_table(LEGACY_EVENTS_V5).unwrap();
    write_txn.commit().unwrap();
    drop(db);

    let error = match RedbStore::open(&path) {
        Ok(_) => panic!("v5 event epoch must not open as an empty v6 store"),
        Err(error) => error,
    };
    assert!(matches!(error, redb::Error::UpgradeRequired(6)));

    let db = Database::create(&path).unwrap();
    let read_txn = db.begin_read().unwrap();
    let table_names: BTreeSet<_> = read_txn
        .list_tables()
        .unwrap()
        .map(|table| table.name().to_owned())
        .collect();
    assert_eq!(table_names, BTreeSet::from(["events_v5".to_owned()]));
    assert!(!table_names.contains(EVENTS.name()));
}

#[test]
fn v5_displaced_epoch_is_rejected_before_any_v6_table_is_created() {
    const LEGACY_DISPLACED_V5: TableDefinition<&str, &[u8]> =
        TableDefinition::new("outbox_displaced_v5");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-displaced-epoch.redb");
    let db = Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    write_txn.open_table(LEGACY_DISPLACED_V5).unwrap();
    write_txn.commit().unwrap();
    drop(db);

    let error = match RedbStore::open(&path) {
        Ok(_) => panic!("v5 displaced epoch must not open as an empty v6 store"),
        Err(error) => error,
    };
    assert!(matches!(error, redb::Error::UpgradeRequired(6)));

    let db = Database::create(&path).unwrap();
    let read_txn = db.begin_read().unwrap();
    let table_names: BTreeSet<_> = read_txn
        .list_tables()
        .unwrap()
        .map(|table| table.name().to_owned())
        .collect();
    assert_eq!(
        table_names,
        BTreeSet::from(["outbox_displaced_v5".to_owned()])
    );
    assert!(!table_names.contains(OUTBOX_DISPLACED.name()));
}

#[test]
fn healthy_v6_reopen_starts_no_application_write_transaction() {
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
    let read_txn = reopened.db.begin_read().unwrap();
    let schema_meta = read_txn.open_table(SCHEMA_META).unwrap();
    assert_eq!(
        schema_meta
            .get(SCHEMA_VERSION_KEY)
            .unwrap()
            .unwrap()
            .value(),
        SCHEMA_VERSION
    );
}

#[test]
fn pending_ephemeral_count_gates_one_recovery_write_then_returns_to_fast_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ephemeral-recovery.redb");
    let keys = nostr::Keys::generate();
    let frozen_id = EventId::from_byte_array([7; 32]);

    let mut store = RedbStore::open(&path).unwrap();
    let receipt_id = store
        .accept_ephemeral(frozen_id, keys.public_key())
        .unwrap();
    drop(store);

    let recovered = RedbStore::open(&path).unwrap();
    assert_eq!(recovered.open_write_transactions(), 1);
    let receipt = recovered
        .reattach_receipt(receipt_id)
        .unwrap()
        .expect("retained ephemeral receipt");
    assert_eq!(receipt.state, ReceiptState::Abandoned);
    drop(recovered);

    let healthy = RedbStore::open(&path).unwrap();
    assert_eq!(healthy.open_write_transactions(), 0);
}

#[test]
fn surrogate_allocators_do_not_touch_hot_metadata_rows_until_one_flush() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("allocator-flush.redb");
    let store = RedbStore::open(&path).unwrap();
    let write_txn = store.db.begin_write().unwrap();
    {
        let mut canonical = CanonicalWriteTables::open(&write_txn).unwrap();
        for expected in 1..=128 {
            assert_eq!(canonical.allocate_key().unwrap(), expected);
        }
        for expected in 1..=16 {
            assert_eq!(canonical.allocate_relay_key().unwrap(), expected);
        }
        assert!(canonical.store_meta.get(NEXT_EVENT_KEY).unwrap().is_none());
        assert!(canonical.relay_meta.get(NEXT_RELAY_KEY).unwrap().is_none());

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
                .relay_meta
                .get(NEXT_RELAY_KEY)
                .unwrap()
                .unwrap()
                .value(),
            17
        );
    }
    write_txn.commit().unwrap();
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
            frozen,
            replaceable_base: None,
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "range-proof".into(),
            durability: WriteDurability::Durable,
            routing: "range-proof".into(),
            sig_state: IntentSigState::Pending,
            accepted_at: Timestamp::from(created_at),
            correlation: None,
        })
        .expect("accept fixture intent");
    let intent = outcome.journaled_intent_id().expect("intent id");
    store
        .promote_signed(intent, signed.sig)
        .expect("promote fixture intent");
    (intent, signed)
}

/// Issue #87's measurable bound: 128 unrelated intents must add zero
/// visited rows to target-intent attempt or route-revision recovery.
/// Relay URLs deliberately share textual prefixes, and intent 1 coexists
/// with prefix-adversarial ids 10/100.
#[test]
fn outbox_ranges_visit_only_target_intent_and_exact_relay_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("outbox-ranges.redb");
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
    let lanes = store.bootstrap_outbox_lanes(target).unwrap();
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
            AttemptOutcome::GaveUp,
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
        let noise_lane = store.bootstrap_outbox_lanes(intent).unwrap().remove(0);
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

    store.reset_outbox_range_rows();
    let attempts = store.recover_attempts(target).unwrap();
    let revisions = store.recover_route_revisions(target).unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(revisions.len(), 2);
    assert_eq!(store.outbox_range_rows(), (2, 2));
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
        source: nmp_grammar::SourceAuthority::AuthorOutboxes,
        access: nmp_grammar::AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let key = compute_coverage_key(&atom);
    let relay = RelayUrl::parse("wss://relay.example").unwrap();
    let row_key = RedbStore::coverage_row_key(key, &relay);

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

/// #106's legacy-purge falsifier: a coverage row written under the OLD
/// (pre-#106, unversioned) key format is silently unreachable via
/// `get_coverage` (its key never matches anything `record_coverage`
/// computes anymore) and `gc` deletes it outright, tracked via
/// `GcReport::legacy_coverage_rows_purged` (disjoint from the ordinary
/// shrink/delete counters).
#[test]
fn gc_purges_legacy_unversioned_coverage_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.redb");
    let mut store = RedbStore::open(&db_path).unwrap();
    let relay = RelayUrl::parse("wss://relay.example").unwrap();

    // Write a legacy-shaped row directly (bypassing `record_coverage`,
    // which always writes under the CURRENT version prefix) -- the
    // exact shape a pre-#106 row would have on disk.
    let legacy_shape = ConcreteFilter {
        kinds: Some(std::collections::BTreeSet::from([1u16])),
        authors: Some(std::collections::BTreeSet::from(["aa".to_string()])),
        ..ConcreteFilter::default()
    };
    let legacy_key = compute_coverage_key(&ContextualAtom {
        filter: legacy_shape.clone(),
        source: nmp_grammar::SourceAuthority::AuthorOutboxes,
        access: nmp_grammar::AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    });
    let mut legacy_hex = String::new();
    {
        use std::fmt::Write as _;
        for byte in legacy_key.as_bytes() {
            let _ = write!(legacy_hex, "{byte:02x}");
        }
    }
    let legacy_row_key = format!("{legacy_hex}:{}", relay.as_str());
    let legacy_record = CoverageRowRecord {
        shape: ShapeRecord::from(&legacy_shape),
        from: 0,
        through: 100,
    };
    {
        let write_txn = store.db.begin_write().unwrap();
        {
            let mut coverage = write_txn.open_table(COVERAGE).unwrap();
            coverage
                .insert(
                    legacy_row_key.as_str(),
                    serde_json::to_string(&legacy_record).unwrap().as_str(),
                )
                .unwrap();
        }
        write_txn.commit().unwrap();
    }

    let report = store.gc(&ClaimSet::new(Vec::new())).unwrap();
    assert_eq!(
        report.legacy_coverage_rows_purged, 1,
        "the unversioned legacy row must be purged"
    );

    let read_txn = store.db.begin_read().unwrap();
    let coverage = read_txn.open_table(COVERAGE).unwrap();
    assert!(
        coverage.get(legacy_row_key.as_str()).unwrap().is_none(),
        "the legacy row must be gone after gc"
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
    let read_txn = store.db.begin_read().unwrap();
    let event_ids = read_txn.open_table(EVENT_IDS).unwrap();
    let events = read_txn.open_table(EVENTS).unwrap();
    let local = read_txn.open_table(EVENT_LOCAL).unwrap();
    let event_key = event_ids
        .get(id.as_bytes())
        .unwrap()
        .expect("raw id mapping")
        .value();
    let event_bytes = events
        .get(event_key)
        .unwrap()
        .expect("raw event row")
        .value()
        .to_vec();
    let local_bytes = local
        .get(event_key)
        .unwrap()
        .map(|value| value.value().to_vec());
    (event_key, event_bytes, local_bytes)
}

fn raw_observation_rows(store: &RedbStore, event_key: EventKey) -> Vec<(Vec<u8>, u64)> {
    let read_txn = store.db.begin_read().unwrap();
    let observations = read_txn.open_table(EVENT_OBSERVATIONS).unwrap();
    let (lower, upper) = observation_range(event_key);
    observations
        .range::<&[u8; 12]>(&lower..=&upper)
        .unwrap()
        .map(|entry| {
            let (key, at) = entry.unwrap();
            (key.value().to_vec(), at.value())
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
        let read_txn = store.db.begin_read().unwrap();
        let relay_keys = read_txn.open_table(RELAY_KEYS).unwrap();
        let relay_refs = read_txn.open_table(RELAY_REFS).unwrap();
        let relay_key = relay_keys.get(relay.as_str()).unwrap().unwrap().value();
        assert_eq!(relay_refs.get(relay_key).unwrap().unwrap().value(), 2);
        relay_key
    };
    assert_canonical_integrity(&store.db);

    store.remove(first.id, RetractReason::Deleted).unwrap();
    {
        let read_txn = store.db.begin_read().unwrap();
        let relay_refs = read_txn.open_table(RELAY_REFS).unwrap();
        assert_eq!(relay_refs.get(first_relay_key).unwrap().unwrap().value(), 1);
    }
    assert_canonical_integrity(&store.db);

    store.remove(second.id, RetractReason::Deleted).unwrap();
    {
        let read_txn = store.db.begin_read().unwrap();
        assert!(read_txn
            .open_table(RELAY_KEYS)
            .unwrap()
            .get(relay.as_str())
            .unwrap()
            .is_none());
        assert_eq!(read_txn.open_table(RELAYS).unwrap().len().unwrap(), 0);
        assert_eq!(read_txn.open_table(RELAY_REFS).unwrap().len().unwrap(), 0);
        assert_eq!(
            read_txn
                .open_table(EVENT_OBSERVATIONS)
                .unwrap()
                .len()
                .unwrap(),
            0
        );
    }
    assert_canonical_integrity(&store.db);

    let third = make_event(3);
    store
        .insert(
            third,
            RelayObserved::new(relay.clone(), Timestamp::from(30u64)),
        )
        .unwrap();
    let read_txn = store.db.begin_read().unwrap();
    let new_relay_key = read_txn
        .open_table(RELAY_KEYS)
        .unwrap()
        .get(relay.as_str())
        .unwrap()
        .unwrap()
        .value();
    assert!(new_relay_key > first_relay_key);
}

#[test]
fn batch_relay_refcounts_flush_once_per_distinct_relay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-refcount-batch.redb");
    let store = RedbStore::open(&path).unwrap();
    let relay = RelayUrl::parse("wss://one-hot-refcount.example").unwrap();
    let write_txn = store.db.begin_write().unwrap();
    {
        let mut canonical = CanonicalWriteTables::open(&write_txn).unwrap();
        let relay_key = canonical.intern_relay(&relay).unwrap();
        for _ in 0..1_114 {
            canonical.increment_relay_ref(relay_key).unwrap();
        }
        assert_eq!(canonical.relay_ref_counts.len(), 1);
        assert_eq!(canonical.relay_ref_counts[&relay_key], 1_114);
        assert_eq!(
            canonical
                .relay_refs
                .get(relay_key)
                .unwrap()
                .unwrap()
                .value(),
            0,
            "the durable hot row stays untouched until the batch flush"
        );
        canonical.flush_pending().unwrap();
        assert!(canonical.relay_ref_counts.is_empty());
        assert_eq!(
            canonical
                .relay_refs
                .get(relay_key)
                .unwrap()
                .unwrap()
                .value(),
            1_114
        );
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
    assert_canonical_integrity(&store.db);

    let read_txn = store.db.begin_read().unwrap();
    let relay_keys = read_txn.open_table(RELAY_KEYS).unwrap();
    assert!(relay_keys.get(old_relay.as_str()).unwrap().is_none());
    let winner_key = relay_keys.get(new_relay.as_str()).unwrap().unwrap().value();
    assert_eq!(
        read_txn
            .open_table(RELAY_REFS)
            .unwrap()
            .get(winner_key)
            .unwrap()
            .unwrap()
            .value(),
        1
    );
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
    let read_txn = store.db.begin_read().unwrap();
    let relay_refs = read_txn.open_table(RELAY_REFS).unwrap();
    assert_eq!(
        relay_refs
            .iter()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .1
            .value(),
        1
    );
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
    store.remove(first.id, RetractReason::Expired).unwrap();
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
    assert_canonical_integrity(&reopened.db);
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
    assert_canonical_integrity(&store.db);

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
    assert_canonical_integrity(&store.db);

    let deletion = EventBuilder::new(Kind::EventDeletion, "")
        .tag(Tag::event(target.id))
        .custom_created_at(Timestamp::from(40u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(deletion, observed(relay1.clone(), 40))
        .unwrap();
    assert_canonical_integrity(&store.db);

    let expiring = EventBuilder::new(Kind::TextNote, "expiring")
        .tag(Tag::expiration(Timestamp::from(60u64)))
        .custom_created_at(Timestamp::from(50u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(expiring, observed(relay1.clone(), 50))
        .unwrap();
    store.expire_due(Timestamp::from(60u64)).unwrap();
    assert_canonical_integrity(&store.db);

    let gc_candidate = EventBuilder::new(Kind::TextNote, "gc")
        .custom_created_at(Timestamp::from(70u64))
        .sign_with_keys(&keys)
        .unwrap();
    store
        .insert(gc_candidate, observed(relay1.clone(), 70))
        .unwrap();
    store.gc(&ClaimSet::new(Vec::new())).unwrap();
    assert_canonical_integrity(&store.db);

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
            frozen,
            replaceable_base: None,
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "integrity".into(),
            durability: WriteDurability::Durable,
            routing: "integrity".into(),
            sig_state: IntentSigState::Pending,
            accepted_at: Timestamp::from(80u64),
            correlation: None,
        })
        .unwrap();
    assert_canonical_integrity(&store.db);
    store
        .compensate_write(accepted.journaled_intent_id().unwrap())
        .unwrap();
    assert_canonical_integrity(&store.db);
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
    assert_eq!(
        store.query_work(),
        (25, 0, 0),
        "an index-covered ID projection must not read or own 64 KiB event values"
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
    let claim_key = id_tombstone_key(&hidden.id, &hidden.pubkey);
    let write_txn = store.db.begin_write().unwrap();
    {
        let mut claims = write_txn.open_table(OUTBOX_SUPPRESS_BY_ID).unwrap();
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
    let write_txn = store.db.begin_write().unwrap();
    {
        let mut event_ids = write_txn.open_table(EVENT_IDS).unwrap();
        event_ids.remove(event.id.as_bytes()).unwrap();
    }
    write_txn.commit().unwrap();

    let error = store
        .query_newest_ids(&Filter::new().kind(Kind::from(9u16)), 1)
        .unwrap_err();
    assert!(error.0.contains("canonical id map"));
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
    assert_eq!(
        store.query_work(),
        (10, 10, 10),
        concat!(
            "the redb range must begin strictly after the exact cursor key; ",
            "none of the 120 newer/equal-before rows may be scanned or skipped"
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
    assert_eq!(
        store.query_work(),
        (9, 9, 9),
        concat!(
            "three roots at limit three must consume and materialize exactly nine rows; ",
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
    let rows = store
        .query_newest_observed_by(&filter, &eligible, 3)
        .unwrap();

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
fn fixed_ordered_indexes_use_inclusive_equal_time_ranges_and_id_ascending_ties() {
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
            OrderedIndex::AuthorKind,
        ),
    ];

    for (filter, expected_index) in filters {
        let read_txn = store.db.begin_read().unwrap();
        let plan = plan_ordered_query(&read_txn, &filter).unwrap();
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

#[test]
fn cardinality_planner_selects_smallest_real_tag_bucket_for_complete_query() {
    use nostr::{Alphabet, EventBuilder, Tag};

    let dir = tempfile::tempdir().unwrap();
    let mut store = RedbStore::open(dir.path().join("cardinality-plan.redb")).unwrap();
    let write_txn = store.db.begin_write().unwrap();
    {
        let mut sample_meta = write_txn.open_table(INDEX_CARDINALITY_SAMPLE_META).unwrap();
        sample_meta
            .insert(INDEX_CARDINALITY_SAMPLE_KEY, [0x42; 32].as_slice())
            .unwrap();
    }
    write_txn.commit().unwrap();
    let keys = nostr::Keys::new(nostr::SecretKey::from_slice(&[1; 32]).unwrap());
    let member = nostr::Keys::new(nostr::SecretKey::from_slice(&[2; 32]).unwrap())
        .public_key()
        .to_hex();
    let relay = RelayUrl::parse("wss://cardinality.example").unwrap();
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
    // skips only #p, not every tag predicate.
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
    let read_txn = store.db.begin_read().unwrap();
    let plan = plan_ordered_query(&read_txn, &filter).unwrap();
    assert_eq!(plan.index, OrderedIndex::Tag(p));
    assert!(
        plan.estimated_rows <= 6,
        "sampled physical count cannot exceed the real bucket"
    );
    drop(read_txn);

    store.reset_query_work();
    let rows = store.query(&filter).unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(store.query_work(), (6, 6, 5));
    assert_canonical_integrity(&store.db);
}

#[test]
fn cardinality_sampling_is_keyed_stable_and_near_one_sixteenth() {
    let sample_key = [0x42; 32];
    let sampled = (0..65_536u64)
        .filter(|value| {
            let mut id = [0u8; 32];
            id[24..].copy_from_slice(&value.to_be_bytes());
            event_is_cardinality_sample(&sample_key, &EventId::from_byte_array(id))
        })
        .count();
    assert_eq!(sampled, 4_053);
    assert!(!event_is_cardinality_sample(
        &[0x43; 32],
        &EventId::from_byte_array([0; 32])
    ));
}

#[test]
fn cardinality_planner_never_materializes_unbounded_author_kind_products() {
    let dir = tempfile::tempdir().unwrap();
    let store = RedbStore::open(dir.path().join("bounded-composite-plan.redb")).unwrap();
    let authors: BTreeSet<_> = (0..65)
        .map(|_| nostr::Keys::generate().public_key())
        .collect();
    let kinds: BTreeSet<_> = (0..65u16).map(Kind::from).collect();
    assert!(authors.len() * kinds.len() > MAX_COMPOSITE_QUERY_RANGES);

    let filter = Filter::new().authors(authors).kinds(kinds);
    let read_txn = store.db.begin_read().unwrap();
    let plan = plan_ordered_query(&read_txn, &filter).unwrap();
    assert_eq!(plan.index, OrderedIndex::Author);
    assert_eq!(plan.prefixes.len(), 65);
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
fn missing_cardinality_epoch_rebuilds_atomically_from_fixed_ordered_indexes() {
    use nostr::EventBuilder;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cardinality-rebuild.redb");
    let keys = nostr::Keys::generate();
    let relay = RelayUrl::parse("wss://cardinality-rebuild.example").unwrap();
    let mut store = RedbStore::open(&path).unwrap();
    for i in 0..7u64 {
        let event = EventBuilder::new(Kind::TextNote, format!("row-{i}"))
            .custom_created_at(Timestamp::from(i + 1))
            .sign_with_keys(&keys)
            .unwrap();
        store
            .insert(
                event,
                RelayObserved::new(relay.clone(), Timestamp::from(i + 1)),
            )
            .unwrap();
    }
    drop(store);

    let db = Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    {
        let mut meta = write_txn.open_table(INDEX_CARDINALITY_META).unwrap();
        meta.remove(INDEX_CARDINALITY_VERSION_KEY).unwrap();
        let mut sample_meta = write_txn.open_table(INDEX_CARDINALITY_SAMPLE_META).unwrap();
        sample_meta.remove(INDEX_CARDINALITY_SAMPLE_KEY).unwrap();
        let mut cardinality = write_txn.open_table(INDEX_CARDINALITY).unwrap();
        cardinality
            .insert(global_cardinality_key().as_slice(), 999)
            .unwrap();
    }
    write_txn.commit().unwrap();
    drop(db);

    let reopened = RedbStore::open(&path).unwrap();
    assert_eq!(
        reopened.open_write_transactions(),
        1,
        "the unhealthy sidecar is rebuilt in one write transaction"
    );
    assert_eq!(reopened.query(&Filter::new()).unwrap().len(), 7);
    let read_txn = reopened.db.begin_read().unwrap();
    let sample_meta = read_txn.open_table(INDEX_CARDINALITY_SAMPLE_META).unwrap();
    assert_eq!(
        sample_meta
            .get(INDEX_CARDINALITY_SAMPLE_KEY)
            .unwrap()
            .unwrap()
            .value()
            .len(),
        32
    );
    assert_canonical_integrity(&reopened.db);
}

#[test]
fn malformed_cardinality_sample_key_fails_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("malformed-cardinality-sample-key.redb");
    drop(RedbStore::open(&path).unwrap());

    let db = Database::create(&path).unwrap();
    let write_txn = db.begin_write().unwrap();
    {
        let mut sample_meta = write_txn.open_table(INDEX_CARDINALITY_SAMPLE_META).unwrap();
        sample_meta
            .insert(INDEX_CARDINALITY_SAMPLE_KEY, [1u8, 2].as_slice())
            .unwrap();
    }
    write_txn.commit().unwrap();
    drop(db);

    assert!(matches!(
        RedbStore::open(&path),
        Err(redb::Error::Corrupted(message))
            if message == "invalid cardinality sample key length"
    ));
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
    assert_canonical_integrity(&store.db);
}

#[test]
fn cardinality_planner_is_differentially_equivalent_over_mixed_filters() {
    use nostr::{Alphabet, EventBuilder, Tag};

    fn next(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state
    }

    let dir = tempfile::tempdir().unwrap();
    let mut redb = RedbStore::open(dir.path().join("planner-differential.redb")).unwrap();
    let mut memory = crate::MemoryStore::new();
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
        redb.insert(event.clone(), observed.clone()).unwrap();
        memory.insert(event.clone(), observed).unwrap();
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
        let memory_complete: BTreeSet<_> = memory
            .query(&filter)
            .unwrap()
            .into_iter()
            .map(|row| row.event.id)
            .collect();
        assert_eq!(redb_complete, memory_complete, "complete round {round}");

        let limit = 1 + (random as usize % 12);
        let redb_newest: Vec<_> = redb
            .query_newest(&filter, limit)
            .unwrap()
            .into_iter()
            .map(|row| row.event.id)
            .collect();
        let memory_newest: Vec<_> = memory
            .query_newest(&filter, limit)
            .unwrap()
            .into_iter()
            .map(|row| row.event.id)
            .collect();
        assert_eq!(redb_newest, memory_newest, "bounded round {round}");
        assert_eq!(
            redb.query_newest_ids(&filter, limit).unwrap(),
            memory.query_newest_ids(&filter, limit).unwrap(),
            "projected bounded round {round}"
        );
    }
    assert_canonical_integrity(&redb.db);
}
