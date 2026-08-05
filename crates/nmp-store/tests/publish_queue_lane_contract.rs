//! Publish-queue lane substrate contract (issue #94).

use std::collections::BTreeSet;
use std::path::Path;

use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, AuthDenial, AuthDenialSource,
    CloseIntentOutcome, EventStore, HandoffEvidence, IntentId, IntentSigState, MemoryStore,
    PublishQueueAttemptHandoff, PublishQueueAttemptOutcome, PublishQueueDeadline,
    PublishQueueDeadlineKind, PublishQueueInFlightPhase, PublishQueueLane, PublishQueueLaneKey,
    PublishQueueLaneState, PublishQueuePostHandoffState, PublishQueueTerminalOutcome,
    PublishQueueTransientCause, RedbStore,
};
use nostr::{Event, EventBuilder, Keys, Kind, RelayUrl, Timestamp};
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
        replaceable_base: None,
        monotonic_stamp: false,
        expected_pubkey: keys.public_key(),
        signing_identity_ref: "lane-contract".into(),
        routing: "lane-contract".into(),
        sig_state: IntentSigState::Pending,
        accepted_at: Timestamp::from(accepted_at),
        correlation: None,
    }
}

fn seed(
    store: &mut dyn EventStore,
    content: &str,
    created_at: u64,
    relay: RelayUrl,
) -> (IntentId, u64, Event, PublishQueueLaneKey, PublishQueueLane) {
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
    let lanes = store.bootstrap_publish_queue_lanes(intent_id).unwrap();
    assert_eq!(lanes.len(), 1);
    let lane = lanes[0].clone();
    assert_eq!(lane.revision, 1);
    assert_eq!(lane.last_ordinal, 0);
    assert_eq!(lane.state, PublishQueueLaneState::WaitingConnection);
    let key = PublishQueueLaneKey { intent_id, relay };
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
        assert_eq!(
            store.bootstrap_publish_queue_lanes(intent).unwrap(),
            vec![seeded]
        );

        let eligible = store
            .set_lane_eligible(&key, 1, Timestamp::from(101))
            .unwrap();
        assert_eq!(eligible.revision, 2);
        assert_eq!(
            eligible.state,
            PublishQueueLaneState::Eligible {
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
        assert_eq!(first.outcome, PublishQueueAttemptOutcome::Started);
        assert_eq!(awaiting_handoff.revision, 3);
        assert_eq!(
            awaiting_handoff.state,
            PublishQueueLaneState::InFlight {
                ordinal: 1,
                phase: PublishQueueInFlightPhase::AwaitingHandoff,
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
                PublishQueueAttemptHandoff {
                    at: Timestamp::from(102),
                    result: HandoffEvidence::NotHandedOff,
                },
                PublishQueuePostHandoffState::Terminal {
                    outcome: PublishQueueAttemptOutcome::Started,
                    finished_at: Timestamp::from(102),
                },
            )
            .is_err());
        assert_eq!(
            store.recover_attempt_details(intent).unwrap()[0].handoff,
            None,
            "an invalid handoff transition must leave no detail mutation"
        );
        assert_eq!(
            store.recover_publish_queue_lanes(intent).unwrap()[0].revision,
            3
        );

        let ack_deadline = Timestamp::from(120);
        let awaiting_ack = store
            .record_lane_handoff(
                &key,
                3,
                1,
                PublishQueueAttemptHandoff {
                    at: Timestamp::from(103),
                    result: HandoffEvidence::Written,
                },
                PublishQueuePostHandoffState::AwaitingAck {
                    deadline: ack_deadline,
                },
            )
            .unwrap();
        assert_eq!(awaiting_ack.revision, 4);
        assert_eq!(
            store.due_publish_queue_deadlines(ack_deadline, 10).unwrap()[0].kind,
            PublishQueueDeadlineKind::AckTimeout
        );

        let retry_at = Timestamp::from(130);
        let transient = store
            .set_lane_transient(
                &key,
                4,
                1,
                retry_at,
                PublishQueueTransientCause::AckTimeout,
                Some("ack deadline elapsed".into()),
            )
            .unwrap();
        assert_eq!(transient.revision, 5);
        assert!(store
            .due_publish_queue_deadlines(ack_deadline, 10)
            .unwrap()
            .is_empty());
        let retry_due = store.due_publish_queue_deadlines(retry_at, 10).unwrap();
        assert_eq!(retry_due.len(), 1);
        assert_eq!(retry_due[0].kind, PublishQueueDeadlineKind::RetryEligible);
        assert_eq!(retry_due[0].lane_revision, 5);

        let eligible = store.set_lane_eligible(&key, 5, retry_at).unwrap();
        assert_eq!(eligible.revision, 6);
        assert!(store
            .due_publish_queue_deadlines(retry_at, 10)
            .unwrap()
            .is_empty());

        let (second, _) = store
            .start_lane_attempt(&key, 6, signed, Timestamp::from(131))
            .unwrap();
        assert_eq!(second.ordinal, 2);
        assert_eq!(
            store.recover_publish_queue_lanes(intent).unwrap()[0].last_ordinal,
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
                PublishQueueAttemptHandoff {
                    at: Timestamp::from(132),
                    result: HandoffEvidence::Written,
                },
                PublishQueuePostHandoffState::Terminal {
                    outcome: PublishQueueAttemptOutcome::Acked,
                    finished_at: Timestamp::from(133),
                },
            )
            .unwrap();
        assert_eq!(terminal.revision, 8);
        assert_eq!(
            terminal.state,
            PublishQueueLaneState::Terminal {
                ordinal: 2,
                outcome: PublishQueueTerminalOutcome::Acked
            }
        );
        let details = store.recover_attempt_details(intent).unwrap();
        assert_eq!(details.len(), 2);
        assert_eq!(
            details[0].handoff.as_ref().unwrap().result,
            HandoffEvidence::Written
        );
        assert_eq!(details[1].terminal, Some(PublishQueueAttemptOutcome::Acked));

        assert_eq!(
            store.close_terminal_intent(intent).unwrap(),
            CloseIntentOutcome::Closed
        );
        assert_eq!(
            store.close_terminal_intent(intent).unwrap(),
            CloseIntentOutcome::AlreadyClosed
        );
        assert!(store.reattach_receipt(receipt).unwrap().is_some());
        assert_eq!(
            store.recover_publish_queue_lanes(intent).unwrap(),
            vec![terminal]
        );
        assert_eq!(store.recover_attempts(intent).unwrap().len(), 2);
        assert_eq!(store.recover_attempt_details(intent).unwrap().len(), 2);
    });
}

