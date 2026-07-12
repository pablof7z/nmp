//! Durable outbox-lane substrate contract (issue #94).

use std::collections::BTreeSet;
use std::path::Path;

use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, AttemptHandoffDetail, AttemptOutcome,
    CloseIntentOutcome, DeadlineKind, EventStore, HandoffEvidence, InFlightPhase, IntentId,
    IntentSigState, LaneDeadline, LaneKey, LaneState, MemoryStore, PostHandoffState, RecoveredLane,
    RedbStore, TransientCause, WriteDurability,
};
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, RelayUrl, Timestamp};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

fn signed_and_frozen(keys: &Keys, content: &str, created_at: u64) -> (Event, Event) {
    let signed = EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign event");
    let frozen = Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        sentinel_signature(),
    );
    (signed, frozen)
}

fn accept(frozen: Event, keys: &Keys, accepted_at: u64) -> AcceptWrite {
    AcceptWrite {
        frozen,
        expected_pubkey: keys.public_key(),
        signing_identity_ref: "lane-contract".into(),
        durability: WriteDurability::Durable,
        routing: "lane-contract".into(),
        sig_state: IntentSigState::Pending,
        accepted_at: Timestamp::from(accepted_at),
    }
}

fn seed(
    store: &mut dyn EventStore,
    content: &str,
    created_at: u64,
    relay: RelayUrl,
) -> (IntentId, u64, Event, LaneKey, RecoveredLane) {
    let keys = Keys::generate();
    let (signed, frozen) = signed_and_frozen(&keys, content, created_at);
    let accepted = store
        .accept_write(accept(frozen, &keys, created_at))
        .unwrap();
    let (intent_id, receipt_id) = match accepted {
        AcceptOutcome::Inserted {
            intent_id,
            receipt_id,
            ..
        } => (intent_id, receipt_id),
        other => panic!("expected inserted intent, got {other:?}"),
    };
    store.promote_signed(intent_id, signed.sig).unwrap();
    store
        .record_route_revision(intent_id, BTreeSet::from([relay.clone()]))
        .unwrap();
    let lanes = store.bootstrap_outbox_lanes(intent_id).unwrap();
    assert_eq!(lanes.len(), 1);
    let lane = lanes[0].clone();
    assert_eq!(lane.revision, 1);
    assert_eq!(lane.last_ordinal, 0);
    assert_eq!(lane.state, LaneState::WaitingConnection);
    let key = LaneKey { intent_id, relay };
    assert_eq!(lane.key, key);
    (intent_id, receipt_id, signed, key, lane)
}

fn for_each_backend(mut body: impl FnMut(&mut dyn EventStore)) {
    let mut memory = MemoryStore::new();
    body(&mut memory);

    let dir = tempfile::tempdir().unwrap();
    let mut redb = RedbStore::open(dir.path().join("store.redb")).unwrap();
    body(&mut redb);
}

