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
        core.white_box("attribution.observe_atom", |s| {
            s.attribution.observe_atom(&incumbent)
        });
        core.white_box("attribution.observe_atom", |s| {
            s.attribution.observe_atom(&added)
        });
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
            .handoff_children_of(&self.plan_sub_id)
            .iter()
            .next()
            .cloned()
            .unwrap()
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
        self.core
            .white_box("attribution.release_live_request_claims", |s| {
                s.attribution.release_live_request_claims(&self.plan_sub_id)
            });
        self.core.white_box("plan_execution_metadata.remove", |s| {
            s.plan_execution_metadata.remove(&self.plan_sub_id)
        });
        self.core.white_box("attribution.release_atom", |s| {
            s.attribution.release_atom(&self.incumbent)
        });
        self.core.white_box("attribution.release_atom", |s| {
            s.attribution.release_atom(&self.added)
        });
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
    let handoff = fixture.core.white_box("nip77.take_handoff", |s| {
        s.nip77.take_handoff(&candidate).unwrap()
    });
    fixture
        .core
        .white_box("abandon_sub", |s| s.abandon_sub(&candidate));
    fixture.core.white_box("open_neg_session", |s| {
        s.open_neg_session(handoff, &mut Vec::new())
    });
    let neg = fixture
        .core
        .nip77
        .session_children_of(&fixture.plan_sub_id)
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
    let handoff = fixture.core.white_box("nip77.take_handoff", |s| {
        s.nip77.take_handoff(&candidate).unwrap()
    });
    fixture
        .core
        .white_box("abandon_sub", |s| s.abandon_sub(&candidate));
    fixture.core.white_box("open_neg_session", |s| {
        s.open_neg_session(handoff, &mut Vec::new())
    });
    let neg = fixture
        .core
        .nip77
        .session_children_of(&fixture.plan_sub_id)
        .iter()
        .next()
        .cloned()
        .unwrap();
    let session = fixture.core.white_box("nip77.take_session", |s| {
        s.nip77.take_session(&neg).unwrap()
    });
    fixture.core.white_box("finish_neg_session", |s| {
        s.finish_neg_session(
            neg.clone(),
            fixture.relay.clone(),
            session,
            BTreeSet::from([EventId::from_byte_array([7; 32])]),
            &mut Vec::new(),
        )
    });
    fixture.update();
    fixture.assert_role_updated(&neg);
    fixture.finish();
}

#[test]
fn zero_wire_metadata_attach_extends_candidate_and_backlog_generations() {
    let mut fixture = Fixture::new();
    let candidate = fixture.begin_candidate();
    let handoff = fixture.core.white_box("nip77.take_handoff", |s| {
        s.nip77.take_handoff(&candidate).unwrap()
    });
    fixture.core.white_box("handoff_fallback_to_req", |s| {
        s.handoff_fallback_to_req(handoff, &mut Vec::new())
    });
    let backlog = fixture
        .core
        .nip77
        .backfill_children_of(&fixture.plan_sub_id)
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
    let handoff = fixture.core.white_box("nip77.take_handoff", |s| {
        s.nip77.take_handoff(&candidate).unwrap()
    });
    fixture.core.white_box("activate_live_and_open_neg", |s| {
        s.activate_live_and_open_neg(handoff, &mut Vec::new())
    });
    assert_eq!(
        fixture.core.nip77.live_for_plan(&fixture.plan_sub_id),
        Some(&candidate)
    );
    assert!(!fixture.core.nip77.sessions_is_empty());

    fixture.core.white_box("on_relay_disconnected", |s| {
        s.on_relay_disconnected(handle, fixture.session.clone(), DisconnectReason::Error)
    });
    assert!(fixture.core.nip77.live_is_empty());
    assert!(fixture.core.nip77.handoffs_is_empty());
    assert!(fixture.core.nip77.handoffs_is_empty());
    assert!(fixture.core.nip77.sessions_is_empty());
    assert!(fixture.core.nip77.sessions_is_empty());
    assert!(fixture.core.nip77.backfills_is_empty());
    assert!(fixture.core.nip77.backfills_is_empty());
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
            source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let plan_sub_id = SubId::for_wire(relay.clone(), &atom.filter, &atom.source, atom.access);
        core.white_box("attribution.observe_atom", |s| {
            s.attribution.observe_atom(&atom)
        });
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
