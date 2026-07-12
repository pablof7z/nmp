//! U5 process-death proofs. This entire module, including the failpoint API,
//! exists only in the `nmp-store` unit-test build.

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use nostr::{EventBuilder, Filter, JsonUtil, Keys, Kind};
use redb::ReadableTableMetadata;
use tempfile::TempDir;
use wait_timeout::ChildExt;

use super::*;
use crate::{sentinel_signature, HandoffEvidence};

const WORKER: &str = "redb_store::crash_atomicity_tests::redb_crash_worker";
const SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const RELAY: &str = "wss://crash-proof.example";

fn keys() -> Keys {
    Keys::parse(SECRET).expect("fixed crash-proof key")
}

fn pair(kind: Kind, content: &str, created_at: u64) -> (Event, Event) {
    let keys = keys();
    let signed = EventBuilder::new(kind, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(&keys)
        .expect("sign deterministic event");
    let frozen = Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        sentinel_signature(),
    );
    (frozen, signed)
}

fn event_pair() -> (Event, Event) {
    pair(Kind::TextNote, "u5-crash-proof", 1_000)
}

fn accept(frozen: Event) -> AcceptWrite {
    AcceptWrite {
        frozen,
        expected_pubkey: keys().public_key(),
        signing_identity_ref: "u5-fixed-key".into(),
        durability: WriteDurability::Durable,
        routing: "u5-fixed-route".into(),
        sig_state: IntentSigState::Pending,
        accepted_at: Timestamp::from(1_000),
    }
}

fn accepted(store: &mut RedbStore) -> (IntentId, u64) {
    let (frozen, _) = event_pair();
    let outcome = store.accept_write(accept(frozen)).expect("accept");
    (
        outcome.journaled_intent_id().expect("intent id"),
        outcome.journaled_receipt_id().expect("receipt id"),
    )
}

fn fixture() -> (TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    (dir, path)
}

fn table_len(path: &Path, table: TableDefinition<&str, &str>) -> u64 {
    let db = Database::open(path).expect("open raw database after crash");
    let txn = db.begin_read().expect("begin raw read");
    txn.open_table(table)
        .expect("open raw table")
        .len()
        .expect("count raw rows")
}

fn binary_table_len(path: &Path, table: TableDefinition<&str, &[u8]>) -> u64 {
    let db = Database::open(path).expect("open raw database after crash");
    let txn = db.begin_read().expect("begin raw read");
    txn.open_table(table)
        .expect("open raw binary table")
        .len()
        .expect("count raw binary rows")
}

