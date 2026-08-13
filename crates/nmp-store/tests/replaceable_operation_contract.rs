use nmp_store::{
    AcceptOutcome, AcceptWrite, AcceptWritePayload, AccessContextId, EventStore, IntentSigState,
    MaterializationAttempt, MaterializationCandidate, MaterializationId, MaterializationWait,
    MemoryStore, PendingMaterializationState, PromoteOutcome, PromotionTarget,
    PublishQueueReceiptPayload, QualifiedSource, RedbStore, ReplaceableOperationReceiptProgress,
    ReplaceableOperationReceiptState, ReplayFormatId, ReplayProgramId, SemanticAccept,
    SemanticInstallOutcome, SemanticPlan, SemanticRematerialize, SourceEvidence, SourcePlanId,
    StartingSource, StartingSourceRequirement, VerifiedSignature,
};
use nostr::nips::nip01::Coordinate;
use nostr::{Filter, Keys, Kind, Timestamp, UnsignedEvent};

fn coordinate(keys: &Keys) -> Coordinate {
    Coordinate {
        kind: Kind::ContactList,
        public_key: keys.public_key(),
        identifier: String::new(),
    }
}

fn source() -> SourceEvidence {
    SourceEvidence {
        plan: SourcePlanId([3; 32]),
        access: AccessContextId([4; 32]),
        qualified: QualifiedSource::Absent,
    }
}

fn event_source(byte: u8, created_at: u64) -> SourceEvidence {
    SourceEvidence {
        plan: SourcePlanId([3; 32]),
        access: AccessContextId([4; 32]),
        qualified: QualifiedSource::Event {
            event_id: nostr::EventId::from_byte_array([byte; 32]),
            created_at: Timestamp::from(created_at),
        },
    }
}

fn starting_source() -> StartingSourceRequirement {
    StartingSourceRequirement {
        plan: SourcePlanId([3; 32]),
        access: AccessContextId([4; 32]),
        source: StartingSource::Absent,
    }
}

fn bodyless_accept(
    coordinate: Coordinate,
    snapshot: Option<&nmp_store::SemanticCurrentState>,
    existing: Vec<nmp_store::IntentId>,
    byte: u8,
) -> SemanticAccept {
    SemanticAccept {
        coordinate,
        program: ReplayProgramId([7; 16]),
        format: ReplayFormatId([9; 16]),
        expected_source_revision: snapshot.map(|state| state.source_revision.clone()),
        expected_program_digest: snapshot.map(|state| state.program_digest),
        expected_current_materialization: snapshot
            .and_then(|state| state.generation())
            .map(|generation| generation.materialization.materialization_id),
        expected_materialization_revision: snapshot.map(|state| state.materialization_revision),
        starting_source: starting_source(),
        source: source(),
        plan: SemanticPlan::new(1, vec![byte]).unwrap(),
        materialization: MaterializationAttempt::Waiting(MaterializationWait::Content),
        contributing_operations: existing,
        resolved_operations: Vec::new(),
    }
}

fn accept_operation(
    store: &mut dyn EventStore,
    keys: &Keys,
    accepted_at: u64,
    accept: SemanticAccept,
) -> (nmp_store::IntentId, u64) {
    let outcome = store
        .accept_write(AcceptWrite {
            payload: AcceptWritePayload::ReplaceableOperation(Box::new(accept)),
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "test-key".into(),
            accepted_at: Timestamp::from(accepted_at),
            correlation: None,
        })
        .unwrap();
    match outcome {
        AcceptOutcome::ReplaceableOperation {
            intent_id,
            receipt_id,
            current,
        } => {
            assert!(current.generation().is_none());
            (intent_id, receipt_id)
        }
        other => panic!("expected bodyless replaceable operation, got {other:?}"),
    }
}

fn assert_receipt_current(
    store: &dyn EventStore,
    receipt_id: u64,
    expected_materialization: MaterializationId,
    expected_sig_state: IntentSigState,
) {
    let receipt = store.reattach_receipt(receipt_id).unwrap().unwrap();
    match receipt.payload {
        PublishQueueReceiptPayload::ReplaceableOperation {
            state:
                ReplaceableOperationReceiptState::Contributing {
                    progress: ReplaceableOperationReceiptProgress::Current(current),
                },
            ..
        } => {
            assert_eq!(
                current.materialization.materialization_id,
                expected_materialization
            );
            assert_eq!(current.sig_state, expected_sig_state);
        }
        other => panic!("expected contributing materialization receipt, got {other:?}"),
    }
}

