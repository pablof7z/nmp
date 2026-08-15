//! U5 process-death proofs. This entire module, including the failpoint API,
//! exists only in the `nmp-store` unit-test build.

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use nmp_grammar::{AccessContext, SourceAuthority};
use nostr::nips::nip01::Coordinate;
use nostr::{EventBuilder, Filter, JsonUtil, Keys, Kind, UnsignedEvent};
use redb::ReadableTableMetadata;
use tempfile::TempDir;
use wait_timeout::ChildExt;

use super::*;
use crate::{
    sentinel_signature, HandoffEvidence, MaterializationId, MaterializationReceipt,
    MaterializationRef, PublishQueueReceiptPayload, ReplaceableOperationReceiptState,
    SemanticCurrentState, SemanticGeneration, SemanticInstallOutcome,
};

/// The verified, intent-bound evidence `promote_signed` takes (#768). Every
/// event promoted below is one this fixture just signed itself, so the
/// verification succeeding is part of the setup, not the property under test.
fn evidence(signed: &Event) -> VerifiedSignature {
    VerifiedSignature::verify(signed).expect("fixture events are validly signed")
}

const WORKER: &str = "redb_store::crash_atomicity_tests::redb_crash_worker";
const SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const RELAY: &str = "wss://crash-proof.example";
const RELAY_TWO: &str = "wss://crash-proof-two.example";

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

fn packed_event(index: u64) -> Event {
    EventBuilder::new(Kind::TextNote, format!("packed-crash-{index}"))
        .custom_created_at(Timestamp::from(2_000 + index))
        .sign_with_keys(&keys())
        .expect("sign packed crash event")
}

fn retention_atom() -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(BTreeSet::from([keys().public_key().to_hex()])),
            ids: None,
            tags: BTreeMap::new(),
            since: None,
            until: None,
            limit: None,
        },
        source: SourceAuthority::AuthorOutboxes,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn request_coverage_batch() -> Vec<(ContextualAtom, RelayUrl, CoverageInterval)> {
    let atom = retention_atom();
    let interval = CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(2_000u64));
    vec![
        (
            atom.clone(),
            RelayUrl::parse(RELAY).expect("first relay"),
            interval,
        ),
        (
            atom,
            RelayUrl::parse(RELAY_TWO).expect("second relay"),
            interval,
        ),
    ]
}

fn accept(frozen: Event) -> AcceptWrite {
    AcceptWrite {
        payload: crate::AcceptWritePayload::Event {
            frozen: Box::new(frozen),
            replaceable_base: None,
            monotonic_stamp: false,
            routing: "u5-fixed-route".into(),
            sig_state: IntentSigState::Pending,
        },
        expected_pubkey: keys().public_key(),
        signing_identity_ref: "u5-fixed-key".into(),
        accepted_at: Timestamp::from(1_000),
        correlation: None,
    }
}