#[test]
fn suspended_attempt_is_atomic_deadline_free_and_resumes_with_the_next_ordinal() {
    for_each_backend(|store| {
        let relay = RelayUrl::parse("wss://waiting-auth.example").unwrap();
        let (intent, _, signed, key, _) = seed(store, "waiting auth", 150, relay);
        store
            .set_lane_eligible(&key, 1, Timestamp::from(151))
            .unwrap();
        store
            .start_lane_attempt(&key, 2, signed.clone(), Timestamp::from(152))
            .unwrap();

        let waiting = store
            .suspend_lane_attempt(
                &key,
                3,
                1,
                Timestamp::from(153),
                PublishQueueTransientCause::AuthRequired,
                Some("auth-required: authenticate".into()),
                true,
            )
            .unwrap();
        assert_eq!(waiting.revision, 4);
        assert_eq!(waiting.state, PublishQueueLaneState::WaitingAuth);
        assert_eq!(store.next_publish_queue_deadline().unwrap(), None);
        assert!(store
            .due_publish_queue_deadlines(Timestamp::from(u64::MAX), 10)
            .unwrap()
            .is_empty());
        let details = store.recover_attempt_details(intent).unwrap();
        let transient = details[0].transient.as_ref().unwrap();
        assert_eq!(transient.eligible_at, Timestamp::from(153));
        assert_eq!(transient.cause, PublishQueueTransientCause::AuthRequired);
        assert_eq!(
            transient.raw_reason.as_deref(),
            Some("auth-required: authenticate")
        );

        store
            .set_lane_eligible(&key, 4, Timestamp::from(200))
            .unwrap();
        let (second, _) = store
            .start_lane_attempt(&key, 5, signed, Timestamp::from(200))
            .unwrap();
        assert_eq!(second.ordinal, 2);
    });
}