fn assert_receipt_waiting(store: &dyn EventStore, receipt_id: u64, expected: MaterializationWait) {
    let receipt = store.reattach_receipt(receipt_id).unwrap().unwrap();
    assert!(matches!(
        receipt.payload,
        PublishQueueReceiptPayload::ReplaceableOperation {
            state: ReplaceableOperationReceiptState::Contributing {
                progress: ReplaceableOperationReceiptProgress::Waiting {
                    reason: waiting,
                    current: None,
                },
            },
            ..
        } if waiting == expected
    ));
}

fn install_shared_materialization(
    store: &mut dyn EventStore,
    keys: &Keys,
    coordinate: &Coordinate,
    members: Vec<nmp_store::IntentId>,
) -> nmp_store::SemanticCurrentState {
    let snapshot = store
        .replaceable_operation_snapshot(coordinate)
        .unwrap()
        .unwrap();
    let created_at = snapshot
        .operations
        .iter()
        .map(|operation| operation.accepted_at.as_secs())
        .max()
        .unwrap();
    let event = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(created_at),
        Kind::ContactList,
        Vec::new(),
        "materialized",
    );
    let outcome = store
        .install_replaceable_materialization(SemanticRematerialize {
            coordinate: coordinate.clone(),
            expected_source_revision: snapshot.current.source_revision.clone(),
            expected_program_digest: snapshot.current.program_digest,
            expected_current_materialization: None,
            expected_materialization_revision: snapshot.current.materialization_revision,
            source: source(),
            evaluated_at: Timestamp::from(created_at),
            materialization: MaterializationAttempt::Complete(MaterializationCandidate {
                event,
                routing: "test-route".into(),
                sig_state: PendingMaterializationState::Pending,
            }),
            contributing_operations: members,
            resolved_operations: Vec::new(),
        })
        .unwrap();
    match outcome {
        SemanticInstallOutcome::Installed { current, .. } => current,
        other => panic!("expected installed materialization, got {other:?}"),
    }
}

fn exercise_bodyless_shared_lifecycle(store: &mut dyn EventStore) {
    let keys = Keys::generate();
    let coordinate = coordinate(&keys);
    let (first, first_receipt) = accept_operation(
        store,
        &keys,
        10,
        bodyless_accept(coordinate.clone(), None, Vec::new(), 1),
    );
    let before_second = store
        .replaceable_operation_snapshot(&coordinate)
        .unwrap()
        .unwrap();
    let (second, second_receipt) = accept_operation(
        store,
        &keys,
        11,
        bodyless_accept(
            coordinate.clone(),
            Some(&before_second.current),
            vec![first],
            2,
        ),
    );
    let current = install_shared_materialization(store, &keys, &coordinate, vec![first, second]);
    let generation = current.generation().cloned().unwrap();
    assert_eq!(
        generation.materialization.materialization_id,
        MaterializationId(1)
    );
    assert_eq!(generation.members.len(), 2);
    assert_receipt_current(
        store,
        first_receipt,
        MaterializationId(1),
        IntentSigState::Pending,
    );
    assert_receipt_current(
        store,
        second_receipt,
        MaterializationId(1),
        IntentSigState::Pending,
    );

    let row = store
        .query(
            &Filter::new()
                .kind(Kind::ContactList)
                .author(keys.public_key()),
        )
        .unwrap()
        .pop()
        .unwrap();
    let signed = UnsignedEvent::new(
        row.event.pubkey,
        row.event.created_at,
        row.event.kind,
        row.event.tags.clone(),
        row.event.content.clone(),
    )
    .sign_with_keys(&keys)
    .unwrap();
    let promoted = store
        .promote_signed(
            PromotionTarget::ReplaceableMaterialization(Box::new(
                nmp_store::ReplaceableMaterializationTarget {
                    coordinate: coordinate.clone(),
                    expected_source_revision: current.source_revision.clone(),
                    expected_program_digest: current.program_digest,
                    expected_materialization: MaterializationId(1),
                    expected_event_id: generation.materialization.event_id,
                },
            )),
            VerifiedSignature::verify(&signed).unwrap(),
        )
        .unwrap();
    assert!(matches!(
        promoted,
        PromoteOutcome::MaterializationPromoted { ref members, .. } if members == &vec![first, second]
    ));
    assert_receipt_current(
        store,
        first_receipt,
        MaterializationId(1),
        IntentSigState::Signed,
    );
    assert_receipt_current(
        store,
        second_receipt,
        MaterializationId(1),
        IntentSigState::Signed,
    );
}