/// #591: same fixture as [`accept`], but carrying a correlation token --
/// used to prove `PUBLISH_QUEUE_CORRELATIONS`' row commits or rolls back in the
/// SAME transaction as the receipt it names, not a separate one.
fn accept_with_correlation(frozen: Event, token: &str) -> AcceptWrite {
    AcceptWrite {
        correlation: Some(
            nmp_grammar::CorrelationToken::try_from(token).expect("fixture token is well-formed"),
        ),
        ..accept(frozen)
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

fn semantic_coordinate() -> Coordinate {
    Coordinate {
        kind: Kind::ContactList,
        public_key: keys().public_key(),
        identifier: String::new(),
    }
}

fn semantic_source() -> crate::SourceEvidence {
    crate::SourceEvidence {
        plan: crate::SourcePlanId([3; 32]),
        access: crate::AccessContextId([4; 32]),
        qualified: crate::QualifiedSource::Absent,
    }
}

fn semantic_accept_write() -> AcceptWrite {
    AcceptWrite {
        payload: crate::AcceptWritePayload::ReplaceableOperation(Box::new(crate::SemanticAccept {
            coordinate: semantic_coordinate(),
            program: crate::ReplayProgramId([7; 16]),
            format: crate::ReplayFormatId([9; 16]),
            expected_source_revision: None,
            expected_program_digest: None,
            expected_current_materialization: None,
            starting_source: crate::StartingSourceRequirement {
                plan: crate::SourcePlanId([3; 32]),
                access: crate::AccessContextId([4; 32]),
                source: crate::StartingSource::Absent,
            },
            source: semantic_source(),
            source_event: None,
            plan: crate::SemanticPlan::new(1, vec![42]).unwrap(),
            materialized: None,
            contributing_operations: Vec::new(),
            resolved_operations: Vec::new(),
        })),
        expected_pubkey: keys().public_key(),
        signing_identity_ref: "semantic-u5-key".into(),
        accepted_at: Timestamp::from(1_000),
        correlation: None,
    }
}

fn semantic_rematerialize(store: &RedbStore) -> crate::SemanticRematerialize {
    let snapshot = store
        .replaceable_operation_snapshot(&semantic_coordinate())
        .unwrap()
        .unwrap();
    crate::SemanticRematerialize {
        coordinate: semantic_coordinate(),
        expected_source_revision: snapshot.current.source_revision.clone(),
        expected_program_digest: snapshot.current.program_digest,
        expected_current_materialization: None,
        source: semantic_source(),
        evaluated_at: Timestamp::from(1_000),
        materialized: Some(crate::MaterializationCandidate {
            event: UnsignedEvent::new(
                keys().public_key(),
                Timestamp::from(1_000),
                Kind::ContactList,
                Vec::new(),
                "semantic-u5-body",
            ),
            routing: "semantic-u5-route".into(),
            sig_state: crate::PendingMaterializationState::Pending,
        }),
        contributing_operations: vec![snapshot.operations[0].intent_id],
        resolved_operations: Vec::new(),
    }
}

fn qualified_source(content: &str, created_at: u64) -> Event {
    EventBuilder::new(Kind::ContactList, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(&keys())
        .expect("sign semantic source")
}

fn seed_qualified_semantic_generation(store: &mut RedbStore) -> (u64, EventId, EventId) {
    let relay = RelayUrl::parse(RELAY).unwrap();
    let source = qualified_source("B0", 1);
    store
        .insert(
            source.clone(),
            RelayObserved::new(relay, Timestamp::from(1)),
        )
        .unwrap();
    let stored = store
        .query(&Filter::new().id(source.id))
        .unwrap()
        .pop()
        .unwrap();
    let evidence = crate::SourceEvidence {
        plan: crate::SourcePlanId([3; 32]),
        access: crate::AccessContextId([4; 32]),
        qualified: crate::QualifiedSource::Event {
            event_id: source.id,
            created_at: source.created_at,
        },
    };
    let outcome = store
        .accept_write(AcceptWrite {
            payload: crate::AcceptWritePayload::ReplaceableOperation(Box::new(
                crate::SemanticAccept {
                    coordinate: semantic_coordinate(),
                    program: crate::ReplayProgramId([7; 16]),
                    format: crate::ReplayFormatId([9; 16]),
                    expected_source_revision: None,
                    expected_program_digest: None,
                    expected_current_materialization: None,
                    starting_source: crate::StartingSourceRequirement {
                        plan: crate::SourcePlanId([3; 32]),
                        access: crate::AccessContextId([4; 32]),
                        source: crate::StartingSource::Event(source.id),
                    },
                    source: evidence,
                    source_event: Some(stored),
                    plan: crate::SemanticPlan::new(1, vec![42]).unwrap(),
                    materialized: Some(crate::MaterializationCandidate {
                        event: UnsignedEvent::new(
                            keys().public_key(),
                            Timestamp::from(2),
                            Kind::ContactList,
                            Vec::new(),
                            "E1",
                        ),
                        routing: "semantic-source-route".into(),
                        sig_state: crate::PendingMaterializationState::Pending,
                    }),
                    contributing_operations: Vec::new(),
                    resolved_operations: Vec::new(),
                },
            )),
            expected_pubkey: keys().public_key(),
            signing_identity_ref: "semantic-source-key".into(),
            accepted_at: Timestamp::from(0),
            correlation: None,
        })
        .unwrap();
    let AcceptOutcome::ReplaceableOperation {
        receipt_id,
        installed: Some(installed),
        ..
    } = outcome
    else {
        panic!("expected initial complete semantic generation, got {outcome:?}");
    };
    (receipt_id, installed.event.id, source.id)
}

fn semantic_source_install(store: &RedbStore) -> crate::SemanticSourceInstall {
    let snapshot = store
        .replaceable_operation_snapshot(&semantic_coordinate())
        .unwrap()
        .unwrap();
    let source = qualified_source("B5", 5);
    let mut seen = BTreeMap::new();
    seen.insert(RelayUrl::parse(RELAY).unwrap(), Timestamp::from(5));
    crate::SemanticSourceInstall {
        source: StoredEvent {
            event: source.clone(),
            provenance: Provenance { seen, local: None },
        },
        successor: crate::SemanticRematerialize {
            coordinate: semantic_coordinate(),
            expected_source_revision: snapshot.current.source_revision.clone(),
            expected_program_digest: snapshot.current.program_digest,
            expected_current_materialization: snapshot
                .current
                .generation
                .as_ref()
                .map(|generation| generation.materialization.materialization_id),
            source: crate::SourceEvidence {
                plan: crate::SourcePlanId([3; 32]),
                access: crate::AccessContextId([4; 32]),
                qualified: crate::QualifiedSource::Event {
                    event_id: source.id,
                    created_at: source.created_at,
                },
            },
            evaluated_at: Timestamp::from(5),
            materialized: Some(crate::MaterializationCandidate {
                event: UnsignedEvent::new(
                    keys().public_key(),
                    Timestamp::from(6),
                    Kind::ContactList,
                    Vec::new(),
                    "E2",
                ),
                routing: "semantic-source-route".into(),
                sig_state: crate::PendingMaterializationState::Pending,
            }),
            contributing_operations: snapshot
                .operations
                .iter()
                .map(|operation| operation.intent_id)
                .collect(),
            resolved_operations: Vec::new(),
        },
    }
}

fn semantic_promotion_target(
    store: &RedbStore,
) -> (crate::PromotionTarget, crate::VerifiedSignature) {
    let snapshot = store
        .replaceable_operation_snapshot(&semantic_coordinate())
        .unwrap()
        .unwrap();
    let generation = snapshot.current.generation.as_ref().unwrap();
    let row = store
        .query(
            &Filter::new()
                .kind(Kind::ContactList)
                .author(keys().public_key()),
        )
        .unwrap()
        .pop()
        .unwrap();
    let signed = UnsignedEvent::new(
        row.event.pubkey,
        row.event.created_at,
        row.event.kind,
        row.event.tags.clone(),
        row.event.content,
    )
    .sign_with_keys(&keys())
    .unwrap();
    (
        crate::PromotionTarget::ReplaceableMaterialization(Box::new(
            crate::ReplaceableMaterializationTarget {
                coordinate: semantic_coordinate(),
                expected_source_revision: snapshot.current.source_revision,
                expected_program_digest: snapshot.current.program_digest,
                expected_materialization: generation.materialization.materialization_id,
                expected_event_id: generation.materialization.event_id,
            },
        )),
        evidence(&signed),
    )
}

fn fixture() -> (TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    (dir, path)
}

fn publish_queue_table_len(
    path: &Path,
    table: TableDefinition<&'static [u8; 8], &'static [u8]>,
) -> u64 {
    let db = Database::open(path).expect("open raw database after crash");
    let txn = db.begin_read().expect("begin raw read");
    txn.open_table(table)
        .expect("open raw table")
        .len()
        .expect("count raw rows")
}

fn correlation_table_len(path: &Path) -> u64 {
    let db = Database::open(path).expect("open raw database after crash");
    let txn = db.begin_read().expect("begin raw read");
    txn.open_table(PUBLISH_QUEUE_CORRELATIONS)
        .expect("open correlation table")
        .len()
        .expect("count correlation rows")
}

fn event_table_len(path: &Path) -> u64 {
    let db = Database::open(path).expect("open raw database after crash");
    let txn = db.begin_read().expect("begin raw read");
    txn.open_table(EVENTS)
        .expect("open raw event table")
        .len()
        .expect("count raw event rows")
}

fn assert_path_canonical_integrity(path: &Path) {
    let db = Database::open(path).expect("open raw database for canonical integrity audit");
    assert_canonical_integrity(&db);
    drop(db);

    let store = RedbStore::open(path).expect("open recovered store for semantic digest");
    let first = crate::semantic_oracle::recovered_semantic_digest(&store);
    drop(store);
    let reopened = RedbStore::open(path).expect("reopen recovered store for semantic digest");
    assert_eq!(
        crate::semantic_oracle::recovered_semantic_digest(&reopened),
        first,
        "semantic state changed on the second reopen after an injected crash"
    );
}

fn crash_worker(path: &Path, point: &str) {
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
    assert_path_canonical_integrity(path);
}

fn crash(path: &Path, point: &str) {
    crash_worker(path, point);
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
        "accept-before-commit-with-correlation" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::AcceptBeforeCommit)
                    .expect("open worker store");
            let (frozen, _) = event_pair();
            let _ = store.accept_write(accept_with_correlation(frozen, "u5-correlation-token"));
        }
        "semantic-accept-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::SemanticAcceptBeforeCommit)
                    .expect("open semantic accept worker store");
            let _ = store.accept_write(semantic_accept_write());
        }
        "semantic-install-before-commit" => {
            let mut store = RedbStore::open_with_crash_point(
                path,
                RedbCrashPoint::SemanticRematerializeBeforeCommit,
            )
            .expect("open semantic install worker store");
            let rematerialize = semantic_rematerialize(&store);
            let _ = store.install_replaceable_materialization(rematerialize);
        }
        "semantic-source-install-before-commit" => {
            let mut store = RedbStore::open_with_crash_point(
                path,
                RedbCrashPoint::SemanticSourceInstallBeforeCommit,
            )
            .expect("open semantic source install worker store");
            let install = semantic_source_install(&store);
            let _ = store.install_replaceable_source_materialization(install);
        }
        "semantic-promote-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::SemanticPromoteBeforeCommit)
                    .expect("open semantic promotion worker store");
            let (target, verified) = semantic_promotion_target(&store);
            let _ = store.promote_signed(target, verified);
        }
        "promote-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::PromoteBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
            let _ = store.promote_signed(crate::PromotionTarget::Event(intent), evidence(&signed));
        }
        "compensate-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::CompensateBeforeCommit)
                    .expect("open worker store");
            let intent = store
                .recover_publish_queue()
                .expect("recover delivery")
                .last()
                .expect("latest intent")
                .intent_id;
            let _ = store.compensate_write(intent);
        }
        "cancel-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::CompensateBeforeCommit)
                    .expect("open worker store");
            let intent = store
                .recover_publish_queue()
                .expect("recover delivery")
                .last()
                .expect("latest intent")
                .intent_id;
            let _ = store.cancel_write(intent);
        }
        "observation-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::ObservationBeforeCommit)
                    .expect("open worker store");
            let relay = RelayUrl::parse(RELAY_TWO).expect("second relay");
            let _ = store.insert(signed, RelayObserved::new(relay, Timestamp::from(2_000u64)));
        }
        "observation-after-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::ObservationAfterCommit)
                    .expect("open worker store");
            let relay = RelayUrl::parse(RELAY).expect("relay");
            let _ = store.insert(signed, RelayObserved::new(relay, Timestamp::from(2_000u64)));
        }
        "coverage-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::CoverageBeforeCommit)
                    .expect("open worker store");
            let _ = store.record_coverage(&request_coverage_batch());
        }
        "coverage-after-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::CoverageAfterCommit)
                    .expect("open worker store");
            let _ = store.record_coverage(&request_coverage_batch());
        }
        "gc-before-commit" => {
            let mut store = RedbStore::open_with_crash_point(path, RedbCrashPoint::GcBeforeCommit)
                .expect("open worker store");
            let _ = store.gc(&GcRetentionSet::new(Vec::new()));
        }
        "gc-after-commit" => {
            let mut store = RedbStore::open_with_crash_point(path, RedbCrashPoint::GcAfterCommit)
                .expect("open worker store");
            let _ = store.gc(&GcRetentionSet::new(Vec::new()));
        }
        "postings-before-segments"
        | "postings-after-segments"
        | "postings-before-catalog"
        | "postings-after-catalog"
        | "postings-before-commit"
        | "postings-after-commit" => {
            let mut store = RedbStore::open(path).expect("open packed ingest worker store");
            let event = packed_event(0);
            let _ = store.insert(event, RelayObserved::new(relay, Timestamp::from(3_000u64)));
        }
        "postings-before-death" | "postings-after-death" => {
            let mut store = RedbStore::open(path).expect("open packed death worker store");
            let _ = store.remove(packed_event(0).id, RetractReason::Deleted);
        }
        "postings-before-compaction-output" | "postings-after-compaction-output" => {
            let mut store = RedbStore::open(path).expect("open packed compaction worker store");
            let _ = store.insert(
                packed_event(7),
                RelayObserved::new(relay, Timestamp::from(3_007u64)),
            );
        }
        "route-revision-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::RouteRevisionBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
            let _ = store.record_route_revision(intent, BTreeSet::from([relay]));
        }
        "lane-bootstrap-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::LaneBootstrapBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
            let _ = store.bootstrap_publish_queue_lanes(intent);
        }
        "lane-transition-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::LaneTransitionBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
            let lane = store.recover_publish_queue_lanes(intent).unwrap().remove(0);
            let _ = store.set_lane_transient(
                &lane.key,
                lane.revision,
                lane.last_ordinal,
                Timestamp::from(2_000u64),
                PublishQueueTransientCause::ConnectionLost,
                None,
            );
        }
        "lane-start-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::LaneStartBeforeCommit)
                    .expect("open worker store");
            let recovered = store
                .recover_publish_queue()
                .expect("recover delivery")
                .remove(0);
            let intent = recovered.intent_id;
            let crate::PublishQueueWork::Event { frozen, .. } = recovered.work else {
                panic!("lane fixture recovered non-event work")
            };
            let lane = store.recover_publish_queue_lanes(intent).unwrap().remove(0);
            store
                .start_lane_attempt(&lane.key, lane.revision, frozen, Timestamp::from(1_500u64))
                .expect("lane start reaches crash seam");
        }
        "lane-handoff-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::LaneHandoffBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
            let lane = store.recover_publish_queue_lanes(intent).unwrap().remove(0);
            let _ = store.record_lane_handoff(
                &lane.key,
                lane.revision,
                lane.last_ordinal,
                PublishQueueAttemptHandoff {
                    at: Timestamp::from(1_600u64),
                    result: HandoffEvidence::Written,
                },
                PublishQueuePostHandoffState::AwaitingAck {
                    deadline: Timestamp::from(1_630u64),
                },
            );
        }
        "lane-close-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::LaneCloseBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
            let _ = store.close_terminal_intent(intent);
        }
        "lane-finish-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::FinishAttemptBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
            let lane = store.recover_publish_queue_lanes(intent).unwrap().remove(0);
            store
                .finish_lane_attempt(
                    &lane.key,
                    lane.revision,
                    lane.last_ordinal,
                    PublishQueueAttemptOutcome::Acked,
                    Timestamp::from(1_610u64),
                )
                .expect("lane finish reaches crash seam");
        }
        "lane-auth-denial-before-commit" => {
            let mut store =
                RedbStore::open_with_crash_point(path, RedbCrashPoint::DenyLaneAuthBeforeCommit)
                    .expect("open worker store");
            let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
            let lane = store.recover_publish_queue_lanes(intent).unwrap().remove(0);
            store
                .deny_lane_auth(
                    &lane.key,
                    lane.revision,
                    AuthDenial {
                        source: AuthDenialSource::Policy,
                        reason: "account not permitted".into(),
                    },
                )
                .expect("AUTH denial reaches crash seam");
        }
        "terminal-retention-before-commit" => {
            let mut store = RedbStore::open_with_crash_point(
                path,
                RedbCrashPoint::TerminalRetentionBeforeCommit,
            )
            .expect("open worker store");
            let _ = publish_queue_ops::maintain_terminal_receipts_at(
                &mut store,
                Timestamp::from(u64::MAX / 4),
                crate::terminal_retention::TerminalRetentionLimits {
                    max_age_secs: u64::MAX,
                    max_count: 0,
                    max_bytes: u64::MAX,
                },
            );
        }
        other => panic!("unknown crash point {other}"),
    }
    panic!("crash seam did not abort at {point}");
}