fn crash(path: &Path, point: &str) {
    let stdout = tempfile::NamedTempFile::new().expect("worker stdout file");
    let stderr = tempfile::NamedTempFile::new().expect("worker stderr file");
    let mut child = Command::new(std::env::current_exe().expect("current test executable"))
        .arg("--exact")
        .arg(WORKER)
        .arg("--nocapture")
        .env("NMP_U5_CRASH_DB", path)
        .env("NMP_U5_CRASH_POINT", point)
        .stdout(Stdio::from(stdout.reopen().expect("clone stdout")))
        .stderr(Stdio::from(stderr.reopen().expect("clone stderr")))
        .spawn()
        .expect("spawn crash worker");
    let status = match child
        .wait_timeout(Duration::from_secs(10))
        .expect("bounded wait for crash worker")
    {
        Some(status) => status,
        None => {
            child.kill().expect("kill hung crash worker");
            child.wait().expect("reap hung crash worker");
            panic!("crash worker timed out at {point}");
        }
    };
    let stdout = std::fs::read_to_string(stdout.path()).expect("read worker stdout");
    let stderr = std::fs::read_to_string(stderr.path()).expect("read worker stderr");
    assert_eq!(
        status.signal(),
        Some(libc::SIGABRT),
        "worker must abort at {point}; status={status:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn redb_crash_worker() {
    let Ok(point) = std::env::var("NMP_U5_CRASH_POINT") else {
        return;
    };
    let path = std::env::var("NMP_U5_CRASH_DB").expect("worker database path");
    let (_, signed) = event_pair();
    let relay = RelayUrl::parse(RELAY).expect("relay");
    match point.as_str() {
        "accept-after-event" => {
            let mut store = RedbStore::open_with_crash_point(
                path,
                RedbCrashPoint::AcceptAfterEventBeforeJournal,
            )
            .expect("open worker store");
            let (frozen, _) = event_pair();
            let _ = store.accept_write(accept(frozen));
        }
        "accept-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::AcceptBeforeCommit)
                    .expect("open worker store");
            let (frozen, _) = event_pair();
            let _ = store.accept_write(accept(frozen));
        }
        "promote-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::PromoteBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_outbox()[0].intent_id;
            let _ = store.promote_signed(intent, signed.sig);
        }
        "compensate-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::CompensateBeforeCommit)
                    .expect("open worker store");
            let intent = store
                .recover_outbox()
                .last()
                .expect("latest intent")
                .intent_id;
            let _ = store.compensate_write(intent);
        }
        "start-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::StartAttemptBeforeCommit)
                    .expect("open worker store");
            let recovered = store.recover_outbox().remove(0);
            let _ = store.start_attempt(recovered.intent_id, relay, recovered.frozen);
        }
        "route-revision-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::RouteRevisionBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_outbox()[0].intent_id;
            let _ = store.record_route_revision(intent, BTreeSet::from([relay]));
        }
        "finish-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::FinishAttemptBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_outbox()[0].intent_id;
            let _ = store.finish_attempt(intent, &relay, 1, AttemptOutcome::Acked);
        }
        "lane-bootstrap-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::LaneBootstrapBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_outbox()[0].intent_id;
            let _ = store.bootstrap_outbox_lanes(intent);
        }
        "lane-transition-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::LaneTransitionBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_outbox()[0].intent_id;
            let lane = store.recover_outbox_lanes(intent).unwrap().remove(0);
            let _ = store.set_lane_transient(
                &lane.key,
                lane.revision,
                lane.last_ordinal,
                Timestamp::from(2_000u64),
                TransientCause::ConnectionLost,
                None,
            );
        }
        "lane-start-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::LaneStartBeforeCommit)
                    .expect("open worker store");
            let recovered = store.recover_outbox().remove(0);
            let intent = recovered.intent_id;
            let lane = store.recover_outbox_lanes(intent).unwrap().remove(0);
            store
                .start_lane_attempt(
                    &lane.key,
                    lane.revision,
                    recovered.frozen,
                    Timestamp::from(1_500u64),
                )
                .expect("lane start reaches crash seam");
        }
        "lane-handoff-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::LaneHandoffBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_outbox()[0].intent_id;
            let lane = store.recover_outbox_lanes(intent).unwrap().remove(0);
            let _ = store.record_lane_handoff(
                &lane.key,
                lane.revision,
                lane.last_ordinal,
                AttemptHandoffDetail {
                    at: Timestamp::from(1_600u64),
                    result: HandoffEvidence::Written,
                },
                PostHandoffState::AwaitingAck {
                    deadline: Timestamp::from(1_630u64),
                },
            );
        }
        "lane-close-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::LaneCloseBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_outbox()[0].intent_id;
            let _ = store.close_terminal_intent(intent);
        }
        "lane-finish-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::FinishAttemptBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_outbox()[0].intent_id;
            let lane = store.recover_outbox_lanes(intent).unwrap().remove(0);
            store
                .finish_lane_attempt(
                    &lane.key,
                    lane.revision,
                    lane.last_ordinal,
                    AttemptOutcome::Acked,
                    Timestamp::from(1_610u64),
                )
                .expect("lane finish reaches crash seam");
        }
        other => panic!("unknown crash point {other}"),
    }
    panic!("crash seam did not abort at {point}");
}