#[test]
fn bodyless_accept_install_shared_generation_and_promote_have_memory_redb_parity() {
    let mut memory = MemoryStore::new();
    exercise_bodyless_shared_lifecycle(&mut memory);

    let dir = tempfile::tempdir().unwrap();
    let mut redb = RedbStore::open(dir.path().join("replaceable.redb")).unwrap();
    exercise_bodyless_shared_lifecycle(&mut redb);
}

#[test]
fn redb_reopens_bodyless_operation_and_current_generation_without_body_copies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.redb");
    let keys = Keys::generate();
    let coordinate = coordinate(&keys);
    let (intent, receipt) = {
        let mut store = RedbStore::open(&path).unwrap();
        let accepted = accept_operation(
            &mut store,
            &keys,
            10,
            bodyless_accept(coordinate.clone(), None, Vec::new(), 42),
        );
        let current =
            install_shared_materialization(&mut store, &keys, &coordinate, vec![accepted.0]);
        assert_eq!(
            current
                .generation()
                .unwrap()
                .materialization
                .materialization_id,
            MaterializationId(1)
        );
        accepted
    };

    let store = RedbStore::open(&path).unwrap();
    let snapshot = store
        .replaceable_operation_snapshot(&coordinate)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.operations.len(), 1);
    assert_eq!(snapshot.operations[0].intent_id, intent);
    assert_eq!(snapshot.operations[0].plan.bytes(), &[42]);
    assert_eq!(
        snapshot
            .current
            .generation()
            .unwrap()
            .materialization
            .materialization_id,
        MaterializationId(1)
    );
    assert_receipt_current(
        &store,
        receipt,
        MaterializationId(1),
        IntentSigState::Pending,
    );
    assert_eq!(store.recover_publish_queue().unwrap().len(), 1);
}

#[test]
fn content_pending_reconstructs_and_an_old_target_revision_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("content-pending.redb");
    let keys = Keys::generate();
    let coordinate = coordinate(&keys);
    let (first, first_receipt, stale_attempt) = {
        let mut store = RedbStore::open(&path).unwrap();
        let (first, receipt) = accept_operation(
            &mut store,
            &keys,
            10,
            bodyless_accept(coordinate.clone(), None, Vec::new(), 42),
        );
        let snapshot = store
            .replaceable_operation_snapshot(&coordinate)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.current.materialization_revision.0, 1);
        assert_eq!(
            snapshot.current.waiting(),
            Some(MaterializationWait::Content)
        );
        assert_receipt_waiting(&store, receipt, MaterializationWait::Content);
        let stale_attempt = SemanticRematerialize {
            coordinate: coordinate.clone(),
            expected_source_revision: snapshot.current.source_revision.clone(),
            expected_program_digest: snapshot.current.program_digest,
            expected_current_materialization: None,
            expected_materialization_revision: snapshot.current.materialization_revision,
            source: source(),
            evaluated_at: Timestamp::from(10),
            materialization: MaterializationAttempt::Complete(MaterializationCandidate {
                event: UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(10),
                    Kind::ContactList,
                    Vec::new(),
                    "stale plaintext result",
                ),
                routing: "test-route".into(),
                sig_state: PendingMaterializationState::Pending,
            }),
            contributing_operations: vec![first],
            resolved_operations: Vec::new(),
        };
        (first, receipt, stale_attempt)
    };

    let mut store = RedbStore::open(&path).unwrap();
    let reopened = store
        .replaceable_operation_snapshot(&coordinate)
        .unwrap()
        .unwrap();
    assert_eq!(reopened.operations[0].plan.bytes(), &[42]);
    assert_eq!(reopened.current.materialization_revision.0, 1);
    assert_eq!(
        reopened.current.waiting(),
        Some(MaterializationWait::Content)
    );
    assert_receipt_waiting(&store, first_receipt, MaterializationWait::Content);

    let (second, second_receipt) = accept_operation(
        &mut store,
        &keys,
        11,
        bodyless_accept(coordinate.clone(), Some(&reopened.current), vec![first], 43),
    );
    assert!(matches!(
        store
            .install_replaceable_materialization(stale_attempt)
            .unwrap(),
        SemanticInstallOutcome::Stale
    ));
    let current = store
        .replaceable_operation_snapshot(&coordinate)
        .unwrap()
        .unwrap();
    assert_eq!(current.operations.len(), 2);
    assert_eq!(current.operations[1].intent_id, second);
    assert_eq!(current.current.materialization_revision.0, 2);
    assert_eq!(
        current.current.waiting(),
        Some(MaterializationWait::Content)
    );
    assert_receipt_waiting(&store, first_receipt, MaterializationWait::Content);
    assert_receipt_waiting(&store, second_receipt, MaterializationWait::Content);
}

