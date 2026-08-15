//! Local-refusal, retry, and stale-terminal NIP-77 metadata proofs.

use super::*;

#[test]
fn refused_live_candidate_never_becomes_active_and_keeps_one_owned_retry_deadline() {
    let mut fixture = Fixture::new();
    let candidate = fixture.begin_candidate();
    fixture.refuse(&candidate);
    assert!(fixture.core.active_nip77_live.is_empty());
    assert_eq!(
        fixture.core.pending_neg_handoffs_by_plan[&fixture.plan_sub_id],
        BTreeSet::from([candidate.clone()])
    );
    assert_eq!(fixture.core.attempts.counts().retry_jobs, 1);
    assert!(fixture.core.next_deadline().unwrap().is_some());

    let due = fixture.core.next_deadline().unwrap().unwrap();
    let retried = fixture.core.handle(EngineMsg::Tick(due));
    assert!(retried
        .iter()
        .any(|effect| matches!(effect, Effect::Wire(_))));
    assert!(fixture.core.active_nip77_live.is_empty());
    assert_eq!(fixture.core.attempts.counts().attempts, 1);
    fixture.finish();
}

#[test]
fn refused_live_candidate_retry_receives_later_zero_wire_plan_metadata() {
    let mut fixture = Fixture::new();
    let candidate = fixture.begin_candidate();
    fixture.refuse(&candidate);
    fixture.update();

    let due = fixture.core.next_deadline().unwrap().unwrap();
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
    assert!(candidate
        .core
        .pending_neg_handoffs
        .contains_key(&candidate_id));
    assert!(candidate.core.active_nip77_live.is_empty());
    assert_eq!(candidate.core.attempts.counts().retry_jobs, 1);
    candidate.finish();

    let mut missing = Fixture::new();
    let live = missing.begin_candidate();
    let handoff = missing.core.take_pending_neg_handoff(&live).unwrap();
    missing.core.abandon_sub(&live);
    missing.core.open_neg_session(handoff, &mut Vec::new());
    let neg = missing.core.neg_sessions_by_plan[&missing.plan_sub_id]
        .iter()
        .next()
        .cloned()
        .unwrap();
    let session = missing.core.take_neg_session(&neg).unwrap();
    missing.core.finish_neg_session(
        neg,
        missing.relay.clone(),
        session,
        BTreeSet::from([EventId::from_byte_array([11; 32])]),
        &mut Vec::new(),
    );
    let missing_id = missing.core.pending_backfills_by_plan[&missing.plan_sub_id]
        .iter()
        .next()
        .cloned()
        .unwrap();
    missing.refuse(&missing_id);
    assert!(missing.stray_eose(&missing_id).is_empty());
    assert!(missing.core.pending_backfills.contains_key(&missing_id));
    assert_eq!(missing.core.attempts.counts().retry_jobs, 1);
    missing.finish();

    let mut backlog = Fixture::new();
    let live = backlog.begin_candidate();
    let handoff = backlog.core.take_pending_neg_handoff(&live).unwrap();
    backlog
        .core
        .handoff_fallback_to_req(handoff, &mut Vec::new());
    let backlog_id = backlog.core.pending_backfills_by_plan[&backlog.plan_sub_id]
        .iter()
        .next()
        .cloned()
        .unwrap();
    backlog.refuse(&backlog_id);
    assert!(backlog.stray_eose(&backlog_id).is_empty());
    assert!(backlog.core.pending_backfills.contains_key(&backlog_id));
    assert_eq!(backlog.core.attempts.counts().retry_jobs, 1);
    backlog.finish();
}

#[test]
fn refused_missing_id_and_backlog_roles_each_keep_one_retry_and_teardown_exactly() {
    let mut missing = Fixture::new();
    let candidate = missing.begin_candidate();
    let handoff = missing.core.take_pending_neg_handoff(&candidate).unwrap();
    missing.core.abandon_sub(&candidate);
    missing.core.open_neg_session(handoff, &mut Vec::new());
    let neg = missing.core.neg_sessions_by_plan[&missing.plan_sub_id]
        .iter()
        .next()
        .cloned()
        .unwrap();
    let session = missing.core.take_neg_session(&neg).unwrap();
    missing.core.finish_neg_session(
        neg,
        missing.relay.clone(),
        session,
        BTreeSet::from([EventId::from_byte_array([9; 32])]),
        &mut Vec::new(),
    );
    let missing_id = missing.core.pending_backfills_by_plan[&missing.plan_sub_id]
        .iter()
        .next()
        .cloned()
        .unwrap();
    missing.refuse(&missing_id);
    assert_eq!(missing.core.attempts.counts().retry_jobs, 1);
    assert!(missing.core.active_nip77_live.is_empty());
    missing.finish();

    let mut backlog = Fixture::new();
    let candidate = backlog.begin_candidate();
    let handoff = backlog.core.take_pending_neg_handoff(&candidate).unwrap();
    backlog
        .core
        .handoff_fallback_to_req(handoff, &mut Vec::new());
    let backlog_id = backlog.core.pending_backfills_by_plan[&backlog.plan_sub_id]
        .iter()
        .next()
        .cloned()
        .unwrap();
    backlog.refuse(&backlog_id);
    assert_eq!(backlog.core.attempts.counts().retry_jobs, 1);
    assert!(backlog.core.active_nip77_live.is_empty());
    backlog.finish();
}

#[test]
fn missing_id_retry_stays_claimless_when_plan_metadata_grows() {
    let mut fixture = Fixture::new();
    let candidate = fixture.begin_candidate();
    let handoff = fixture.core.take_pending_neg_handoff(&candidate).unwrap();
    fixture.core.abandon_sub(&candidate);
    fixture.core.open_neg_session(handoff, &mut Vec::new());
    let neg = fixture.core.neg_sessions_by_plan[&fixture.plan_sub_id]
        .iter()
        .next()
        .cloned()
        .unwrap();
    let session = fixture.core.take_neg_session(&neg).unwrap();
    fixture.core.finish_neg_session(
        neg,
        fixture.relay.clone(),
        session,
        BTreeSet::from([EventId::from_byte_array([13; 32])]),
        &mut Vec::new(),
    );
    let missing_id = fixture.core.pending_backfills_by_plan[&fixture.plan_sub_id]
        .iter()
        .next()
        .cloned()
        .unwrap();
    fixture.refuse(&missing_id);
    fixture.update();
    let due = fixture.core.next_deadline().unwrap().unwrap();
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