#[test]
fn accept_is_all_or_nothing_at_both_internal_transaction_boundaries() {
    for point in ["accept-after-event", "accept-before-commit"] {
        let (_dir, path) = fixture();
        RedbStore::open(&path).expect("initialize store");
        crash(&path, point);

        assert_eq!(
            binary_table_len(&path, EVENTS),
            0,
            "no orphan event at {point}"
        );
        assert_eq!(
            table_len(&path, OUTBOX_INTENTS),
            0,
            "no orphan intent at {point}"
        );
        assert_eq!(
            table_len(&path, OUTBOX_RECEIPTS),
            0,
            "no orphan receipt at {point}"
        );

        let mut reopened = RedbStore::open(&path).expect("reopen after crash");
        let (frozen, _) = event_pair();
        assert!(reopened
            .query(&Filter::new().id(frozen.id))
            .unwrap()
            .is_empty());
        assert!(reopened.recover_outbox().is_empty());
        assert!(reopened.reattach_receipt(1).unwrap().is_none());

        let outcome = reopened
            .accept_write(accept(frozen))
            .expect("accept after rollback");
        assert_eq!(outcome.journaled_intent_id(), Some(IntentId(1)));
        assert_eq!(outcome.journaled_receipt_id(), Some(1));
        assert_eq!(reopened.query(&Filter::new()).unwrap().len(), 1);
        assert_eq!(reopened.recover_outbox().len(), 1);
    }
}

#[test]
fn route_revision_is_absent_or_fully_recoverable_across_process_death() {
    let (_dir, path) = fixture();
    let relay = RelayUrl::parse(RELAY).expect("relay");
    let intent = {
        let mut store = RedbStore::open(&path).expect("open");
        accepted(&mut store).0
    };
    crash(&path, "route-revision-before-commit");
    let mut reopened = RedbStore::open(&path).expect("reopen route crash");
    assert!(reopened.recover_route_revisions(intent).unwrap().is_empty());
    let committed = reopened
        .record_route_revision(intent, BTreeSet::from([relay.clone()]))
        .expect("commit route revision after rollback");
    assert_eq!(committed.ordinal, 1, "aborted revision cannot burn ordinal");
    drop(reopened);
    let store = RedbStore::open(&path).expect("reopen committed route");
    assert_eq!(
        store.recover_route_revisions(intent).unwrap()[0].relays,
        BTreeSet::from([relay])
    );
}

#[test]
fn promotion_and_displaced_compensation_are_atomic_across_process_death() {
    let (_dir, path) = fixture();
    let (frozen, signed) = event_pair();
    let (intent, receipt) = {
        let mut store = RedbStore::open(&path).expect("open");
        accepted(&mut store)
    };
    crash(&path, "promote-before-commit");
    {
        let mut store = RedbStore::open(&path).expect("reopen promotion crash");
        assert_eq!(store.recover_outbox()[0].sig_state, IntentSigState::Pending);
        assert_eq!(
            store.query(&Filter::new().id(frozen.id)).unwrap()[0]
                .event
                .sig,
            sentinel_signature()
        );
        assert_eq!(
            store.reattach_receipt(receipt).unwrap().unwrap().state,
            ReceiptState::Accepted
        );
        store
            .promote_signed(intent, signed.sig)
            .expect("commit promotion");
    }
    let store = RedbStore::open(&path).expect("reopen promoted state");
    assert_eq!(
        store.query(&Filter::new().id(signed.id)).unwrap()[0]
            .event
            .as_json(),
        signed.as_json()
    );
    assert_eq!(
        store.reattach_receipt(receipt).unwrap().unwrap().state,
        ReceiptState::Signed
    );

    let (_dir, path) = fixture();
    let (older, _) = pair(Kind::ContactList, "older", 900);
    let older_id = older.id;
    let (newer, _) = pair(Kind::ContactList, "newer", 1_000);
    let newer_id = newer.id;
    let (intent, receipt) = {
        let mut store = RedbStore::open(&path).expect("open");
        store.accept_write(accept(older)).expect("accept older");
        let outcome = store.accept_write(accept(newer)).expect("accept newer");
        (
            outcome.journaled_intent_id().unwrap(),
            outcome.journaled_receipt_id().unwrap(),
        )
    };
    crash(&path, "compensate-before-commit");
    {
        let mut store = RedbStore::open(&path).expect("reopen compensation crash");
        assert_eq!(store.query(&Filter::new().id(newer_id)).unwrap().len(), 1);
        assert!(store.query(&Filter::new().id(older_id)).unwrap().is_empty());
        assert_eq!(store.recover_outbox().len(), 2);
        assert!(matches!(
            store.compensate_write(intent).unwrap(),
            CompensateOutcome::Compensated { .. }
        ));
    }
    let store = RedbStore::open(&path).expect("reopen compensated state");
    assert!(store.query(&Filter::new().id(newer_id)).unwrap().is_empty());
    assert_eq!(store.query(&Filter::new().id(older_id)).unwrap().len(), 1);
    assert_eq!(store.recover_outbox().len(), 1);
    assert_eq!(
        store.reattach_receipt(receipt).unwrap().unwrap().state,
        ReceiptState::Compensated
    );
}