#[test]
fn lane_lifecycle_is_exact_and_backend_identical() {
    for_each_backend(|store| {
        let relay = RelayUrl::parse("wss://lane-lifecycle.example").unwrap();
        let (intent, receipt, signed, key, seeded) = seed(store, "lane lifecycle", 100, relay);

        // Bootstrap is deterministic and idempotent.
        assert_eq!(store.bootstrap_outbox_lanes(intent).unwrap(), vec![seeded]);

        let eligible = store
            .set_lane_eligible(&key, 1, Timestamp::from(101))
            .unwrap();
        assert_eq!(eligible.revision, 2);
        assert_eq!(
            eligible.state,
            LaneState::Eligible {
                since: Timestamp::from(101)
            }
        );
        assert!(store
            .set_lane_waiting(&key, 1, false)
            .unwrap_err()
            .to_string()
            .contains("revision"));

        let (first, awaiting_handoff) = store
            .start_lane_attempt(&key, 2, signed.clone(), Timestamp::from(102))
            .unwrap();
        assert_eq!(first.ordinal, 1);
        assert_eq!(first.outcome, AttemptOutcome::Started);
        assert_eq!(awaiting_handoff.revision, 3);
        assert_eq!(
            awaiting_handoff.state,
            LaneState::InFlight {
                ordinal: 1,
                phase: InFlightPhase::AwaitingHandoff,
            }
        );
        let details = store.recover_attempt_details(intent).unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].ordinal, 1);
        assert_eq!(details[0].started_at, Some(Timestamp::from(102)));
        assert_eq!(details[0].handoff, None);
        assert!(store
            .record_lane_handoff(
                &key,
                3,
                1,
                AttemptHandoffDetail {
                    at: Timestamp::from(102),
                    result: HandoffEvidence::NotHandedOff,
                },
                PostHandoffState::Terminal {
                    outcome: AttemptOutcome::Started,
                    finished_at: Timestamp::from(102),
                },
            )
            .is_err());
        assert_eq!(
            store.recover_attempt_details(intent).unwrap()[0].handoff,
            None,
            "an invalid handoff transition must leave no detail mutation"
        );
        assert_eq!(store.recover_outbox_lanes(intent).unwrap()[0].revision, 3);

        let ack_deadline = Timestamp::from(120);
        let awaiting_ack = store
            .record_lane_handoff(
                &key,
                3,
                1,
                AttemptHandoffDetail {
                    at: Timestamp::from(103),
                    result: HandoffEvidence::Written,
                },
                PostHandoffState::AwaitingAck {
                    deadline: ack_deadline,
                },
            )
            .unwrap();
        assert_eq!(awaiting_ack.revision, 4);
        assert_eq!(
            store.due_outbox_deadlines(ack_deadline, 10).unwrap()[0].kind,
            DeadlineKind::AckTimeout
        );

        let retry_at = Timestamp::from(130);
        let transient = store
            .set_lane_transient(
                &key,
                4,
                1,
                retry_at,
                TransientCause::AckTimeout,
                Some("ack deadline elapsed".into()),
            )
            .unwrap();
        assert_eq!(transient.revision, 5);
        assert!(store
            .due_outbox_deadlines(ack_deadline, 10)
            .unwrap()
            .is_empty());
        let retry_due = store.due_outbox_deadlines(retry_at, 10).unwrap();
        assert_eq!(retry_due.len(), 1);
        assert_eq!(retry_due[0].kind, DeadlineKind::RetryEligible);
        assert_eq!(retry_due[0].lane_revision, 5);

        let eligible = store.set_lane_eligible(&key, 5, retry_at).unwrap();
        assert_eq!(eligible.revision, 6);
        assert!(store.due_outbox_deadlines(retry_at, 10).unwrap().is_empty());

        let (second, _) = store
            .start_lane_attempt(&key, 6, signed, Timestamp::from(131))
            .unwrap();
        assert_eq!(second.ordinal, 2);
        assert!(store
            .finish_attempt(intent, &key.relay, 1, AttemptOutcome::Acked)
            .is_err());
        assert_eq!(
            store.recover_outbox_lanes(intent).unwrap()[0].last_ordinal,
            2
        );
        assert_eq!(
            store.recover_attempt_details(intent).unwrap()[0].terminal,
            None
        );
        let terminal = store
            .record_lane_handoff(
                &key,
                7,
                2,
                AttemptHandoffDetail {
                    at: Timestamp::from(132),
                    result: HandoffEvidence::Written,
                },
                PostHandoffState::Terminal {
                    outcome: AttemptOutcome::Acked,
                    finished_at: Timestamp::from(133),
                },
            )
            .unwrap();
        assert_eq!(terminal.revision, 8);
        assert_eq!(
            terminal.state,
            LaneState::Terminal {
                ordinal: 2,
                outcome: AttemptOutcome::Acked
            }
        );
        let details = store.recover_attempt_details(intent).unwrap();
        assert_eq!(details.len(), 2);
        assert_eq!(
            details[0].handoff.as_ref().unwrap().result,
            HandoffEvidence::Written
        );
        assert_eq!(details[1].terminal, Some(AttemptOutcome::Acked));

        assert_eq!(
            store.close_terminal_intent(intent).unwrap(),
            CloseIntentOutcome::Closed
        );
        assert_eq!(
            store.close_terminal_intent(intent).unwrap(),
            CloseIntentOutcome::AlreadyClosed
        );
        assert!(store.reattach_receipt(receipt).unwrap().is_some());
        assert_eq!(store.recover_outbox_lanes(intent).unwrap(), vec![terminal]);
        assert_eq!(store.recover_attempts(intent).unwrap().len(), 2);
        assert_eq!(store.recover_attempt_details(intent).unwrap().len(), 2);
    });
}