#[test]
fn terminal_retention_whole_closure_eviction_is_atomic_across_process_death() {
    let (_dir, path) = fixture();
    let receipt_id = {
        let mut store = RedbStore::open(&path).expect("initialize retention fixture");
        let (frozen, _) = event_pair();
        let accepted = store
            .accept_write(accept_with_correlation(frozen, "retention-crash-token"))
            .expect("accept retention fixture");
        store
            .cancel_write(accepted.journaled_intent_id().expect("intent id"))
            .expect("terminalize retention fixture");
        accepted.journaled_receipt_id().expect("receipt id")
    };

    crash(&path, "terminal-retention-before-commit");

    let store = RedbStore::open(&path).expect("reopen rolled-back retention fixture");
    assert_eq!(
        store.lookup_correlation("retention-crash-token").unwrap(),
        Some(receipt_id)
    );
    let receipt = store.reattach_receipt(receipt_id).unwrap().unwrap();
    match receipt.payload {
        PublishQueueReceiptPayload::Event { event_id, state } => {
            assert_eq!(
                state,
                ReceiptState::Cancelled,
                "unexpected state for event {event_id}"
            );
        }
        PublishQueueReceiptPayload::ReplaceableOperation {
            coordinate, state, ..
        } => {
            panic!(
                "expected an event receipt, got replaceable operation {coordinate:?} in state {state:?}"
            );
        }
    }
}