#[test]
fn auth_denial_is_a_durable_terminal_lane_fact_and_revision_precedes_idempotence() {
    for_each_backend(|store| {
        let relay = RelayUrl::parse("wss://auth-denial.example").unwrap();
        let (intent, _, _, key, seeded) = seed(store, "auth denial", 175, relay);
        let waiting = store.set_lane_waiting(&key, seeded.revision, true).unwrap();
        let denial = AuthDenial {
            source: AuthDenialSource::Policy,
            reason: "account not permitted".into(),
        };

        let terminal = store
            .deny_lane_auth(&key, waiting.revision, denial.clone())
            .unwrap();
        assert_eq!(terminal.revision, waiting.revision + 1);
        assert_eq!(terminal.last_ordinal, 0);
        assert_eq!(
            terminal.state,
            PublishQueueLaneState::Terminal {
                ordinal: 0,
                outcome: PublishQueueTerminalOutcome::AuthDenied(denial.clone()),
            }
        );
        assert_eq!(
            store.recover_publish_queue_lanes(intent).unwrap(),
            vec![terminal.clone()]
        );
        assert!(store.recover_attempts(intent).unwrap().is_empty());
        assert!(store.recover_attempt_details(intent).unwrap().is_empty());

        let idempotent = store
            .deny_lane_auth(&key, terminal.revision, denial.clone())
            .unwrap();
        assert_eq!(idempotent, terminal);

        let stale = store
            .deny_lane_auth(&key, waiting.revision, denial)
            .unwrap_err();
        assert!(
            stale.to_string().contains("revision"),
            "stale denial must be refused before idempotence: {stale}"
        );
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
            .bootstrap_publish_queue_lanes(empty_intent)
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
                    PublishQueueTransientCause::ConnectionLost,
                    None,
                )
                .unwrap();
            assert!(store.close_terminal_intent(intent).is_err());
            keys.push((key, deadline));
        }

        assert_eq!(
            store.next_publish_queue_deadline().unwrap(),
            Some(Timestamp::from(10))
        );
        let due = store
            .due_publish_queue_deadlines(Timestamp::from(30), 2)
            .unwrap();
        assert_eq!(due.len(), 2);
        assert_eq!(
            due.iter().map(|row| row.at.as_secs()).collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert_eq!(
            due.iter().map(|row| row.kind).collect::<Vec<_>>(),
            vec![PublishQueueDeadlineKind::RetryEligible; 2]
        );
        assert!(store
            .due_publish_queue_deadlines(Timestamp::from(30), 0)
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
                PublishQueueTransientCause::ConnectionLost,
                None,
            )
            .unwrap();
    }
    let due = store
        .due_publish_queue_deadlines(Timestamp::from(20_000), 7)
        .unwrap();
    assert_eq!(due.len(), 7);
    assert_eq!(
        due.iter().map(|row| row.at.as_secs()).collect::<Vec<_>>(),
        (10_000..10_007).collect::<Vec<_>>()
    );
    assert!(store
        .due_publish_queue_deadlines(Timestamp::from(20_000), 1_025)
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
        for lane in store.bootstrap_publish_queue_lanes(intent).unwrap() {
            store
                .set_lane_transient(
                    &lane.key,
                    lane.revision,
                    0,
                    Timestamp::from(700),
                    PublishQueueTransientCause::ConnectionLost,
                    None,
                )
                .unwrap();
        }
        assert_eq!(
            store
                .due_publish_queue_deadlines(Timestamp::from(700), 10)
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
        let lanes = store.bootstrap_publish_queue_lanes(intent).unwrap();
        assert_eq!(lanes.len(), 3);
        let root = lanes
            .iter()
            .find(|lane| lane.key.relay == root_plain)
            .unwrap();
        store
            .set_lane_transient(
                &PublishQueueLaneKey {
                    intent_id: intent,
                    relay: root_slash,
                },
                root.revision,
                0,
                Timestamp::from(711),
                PublishQueueTransientCause::ConnectionLost,
                None,
            )
            .unwrap();
        assert_eq!(store.recover_publish_queue_lanes(intent).unwrap().len(), 3);
        assert!(store
            .recover_publish_queue_lanes(intent)
            .unwrap()
            .iter()
            .any(|lane| lane.key.relay == path_plain));
        assert!(store
            .recover_publish_queue_lanes(intent)
            .unwrap()
            .iter()
            .any(|lane| lane.key.relay == path_slash));
    });
}