#[test]
fn due_deadlines_are_ordered_bounded_and_close_rejects_nonterminal_lanes() {
    for_each_backend(|store| {
        let empty_keys = Keys::generate();
        let (_, empty_frozen) = signed_and_frozen(&empty_keys, "no routes", 190);
        let empty_intent = store
            .accept_write(accept(empty_frozen, &empty_keys, 190))
            .unwrap()
            .journaled_intent_id()
            .unwrap();
        assert!(store
            .bootstrap_outbox_lanes(empty_intent)
            .unwrap()
            .is_empty());
        assert!(store.close_terminal_intent(empty_intent).is_err());

        let inputs = [("late", 30_u64), ("early", 10), ("middle", 20)];
        let mut keys = Vec::new();
        for (index, (name, deadline)) in inputs.into_iter().enumerate() {
            let relay = RelayUrl::parse(&format!("wss://{name}.deadlines.example")).unwrap();
            let (intent, _, _, key, _) = seed(store, name, 200 + index as u64, relay);
            store
                .set_lane_transient(
                    &key,
                    1,
                    0,
                    Timestamp::from(deadline),
                    TransientCause::ConnectionLost,
                    None,
                )
                .unwrap();
            assert!(store.close_terminal_intent(intent).is_err());
            keys.push((key, deadline));
        }

        assert_eq!(
            store.next_outbox_deadline().unwrap(),
            Some(Timestamp::from(10))
        );
        let due = store.due_outbox_deadlines(Timestamp::from(30), 2).unwrap();
        assert_eq!(due.len(), 2);
        assert_eq!(
            due.iter().map(|row| row.at.as_secs()).collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert_eq!(
            due.iter().map(|row| row.kind).collect::<Vec<_>>(),
            vec![DeadlineKind::RetryEligible; 2]
        );
        assert!(store
            .due_outbox_deadlines(Timestamp::from(30), 0)
            .unwrap()
            .is_empty());
    });
}

#[test]
fn deadline_scale_read_returns_only_the_ordered_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = RedbStore::open(dir.path().join("deadline-scale.redb")).unwrap();
    for index in (0..128u64).rev() {
        let relay = RelayUrl::parse(&format!("wss://scale-{index:03}.example")).unwrap();
        let (_, _, _, key, lane) = seed(&mut store, &format!("scale-{index}"), 500 + index, relay);
        store
            .set_lane_transient(
                &key,
                lane.revision,
                0,
                Timestamp::from(10_000 + index),
                TransientCause::ConnectionLost,
                None,
            )
            .unwrap();
    }
    let due = store
        .due_outbox_deadlines(Timestamp::from(20_000), 7)
        .unwrap();
    assert_eq!(due.len(), 7);
    assert_eq!(
        due.iter().map(|row| row.at.as_secs()).collect::<Vec<_>>(),
        (10_000..10_007).collect::<Vec<_>>()
    );
    assert!(store
        .due_outbox_deadlines(Timestamp::from(20_000), 1_025)
        .unwrap_err()
        .to_string()
        .contains("limit"));
}

#[test]
fn equal_time_equal_intent_deadlines_use_canonical_relay_order_on_both_backends() {
    for_each_backend(|store| {
        let keys = Keys::generate();
        let (signed, frozen) = signed_and_frozen(&keys, "same-time", 640);
        let intent = store
            .accept_write(accept(frozen, &keys, 640))
            .unwrap()
            .journaled_intent_id()
            .unwrap();
        store.promote_signed(intent, signed.sig).unwrap();
        let relays = BTreeSet::from([
            RelayUrl::parse("wss://z.example").unwrap(),
            RelayUrl::parse("wss://aa.example").unwrap(),
            RelayUrl::parse("wss://a.example/path").unwrap(),
        ]);
        store.record_route_revision(intent, relays.clone()).unwrap();
        for lane in store.bootstrap_outbox_lanes(intent).unwrap() {
            store
                .set_lane_transient(
                    &lane.key,
                    lane.revision,
                    0,
                    Timestamp::from(700),
                    TransientCause::ConnectionLost,
                    None,
                )
                .unwrap();
        }
        assert_eq!(
            store
                .due_outbox_deadlines(Timestamp::from(700), 10)
                .unwrap()
                .into_iter()
                .map(|deadline| deadline.key.relay)
                .collect::<Vec<_>>(),
            relays.into_iter().collect::<Vec<_>>()
        );
    });
}

