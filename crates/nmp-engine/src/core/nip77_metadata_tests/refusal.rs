//! Local-refusal, retry, and stale-terminal NIP-77 metadata proofs.

use super::*;

#[test]
fn refused_live_candidate_never_becomes_active_and_keeps_one_owned_retry_deadline() {
    let mut fixture = Fixture::new();
    let candidate = fixture.begin_candidate();
    fixture.refuse(&candidate);
    assert_eq!(fixture.core.nip77.live_for_plan(&fixture.plan_sub_id), None);
    assert_eq!(
        fixture
            .core
            .nip77
            .sole_child_in_phase(&fixture.plan_sub_id, RepairPhase::Handoff),
        candidate
    );
    assert_eq!(fixture.core.attempts.counts().retry_jobs, 1);
    assert!(fixture.core.next_deadline().is_some());

    let due = fixture.core.next_deadline().unwrap();
    let retried = fixture.core.handle(EngineMsg::Tick(due));
    assert!(retried
        .iter()
        .any(|effect| matches!(effect, Effect::Wire(_))));
    assert_eq!(fixture.core.nip77.live_for_plan(&fixture.plan_sub_id), None);
    assert_eq!(fixture.core.attempts.counts().attempts, 1);
    fixture.finish();
}

#[test]
fn refused_live_candidate_retry_receives_later_zero_wire_plan_metadata() {
    let mut fixture = Fixture::new();
    let candidate = fixture.begin_candidate();
    fixture.refuse(&candidate);
    fixture.update();

    let due = fixture.core.next_deadline().unwrap();
    fixture.core.handle(EngineMsg::Tick(due));
    fixture.assert_role_updated(&candidate);
    let attempt_id = fixture.core.pending_request_evidence
        [&(fixture.session.clone(), candidate.clone())]
        .back()
        .unwrap()
        .attempt_id;
    fixture
        .core
        .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id,
            handle: TransportRelayHandle {
                slot: 92,
                generation: 1,
            },
        });
    assert_eq!(
        fixture
            .core
            .active_request_evidence
            .values()
            .next()
            .unwrap()
            .owner_demands,
        BTreeSet::from([
            DemandKey::for_atom(&fixture.incumbent),
            DemandKey::for_atom(&fixture.added),
        ])
    );
    fixture.finish();
}

#[test]
fn stray_eose_cannot_advance_refused_candidate_missing_id_or_backlog_roles() {
    let mut candidate = Fixture::new();
    let candidate_id = candidate.begin_candidate();
    candidate.refuse(&candidate_id);
    assert!(candidate.stray_eose(&candidate_id).is_empty());
    assert_eq!(
        candidate.core.nip77.phase_of(&candidate_id),
        Some(RepairPhase::Handoff),
        "a stray EOSE must not advance the refused candidate out of its handoff"
    );
    assert_eq!(
        candidate.core.nip77.live_for_plan(&candidate.plan_sub_id),
        None
    );
    assert_eq!(candidate.core.attempts.counts().retry_jobs, 1);
    candidate.finish();

    let mut missing = Fixture::new();
    let live = missing.begin_candidate();
    let neg = missing.open_neg(&live);
    let missing_id = missing.finish_neg_with_missing_id(&neg, EventId::from_byte_array([11; 32]));
    missing.refuse(&missing_id);
    assert!(missing.stray_eose(&missing_id).is_empty());
    assert_eq!(
        missing.core.nip77.phase_of(&missing_id),
        Some(RepairPhase::Backfill),
        "a stray EOSE must not advance the refused missing-ids fetch out of its backfill"
    );
    assert_eq!(missing.core.attempts.counts().retry_jobs, 1);
    missing.finish();

    let mut backlog = Fixture::new();
    let live = backlog.begin_candidate();
    let backlog_id = backlog.fallback_to_backlog(&live);
    backlog.refuse(&backlog_id);
    assert!(backlog.stray_eose(&backlog_id).is_empty());
    assert_eq!(
        backlog.core.nip77.phase_of(&backlog_id),
        Some(RepairPhase::Backfill),
        "a stray EOSE must not advance the refused backlog fallback out of its backfill"
    );
    assert_eq!(backlog.core.attempts.counts().retry_jobs, 1);
    backlog.finish();
}

#[test]
fn refused_missing_id_and_backlog_roles_each_keep_one_retry_and_teardown_exactly() {
    let mut missing = Fixture::new();
    let candidate = missing.begin_candidate();
    let neg = missing.open_neg(&candidate);
    let missing_id = missing.finish_neg_with_missing_id(&neg, EventId::from_byte_array([9; 32]));
    missing.refuse(&missing_id);
    assert_eq!(missing.core.attempts.counts().retry_jobs, 1);
    assert_eq!(missing.core.nip77.live_for_plan(&missing.plan_sub_id), None);
    missing.finish();

    let mut backlog = Fixture::new();
    let candidate = backlog.begin_candidate();
    let backlog_id = backlog.fallback_to_backlog(&candidate);
    backlog.refuse(&backlog_id);
    assert_eq!(backlog.core.attempts.counts().retry_jobs, 1);
    assert_eq!(backlog.core.nip77.live_for_plan(&backlog.plan_sub_id), None);
    backlog.finish();
}

#[test]
fn missing_id_retry_stays_claimless_when_plan_metadata_grows() {
    let mut fixture = Fixture::new();
    let candidate = fixture.begin_candidate();
    let neg = fixture.open_neg(&candidate);
    let missing_id = fixture.finish_neg_with_missing_id(&neg, EventId::from_byte_array([13; 32]));
    fixture.refuse(&missing_id);
    fixture.update();
    let due = fixture.core.next_deadline().unwrap();
    fixture.core.handle(EngineMsg::Tick(due));

    let pending = fixture.core.pending_request_evidence
        [&(fixture.session.clone(), missing_id.clone())]
        .back()
        .unwrap();
    assert!(pending.owner_demands.is_empty());
    assert!(fixture
        .core
        .attribution
        .current_claims(&missing_id)
        .is_empty());
    let attempt = fixture.core.attempts.get(pending.attempt_id).unwrap();
    assert!(attempt.owner_demands.is_empty());
    assert!(attempt.coverage_claims.is_empty());
    fixture.finish();
}