#[test]
fn attempt_started_and_terminal_facts_never_partially_commit() {
    let (_dir, path) = fixture();
    let (_, signed) = event_pair();
    let relay = RelayUrl::parse(RELAY).expect("relay");
    let intent = {
        let mut store = RedbStore::open(&path).expect("open");
        let (intent, _) = accepted(&mut store);
        store.promote_signed(intent, signed.sig).expect("promote");
        intent
    };
    crash(&path, "start-before-commit");
    {
        let mut store = RedbStore::open(&path).expect("reopen start crash");
        assert!(store.recover_attempts(intent).unwrap().is_empty());
        let started = store
            .start_attempt(intent, relay.clone(), signed.clone())
            .unwrap();
        assert_eq!(
            (started.ordinal, started.outcome),
            (1, AttemptOutcome::Started)
        );
    }
    crash(&path, "finish-before-commit");
    {
        let mut store = RedbStore::open(&path).expect("reopen finish crash");
        let attempts = store.recover_attempts(intent).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].outcome, AttemptOutcome::Started);
        assert_eq!(attempts[0].event.as_json(), signed.as_json());
        store
            .finish_attempt(intent, &relay, 1, AttemptOutcome::Acked)
            .unwrap();
    }
    let store = RedbStore::open(&path).expect("final reopen");
    assert_eq!(
        store.recover_attempts(intent).unwrap()[0].outcome,
        AttemptOutcome::Acked
    );
}