#[test]
fn source_supersession_invalidates_an_inflight_materialization() {
    let keys = Keys::generate();
    let coordinate = coordinate(&keys);
    let mut store = MemoryStore::new();
    let (intent, _) = accept_operation(
        &mut store,
        &keys,
        10,
        bodyless_accept(coordinate.clone(), None, Vec::new(), 42),
    );
    let before = store
        .replaceable_operation_snapshot(&coordinate)
        .unwrap()
        .unwrap();
    let stale = SemanticRematerialize {
        coordinate: coordinate.clone(),
        expected_source_revision: before.current.source_revision.clone(),
        expected_program_digest: before.current.program_digest,
        expected_current_materialization: None,
        expected_materialization_revision: before.current.materialization_revision,
        source: source(),
        evaluated_at: Timestamp::from(10),
        materialization: MaterializationAttempt::Complete(MaterializationCandidate {
            event: UnsignedEvent::new(
                keys.public_key(),
                Timestamp::from(10),
                Kind::ContactList,
                Vec::new(),
                "stale source result",
            ),
            routing: "test-route".into(),
            sig_state: PendingMaterializationState::Pending,
        }),
        contributing_operations: vec![intent],
        resolved_operations: Vec::new(),
    };

    let advanced = store
        .install_replaceable_materialization(SemanticRematerialize {
            coordinate: coordinate.clone(),
            expected_source_revision: before.current.source_revision,
            expected_program_digest: before.current.program_digest,
            expected_current_materialization: None,
            expected_materialization_revision: before.current.materialization_revision,
            source: event_source(8, 11),
            evaluated_at: Timestamp::from(11),
            materialization: MaterializationAttempt::Waiting(MaterializationWait::Content),
            contributing_operations: vec![intent],
            resolved_operations: Vec::new(),
        })
        .unwrap();
    assert!(matches!(advanced, SemanticInstallOutcome::Waiting(_)));
    assert!(matches!(
        store.install_replaceable_materialization(stale).unwrap(),
        SemanticInstallOutcome::Stale
    ));
}

#[test]
fn source_wait_and_content_wait_are_distinct_and_cannot_be_misreported() {
    let keys = Keys::generate();
    let coordinate = coordinate(&keys);
    let mut unresolved = source();
    unresolved.qualified = QualifiedSource::Unresolved;
    let source_pending = SemanticAccept {
        coordinate: coordinate.clone(),
        program: ReplayProgramId([7; 16]),
        format: ReplayFormatId([9; 16]),
        expected_source_revision: None,
        expected_program_digest: None,
        expected_current_materialization: None,
        expected_materialization_revision: None,
        starting_source: starting_source(),
        source: unresolved.clone(),
        plan: SemanticPlan::new(1, vec![1]).unwrap(),
        materialization: MaterializationAttempt::Waiting(MaterializationWait::Source),
        contributing_operations: Vec::new(),
        resolved_operations: Vec::new(),
    };

    let mut store = MemoryStore::new();
    let (_, receipt) = accept_operation(&mut store, &keys, 10, source_pending);
    assert_receipt_waiting(&store, receipt, MaterializationWait::Source);

    let wrongly_content_pending = SemanticAccept {
        coordinate,
        program: ReplayProgramId([7; 16]),
        format: ReplayFormatId([9; 16]),
        expected_source_revision: None,
        expected_program_digest: None,
        expected_current_materialization: None,
        expected_materialization_revision: None,
        starting_source: starting_source(),
        source: unresolved,
        plan: SemanticPlan::new(1, vec![2]).unwrap(),
        materialization: MaterializationAttempt::Waiting(MaterializationWait::Content),
        contributing_operations: Vec::new(),
        resolved_operations: Vec::new(),
    };
    let mut second_store = MemoryStore::new();
    assert!(matches!(
        second_store.accept_write(AcceptWrite {
            payload: AcceptWritePayload::ReplaceableOperation(Box::new(wrongly_content_pending)),
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "test-key".into(),
            accepted_at: Timestamp::from(11),
            correlation: None,
        }),
        Ok(AcceptOutcome::ReplaceableOperationRefused(
            nmp_store::SemanticRefusal::MaterializationWaitMismatch
        ))
    ));
}