#[test]
fn accept_is_all_or_nothing_at_both_internal_transaction_boundaries() {
    for point in ["accept-after-event", "accept-before-commit"] {
        let (_dir, path) = fixture();
        RedbStore::open(&path).expect("initialize store");
        crash(&path, point);

        assert_eq!(event_table_len(&path), 0, "no orphan event at {point}");
        assert_eq!(
            publish_queue_table_len(&path, PUBLISH_QUEUE_INTENTS),
            0,
            "no orphan intent at {point}"
        );
        assert_eq!(
            publish_queue_table_len(&path, PUBLISH_QUEUE_RECEIPTS),
            0,
            "no orphan receipt at {point}"
        );

        let mut reopened = RedbStore::open(&path).expect("reopen after crash");
        let (frozen, _) = event_pair();
        assert!(reopened
            .query(&Filter::new().id(frozen.id))
            .unwrap()
            .is_empty());
        assert!(reopened
            .recover_publish_queue()
            .expect("recover delivery")
            .is_empty());
        assert!(reopened.reattach_receipt(1).unwrap().is_none());

        let outcome = reopened
            .accept_write(accept(frozen))
            .expect("accept after rollback");
        assert_eq!(outcome.journaled_intent_id(), Some(IntentId(1)));
        assert_eq!(outcome.journaled_receipt_id(), Some(1));
        assert_eq!(reopened.query(&Filter::new()).unwrap().len(), 1);
        assert_eq!(
            reopened
                .recover_publish_queue()
                .expect("recover delivery")
                .len(),
            1
        );
        drop(reopened);
        assert_path_canonical_integrity(&path);
    }
}

#[test]
fn semantic_accept_and_materialization_are_crash_atomic() {
    let (_dir, path) = fixture();
    RedbStore::open(&path).expect("initialize semantic store");
    crash(&path, "semantic-accept-before-commit");

    {
        let mut store = RedbStore::open(&path).expect("reopen semantic accept crash");
        assert!(store
            .replaceable_operation_snapshot(&semantic_coordinate())
            .unwrap()
            .is_none());
        assert!(store.recover_publish_queue().unwrap().is_empty());
        assert!(store.reattach_receipt(1).unwrap().is_none());
        let accepted = store
            .accept_write(semantic_accept_write())
            .expect("accept after rollback");
        assert_eq!(accepted.journaled_intent_id(), Some(IntentId(1)));
        assert_eq!(accepted.journaled_receipt_id(), Some(1));
    }

    crash(&path, "semantic-install-before-commit");
    {
        let mut store = RedbStore::open(&path).expect("reopen semantic install crash");
        let snapshot = store
            .replaceable_operation_snapshot(&semantic_coordinate())
            .unwrap()
            .unwrap();
        assert!(snapshot.current.generation.is_none());
        assert!(store
            .query(
                &Filter::new()
                    .kind(Kind::ContactList)
                    .author(keys().public_key())
            )
            .unwrap()
            .is_empty());
        let receipt = store.reattach_receipt(1).unwrap().unwrap();
        assert!(matches!(
            receipt.payload,
            PublishQueueReceiptPayload::ReplaceableOperation {
                state: ReplaceableOperationReceiptState::Contributing { current: None },
                ..
            }
        ));

        let rematerialize = semantic_rematerialize(&store);
        let installed = store
            .install_replaceable_materialization(rematerialize)
            .expect("install after rollback");
        assert!(matches!(
            installed,
            SemanticInstallOutcome::Installed {
                current: SemanticCurrentState {
                    generation: Some(SemanticGeneration {
                        materialization: MaterializationRef {
                            materialization_id: MaterializationId(1),
                            ..
                        },
                        ..
                    }),
                    ..
                },
                ..
            }
        ));
    }
    let store = RedbStore::open(&path).expect("reopen committed semantic materialization");
    assert_eq!(
        store
            .replaceable_operation_snapshot(&semantic_coordinate())
            .unwrap()
            .unwrap()
            .current
            .generation
            .unwrap()
            .materialization
            .materialization_id,
        MaterializationId(1)
    );
    drop(store);
    assert_path_canonical_integrity(&path);
}

#[test]
fn semantic_source_and_effective_successor_are_one_crash_atomic_transition() {
    let (_dir, path) = fixture();
    let (receipt_id, first_id, base_id) = {
        let mut store = RedbStore::open(&path).unwrap();
        seed_qualified_semantic_generation(&mut store)
    };

    crash(&path, "semantic-source-install-before-commit");
    {
        let store = RedbStore::open(&path).expect("reopen source install crash");
        let snapshot = store
            .replaceable_operation_snapshot(&semantic_coordinate())
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.source.unwrap().event.id, base_id);
        assert_eq!(
            snapshot
                .current
                .generation
                .unwrap()
                .materialization
                .event_id,
            first_id
        );
        let receipt = store.reattach_receipt(receipt_id).unwrap().unwrap();
        assert!(matches!(
            receipt.payload,
            PublishQueueReceiptPayload::ReplaceableOperation {
                state: ReplaceableOperationReceiptState::Contributing {
                    current: Some(MaterializationReceipt {
                        materialization: MaterializationRef { event_id, .. },
                        ..
                    })
                },
                ..
            } if event_id == first_id
        ));
        let row = store
            .query(
                &Filter::new()
                    .kind(Kind::ContactList)
                    .author(keys().public_key()),
            )
            .unwrap();
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].event.id, first_id);
        assert_ne!(row[0].event.content, "B5");
    }

    {
        let mut store = RedbStore::open(&path).unwrap();
        let install = semantic_source_install(&store);
        let newer_id = install.source.event.id;
        let SemanticInstallOutcome::Installed { installed, .. } = store
            .install_replaceable_source_materialization(install)
            .unwrap()
        else {
            panic!("source successor must install after the rolled-back crash");
        };
        let snapshot = store
            .replaceable_operation_snapshot(&semantic_coordinate())
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.source.unwrap().event.id, newer_id);
        assert_eq!(
            snapshot
                .current
                .generation
                .unwrap()
                .materialization
                .event_id,
            installed.event.id
        );
        let receipt = store.reattach_receipt(receipt_id).unwrap().unwrap();
        assert!(matches!(
            receipt.payload,
            PublishQueueReceiptPayload::ReplaceableOperation {
                state: ReplaceableOperationReceiptState::Contributing {
                    current: Some(MaterializationReceipt {
                        materialization: MaterializationRef { event_id, .. },
                        ..
                    })
                },
                ..
            } if event_id == installed.event.id
        ));
    }
    assert_path_canonical_integrity(&path);
}