#[test]
fn bootstrap_cannot_hide_two_contradictory_live_ordinals() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("existing-lane-contradiction.redb");
    let relay = RelayUrl::parse("wss://existing-lane.example").unwrap();
    let intent = {
        let mut store = reopen(&path);
        let (intent, _, signed, key, lane) = seed(&mut store, "existing-lane", 721, relay.clone());
        let lane = store
            .set_lane_eligible(&key, lane.revision, Timestamp::from(722))
            .unwrap();
        store
            .start_lane_attempt(&key, lane.revision, signed, Timestamp::from(723))
            .unwrap();
        intent
    };
    duplicate_attempt(&path, intent, &relay, 1, 2, true);
    let mut store = reopen(&path);
    assert!(store
        .bootstrap_publish_queue_lanes(intent)
        .unwrap_err()
        .to_string()
        .contains("contradictory live"));
}

fn reopen(path: &Path) -> RedbStore {
    RedbStore::open(path).expect("reopen durable store")
}

fn raw_relay_id(path: &Path, relay: &RelayUrl) -> u32 {
    const RELAY_IDS: TableDefinition<&[u8], &[u8; 4]> =
        TableDefinition::new("publish_queue_relay_ids");
    let db = Database::open(path).unwrap();
    let read = db.begin_read().unwrap();
    let table = read.open_table(RELAY_IDS).unwrap();
    u32::from_be_bytes(
        *table
            .get(relay.as_str().as_bytes())
            .unwrap()
            .expect("relay is interned")
            .value(),
    )
}

fn raw_lane_key(intent: IntentId, relay_id: u32) -> [u8; 12] {
    let mut key = [0; 12];
    key[..8].copy_from_slice(&intent.0.to_be_bytes());
    key[8..].copy_from_slice(&relay_id.to_be_bytes());
    key
}

fn raw_attempt_key(intent: IntentId, relay_id: u32, ordinal: u64) -> [u8; 20] {
    let mut key = [0; 20];
    key[..12].copy_from_slice(&raw_lane_key(intent, relay_id));
    key[12..].copy_from_slice(&ordinal.to_be_bytes());
    key
}

fn duplicate_attempt(
    path: &Path,
    intent: IntentId,
    relay: &RelayUrl,
    source_ordinal: u64,
    target_ordinal: u64,
    with_detail: bool,
) {
    const ATTEMPTS: TableDefinition<&[u8; 20], &[u8]> =
        TableDefinition::new("publish_queue_attempts");
    const DETAILS: TableDefinition<&[u8; 20], &[u8]> =
        TableDefinition::new("publish_queue_attempt_details");
    let relay_id = raw_relay_id(path, relay);
    let source = raw_attempt_key(intent, relay_id, source_ordinal);
    let target = raw_attempt_key(intent, relay_id, target_ordinal);
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut table = write.open_table(ATTEMPTS).unwrap();
        let encoded = table
            .get(&source)
            .unwrap()
            .expect("source attempt")
            .value()
            .to_vec();
        table.insert(&target, encoded.as_slice()).unwrap();
    }
    if with_detail {
        let mut table = write.open_table(DETAILS).unwrap();
        let encoded = table
            .get(&source)
            .unwrap()
            .expect("source attempt details")
            .value()
            .to_vec();
        table.insert(&target, encoded.as_slice()).unwrap();
    }
    write.commit().unwrap();
}