#[test]
fn lane_cursor_detail_deadline_and_close_are_atomic_across_process_death() {
    let (_dir, path) = fixture();
    let (_, signed) = event_pair();
    let relay = RelayUrl::parse(RELAY).expect("relay");
    let intent = {
        let mut store = RedbStore::open(&path).expect("open");
        let (intent, _) = accepted(&mut store);
        store.promote_signed(intent, signed.sig).expect("promote");
        store
            .record_route_revision(intent, BTreeSet::from([relay.clone()]))
            .expect("route");
        intent
    };

    crash(&path, "lane-bootstrap-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen bootstrap crash");
    assert!(store.recover_outbox_lanes(intent).unwrap().is_empty());
    let mut lane = store.bootstrap_outbox_lanes(intent).unwrap().remove(0);
    assert_eq!(lane.state, LaneState::WaitingConnection);
    drop(store);

    crash(&path, "lane-transition-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen transition crash");
    lane = store.recover_outbox_lanes(intent).unwrap().remove(0);
    assert_eq!(lane.state, LaneState::WaitingConnection);
    assert_eq!(store.next_outbox_deadline().unwrap(), None);
    store
        .set_lane_eligible(&lane.key, lane.revision, Timestamp::from(1_500u64))
        .unwrap();
    drop(store);

    crash(&path, "lane-start-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen start crash");
    lane = store.recover_outbox_lanes(intent).unwrap().remove(0);
    assert!(matches!(lane.state, LaneState::Eligible { .. }));
    assert!(store.recover_attempts(intent).unwrap().is_empty());
    assert!(store.recover_attempt_details(intent).unwrap().is_empty());
    store
        .start_lane_attempt(
            &lane.key,
            lane.revision,
            signed.clone(),
            Timestamp::from(1_500u64),
        )
        .unwrap();
    drop(store);

    crash(&path, "lane-handoff-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen handoff crash");
    lane = store.recover_outbox_lanes(intent).unwrap().remove(0);
    assert!(matches!(
        lane.state,
        LaneState::InFlight {
            phase: InFlightPhase::AwaitingHandoff,
            ..
        }
    ));
    assert!(store.recover_attempt_details(intent).unwrap()[0]
        .handoff
        .is_none());
    assert_eq!(store.next_outbox_deadline().unwrap(), None);
    let handoff = AttemptHandoffDetail {
        at: Timestamp::from(1_600u64),
        result: HandoffEvidence::Written,
    };
    store
        .record_lane_handoff(
            &lane.key,
            lane.revision,
            lane.last_ordinal,
            handoff.clone(),
            PostHandoffState::AwaitingAck {
                deadline: Timestamp::from(1_630u64),
            },
        )
        .unwrap();
    assert_eq!(
        store.next_outbox_deadline().unwrap(),
        Some(Timestamp::from(1_630u64))
    );
    drop(store);

    crash(&path, "lane-finish-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen lane finish crash");
    lane = store.recover_outbox_lanes(intent).unwrap().remove(0);
    assert!(matches!(
        lane.state,
        LaneState::InFlight {
            phase: InFlightPhase::AwaitingAck { .. },
            ..
        }
    ));
    assert!(store.recover_attempt_details(intent).unwrap()[0]
        .terminal
        .is_none());
    assert_eq!(
        store.next_outbox_deadline().unwrap(),
        Some(Timestamp::from(1_630u64))
    );
    lane = store
        .finish_lane_attempt(
            &lane.key,
            lane.revision,
            lane.last_ordinal,
            AttemptOutcome::Acked,
            Timestamp::from(1_610u64),
        )
        .unwrap();
    assert!(matches!(lane.state, LaneState::Terminal { .. }));
    let committed_detail = store.recover_attempt_details(intent).unwrap().remove(0);
    assert_eq!(committed_detail.terminal, Some(AttemptOutcome::Acked));
    assert_eq!(
        committed_detail.finished_at,
        Some(Timestamp::from(1_610u64))
    );
    assert_eq!(store.next_outbox_deadline().unwrap(), None);
    drop(store);

    crash(&path, "lane-close-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen close crash");
    assert_eq!(store.recover_outbox().len(), 1);
    assert_eq!(store.recover_outbox_lanes(intent).unwrap().len(), 1);
    assert_eq!(store.recover_attempts(intent).unwrap().len(), 1);
    assert_eq!(store.recover_attempt_details(intent).unwrap().len(), 1);
    assert_eq!(
        store.close_terminal_intent(intent).unwrap(),
        CloseIntentOutcome::Closed
    );
    drop(store);

    let store = RedbStore::open(&path).expect("final reopen");
    assert!(store.recover_outbox().is_empty());
    assert_eq!(store.recover_outbox_lanes(intent).unwrap().len(), 1);
    assert_eq!(
        store.recover_attempts(intent).unwrap()[0].outcome,
        AttemptOutcome::Acked
    );
    assert_eq!(store.recover_attempt_details(intent).unwrap().len(), 1);
}

#[test]
fn committed_pending_row_and_journal_survive_real_reopen_as_one_fact() {
    let (_dir, path) = fixture();
    let (frozen, _) = event_pair();
    let (intent, receipt) = {
        let mut store = RedbStore::open(&path).expect("open");
        accepted(&mut store)
    };
    let store = RedbStore::open(&path).expect("reopen committed accept");
    let rows = store.query(&Filter::new().id(frozen.id)).unwrap();
    assert_eq!(rows.len(), 1);
    let local = rows[0].provenance.local.as_ref().expect("local provenance");
    assert_eq!(local.sig_state, SigState::Pending);
    assert_eq!(local.owners, BTreeSet::from([intent]));
    let recovered = store.recover_outbox();
    assert_eq!(
        (
            recovered.len(),
            recovered[0].intent_id,
            recovered[0].receipt_id
        ),
        (1, intent, receipt)
    );
}