#[test]
fn semantic_shared_promotion_is_crash_atomic() {
    let (_dir, path) = fixture();
    let (receipt_a, receipt_b) = {
        let mut store = RedbStore::open(&path).unwrap();
        let first = store.accept_write(semantic_accept_write()).unwrap();
        let first_intent = first.journaled_intent_id().unwrap();
        let first_receipt = first.journaled_receipt_id().unwrap();
        let snapshot = store
            .replaceable_operation_snapshot(&semantic_coordinate())
            .unwrap()
            .unwrap();
        let mut second_write = semantic_accept_write();
        let crate::AcceptWritePayload::ReplaceableOperation(second) = &mut second_write.payload
        else {
            unreachable!()
        };
        second.expected_source_revision = Some(snapshot.current.source_revision.clone());
        second.expected_program_digest = Some(snapshot.current.program_digest);
        second.contributing_operations = vec![first_intent];
        second.plan = crate::SemanticPlan::new(1, vec![43]).unwrap();
        second_write.accepted_at = Timestamp::from(1_001);
        let second = store.accept_write(second_write).unwrap();
        let second_intent = second.journaled_intent_id().unwrap();
        let second_receipt = second.journaled_receipt_id().unwrap();
        let snapshot = store
            .replaceable_operation_snapshot(&semantic_coordinate())
            .unwrap()
            .unwrap();
        let mut rematerialize = crate::SemanticRematerialize {
            coordinate: semantic_coordinate(),
            expected_source_revision: snapshot.current.source_revision.clone(),
            expected_program_digest: snapshot.current.program_digest,
            expected_current_materialization: None,
            source: semantic_source(),
            evaluated_at: Timestamp::from(1_001),
            materialized: Some(crate::MaterializationCandidate {
                event: UnsignedEvent::new(
                    keys().public_key(),
                    Timestamp::from(1_001),
                    Kind::ContactList,
                    Vec::new(),
                    "semantic-u5-shared",
                ),
                routing: "semantic-u5-route".into(),
                sig_state: crate::PendingMaterializationState::Pending,
            }),
            contributing_operations: vec![first_intent, second_intent],
            resolved_operations: Vec::new(),
        };
        rematerialize.contributing_operations.sort();
        assert!(matches!(
            store
                .install_replaceable_materialization(rematerialize)
                .unwrap(),
            SemanticInstallOutcome::Installed { .. }
        ));
        (first_receipt, second_receipt)
    };

    crash(&path, "semantic-promote-before-commit");
    {
        let mut store = RedbStore::open(&path).unwrap();
        let row = store
            .query(
                &Filter::new()
                    .kind(Kind::ContactList)
                    .author(keys().public_key()),
            )
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(row.event.sig, sentinel_signature());
        for receipt_id in [receipt_a, receipt_b] {
            let receipt = store.reattach_receipt(receipt_id).unwrap().unwrap();
            assert!(matches!(
                receipt.payload,
                PublishQueueReceiptPayload::ReplaceableOperation {
                    state: ReplaceableOperationReceiptState::Contributing {
                        current: Some(MaterializationReceipt {
                            sig_state: IntentSigState::Pending,
                            ..
                        }),
                    },
                    ..
                }
            ));
        }
        let (target, verified) = semantic_promotion_target(&store);
        assert!(matches!(
            store.promote_signed(target, verified).unwrap(),
            PromoteOutcome::MaterializationPromoted { ref members, .. } if members.len() == 2
        ));
    }
    let store = RedbStore::open(&path).unwrap();
    for receipt_id in [receipt_a, receipt_b] {
        let receipt = store.reattach_receipt(receipt_id).unwrap().unwrap();
        assert!(matches!(
            receipt.payload,
            PublishQueueReceiptPayload::ReplaceableOperation {
                state: ReplaceableOperationReceiptState::Contributing {
                    current: Some(MaterializationReceipt {
                        sig_state: IntentSigState::Signed,
                        ..
                    }),
                },
                ..
            }
        ));
    }
}

#[test]
fn terminal_destinations_close_every_semantic_receipt_and_compact_the_program() {
    let (_dir, path) = fixture();
    let relay_one = RelayUrl::parse(RELAY).unwrap();

    let (receipt_a, receipt_b, intent_a, intent_b, close, stale_install) = {
        let mut store = RedbStore::open(&path).unwrap();
        let first_write = semantic_accept_write();
        let first = store.accept_write(first_write).unwrap();
        let intent_a = first.journaled_intent_id().unwrap();
        let receipt_a = first.journaled_receipt_id().unwrap();

        let snapshot = store
            .replaceable_operation_snapshot(&semantic_coordinate())
            .unwrap()
            .unwrap();
        let mut second_write = semantic_accept_write();
        let crate::AcceptWritePayload::ReplaceableOperation(second) = &mut second_write.payload
        else {
            unreachable!()
        };
        second.expected_source_revision = Some(snapshot.current.source_revision.clone());
        second.expected_program_digest = Some(snapshot.current.program_digest);
        second.contributing_operations = vec![intent_a];
        second.plan = crate::SemanticPlan::new(1, vec![43]).unwrap();
        second_write.accepted_at = Timestamp::from(1_001);
        let second = store.accept_write(second_write).unwrap();
        let intent_b = second.journaled_intent_id().unwrap();
        let receipt_b = second.journaled_receipt_id().unwrap();

        let snapshot = store
            .replaceable_operation_snapshot(&semantic_coordinate())
            .unwrap()
            .unwrap();
        let installed = store
            .install_replaceable_materialization(crate::SemanticRematerialize {
                coordinate: semantic_coordinate(),
                expected_source_revision: snapshot.current.source_revision.clone(),
                expected_program_digest: snapshot.current.program_digest,
                expected_current_materialization: None,
                source: semantic_source(),
                evaluated_at: Timestamp::from(1_001),
                materialized: Some(crate::MaterializationCandidate {
                    event: UnsignedEvent::new(
                        keys().public_key(),
                        Timestamp::from(1_001),
                        Kind::ContactList,
                        Vec::new(),
                        "finite-round",
                    ),
                    routing: "finite-round-route".into(),
                    sig_state: crate::PendingMaterializationState::Pending,
                }),
                contributing_operations: vec![intent_a, intent_b],
                resolved_operations: Vec::new(),
            })
            .unwrap();
        assert!(matches!(
            installed,
            SemanticInstallOutcome::Installed { .. }
        ));
        let (target, verified) = semantic_promotion_target(&store);
        store.promote_signed(target, verified).unwrap();

        let snapshot = store
            .replaceable_operation_snapshot(&semantic_coordinate())
            .unwrap()
            .unwrap();
        let generation = snapshot.current.generation.clone().unwrap();
        let owner = *generation.members.first().unwrap();
        store
            .record_route_revision(owner, BTreeSet::from([relay_one]))
            .unwrap();
        let mut lane = store
            .bootstrap_publish_queue_lanes(owner)
            .unwrap()
            .remove(0);
        lane = store
            .set_lane_eligible(&lane.key, lane.revision, Timestamp::from(1_010))
            .unwrap();
        let signed = store
            .query(&Filter::new().id(generation.materialization.event_id))
            .unwrap()
            .remove(0)
            .event;
        let (_, started) = store
            .start_lane_attempt(&lane.key, lane.revision, signed, Timestamp::from(1_011))
            .unwrap();
        lane = store
            .record_lane_handoff(
                &started.key,
                started.revision,
                started.last_ordinal,
                PublishQueueAttemptHandoff {
                    at: Timestamp::from(1_012),
                    result: HandoffEvidence::Written,
                },
                PublishQueuePostHandoffState::AwaitingAck {
                    deadline: Timestamp::from(1_020),
                },
            )
            .unwrap();
        store
            .finish_lane_attempt(
                &lane.key,
                lane.revision,
                lane.last_ordinal,
                PublishQueueAttemptOutcome::Acked,
                Timestamp::from(1_013),
            )
            .unwrap();

        let close = crate::SemanticCohortClose {
            coordinate: semantic_coordinate(),
            expected_source_revision: snapshot.current.source_revision,
            expected_program_digest: snapshot.current.program_digest,
            expected_materialization: generation.materialization,
            destination: crate::SemanticDestinationPlanClosure::AllCurrentDestinationsTerminal,
        };
        let stale_install = semantic_source_install(&store);
        let outcome = store
            .close_replaceable_operation_cohort(close.clone())
            .unwrap();
        assert!(matches!(
            outcome,
            crate::SemanticCohortCloseOutcome::Closed { ref members, .. }
                if members.len() == 2
        ));
        (
            receipt_a,
            receipt_b,
            intent_a,
            intent_b,
            close,
            stale_install,
        )
    };

    let mut store = RedbStore::open(&path).unwrap();
    assert!(store
        .replaceable_operation_snapshot(&semantic_coordinate())
        .unwrap()
        .is_none());
    assert!(store.recover_publish_queue().unwrap().is_empty());
    for (receipt_id, intent_id) in [(receipt_a, intent_a), (receipt_b, intent_b)] {
        let receipt = store.reattach_receipt(receipt_id).unwrap().unwrap();
        assert_eq!(receipt.intent_id, Some(intent_id));
        assert!(matches!(
            receipt.payload,
            PublishQueueReceiptPayload::ReplaceableOperation {
                state: ReplaceableOperationReceiptState::Settled,
                ..
            }
        ));
    }
    assert_eq!(
        store.close_replaceable_operation_cohort(close).unwrap(),
        crate::SemanticCohortCloseOutcome::Stale
    );
    assert_eq!(
        store
            .install_replaceable_source_materialization(stale_install)
            .unwrap(),
        SemanticInstallOutcome::Stale
    );
}