fn corrupt_fixed_row_version<const N: usize>(
    path: &Path,
    table: TableDefinition<&'static [u8; N], &'static [u8]>,
    key: &[u8; N],
) {
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut table = write.open_table(table).unwrap();
        let mut encoded = table
            .get(key)
            .unwrap()
            .expect("raw corruption target must exist")
            .value()
            .to_vec();
        encoded[4] = 99;
        table.insert(key, encoded.as_slice()).unwrap();
    }
    write.commit().unwrap();
}

fn copy_lane_value(path: &Path, source: &[u8; 12], target: &[u8; 12]) {
    const LANES: TableDefinition<&[u8; 12], &[u8]> = TableDefinition::new("publish_queue_lanes");
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut table = write.open_table(LANES).unwrap();
        let encoded = table
            .get(source)
            .unwrap()
            .expect("source lane")
            .value()
            .to_vec();
        table.insert(target, encoded.as_slice()).unwrap();
    }
    write.commit().unwrap();
}

fn write_lane_value(path: &Path, key: &[u8; 12], encoded: &[u8]) {
    const LANES: TableDefinition<&[u8; 12], &[u8]> = TableDefinition::new("publish_queue_lanes");
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut table = write.open_table(LANES).unwrap();
        table.insert(key, encoded).unwrap();
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
        let donor = RelayUrl::parse(if terminal_attempt {
            "wss://waiting-donor.example"
        } else {
            "wss://terminal-donor.example"
        })
        .unwrap();
        let intent = {
            let mut store = reopen(&path);
            let (intent, _, signed, key, lane) =
                seed(&mut store, "state-mismatch", 274, relay.clone());
            store
                .record_route_revision(intent, BTreeSet::from([relay.clone(), donor.clone()]))
                .unwrap();
            let lanes = store.bootstrap_publish_queue_lanes(intent).unwrap();
            let donor_lane = lanes
                .iter()
                .find(|lane| lane.key.relay == donor)
                .unwrap()
                .clone();
            let target_lane = store
                .set_lane_eligible(&key, lane.revision, Timestamp::from(275))
                .unwrap();
            let (_, target_lane) = store
                .start_lane_attempt(
                    &key,
                    target_lane.revision,
                    signed.clone(),
                    Timestamp::from(276),
                )
                .unwrap();
            if terminal_attempt {
                store
                    .finish_lane_attempt(
                        &key,
                        target_lane.revision,
                        1,
                        PublishQueueAttemptOutcome::Acked,
                        Timestamp::from(277),
                    )
                    .unwrap();
            } else {
                let donor_lane = store
                    .set_lane_eligible(&donor_lane.key, donor_lane.revision, Timestamp::from(275))
                    .unwrap();
                let (_, donor_lane) = store
                    .start_lane_attempt(
                        &donor_lane.key,
                        donor_lane.revision,
                        signed,
                        Timestamp::from(276),
                    )
                    .unwrap();
                store
                    .finish_lane_attempt(
                        &donor_lane.key,
                        donor_lane.revision,
                        1,
                        PublishQueueAttemptOutcome::Acked,
                        Timestamp::from(277),
                    )
                    .unwrap();
            }
            intent
        };
        let target_key = raw_lane_key(intent, raw_relay_id(&path, &relay));
        let donor_key = raw_lane_key(intent, raw_relay_id(&path, &donor));
        if terminal_attempt {
            let mut waiting = b"NMDL\x01\0\0\0".to_vec();
            waiting.extend_from_slice(&4u64.to_be_bytes());
            waiting.extend_from_slice(&1u64.to_be_bytes());
            waiting.push(0);
            write_lane_value(&path, &target_key, &waiting);
        } else {
            copy_lane_value(&path, &donor_key, &target_key);
        }
        let mut store = reopen(&path);
        let error = store
            .bootstrap_publish_queue_lanes(intent)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("terminal attempt and lane") || error.contains("terminal lane lacks"),
            "{error}"
        );
    }
}