#[test]
fn relay_identity_uses_canonical_url_but_preserves_meaningful_path_slashes() {
    for_each_backend(|store| {
        let keys = Keys::generate();
        let (signed, frozen) = signed_and_frozen(&keys, "canonical-relay", 710);
        let intent = store
            .accept_write(accept(frozen, &keys, 710))
            .unwrap()
            .journaled_intent_id()
            .unwrap();
        store.promote_signed(intent, signed.sig).unwrap();
        let root_plain = RelayUrl::parse("wss://same.example").unwrap();
        let root_slash = RelayUrl::parse("wss://same.example/").unwrap();
        assert_eq!(root_plain, root_slash);
        let path_plain = RelayUrl::parse("wss://same.example/foo").unwrap();
        let path_slash = RelayUrl::parse("wss://same.example/foo/").unwrap();
        assert_ne!(path_plain, path_slash);
        let relays = BTreeSet::from([
            root_plain.clone(),
            root_slash.clone(),
            path_plain.clone(),
            path_slash.clone(),
        ]);
        assert_eq!(relays.len(), 3);
        store.record_route_revision(intent, relays.clone()).unwrap();
        let lanes = store.bootstrap_outbox_lanes(intent).unwrap();
        assert_eq!(lanes.len(), 3);
        let root = lanes
            .iter()
            .find(|lane| lane.key.relay == root_plain)
            .unwrap();
        store
            .set_lane_transient(
                &LaneKey {
                    intent_id: intent,
                    relay: root_slash,
                },
                root.revision,
                0,
                Timestamp::from(711),
                TransientCause::ConnectionLost,
                None,
            )
            .unwrap();
        assert_eq!(store.recover_outbox_lanes(intent).unwrap().len(), 3);
        assert!(store
            .recover_outbox_lanes(intent)
            .unwrap()
            .iter()
            .any(|lane| lane.key.relay == path_plain));
        assert!(store
            .recover_outbox_lanes(intent)
            .unwrap()
            .iter()
            .any(|lane| lane.key.relay == path_slash));
    });
}

#[test]
fn legacy_start_rejects_a_second_live_ordinal_and_bootstrap_cannot_hide_one() {
    for_each_backend(|store| {
        let relay = RelayUrl::parse("wss://legacy-live.example").unwrap();
        let (intent, _, signed, _, _) = seed(store, "legacy-live", 720, relay.clone());
        store
            .start_attempt(intent, relay.clone(), signed.clone())
            .unwrap();
        assert!(store
            .start_attempt(intent, relay, signed)
            .unwrap_err()
            .to_string()
            .contains("current attempt is live"));
        assert_eq!(store.recover_attempts(intent).unwrap().len(), 1);
    });

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("existing-lane-contradiction.redb");
    let relay = RelayUrl::parse("wss://existing-lane.example").unwrap();
    let (intent, signed) = {
        let mut store = reopen(&path);
        let (intent, _, signed, _, _) = seed(&mut store, "existing-lane", 721, relay.clone());
        store
            .start_attempt(intent, relay.clone(), signed.clone())
            .unwrap();
        (intent, signed)
    };
    insert_legacy_attempt(&path, intent, &relay, 2, &signed, AttemptOutcome::Started);
    let mut store = reopen(&path);
    assert!(store
        .bootstrap_outbox_lanes(intent)
        .unwrap_err()
        .to_string()
        .contains("contradictory live"));
}

fn reopen(path: &Path) -> RedbStore {
    RedbStore::open(path).expect("reopen durable store")
}

fn relay_hex(relay: &RelayUrl) -> String {
    let canonical: &nostr::Url = relay.into();
    canonical
        .as_str()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn raw_lane_key(intent: IntentId, relay: &RelayUrl) -> String {
    let canonical: &nostr::Url = relay.into();
    let canonical = canonical.as_str();
    format!("{:020}:{:020}:{canonical}", intent.0, canonical.len())
}

fn insert_legacy_attempt(
    path: &Path,
    intent: IntentId,
    relay: &RelayUrl,
    ordinal: u64,
    event: &Event,
    outcome: AttemptOutcome,
) {
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let attempts: TableDefinition<&str, &str> = TableDefinition::new("outbox_attempts");
        let mut table = write.open_table(attempts).unwrap();
        let key = format!(
            "{:020}:{:020}:{}:{:020}",
            intent.0,
            relay.as_str().len(),
            relay.as_str(),
            ordinal
        );
        let value = serde_json::json!({
            "version": 1,
            "intent_id": intent,
            "relay": relay,
            "ordinal": ordinal,
            "event_json": event.as_json(),
            "outcome": outcome,
        });
        table
            .insert(
                key.as_str(),
                serde_json::to_string(&value).unwrap().as_str(),
            )
            .unwrap();
    }
    write.commit().unwrap();
}

fn rewrite_json_row(path: &Path, table_name: &'static str, key: &str, field: &str) {
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let definition: TableDefinition<&str, &str> = TableDefinition::new(table_name);
        let mut table = write.open_table(definition).unwrap();
        let raw = table
            .get(key)
            .unwrap()
            .expect("raw corruption target must exist")
            .value()
            .to_string();
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        value[field] = serde_json::json!(99);
        table
            .insert(key, serde_json::to_string(&value).unwrap().as_str())
            .unwrap();
    }
    write.commit().unwrap();
}

