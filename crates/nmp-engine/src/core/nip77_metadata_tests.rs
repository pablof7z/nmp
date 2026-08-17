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
        let session = RelaySessionKey::unauthenticated(relay.clone());
        let incumbent = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1, 2])),
                since: Some(100),
                ..ConcreteFilter::default()
            },
            routing: ReadRouting::Explicit(vec![relay.clone()]),
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        let added = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1])),
                since: Some(100),
                ..ConcreteFilter::default()
            },
            routing: incumbent.routing.clone(),
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        let plan_sub_id = SubId::for_wire(
            relay.clone(),
            &incumbent.filter,
            &incumbent.routing,
            incumbent.authenticated_as,
        );
        let incumbent_claims = BTreeSet::from([coverage_key(&incumbent)]);
        let incumbent_demands = BTreeSet::from([DemandKey::for_atom(&incumbent)]);
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
        core.set_active_demand(&BTreeSet::from([incumbent.clone(), added.clone()]));
        core.white_box("attribution.retain_live_request_claims", |s| {
            s.attribution
                .retain_live_request_claims(&plan_sub_id, incumbent_claims.clone())
        });
        core.white_box("install_plan_execution_metadata", |s| {
            s.install_plan_execution_metadata(
                plan_sub_id.clone(),
                incumbent.filter.clone(),
                incumbent_claims,
                incumbent_demands,
                BTreeSet::new(),
            )
        });
        core.white_box("prober.force_supported_for_test", |s| {
            s.prober.force_supported_for_test(relay.clone())
        });
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
        self.core.white_box("begin_neg_handoff", |s| {
            s.begin_neg_handoff(
                probed,
                self.plan_sub_id.clone(),
                None,
                self.incumbent.filter.clone(),
                &mut Vec::new(),
            )
        });
        self.core
            .nip77
            .sole_child_in_phase(&self.plan_sub_id, RepairPhase::Handoff)
    }

    /// Advance the candidate out of its handoff and into an open
    /// reconciliation session, returning the NEG wire id.
    ///
    /// The three steps are the reducer's own -- `take_handoff` hands the
    /// typed value to `open_neg_session` exactly as `on_relay_frame`'s EOSE
    /// arm does -- and they are written once here rather than five times
    /// across the proofs below. A proof that repeats a transition inline is
    /// a proof that can get the transition subtly wrong in one copy.
    fn open_neg(&mut self, candidate: &SubId) -> SubId {
        let handoff = self.core.white_box("nip77.take_handoff", |s| {
            s.nip77
                .take_handoff(candidate)
                .expect("the candidate is a pending handoff")
        });
        self.core
            .white_box("abandon_sub", |s| s.abandon_sub(candidate));
        self.core.white_box("open_neg_session", |s| {
            s.open_neg_session(handoff, &mut Vec::new())
        });
        self.core
            .nip77
            .sole_child_in_phase(&self.plan_sub_id, RepairPhase::Reconciling)
    }

    /// Promote the candidate to this plan's live tail and open its
    /// reconciliation session in one step, as an accepted candidate's EOSE
    /// does.
    fn activate_live(&mut self, candidate: &SubId) {
        let handoff = self.core.white_box("nip77.take_handoff", |s| {
            s.nip77
                .take_handoff(candidate)
                .expect("the candidate is a pending handoff")
        });
        self.core.white_box("activate_live_and_open_neg", |s| {
            s.activate_live_and_open_neg(handoff, &mut Vec::new())
        });
    }

    /// Abandon the NIP-77 route for the candidate and fall back to an
    /// ordinary backlog REQ, returning the backlog wire id.
    fn fallback_to_backlog(&mut self, candidate: &SubId) -> SubId {
        let handoff = self.core.white_box("nip77.take_handoff", |s| {
            s.nip77
                .take_handoff(candidate)
                .expect("the candidate is a pending handoff")
        });
        self.core.white_box("handoff_fallback_to_req", |s| {
            s.handoff_fallback_to_req(handoff, &mut Vec::new())
        });
        self.core
            .nip77
            .sole_child_in_phase(&self.plan_sub_id, RepairPhase::Backfill)
    }

    /// Complete the reconciliation session with one missing id outstanding,
    /// returning the temporary missing-ids fetch's wire id.
    fn finish_neg_with_missing_id(&mut self, neg: &SubId, missing: EventId) -> SubId {
        let session = self.core.white_box("nip77.take_session", |s| {
            s.nip77
                .take_session(neg)
                .expect("the reconciliation session is open")
        });
        let relay = self.relay.clone();
        let neg = neg.clone();
        self.core.white_box("finish_neg_session", |s| {
            s.finish_neg_session(
                neg,
                relay,
                session,
                BTreeSet::from([missing]),
                &mut Vec::new(),
            )
        });
        self.core
            .nip77
            .sole_child_in_phase(&self.plan_sub_id, RepairPhase::Backfill)
    }

    fn update(&mut self) {
        self.core.white_box("apply_request_metadata_updates", |s| {
            s.apply_request_metadata_updates(
                &[RequestMetadataUpdate {
                    session: self.session.clone(),
                    sub_id: self.plan_sub_id.clone(),
                    filter_hash: self.incumbent.filter.hash(),
                    added_coverage_claims: BTreeSet::from([coverage_key(&self.added)]),
                    added_owner_demands: BTreeSet::from([DemandKey::for_atom(&self.added)]),
                }],
                &mut Vec::new(),
            )
        });
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
        self.core.white_box("slot_to_relay.insert", |s| {
            s.slot_to_relay
                .insert(handle.slot, (handle, self.session.clone()))
        });
        self.core.white_box("connected_relays.insert", |s| {
            s.connected_relays.insert(self.session.clone())
        });
        self.core.white_box("on_relay_frame", |s| {
            s.on_relay_frame(
                handle,
                self.session.clone(),
                RelayFrame::from_message(RelayMessage::EndOfStoredEvents(std::borrow::Cow::Owned(
                    nostr::SubscriptionId::new(wire_sub_id_string(role_sub_id)),
                ))),
            )
        })
    }

    fn finish(mut self) {
        self.core.white_box("cancel_nip77_repair_for_plan", |s| {
            s.cancel_nip77_repair_for_plan(&self.plan_sub_id, &mut Vec::new())
        });
        self.core.white_box("retire_plan_execution_metadata", |s| {
            s.retire_plan_execution_metadata(&self.plan_sub_id)
        });
        self.core.set_active_demand(&BTreeSet::new());
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
    let neg = fixture.open_neg(&candidate);
    fixture.update();
    fixture.assert_role_updated(&neg);
    fixture.finish();
}

