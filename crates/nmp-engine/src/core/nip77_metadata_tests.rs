//! Exact plan-to-NIP-77 child metadata ownership (#1350).

use super::*;

use nmp_grammar::{ConcreteFilter, ContextualAtom};
use nmp_router::{DemandKey, RequestMetadataUpdate, SubId};
use nmp_store::{coverage_key, RedbStore};
use nostr::EventId;

struct Fixture {
    core: EngineCore,
    relay: RelayUrl,
    session: RelaySessionKey,
    plan_sub_id: SubId,
    incumbent: ContextualAtom,
    added: ContextualAtom,
}

impl Fixture {
    fn new() -> Self {
        let relay = RelayUrl::parse("wss://nip77-plan-metadata.example").unwrap();
        let session = RelaySessionKey::public(relay.clone());
        let incumbent = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1, 2])),
                since: Some(100),
                ..ConcreteFilter::default()
            },
            source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let added = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1])),
                since: Some(100),
                ..ConcreteFilter::default()
            },
            source: incumbent.source.clone(),
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let plan_sub_id = SubId::for_wire(
            relay.clone(),
            &incumbent.filter,
            &incumbent.source,
            incumbent.access,
        );
        let incumbent_claims = BTreeSet::from([coverage_key(&incumbent)]);
        let incumbent_demands = BTreeSet::from([DemandKey::for_atom(&incumbent)]);
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
        core.attribution.observe_atom(&incumbent);
        core.attribution.observe_atom(&added);
        core.attribution
            .retain_live_request_claims(&plan_sub_id, incumbent_claims.clone());
        core.install_plan_execution_metadata(
            plan_sub_id.clone(),
            incumbent.filter.clone(),
            incumbent_claims,
            incumbent_demands,
        );
        core.prober
            .states
            .insert(relay.clone(), crate::negentropy::ProbeState::Supported);
        Self {
            core,
            relay,
            session,
            plan_sub_id,
            incumbent,
            added,
        }
    }

    fn begin_candidate(&mut self) -> SubId {
        let probed = self
            .core
            .prober
            .probed(&self.relay)
            .expect("fixture relay is behaviorally proven");
        self.core.begin_neg_handoff(
            probed,
            self.plan_sub_id.clone(),
            None,
            self.incumbent.filter.clone(),
            &mut Vec::new(),
        );
        self.core.pending_neg_handoffs_by_plan[&self.plan_sub_id]
            .iter()
            .next()
            .cloned()
            .unwrap()
    }

    fn update(&mut self) {
        self.core.apply_request_metadata_updates(
            &[RequestMetadataUpdate {
                session: self.session.clone(),
                sub_id: self.plan_sub_id.clone(),
                filter_hash: self.incumbent.filter.hash(),
                added_coverage_claims: BTreeSet::from([coverage_key(&self.added)]),
                added_owner_demands: BTreeSet::from([DemandKey::for_atom(&self.added)]),
            }],
            &mut Vec::new(),
        );
    }

    fn assert_role_updated(&self, role_sub_id: &SubId) {
        assert_eq!(
            self.core.attribution.current_claims(role_sub_id),
            BTreeSet::from([coverage_key(&self.incumbent), coverage_key(&self.added)])
        );
        let owner_demands = &self.core.pending_request_evidence
            [&(self.session.clone(), role_sub_id.clone())]
            .back()
            .unwrap()
            .owner_demands;
        assert_eq!(
            owner_demands,
            &BTreeSet::from([
                DemandKey::for_atom(&self.incumbent),
                DemandKey::for_atom(&self.added),
            ])
        );
    }

    fn refuse(&mut self, role_sub_id: &SubId) -> Vec<Effect> {
        let attempt_id = self.core.pending_request_evidence
            [&(self.session.clone(), role_sub_id.clone())]
            .back()
            .expect("role owns an exact pending attempt")
            .attempt_id;
        self.core
            .on_wire_request_handoff(RequestHandoffOutcome::Refused {
                attempt_id,
                cause: LocalSendRefusal::SessionUnavailable,
            })
    }

    fn stray_eose(&mut self, role_sub_id: &SubId) -> Vec<Effect> {
        let handle = TransportRelayHandle {
            slot: 94,
            generation: 1,
        };
        self.core
            .slot_to_relay
            .insert(handle.slot, (handle, self.session.clone()));
        self.core.connected_relays.insert(self.session.clone());
        self.core.on_relay_frame(
            handle,
            self.session.clone(),
            RelayFrame::from_message(RelayMessage::EndOfStoredEvents(std::borrow::Cow::Owned(
                nostr::SubscriptionId::new(wire_sub_id_string(role_sub_id)),
            ))),
        )
    }

    fn finish(mut self) {
        self.core
            .cancel_nip77_repair_for_plan(&self.plan_sub_id, &mut Vec::new());
        self.core
            .attribution
            .release_live_request_claims(&self.plan_sub_id);
        self.core.plan_execution_metadata.remove(&self.plan_sub_id);
        self.core.attribution.release_atom(&self.incumbent);
        self.core.attribution.release_atom(&self.added);
        assert_eq!(
            self.core.bench_ownership_census(),
            CoreOwnershipCensus::default()
        );
    }
}