fn rewrite_lane_state(path: &Path, key: &str, state: serde_json::Value) {
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let definition: TableDefinition<&str, &str> = TableDefinition::new("outbox_lanes");
        let mut table = write.open_table(definition).unwrap();
        let raw = table
            .get(key)
            .unwrap()
            .expect("raw lane target must exist")
            .value()
            .to_string();
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        value["state"] = state;
        table
            .insert(key, serde_json::to_string(&value).unwrap().as_str())
            .unwrap();
    }
    write.commit().unwrap();
}

#[test]
fn redb_bootstrap_rejects_cross_table_terminal_state_contradictions() {
    for terminal_attempt in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(if terminal_attempt {
            "terminal-attempt-waiting-lane.redb"
        } else {
            "live-attempt-terminal-lane.redb"
        });
        let relay = RelayUrl::parse(if terminal_attempt {
            "wss://terminal-attempt.example"
        } else {
            "wss://live-attempt.example"
        })
        .unwrap();
        let intent = {
            let mut store = reopen(&path);
            let (intent, _, signed, _, _) = seed(&mut store, "state-mismatch", 274, relay.clone());
            store.start_attempt(intent, relay.clone(), signed).unwrap();
            if terminal_attempt {
                store
                    .finish_attempt(intent, &relay, 1, AttemptOutcome::Acked)
                    .unwrap();
            }
            intent
        };
        rewrite_lane_state(
            &path,
            &raw_lane_key(intent, &relay),
            if terminal_attempt {
                serde_json::json!("WaitingConnection")
            } else {
                serde_json::json!({"Terminal": {"ordinal": 1, "outcome": "Acked"}})
            },
        );
        let mut store = reopen(&path);
        let error = store
            .bootstrap_outbox_lanes(intent)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("terminal attempt and lane") || error.contains("terminal lane lacks"),
            "{error}"
        );
    }
}

fn insert_stale_deadline(path: &Path, deadline: &LaneDeadline) {
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let ordered: TableDefinition<&str, &str> = TableDefinition::new("outbox_deadlines");
        let by_intent: TableDefinition<&str, &str> =
            TableDefinition::new("outbox_deadlines_by_intent");
        let mut ordered = write.open_table(ordered).unwrap();
        let mut by_intent = write.open_table(by_intent).unwrap();
        let relay = relay_hex(&deadline.key.relay);
        let ordered_key = format!(
            "{:020}:{:020}:{relay}",
            deadline.at.as_secs(),
            deadline.key.intent_id.0
        );
        let by_intent_key = format!(
            "{:020}:{:020}:{relay}",
            deadline.key.intent_id.0,
            deadline.at.as_secs()
        );
        let encoded = serde_json::to_string(deadline).unwrap();
        ordered
            .insert(ordered_key.as_str(), encoded.as_str())
            .unwrap();
        by_intent
            .insert(by_intent_key.as_str(), encoded.as_str())
            .unwrap();
    }
    write.commit().unwrap();
}

fn insert_one_sided_deadline(path: &Path, deadline: &LaneDeadline, primary: bool) {
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let relay = relay_hex(&deadline.key.relay);
        let encoded = serde_json::to_string(deadline).unwrap();
        if primary {
            let definition: TableDefinition<&str, &str> = TableDefinition::new("outbox_deadlines");
            let mut table = write.open_table(definition).unwrap();
            let key = format!(
                "{:020}:{:020}:{relay}",
                deadline.at.as_secs(),
                deadline.key.intent_id.0
            );
            table.insert(key.as_str(), encoded.as_str()).unwrap();
        } else {
            let definition: TableDefinition<&str, &str> =
                TableDefinition::new("outbox_deadlines_by_intent");
            let mut table = write.open_table(definition).unwrap();
            let key = format!(
                "{:020}:{:020}:{relay}",
                deadline.key.intent_id.0,
                deadline.at.as_secs()
            );
            table.insert(key.as_str(), encoded.as_str()).unwrap();
        }
    }
    write.commit().unwrap();
}