fn event_and_request_coverage_state(path: &Path) -> (bool, bool, bool) {
    let (_, signed) = event_pair();
    let atom = retention_atom();
    let first = RelayUrl::parse(RELAY).expect("first relay");
    let second = RelayUrl::parse(RELAY_TWO).expect("second relay");
    let store = RedbStore::open(path).expect("reopen coverage-ordering fixture");
    let event_present = !store
        .query(&Filter::new().id(signed.id))
        .expect("query event after crash")
        .is_empty();
    let key = compute_coverage_key(&atom);
    (
        event_present,
        store
            .get_coverage(key, &first)
            .expect("coverage read after crash")
            .is_some(),
        store
            .get_coverage(key, &second)
            .expect("coverage read after crash")
            .is_some(),
    )
}

#[test]
fn event_then_multi_claim_coverage_has_only_allowed_restart_states() {
    for (point, seed_event, expected) in [
        ("observation-before-commit", false, (false, false, false)),
        ("observation-after-commit", false, (true, false, false)),
        ("coverage-before-commit", true, (true, false, false)),
        ("coverage-after-commit", true, (true, true, true)),
    ] {
        let (_dir, path) = fixture();
        if seed_event {
            let (_, signed) = event_pair();
            let mut store = RedbStore::open(&path).expect("initialize coverage-ordering fixture");
            store
                .insert(
                    signed,
                    RelayObserved::new(
                        RelayUrl::parse(RELAY).expect("relay"),
                        Timestamp::from(2_000u64),
                    ),
                )
                .expect("seed durable event");
        } else {
            RedbStore::open(&path).expect("initialize empty coverage-ordering fixture");
        }

        crash(&path, point);
        let first = event_and_request_coverage_state(&path);
        assert_eq!(first, expected, "unexpected recovered state at {point}");
        let second = event_and_request_coverage_state(&path);
        assert_eq!(
            second, first,
            "semantic state changed on second reopen at {point}"
        );
        assert_eq!(
            second.1, second.2,
            "one request-level coverage claim became visible without the other at {point}"
        );
        assert!(
            second.0 || (!second.1 && !second.2),
            "coverage became visible without its event at {point}"
        );
    }
}

#[test]
fn packed_segments_catalog_and_ingest_commit_are_atomic_across_process_death() {
    for point in [
        "postings-before-segments",
        "postings-after-segments",
        "postings-before-catalog",
        "postings-after-catalog",
        "postings-before-commit",
    ] {
        let (_dir, path) = fixture();
        RedbStore::open(&path).expect("initialize packed ingest fixture");
        crash(&path, point);

        let store = RedbStore::open(&path).expect("reopen rolled-back packed ingest");
        assert!(
            store
                .query(&Filter::new().id(packed_event(0).id))
                .expect("query packed ingest rollback")
                .is_empty(),
            "canonical row and every packed artifact must roll back at {point}"
        );
    }

    let (_dir, path) = fixture();
    RedbStore::open(&path).expect("initialize committed packed ingest fixture");
    crash(&path, "postings-after-commit");
    let store = RedbStore::open(&path).expect("reopen committed packed ingest");
    assert_eq!(
        store
            .query(&Filter::new().id(packed_event(0).id))
            .expect("query committed packed ingest")
            .len(),
        1,
        "a process death after commit must retain the canonical row and packed publication"
    );
}

#[test]
fn packed_death_publication_is_atomic_across_process_death() {
    for point in ["postings-before-death", "postings-after-death"] {
        let (_dir, path) = fixture();
        let relay = RelayUrl::parse(RELAY).expect("relay");
        {
            let mut store = RedbStore::open(&path).expect("initialize packed death fixture");
            store
                .insert_batch(
                    [packed_event(0), packed_event(1)]
                        .into_iter()
                        .map(|event| {
                            (
                                event,
                                RelayObserved::new(relay.clone(), Timestamp::from(3_000u64)),
                            )
                        })
                        .collect(),
                )
                .expect("seed one multi-event packed run");
        }

        crash(&path, point);
        let store = RedbStore::open(&path).expect("reopen rolled-back packed death");
        assert_eq!(
            store
                .query(&Filter::new().ids([packed_event(0).id, packed_event(1).id]))
                .expect("query packed death rollback")
                .len(),
            2,
            "canonical removal and the packed death block must roll back together at {point}"
        );
    }
}

#[test]
fn packed_compaction_output_is_never_partially_published() {
    for point in [
        "postings-before-compaction-output",
        "postings-after-compaction-output",
    ] {
        let (_dir, path) = fixture();
        let relay = RelayUrl::parse(RELAY).expect("relay");
        {
            let mut store = RedbStore::open(&path).expect("initialize compaction fixture");
            for index in 0..7 {
                store
                    .insert(
                        packed_event(index),
                        RelayObserved::new(relay.clone(), Timestamp::from(3_000u64 + index)),
                    )
                    .expect("seed one packed run");
            }
        }

        crash(&path, point);
        let store = RedbStore::open(&path).expect("reopen rolled-back compaction");
        assert_eq!(
            store
                .query(&Filter::new())
                .expect("query compaction rollback")
                .len(),
            7,
            "the triggering event and every staged compaction artifact must roll back at {point}"
        );
        assert!(store
            .query(&Filter::new().id(packed_event(7).id))
            .expect("query compaction trigger")
            .is_empty());
    }
}

#[test]
fn correlation_row_is_all_or_nothing_with_its_receipt() {
    let (_dir, path) = fixture();
    RedbStore::open(&path).expect("initialize store");
    crash(&path, "accept-before-commit-with-correlation");

    assert_eq!(event_table_len(&path), 0, "no orphan event");
    assert_eq!(
        publish_queue_table_len(&path, PUBLISH_QUEUE_RECEIPTS),
        0,
        "no orphan receipt"
    );
    assert_eq!(
        correlation_table_len(&path),
        0,
        "no orphan correlation mapping"
    );

    let mut reopened = RedbStore::open(&path).expect("reopen after crash");
    assert_eq!(
        reopened
            .lookup_correlation("u5-correlation-token")
            .expect("lookup after rollback"),
        None,
        "the rolled-back token must not resolve to anything"
    );

    let (frozen, _) = event_pair();
    let outcome = reopened
        .accept_write(accept_with_correlation(frozen, "u5-correlation-token"))
        .expect("accept after rollback");
    let receipt_id = outcome.journaled_receipt_id().expect("receipt id");
    assert_eq!(
        reopened
            .lookup_correlation("u5-correlation-token")
            .expect("lookup after successful accept"),
        Some(receipt_id)
    );
    drop(reopened);

    assert_eq!(publish_queue_table_len(&path, PUBLISH_QUEUE_RECEIPTS), 1);
    assert_eq!(correlation_table_len(&path), 1);
    assert_path_canonical_integrity(&path);
}