#[test]
fn zero_wire_metadata_attach_extends_the_retained_neg_owner_during_missing_id_backfill() {
    let mut fixture = Fixture::new();
    let candidate = fixture.begin_candidate();
    let neg = fixture.open_neg(&candidate);
    fixture.finish_neg_with_missing_id(&neg, EventId::from_byte_array([7; 32]));
    fixture.update();
    fixture.assert_role_updated(&neg);
    fixture.finish();
}

#[test]
fn zero_wire_metadata_attach_extends_candidate_and_backlog_generations() {
    let mut fixture = Fixture::new();
    let candidate = fixture.begin_candidate();
    let backlog = fixture.fallback_to_backlog(&candidate);
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
    fixture.core.white_box("slot_to_relay.insert", |s| {
        s.slot_to_relay
            .insert(handle.slot, (handle, fixture.session.clone()))
    });
    fixture.core.white_box("connected_relays.insert", |s| {
        s.connected_relays.insert(fixture.session.clone())
    });

    let candidate = fixture.begin_candidate();
    let attempt_id = fixture.core.pending_request_evidence
        [&(fixture.session.clone(), candidate.clone())]
        .back()
        .unwrap()
        .attempt_id;
    fixture
        .core
        .on_wire_request_handoff(RequestHandoffOutcome::Accepted { attempt_id, handle });
    fixture.activate_live(&candidate);
    assert_eq!(
        fixture.core.nip77.live_for_plan(&fixture.plan_sub_id),
        Some(&candidate)
    );
    let neg = fixture
        .core
        .nip77
        .sole_child_in_phase(&fixture.plan_sub_id, RepairPhase::Reconciling);

    fixture.core.white_box("on_relay_disconnected", |s| {
        s.on_relay_disconnected(handle, fixture.session.clone(), DisconnectReason::Error)
    });
    // The disconnect alone -- not `finish`'s later plan cancellation -- must
    // leave this plan owning no repair state of any kind: no live tail, no
    // pending handoff, no reconciliation, no backlog fallback. One question
    // over all four, so a fifth cluster cannot be added and quietly escape it.
    assert!(!fixture.core.nip77.has_repair_state(&fixture.plan_sub_id));
    assert_eq!(fixture.core.nip77.phase_of(&neg), None);
    assert_eq!(fixture.core.nip77.phase_of(&candidate), None);
    assert_eq!(fixture.core.attempts.counts().attempts, 0);
    assert_eq!(fixture.core.attempts.counts().session_keys, 0);
    assert_eq!(fixture.core.attempts.counts().retry_jobs, 0);
    assert_eq!(fixture.core.attempts.counts().retry_session_keys, 0);
    fixture.finish();
}