fn insert_stale_deadline(path: &Path, deadline: &PublishQueueDeadline) {
    const ORDERED: TableDefinition<&[u8; 20], &[u8]> =
        TableDefinition::new("publish_queue_deadlines");
    const BY_INTENT: TableDefinition<&[u8; 20], &[u8]> =
        TableDefinition::new("publish_queue_deadlines_by_intent");
    let relay_id = raw_relay_id(path, &deadline.key.relay);
    let ordered_key = raw_deadline_key(deadline.at, deadline.key.intent_id, relay_id, false);
    let by_intent_key = raw_deadline_key(deadline.at, deadline.key.intent_id, relay_id, true);
    let encoded = raw_deadline_value(deadline);
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut ordered = write.open_table(ORDERED).unwrap();
        let mut by_intent = write.open_table(BY_INTENT).unwrap();
        ordered.insert(&ordered_key, encoded.as_slice()).unwrap();
        by_intent
            .insert(&by_intent_key, encoded.as_slice())
            .unwrap();
    }
    write.commit().unwrap();
}

fn raw_deadline_key(at: Timestamp, intent: IntentId, relay_id: u32, by_intent: bool) -> [u8; 20] {
    let mut key = [0; 20];
    if by_intent {
        key[..8].copy_from_slice(&intent.0.to_be_bytes());
        key[8..16].copy_from_slice(&at.as_secs().to_be_bytes());
    } else {
        key[..8].copy_from_slice(&at.as_secs().to_be_bytes());
        key[8..16].copy_from_slice(&intent.0.to_be_bytes());
    }
    key[16..].copy_from_slice(&relay_id.to_be_bytes());
    key
}

fn raw_deadline_value(deadline: &PublishQueueDeadline) -> Vec<u8> {
    let mut encoded = b"NMDD\x01\0\0\0".to_vec();
    encoded.extend_from_slice(&deadline.lane_revision.to_be_bytes());
    encoded.push(match deadline.kind {
        PublishQueueDeadlineKind::RetryEligible => 0,
        PublishQueueDeadlineKind::AckTimeout => 1,
    });
    encoded
}

fn insert_one_sided_deadline(path: &Path, deadline: &PublishQueueDeadline, primary: bool) {
    const ORDERED: TableDefinition<&[u8; 20], &[u8]> =
        TableDefinition::new("publish_queue_deadlines");
    const BY_INTENT: TableDefinition<&[u8; 20], &[u8]> =
        TableDefinition::new("publish_queue_deadlines_by_intent");
    let relay_id = raw_relay_id(path, &deadline.key.relay);
    let key = raw_deadline_key(deadline.at, deadline.key.intent_id, relay_id, !primary);
    let encoded = raw_deadline_value(deadline);
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        if primary {
            let mut table = write.open_table(ORDERED).unwrap();
            table.insert(&key, encoded.as_slice()).unwrap();
        } else {
            let mut table = write.open_table(BY_INTENT).unwrap();
            table.insert(&key, encoded.as_slice()).unwrap();
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
                    PublishQueueAttemptHandoff {
                        at: Timestamp::from(278),
                        result: HandoffEvidence::Written,
                    },
                    PublishQueuePostHandoffState::Terminal {
                        outcome: PublishQueueAttemptOutcome::Acked,
                        finished_at: Timestamp::from(279),
                    },
                )
                .unwrap();
            (intent, key)
        };
        insert_one_sided_deadline(
            &path,
            &PublishQueueDeadline {
                at: Timestamp::from(999),
                key,
                lane_revision: 4,
                kind: PublishQueueDeadlineKind::AckTimeout,
            },
            primary,
        );
        let mut store = reopen(&path);
        assert!(store
            .due_publish_queue_deadlines(Timestamp::from(999), 1)
            .unwrap_err()
            .to_string()
            .contains("cardinalities"));
        assert!(store
            .next_publish_queue_deadline()
            .unwrap_err()
            .to_string()
            .contains("cardinalities"));
        assert!(store
            .close_terminal_intent(intent)
            .unwrap_err()
            .to_string()
            .contains("cardinalities"));
        assert!(store
            .recover_publish_queue()
            .expect("recover delivery")
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
                    PublishQueueAttemptHandoff {
                        at: Timestamp::from(273),
                        result: HandoffEvidence::Written,
                    },
                    PublishQueuePostHandoffState::AwaitingAck {
                        deadline: Timestamp::from(300),
                    },
                )
                .unwrap();
            (intent, key)
        };
        let relay_id = raw_relay_id(&path, &key.relay);
        let lane_storage_key = raw_lane_key(intent, relay_id);
        let attempt_storage_key = raw_attempt_key(intent, relay_id, 1);
        let deadline_storage_key = raw_deadline_key(Timestamp::from(300), intent, relay_id, false);
        match target {
            "lane" => {
                corrupt_fixed_row_version(
                    &path,
                    TableDefinition::new("publish_queue_lanes"),
                    &lane_storage_key,
                );
                assert!(reopen(&path).recover_publish_queue_lanes(intent).is_err());
            }
            "detail" => {
                corrupt_fixed_row_version(
                    &path,
                    TableDefinition::new("publish_queue_attempt_details"),
                    &attempt_storage_key,
                );
                assert!(reopen(&path).recover_attempt_details(intent).is_err());
            }
            "deadline" => {
                corrupt_fixed_row_version(
                    &path,
                    TableDefinition::new("publish_queue_deadlines"),
                    &deadline_storage_key,
                );
                assert!(reopen(&path)
                    .due_publish_queue_deadlines(Timestamp::from(300), 1)
                    .is_err());
            }
            _ => unreachable!(),
        }
    }
}