#[test]
fn one_sided_deadline_index_corruption_fails_closed_before_close() {
    for primary in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(if primary {
            "primary-only.redb"
        } else {
            "secondary-only.redb"
        });
        let relay = RelayUrl::parse(if primary {
            "wss://primary-only.example"
        } else {
            "wss://secondary-only.example"
        })
        .unwrap();
        let (intent, key) = {
            let mut store = reopen(&path);
            let (intent, _, signed, key, lane) = seed(&mut store, "one-sided", 275, relay);
            let lane = store
                .set_lane_eligible(&key, lane.revision, Timestamp::from(276))
                .unwrap();
            let (_, lane) = store
                .start_lane_attempt(&key, lane.revision, signed, Timestamp::from(277))
                .unwrap();
            store
                .record_lane_handoff(
                    &key,
                    lane.revision,
                    1,
                    AttemptHandoffDetail {
                        at: Timestamp::from(278),
                        result: HandoffEvidence::Written,
                    },
                    PostHandoffState::Terminal {
                        outcome: AttemptOutcome::Acked,
                        finished_at: Timestamp::from(279),
                    },
                )
                .unwrap();
            (intent, key)
        };
        insert_one_sided_deadline(
            &path,
            &LaneDeadline {
                at: Timestamp::from(999),
                key,
                lane_revision: 4,
                kind: DeadlineKind::AckTimeout,
            },
            primary,
        );
        let mut store = reopen(&path);
        assert!(store
            .due_outbox_deadlines(Timestamp::from(999), 1)
            .unwrap_err()
            .to_string()
            .contains("cardinalities"));
        assert!(store
            .next_outbox_deadline()
            .unwrap_err()
            .to_string()
            .contains("cardinalities"));
        assert!(store
            .close_terminal_intent(intent)
            .unwrap_err()
            .to_string()
            .contains("cardinalities"));
        assert!(store
            .recover_outbox()
            .iter()
            .any(|open| open.intent_id == intent));
    }
}