#[test]
fn zero_wire_metadata_attach_extends_the_live_candidate_generation() {
    let mut fixture = Fixture::new();
    let candidate = fixture.begin_candidate();
    fixture.update();
    fixture.assert_role_updated(&candidate);
    assert_eq!(fixture.core.plan_execution_metadata.len(), 1);
    fixture.finish();
}

#[test]
fn zero_wire_metadata_attach_extends_the_open_neg_generation() {
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
    fixture.update();
    fixture.assert_role_updated(&neg);
    fixture.finish();
}

#[test]
fn zero_wire_metadata_attach_extends_the_retained_neg_owner_during_missing_id_backfill() {
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
        neg.clone(),
        fixture.relay.clone(),
        session,
        BTreeSet::from([EventId::from_byte_array([7; 32])]),
        &mut Vec::new(),
    );
    fixture.update();
    fixture.assert_role_updated(&neg);
    fixture.finish();
}

#[test]
fn zero_wire_metadata_attach_extends_candidate_and_backlog_generations() {
    let mut fixture = Fixture::new();
    let candidate = fixture.begin_candidate();
    let handoff = fixture.core.take_pending_neg_handoff(&candidate).unwrap();
    fixture
        .core
        .handoff_fallback_to_req(handoff, &mut Vec::new());
    let backlog = fixture.core.pending_backfills_by_plan[&fixture.plan_sub_id]
        .iter()
        .next()
        .cloned()
        .unwrap();
    fixture.update();
    fixture.assert_role_updated(&candidate);
    fixture.assert_role_updated(&backlog);
    fixture.finish();
}

#[test]
fn exact_public_disconnect_retires_the_active_nip77_child_and_every_reverse_owner() {
    let mut fixture = Fixture::new();
    let handle = TransportRelayHandle {
        slot: 91,
        generation: 1,
    };
    fixture
        .core
        .slot_to_relay
        .insert(handle.slot, (handle, fixture.session.clone()));
    fixture
        .core
        .connected_relays
        .insert(fixture.session.clone());

    let candidate = fixture.begin_candidate();
    let attempt_id = fixture.core.pending_request_evidence
        [&(fixture.session.clone(), candidate.clone())]
        .back()
        .unwrap()
        .attempt_id;
    fixture
        .core
        .on_wire_request_handoff(RequestHandoffOutcome::Accepted { attempt_id, handle });
    let handoff = fixture.core.take_pending_neg_handoff(&candidate).unwrap();
    fixture
        .core
        .activate_live_and_open_neg(handoff, &mut Vec::new());
    assert_eq!(
        fixture.core.active_nip77_live.get(&fixture.plan_sub_id),
        Some(&candidate)
    );
    assert!(!fixture.core.neg_sessions_by_plan.is_empty());

    fixture
        .core
        .on_relay_disconnected(handle, fixture.session.clone(), DisconnectReason::Error);
    assert!(fixture.core.active_nip77_live.is_empty());
    assert!(fixture.core.pending_neg_handoffs.is_empty());
    assert!(fixture.core.pending_neg_handoffs_by_plan.is_empty());
    assert!(fixture.core.neg_sessions.is_empty());
    assert!(fixture.core.neg_sessions_by_plan.is_empty());
    assert!(fixture.core.pending_backfills.is_empty());
    assert!(fixture.core.pending_backfills_by_plan.is_empty());
    assert_eq!(fixture.core.attempts.counts().attempts, 0);
    assert_eq!(fixture.core.attempts.counts().session_keys, 0);
    assert_eq!(fixture.core.attempts.counts().retry_jobs, 0);
    assert_eq!(fixture.core.attempts.counts().retry_session_keys, 0);
    fixture.finish();
}

#[path = "nip77_metadata_tests/refusal.rs"]
mod refusal;