/// `Nip77Sessions::assert_consistent`'s falsifier for the corruption a
/// count can never see. Two real plans on one relay, each with its own
/// pending handoff, then only the reverse index's two entries swapped
/// between them -- the forward map (which handoff belongs to which plan)
/// and every count `bench_ownership_census` reports are untouched. A
/// consistency check built from counts alone would read this as healthy;
/// `assert_owner_consistency` compares by identity and must not.
#[test]
#[should_panic(expected = "is not named by the owner it reports")]
fn assert_consistent_catches_a_cardinality_preserving_swap_between_plans() {
    let relay = RelayUrl::parse("wss://nip77-plan-swap.example").unwrap();
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    core.white_box("prober.force_supported_for_test", |s| {
        s.prober.force_supported_for_test(relay.clone())
    });

    let install_plan = |core: &mut EngineCore, kind: u16| -> SubId {
        let atom = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([kind])),
                since: Some(100),
                ..ConcreteFilter::default()
            },
            routing: ReadRouting::Explicit(vec![relay.clone()]),
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        };
        let plan_sub_id = SubId::for_wire(relay.clone(), &atom.filter, &atom.routing, atom.authenticate_as);
        core.set_active_demand(&BTreeSet::from([atom.clone()]));
        core.white_box("attribution.retain_live_request_claims", |s| {
            s.attribution
                .retain_live_request_claims(&plan_sub_id, BTreeSet::from([coverage_key(&atom)]))
        });
        core.white_box("install_plan_execution_metadata", |s| {
            s.install_plan_execution_metadata(
                plan_sub_id.clone(),
                atom.filter.clone(),
                BTreeSet::from([coverage_key(&atom)]),
                BTreeSet::from([DemandKey::for_atom(&atom)]),
                BTreeSet::new(),
            )
        });
        plan_sub_id
    };

    let plan_a = install_plan(&mut core, 1);
    let plan_b = install_plan(&mut core, 2);

    let probed_a = core
        .prober
        .probed(&relay)
        .expect("fixture relay is behaviorally proven");
    core.white_box("begin_neg_handoff", |s| {
        s.begin_neg_handoff(
            probed_a,
            plan_a.clone(),
            None,
            ConcreteFilter {
                kinds: Some(BTreeSet::from([1])),
                since: Some(100),
                ..ConcreteFilter::default()
            },
            &mut Vec::new(),
        )
    });
    let probed_b = core
        .prober
        .probed(&relay)
        .expect("fixture relay is behaviorally proven");
    core.white_box("begin_neg_handoff", |s| {
        s.begin_neg_handoff(
            probed_b,
            plan_b.clone(),
            None,
            ConcreteFilter {
                kinds: Some(BTreeSet::from([2])),
                since: Some(100),
                ..ConcreteFilter::default()
            },
            &mut Vec::new(),
        )
    });

    // Precondition: two plans, one live handoff each, mirror intact.
    let counts = core.nip77.counts();
    assert_eq!(counts.handoffs, 2);
    assert_eq!(counts.handoff_plan_keys, 2);
    assert_eq!(counts.handoff_plan_edges, 2);
    core.assert_owner_consistency("before swap");

    core.white_box("nip77.swap_handoff_owners_for_test", |s| {
        s.nip77.swap_handoff_owners_for_test(&plan_a, &plan_b)
    });

    // Every count above is identical after the swap -- one handoff moved
    // reverse-index owners, zero were created or destroyed.
    let counts = core.nip77.counts();
    assert_eq!(counts.handoffs, 2);
    assert_eq!(counts.handoff_plan_keys, 2);
    assert_eq!(counts.handoff_plan_edges, 2);

    core.assert_owner_consistency("after swap");
}

#[path = "nip77_metadata_tests/refusal.rs"]
mod refusal;