#[test]
fn lane_detail_and_deadline_corruption_fail_closed() {
    for target in ["lane", "detail", "deadline"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("corrupt-{target}.redb"));
        let relay = RelayUrl::parse(&format!("wss://corrupt-{target}.example")).unwrap();
        let (intent, key) = {
            let mut store = reopen(&path);
            let (intent, _, signed, key, lane) = seed(&mut store, target, 270, relay);
            let lane = store
                .set_lane_eligible(&key, lane.revision, Timestamp::from(271))
                .unwrap();
            let (_, lane) = store
                .start_lane_attempt(&key, lane.revision, signed.clone(), Timestamp::from(272))
                .unwrap();
            store
                .record_lane_handoff(
                    &key,
                    lane.revision,
                    1,
                    AttemptHandoffDetail {
                        at: Timestamp::from(273),
                        result: HandoffEvidence::Written,
                    },
                    PostHandoffState::AwaitingAck {
                        deadline: Timestamp::from(300),
                    },
                )
                .unwrap();
            (intent, key)
        };
        let lane_storage_key = raw_lane_key(intent, &key.relay);
        let attempt_storage_key = format!(
            "{:020}:{:020}:{}:{:020}",
            intent.0,
            key.relay.as_str().len(),
            key.relay.as_str(),
            1
        );
        let deadline_storage_key =
            format!("{:020}:{:020}:{}", 300, intent.0, relay_hex(&key.relay));
        match target {
            "lane" => {
                rewrite_json_row(&path, "outbox_lanes", &lane_storage_key, "version");
                assert!(reopen(&path).recover_outbox_lanes(intent).is_err());
            }
            "detail" => {
                rewrite_json_row(
                    &path,
                    "outbox_attempt_details",
                    &attempt_storage_key,
                    "version",
                );
                assert!(reopen(&path).recover_attempt_details(intent).is_err());
            }
            "deadline" => {
                rewrite_json_row(
                    &path,
                    "outbox_deadlines",
                    &deadline_storage_key,
                    "lane_revision",
                );
                assert!(reopen(&path)
                    .due_outbox_deadlines(Timestamp::from(300), 1)
                    .is_err());
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn legacy_v1_bootstrap_is_deterministic_and_rejects_contradictory_live_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-bootstrap.redb");
    let relay = RelayUrl::parse("wss://legacy-bootstrap.example").unwrap();
    let (intent, signed) = {
        let mut store = reopen(&path);
        let keys = Keys::generate();
        let (signed, frozen) = signed_and_frozen(&keys, "legacy", 250);
        let intent = store
            .accept_write(accept(frozen, &keys, 250))
            .unwrap()
            .journaled_intent_id()
            .unwrap();
        store.promote_signed(intent, signed.sig).unwrap();
        store
            .record_route_revision(intent, BTreeSet::from([relay.clone()]))
            .unwrap();
        (intent, signed)
    };
    insert_legacy_attempt(&path, intent, &relay, 1, &signed, AttemptOutcome::Started);
    let mut store = reopen(&path);
    let lane = store.bootstrap_outbox_lanes(intent).unwrap().remove(0);
    assert_eq!(lane.last_ordinal, 1);
    assert_eq!(lane.state, LaneState::LegacyInFlight { ordinal: 1 });
    assert_eq!(store.bootstrap_outbox_lanes(intent).unwrap(), vec![lane]);

    let second_path = dir.path().join("contradictory-bootstrap.redb");
    let (second_intent, second_signed) = {
        let mut store = reopen(&second_path);
        let keys = Keys::generate();
        let (signed, frozen) = signed_and_frozen(&keys, "contradictory", 251);
        let intent = store
            .accept_write(accept(frozen, &keys, 251))
            .unwrap()
            .journaled_intent_id()
            .unwrap();
        store.promote_signed(intent, signed.sig).unwrap();
        store
            .record_route_revision(intent, BTreeSet::from([relay.clone()]))
            .unwrap();
        (intent, signed)
    };
    for ordinal in [1, 2] {
        insert_legacy_attempt(
            &second_path,
            second_intent,
            &relay,
            ordinal,
            &second_signed,
            AttemptOutcome::Started,
        );
    }
    let mut store = reopen(&second_path);
    assert!(store
        .bootstrap_outbox_lanes(second_intent)
        .unwrap_err()
        .to_string()
        .contains("live v1 Started"));
    assert!(store
        .recover_outbox_lanes(second_intent)
        .unwrap()
        .is_empty());
}

#[test]
fn genuine_detail_less_legacy_rows_adopt_for_current_recovery_doors() {
    for (name, raw_outcome) in [
        ("at-most-once", AttemptOutcome::Started),
        ("durable-current", AttemptOutcome::Started),
        ("durable-planned", AttemptOutcome::Started),
        ("old-terminal", AttemptOutcome::Acked),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{name}.redb"));
        let relay = RelayUrl::parse(&format!("wss://{name}.legacy.example")).unwrap();
        let (intent, signed) = {
            let mut store = reopen(&path);
            let keys = Keys::generate();
            let (signed, frozen) = signed_and_frozen(&keys, name, 260);
            let intent = store
                .accept_write(accept(frozen, &keys, 260))
                .unwrap()
                .journaled_intent_id()
                .unwrap();
            store.promote_signed(intent, signed.sig).unwrap();
            store
                .record_route_revision(intent, BTreeSet::from([relay.clone()]))
                .unwrap();
            (intent, signed)
        };
        insert_legacy_attempt(&path, intent, &relay, 1, &signed, raw_outcome.clone());

        let mut store = reopen(&path);
        match name {
            "at-most-once" => {
                assert_eq!(
                    store
                        .finish_attempt(intent, &relay, 1, AttemptOutcome::OutcomeUnknown)
                        .unwrap(),
                    nmp_store::FinishAttemptOutcome::Committed
                );
                assert_eq!(
                    store.recover_attempts(intent).unwrap()[0].outcome,
                    AttemptOutcome::OutcomeUnknown
                );
            }
            "durable-current" => {
                assert_eq!(
                    store
                        .finish_attempt(intent, &relay, 1, AttemptOutcome::Acked)
                        .unwrap(),
                    nmp_store::FinishAttemptOutcome::Committed
                );
                assert_eq!(
                    store.recover_attempts(intent).unwrap()[0].outcome,
                    AttemptOutcome::Acked
                );
            }
            "durable-planned" => {
                let lane = store.bootstrap_outbox_lanes(intent).unwrap().remove(0);
                assert_eq!(lane.state, LaneState::LegacyInFlight { ordinal: 1 });
                assert_eq!(store.recover_attempt_details(intent).unwrap().len(), 1);
                store
                    .set_lane_transient(
                        &lane.key,
                        lane.revision,
                        1,
                        Timestamp::from(280),
                        TransientCause::Interrupted,
                        None,
                    )
                    .unwrap();
            }
            "old-terminal" => {
                assert_eq!(
                    store
                        .finish_attempt(intent, &relay, 1, AttemptOutcome::Acked)
                        .unwrap(),
                    nmp_store::FinishAttemptOutcome::AlreadySame
                );
                assert!(store
                    .finish_attempt(intent, &relay, 1, AttemptOutcome::GaveUp)
                    .unwrap_err()
                    .to_string()
                    .contains("conflicting"));
                assert_eq!(
                    store.recover_outbox_lanes(intent).unwrap()[0].state,
                    LaneState::Terminal {
                        ordinal: 1,
                        outcome: AttemptOutcome::Acked,
                    }
                );
                assert_eq!(store.recover_attempt_details(intent).unwrap().len(), 1);
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn redb_lane_attempt_detail_deadline_and_close_survive_real_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.redb");
    let relay = RelayUrl::parse("wss://reopen-lane.example").unwrap();

    let (intent, receipt, signed, key) = {
        let mut store = reopen(&path);
        let (intent, receipt, signed, key, _) = seed(&mut store, "reopen", 300, relay);
        (intent, receipt, signed, key)
    };
    {
        let mut store = reopen(&path);
        assert_eq!(store.recover_outbox_lanes(intent).unwrap()[0].revision, 1);
        store
            .set_lane_eligible(&key, 1, Timestamp::from(301))
            .unwrap();
        store
            .start_lane_attempt(&key, 2, signed.clone(), Timestamp::from(302))
            .unwrap();
    }
    {
        let mut store = reopen(&path);
        assert_eq!(store.recover_attempts(intent).unwrap()[0].ordinal, 1);
        assert_eq!(
            store.recover_attempt_details(intent).unwrap()[0].started_at,
            Some(Timestamp::from(302))
        );
        store
            .record_lane_handoff(
                &key,
                3,
                1,
                AttemptHandoffDetail {
                    at: Timestamp::from(303),
                    result: HandoffEvidence::Ambiguous,
                },
                PostHandoffState::AwaitingAck {
                    deadline: Timestamp::from(310),
                },
            )
            .unwrap();
    }
    {
        let mut store = reopen(&path);
        let due = store.due_outbox_deadlines(Timestamp::from(310), 1).unwrap();
        assert_eq!(
            (due[0].kind, due[0].lane_revision),
            (DeadlineKind::AckTimeout, 4)
        );
        store
            .set_lane_transient(
                &key,
                4,
                1,
                Timestamp::from(311),
                TransientCause::AckTimeout,
                None,
            )
            .unwrap();
    }
    {
        let mut store = reopen(&path);
        assert_eq!(
            store.due_outbox_deadlines(Timestamp::from(311), 1).unwrap()[0].kind,
            DeadlineKind::RetryEligible
        );
        store
            .set_lane_eligible(&key, 5, Timestamp::from(311))
            .unwrap();
        store
            .start_lane_attempt(&key, 6, signed, Timestamp::from(312))
            .unwrap();
    }
    {
        let mut store = reopen(&path);
        store
            .record_lane_handoff(
                &key,
                7,
                2,
                AttemptHandoffDetail {
                    at: Timestamp::from(313),
                    result: HandoffEvidence::Written,
                },
                PostHandoffState::Terminal {
                    outcome: AttemptOutcome::OutcomeUnknown,
                    finished_at: Timestamp::from(314),
                },
            )
            .unwrap();
    }
    insert_stale_deadline(
        &path,
        &LaneDeadline {
            at: Timestamp::from(999),
            key: key.clone(),
            lane_revision: 7,
            kind: DeadlineKind::AckTimeout,
        },
    );
    {
        let mut store = reopen(&path);
        assert!(store
            .next_outbox_deadline()
            .unwrap_err()
            .to_string()
            .contains("deadline and lane disagree"));
        assert_eq!(
            store.close_terminal_intent(intent).unwrap(),
            CloseIntentOutcome::Closed
        );
        assert_eq!(store.next_outbox_deadline().unwrap(), None);
    }
    {
        let store = reopen(&path);
        assert!(!store
            .recover_outbox()
            .iter()
            .any(|row| row.intent_id == intent));
        assert_eq!(store.recover_outbox_lanes(intent).unwrap().len(), 1);
        assert_eq!(store.recover_attempts(intent).unwrap().len(), 2);
        assert_eq!(store.recover_attempt_details(intent).unwrap().len(), 2);
        assert!(store.reattach_receipt(receipt).unwrap().is_some());
        assert_eq!(store.next_outbox_deadline().unwrap(), None);
    }
    // New attempts remain immutable Started facts; terminal state overlays
    // from additive details while legacy terminal v1 rows remain readable.
    let db = Database::open(&path).unwrap();
    let read = db.begin_read().unwrap();
    let attempts: TableDefinition<&str, &str> = TableDefinition::new("outbox_attempts");
    let table = read.open_table(attempts).unwrap();
    let raw_key = format!(
        "{:020}:{:020}:{}:{:020}",
        intent.0,
        key.relay.as_str().len(),
        key.relay.as_str(),
        2
    );
    let raw: serde_json::Value =
        serde_json::from_str(table.get(raw_key.as_str()).unwrap().unwrap().value()).unwrap();
    assert_eq!(raw["outcome"], serde_json::json!("Started"));
}