#[test]
fn explicit_retention_eviction_and_coverage_lowering_are_atomic_across_process_death() {
    let (_dir, path) = fixture();
    let relay = RelayUrl::parse(RELAY).expect("relay");
    let atom = retention_atom();
    let key = compute_coverage_key(&atom);
    let before = CoverageInterval::new(Timestamp::from(900u64), Timestamp::from(1_100u64));
    let (_, signed) = event_pair();

    {
        let mut store = RedbStore::open(&path).expect("initialize retention fixture");
        store
            .insert(
                signed.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(2_000u64)),
            )
            .expect("insert durable row");
        store
            .record_coverage(&[(atom.clone(), relay.clone(), before)])
            .expect("record covering evidence");
    }

    crash(&path, "gc-before-commit");

    {
        let store = RedbStore::open(&path).expect("reopen rolled-back retention fixture");
        let rows = store
            .query(&Filter::new().id(signed.id))
            .expect("query retained row after crash");
        assert_eq!(rows.len(), 1, "row deletion must roll back with coverage");
        assert_eq!(
            rows[0].provenance.seen.get(&relay),
            Some(&Timestamp::from(2_000u64)),
            "retained provenance must roll back with its row"
        );
        assert_eq!(
            store.get_coverage(key, &relay).expect("coverage read"),
            Some(before),
            "coverage lowering must roll back with row deletion"
        );
    }

    let mut store = RedbStore::open(&path).expect("reopen for successful explicit policy");
    let report = store
        .gc(&GcRetentionSet::new(Vec::new()))
        .expect("apply explicit retention policy");
    assert_eq!(report.events_evicted, 1);
    assert_eq!(report.coverage_rows_shrunk, 1);
    assert!(store
        .query(&Filter::new().id(signed.id))
        .expect("query after explicit eviction")
        .is_empty());
    assert_eq!(
        store.get_coverage(key, &relay).expect("coverage read"),
        Some(CoverageInterval::new(
            Timestamp::from(1_001u64),
            Timestamp::from(1_100u64),
        )),
        "successful explicit policy must lower evidence with row deletion"
    );
}

#[test]
fn committed_retention_eviction_and_coverage_lowering_survive_process_death() {
    let (_dir, path) = fixture();
    let relay = RelayUrl::parse(RELAY).expect("relay");
    let atom = retention_atom();
    let key = compute_coverage_key(&atom);
    let before = CoverageInterval::new(Timestamp::from(900u64), Timestamp::from(1_100u64));
    let (_, signed) = event_pair();

    {
        let mut store = RedbStore::open(&path).expect("initialize committed retention fixture");
        store
            .insert(
                signed.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(2_000u64)),
            )
            .expect("insert durable row");
        store
            .record_coverage(&[(atom.clone(), relay.clone(), before)])
            .expect("record covering evidence");
    }

    crash(&path, "gc-after-commit");
    let store = RedbStore::open(&path).expect("reopen committed retention fixture");
    assert!(
        store
            .query(&Filter::new().id(signed.id))
            .expect("query after committed GC crash")
            .is_empty(),
        "event removal must survive a process death after commit"
    );
    assert_eq!(
        store.get_coverage(key, &relay).expect("coverage read"),
        Some(CoverageInterval::new(
            Timestamp::from(1_001u64),
            Timestamp::from(1_100u64),
        )),
        "coverage lowering must survive the same committed boundary"
    );
}

#[test]
fn relay_observation_dictionary_and_refcount_are_atomic_across_process_death() {
    let (_dir, path) = fixture();
    let (_, signed) = event_pair();
    let first = RelayUrl::parse(RELAY).expect("first relay");
    let second = RelayUrl::parse(RELAY_TWO).expect("second relay");
    {
        let mut store = RedbStore::open(&path).expect("open initial store");
        store
            .insert(
                signed.clone(),
                RelayObserved::new(first.clone(), Timestamp::from(1_000u64)),
            )
            .expect("insert initial observation");
    }

    crash(&path, "observation-before-commit");
    let mut reopened = RedbStore::open(&path).expect("reopen observation crash");
    let row = reopened
        .query(&Filter::new().id(signed.id))
        .unwrap()
        .remove(0);
    assert_eq!(
        row.provenance.seen,
        BTreeMap::from([(first, Timestamp::from(1_000u64))])
    );

    reopened
        .insert(
            signed.clone(),
            RelayObserved::new(second.clone(), Timestamp::from(2_000u64)),
        )
        .expect("commit second observation after rollback");
    drop(reopened);
    assert_path_canonical_integrity(&path);
    let store = RedbStore::open(&path).expect("reopen committed observations");
    let row = store.query(&Filter::new().id(signed.id)).unwrap().remove(0);
    assert_eq!(
        row.provenance.seen.get(&second),
        Some(&Timestamp::from(2_000u64))
    );
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
        assert_eq!(
            store.recover_publish_queue().expect("recover delivery")[0]
                .event_work()
                .expect("ordinary event work")
                .3,
            IntentSigState::Pending
        );
        assert_eq!(
            store.query(&Filter::new().id(frozen.id)).unwrap()[0]
                .event
                .sig,
            sentinel_signature()
        );
        assert_eq!(
            store
                .reattach_receipt(receipt)
                .unwrap()
                .unwrap()
                .event_state(),
            Some(ReceiptState::Accepted)
        );
        store
            .promote_signed(crate::PromotionTarget::Event(intent), evidence(&signed))
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
        store
            .reattach_receipt(receipt)
            .unwrap()
            .unwrap()
            .event_state(),
        Some(ReceiptState::Signed)
    );

    let (_dir, path) = fixture();
    let (older, older_signed) = pair(Kind::ContactList, "older", 900);
    let older_id = older.id;
    let (newer, _) = pair(Kind::ContactList, "newer", 1_000);
    let newer_id = newer.id;
    let (intent, receipt) = {
        let mut store = RedbStore::open(&path).expect("open");
        let older_outcome = store.accept_write(accept(older)).expect("accept older");
        let older_intent = older_outcome.journaled_intent_id().unwrap();
        store
            .promote_signed(
                crate::PromotionTarget::Event(older_intent),
                evidence(&older_signed),
            )
            .expect("promote older");
        let relay = RelayUrl::parse(RELAY).expect("relay");
        store
            .record_route_revision(older_intent, BTreeSet::from([relay]))
            .expect("route older");
        let lane = store
            .bootstrap_publish_queue_lanes(older_intent)
            .expect("bootstrap older lane")
            .remove(0);
        let lane = store
            .set_lane_eligible(&lane.key, lane.revision, Timestamp::from(950u64))
            .expect("make older lane eligible");
        store
            .start_lane_attempt(
                &lane.key,
                lane.revision,
                older_signed,
                Timestamp::from(951u64),
            )
            .expect("start older attempt");
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
        assert_eq!(
            store
                .recover_publish_queue()
                .expect("recover delivery")
                .len(),
            1
        );
        assert!(matches!(
            store.compensate_write(intent).unwrap(),
            CompensateOutcome::Compensated { .. }
        ));
    }
    let store = RedbStore::open(&path).expect("reopen compensated state");
    assert!(store.query(&Filter::new().id(newer_id)).unwrap().is_empty());
    assert!(store.query(&Filter::new().id(older_id)).unwrap().is_empty());
    assert_eq!(
        store
            .recover_publish_queue()
            .expect("recover delivery")
            .len(),
        0
    );
    assert_eq!(
        store
            .reattach_receipt(receipt)
            .unwrap()
            .unwrap()
            .event_state(),
        Some(ReceiptState::Compensated)
    );
}

