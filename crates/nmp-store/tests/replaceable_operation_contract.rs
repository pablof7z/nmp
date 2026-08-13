use nmp_store::{
    AcceptOutcome, AcceptWrite, AcceptWritePayload, AccessContextId, EventStore, IntentSigState,
    MaterializationCandidate, MaterializationId, MemoryStore, PendingMaterializationState,
    PromoteOutcome, PromotionTarget, PublishQueueReceiptPayload, QualifiedSource, RedbStore,
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
            .and_then(|state| state.generation.as_ref())
            .map(|generation| generation.materialization.materialization_id),
        starting_source: starting_source(),
        source: source(),
        plan: SemanticPlan::new(1, vec![byte]).unwrap(),
        materialized: None,
        contributing_operations: existing,
        resolved_operations: Vec::new(),
    }
}

fn body_complete_accept(
    coordinate: Coordinate,
    snapshot: Option<&nmp_store::SemanticCurrentState>,
    existing: Vec<nmp_store::IntentId>,
    byte: u8,
    created_at: u64,
) -> SemanticAccept {
    let mut accept = bodyless_accept(coordinate.clone(), snapshot, existing, byte);
    accept.materialized = Some(MaterializationCandidate {
        event: UnsignedEvent::new(
            coordinate.public_key,
            Timestamp::from(created_at),
            coordinate.kind,
            Vec::new(),
            format!("materialized-{byte}"),
        ),
        routing: "test-route".into(),
        sig_state: PendingMaterializationState::AwaitingSigner,
    });
    accept
}

fn accept_body_complete(
    store: &mut dyn EventStore,
    keys: &Keys,
    accepted_at: u64,
    accept: SemanticAccept,
) -> (nmp_store::IntentId, u64, nostr::EventId) {
    match store
        .accept_write(AcceptWrite {
            payload: AcceptWritePayload::ReplaceableOperation(Box::new(accept)),
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "test-key".into(),
            accepted_at: Timestamp::from(accepted_at),
            correlation: None,
        })
        .unwrap()
    {
        AcceptOutcome::ReplaceableOperation {
            intent_id,
            receipt_id,
            installed: Some(installed),
            ..
        } => (intent_id, receipt_id, installed.event.id),
        other => panic!("expected body-complete operation, got {other:?}"),
    }
}

fn receipt_ids(store: &dyn EventStore, receipt_id: u64) -> (nostr::EventId, nostr::EventId) {
    let receipt = store.reattach_receipt(receipt_id).unwrap().unwrap();
    match receipt.payload {
        PublishQueueReceiptPayload::ReplaceableOperation {
            acceptance: nmp_store::ReplaceableOperationAcceptance::BodyComplete(accepted),
            state:
                ReplaceableOperationReceiptState::Contributing {
                    current: Some(current),
                },
            ..
        } => (accepted, current.materialization.event_id),
        other => panic!("expected body-complete receipt, got {other:?}"),
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
            ..
        } => {
            assert!(current.generation.is_none());
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
                    current: Some(current),
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
            source: source(),
            evaluated_at: Timestamp::from(created_at),
            materialized: Some(MaterializationCandidate {
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
    let generation = current.generation.clone().unwrap();
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
                .generation
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
            .generation
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
fn body_complete_receipt_keeps_accepted_id_while_current_advances_across_reopen() {
    fn exercise(store: &mut dyn EventStore) -> (u64, u64, nostr::EventId, nostr::EventId) {
        let keys = Keys::generate();
        let coordinate = coordinate(&keys);
        let (first, first_receipt, first_event) = accept_body_complete(
            store,
            &keys,
            10,
            body_complete_accept(coordinate.clone(), None, Vec::new(), 1, 10),
        );
        let snapshot = store
            .replaceable_operation_snapshot(&coordinate)
            .unwrap()
            .unwrap();
        let (_, second_receipt, second_event) = accept_body_complete(
            store,
            &keys,
            11,
            body_complete_accept(coordinate, Some(&snapshot.current), vec![first], 2, 11),
        );
        assert_eq!(
            receipt_ids(store, first_receipt),
            (first_event, second_event)
        );
        assert_eq!(
            receipt_ids(store, second_receipt),
            (second_event, second_event)
        );
        (first_receipt, second_receipt, first_event, second_event)
    }

    let mut memory = MemoryStore::new();
    exercise(&mut memory);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("accepted-id.redb");
    let evidence = {
        let mut redb = RedbStore::open(&path).unwrap();
        exercise(&mut redb)
    };
    let redb = RedbStore::open(&path).unwrap();
    assert_eq!(receipt_ids(&redb, evidence.0), (evidence.2, evidence.3));
    assert_eq!(receipt_ids(&redb, evidence.1), (evidence.3, evidence.3));
}