/// #867: the store models ONE current attempt shape. A detail-less attempt
/// row could only have been written by a pre-current writer, so bootstrap
/// refuses it as corruption. There is no compatibility lane state left for it
/// to be adopted into, and no shell detail row is written on its behalf.
#[test]
fn detail_less_attempt_row_is_refused_rather_than_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("detail-less-attempt.redb");
    let relay = RelayUrl::parse("wss://detail-less.example").unwrap();
    let intent = {
        let mut store = reopen(&path);
        let (intent, _, signed, key, lane) = seed(&mut store, "detail-less", 250, relay.clone());
        let lane = store
            .set_lane_eligible(&key, lane.revision, Timestamp::from(251))
            .unwrap();
        store
            .start_lane_attempt(&key, lane.revision, signed, Timestamp::from(252))
            .unwrap();
        intent
    };
    let relay_id = raw_relay_id(&path, &relay);
    let db = Database::open(&path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut details: redb::Table<'_, &[u8; 20], &[u8]> = write
            .open_table(TableDefinition::new("publish_queue_attempt_details"))
            .unwrap();
        details
            .remove(&raw_attempt_key(intent, relay_id, 1))
            .unwrap();
    }
    write.commit().unwrap();
    drop(db);

    let mut store = reopen(&path);
    assert!(store
        .bootstrap_publish_queue_lanes(intent)
        .unwrap_err()
        .to_string()
        .contains("missing its detail row"));
    assert_eq!(store.recover_publish_queue_lanes(intent).unwrap().len(), 1);
}