#[test]
fn cancellation_crash_cannot_claim_a_terminal_fact_before_compensation_commits() {
    let (_dir, path) = fixture();
    let (intent, receipt) = {
        let mut store = RedbStore::open(&path).expect("open");
        accepted(&mut store)
    };

    crash(&path, "cancel-before-commit");
    {
        let mut store = RedbStore::open(&path).expect("reopen after cancellation crash");
        assert_eq!(
            store
                .reattach_receipt(receipt)
                .unwrap()
                .unwrap()
                .event_state(),
            Some(ReceiptState::Accepted)
        );
        assert_eq!(
            store.recover_publish_queue().expect("recover delivery")[0].intent_id,
            intent
        );
        assert!(matches!(
            store.cancel_write(intent).unwrap(),
            CompensateOutcome::Compensated { .. }
        ));
    }
    let store = RedbStore::open(&path).expect("reopen cancelled state");
    assert_eq!(
        store
            .reattach_receipt(receipt)
            .unwrap()
            .unwrap()
            .event_state(),
        Some(ReceiptState::Cancelled)
    );
    assert!(store
        .recover_publish_queue()
        .expect("recover delivery")
        .is_empty());
}

#[test]
fn lane_cursor_detail_deadline_and_close_are_atomic_across_process_death() {
    let (_dir, path) = fixture();
    let (_, signed) = event_pair();
    let relay = RelayUrl::parse(RELAY).expect("relay");
    let intent = {
        let mut store = RedbStore::open(&path).expect("open");
        let (intent, _) = accepted(&mut store);
        store
            .promote_signed(crate::PromotionTarget::Event(intent), evidence(&signed))
            .expect("promote");
        store
            .record_route_revision(intent, BTreeSet::from([relay.clone()]))
            .expect("route");
        intent
    };

    crash(&path, "lane-bootstrap-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen bootstrap crash");
    assert!(store
        .recover_publish_queue_lanes(intent)
        .unwrap()
        .is_empty());
    let mut lane = store
        .bootstrap_publish_queue_lanes(intent)
        .unwrap()
        .remove(0);
    assert_eq!(lane.state, PublishQueueLaneState::WaitingConnection);
    drop(store);

    crash(&path, "lane-transition-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen transition crash");
    lane = store.recover_publish_queue_lanes(intent).unwrap().remove(0);
    assert_eq!(lane.state, PublishQueueLaneState::WaitingConnection);
    assert_eq!(store.next_publish_queue_deadline().unwrap(), None);
    store
        .set_lane_eligible(&lane.key, lane.revision, Timestamp::from(1_500u64))
        .unwrap();
    drop(store);

    crash(&path, "lane-start-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen start crash");
    lane = store.recover_publish_queue_lanes(intent).unwrap().remove(0);
    assert!(matches!(lane.state, PublishQueueLaneState::Eligible { .. }));
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
    lane = store.recover_publish_queue_lanes(intent).unwrap().remove(0);
    assert!(matches!(
        lane.state,
        PublishQueueLaneState::InFlight {
            phase: PublishQueueInFlightPhase::AwaitingHandoff,
            ..
        }
    ));
    assert!(store.recover_attempt_details(intent).unwrap()[0]
        .handoff
        .is_none());
    assert_eq!(store.next_publish_queue_deadline().unwrap(), None);
    let handoff = PublishQueueAttemptHandoff {
        at: Timestamp::from(1_600u64),
        result: HandoffEvidence::Written,
    };
    store
        .record_lane_handoff(
            &lane.key,
            lane.revision,
            lane.last_ordinal,
            handoff.clone(),
            PublishQueuePostHandoffState::AwaitingAck {
                deadline: Timestamp::from(1_630u64),
            },
        )
        .unwrap();
    assert_eq!(
        store.next_publish_queue_deadline().unwrap(),
        Some(Timestamp::from(1_630u64))
    );
    drop(store);

    crash(&path, "lane-finish-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen lane finish crash");
    lane = store.recover_publish_queue_lanes(intent).unwrap().remove(0);
    assert!(matches!(
        lane.state,
        PublishQueueLaneState::InFlight {
            phase: PublishQueueInFlightPhase::AwaitingAck { .. },
            ..
        }
    ));
    assert!(store.recover_attempt_details(intent).unwrap()[0]
        .terminal
        .is_none());
    assert_eq!(
        store.next_publish_queue_deadline().unwrap(),
        Some(Timestamp::from(1_630u64))
    );
    lane = store
        .finish_lane_attempt(
            &lane.key,
            lane.revision,
            lane.last_ordinal,
            PublishQueueAttemptOutcome::Acked,
            Timestamp::from(1_610u64),
        )
        .unwrap();
    assert!(matches!(lane.state, PublishQueueLaneState::Terminal { .. }));
    let committed_detail = store.recover_attempt_details(intent).unwrap().remove(0);
    assert_eq!(
        committed_detail.terminal,
        Some(PublishQueueAttemptOutcome::Acked)
    );
    assert_eq!(
        committed_detail.finished_at,
        Some(Timestamp::from(1_610u64))
    );
    assert_eq!(store.next_publish_queue_deadline().unwrap(), None);
    drop(store);

    crash(&path, "lane-close-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen close crash");
    assert_eq!(
        store
            .recover_publish_queue()
            .expect("recover delivery")
            .len(),
        1
    );
    assert_eq!(store.recover_publish_queue_lanes(intent).unwrap().len(), 1);
    assert_eq!(store.recover_attempts(intent).unwrap().len(), 1);
    assert_eq!(store.recover_attempt_details(intent).unwrap().len(), 1);
    assert_eq!(
        store.close_terminal_intent(intent).unwrap(),
        CloseIntentOutcome::Closed
    );
    drop(store);

    let store = RedbStore::open(&path).expect("final reopen");
    assert!(store
        .recover_publish_queue()
        .expect("recover delivery")
        .is_empty());
    assert_eq!(store.recover_publish_queue_lanes(intent).unwrap().len(), 1);
    assert_eq!(
        store.recover_attempts(intent).unwrap()[0].outcome,
        PublishQueueAttemptOutcome::Acked
    );
    assert_eq!(store.recover_attempt_details(intent).unwrap().len(), 1);
}

#[test]
fn auth_denial_is_not_observable_after_process_death_before_commit() {
    let (_dir, path) = fixture();
    let (_, signed) = event_pair();
    let relay = RelayUrl::parse(RELAY).expect("relay");
    let intent = {
        let mut store = RedbStore::open(&path).expect("open");
        let (intent, _) = accepted(&mut store);
        store
            .promote_signed(crate::PromotionTarget::Event(intent), evidence(&signed))
            .expect("promote");
        store
            .record_route_revision(intent, BTreeSet::from([relay]))
            .expect("route");
        let lane = store
            .bootstrap_publish_queue_lanes(intent)
            .unwrap()
            .remove(0);
        store
            .set_lane_waiting(&lane.key, lane.revision, true)
            .expect("wait for AUTH");
        intent
    };

    crash(&path, "lane-auth-denial-before-commit");
    let mut store = RedbStore::open(&path).expect("reopen after denial crash");
    let waiting = store.recover_publish_queue_lanes(intent).unwrap().remove(0);
    assert_eq!(waiting.state, PublishQueueLaneState::WaitingAuth);
    let denial = AuthDenial {
        source: AuthDenialSource::Policy,
        reason: "account not permitted".into(),
    };
    store
        .deny_lane_auth(&waiting.key, waiting.revision, denial.clone())
        .expect("commit AUTH denial");
    drop(store);

    let store = RedbStore::open(&path).expect("reopen committed denial");
    assert!(matches!(
        store.recover_publish_queue_lanes(intent).unwrap()[0].state,
        PublishQueueLaneState::Terminal {
            outcome: PublishQueueTerminalOutcome::AuthDenied(ref current),
            ordinal: 0,
        } if current == &denial
    ));
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
    let recovered = store.recover_publish_queue().expect("recover delivery");
    assert_eq!(
        (
            recovered.len(),
            recovered[0].intent_id,
            recovered[0].receipt_id
        ),
        (1, intent, receipt)
    );
}