/// The mirror refusal: attempt history that exists without the lane row the
/// current schema always commits first.
#[test]
fn attempt_history_without_its_lane_is_refused_rather_than_reconstructed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("laneless-attempt.redb");
    let relay = RelayUrl::parse("wss://laneless.example").unwrap();
    let intent = {
        let mut store = reopen(&path);
        let (intent, _, signed, key, lane) = seed(&mut store, "laneless", 262, relay.clone());
        let lane = store
            .set_lane_eligible(&key, lane.revision, Timestamp::from(263))
            .unwrap();
        store
            .start_lane_attempt(&key, lane.revision, signed, Timestamp::from(264))
            .unwrap();
        intent
    };
    let relay_id = raw_relay_id(&path, &relay);
    let db = Database::open(&path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut lanes: redb::Table<'_, &[u8; 12], &[u8]> = write
            .open_table(TableDefinition::new("publish_queue_lanes"))
            .unwrap();
        lanes.remove(&raw_lane_key(intent, relay_id)).unwrap();
    }
    write.commit().unwrap();
    drop(db);

    let mut store = reopen(&path);
    assert!(store
        .bootstrap_publish_queue_lanes(intent)
        .unwrap_err()
        .to_string()
        .contains("without its lane row"));
    assert!(store
        .recover_publish_queue_lanes(intent)
        .unwrap()
        .is_empty());
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
        assert_eq!(
            store.recover_publish_queue_lanes(intent).unwrap()[0].revision,
            1
        );
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
                PublishQueueAttemptHandoff {
                    at: Timestamp::from(303),
                    result: HandoffEvidence::Ambiguous,
                },
                PublishQueuePostHandoffState::AwaitingAck {
                    deadline: Timestamp::from(310),
                },
            )
            .unwrap();
    }
    {
        let mut store = reopen(&path);
        let due = store
            .due_publish_queue_deadlines(Timestamp::from(310), 1)
            .unwrap();
        assert_eq!(
            (due[0].kind, due[0].lane_revision),
            (PublishQueueDeadlineKind::AckTimeout, 4)
        );
        store
            .set_lane_transient(
                &key,
                4,
                1,
                Timestamp::from(311),
                PublishQueueTransientCause::AckTimeout,
                None,
            )
            .unwrap();
    }
    {
        let mut store = reopen(&path);
        assert_eq!(
            store
                .due_publish_queue_deadlines(Timestamp::from(311), 1)
                .unwrap()[0]
                .kind,
            PublishQueueDeadlineKind::RetryEligible
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
                PublishQueueAttemptHandoff {
                    at: Timestamp::from(313),
                    result: HandoffEvidence::Written,
                },
                PublishQueuePostHandoffState::Terminal {
                    outcome: PublishQueueAttemptOutcome::GaveUp,
                    finished_at: Timestamp::from(314),
                },
            )
            .unwrap();
    }
    insert_stale_deadline(
        &path,
        &PublishQueueDeadline {
            at: Timestamp::from(999),
            key: key.clone(),
            lane_revision: 7,
            kind: PublishQueueDeadlineKind::AckTimeout,
        },
    );
    {
        let mut store = reopen(&path);
        assert!(store
            .next_publish_queue_deadline()
            .unwrap_err()
            .to_string()
            .contains("deadline and lane disagree"));
        assert_eq!(
            store.close_terminal_intent(intent).unwrap(),
            CloseIntentOutcome::Closed
        );
        assert_eq!(store.next_publish_queue_deadline().unwrap(), None);
    }
    {
        let store = reopen(&path);
        assert!(!store
            .recover_publish_queue()
            .expect("recover delivery")
            .iter()
            .any(|row| row.intent_id == intent));
        assert_eq!(store.recover_publish_queue_lanes(intent).unwrap().len(), 1);
        assert_eq!(store.recover_attempts(intent).unwrap().len(), 2);
        assert_eq!(store.recover_attempt_details(intent).unwrap().len(), 2);
        assert!(store.reattach_receipt(receipt).unwrap().is_some());
        assert_eq!(store.next_publish_queue_deadline().unwrap(), None);
    }
    // Attempt base rows remain immutable Started facts; terminal state
    // overlays from the required detail row.
    let relay_id = raw_relay_id(&path, &key.relay);
    let db = Database::open(&path).unwrap();
    let read = db.begin_read().unwrap();
    let attempts: TableDefinition<&[u8; 20], &[u8]> =
        TableDefinition::new("publish_queue_attempts");
    let table = read.open_table(attempts).unwrap();
    let raw_key = raw_attempt_key(intent, relay_id, 2);
    let raw = table.get(&raw_key).unwrap().unwrap();
    assert_eq!(
        raw.value().last(),
        Some(&0),
        "immutable attempt value retains the Started outcome tag"
    );
}
