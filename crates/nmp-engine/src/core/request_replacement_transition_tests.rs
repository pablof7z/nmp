//! Fresh request replacement transition ownership (#774).

use nmp_grammar::RelaySessionKey;
use std::{borrow::Cow, collections::BTreeSet};

use nmp_store::{coverage_key, RedbStore};

use super::query::PlanDeltaMode;
use super::*;

struct Fixture {
    core: EngineCore,
    relay: RelayUrl,
    session: RelaySessionKey,
    handle: TransportRelayHandle,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let relay = RelayUrl::parse(&format!("wss://{name}.example")).unwrap();
        let session = RelaySessionKey::unauthenticated(relay.clone());
        let handle = TransportRelayHandle {
            slot: 93,
            generation: 1,
        };
        let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 8);
        core.white_box("slot_to_relay.insert", |s| {
            s.slot_to_relay
                .insert(handle.slot, (handle, session.clone()))
        });
        core.white_box("connected_relays.insert", |s| {
            s.connected_relays.insert(session.clone())
        });
        core.white_box("ever_connected_relays.insert", |s| {
            s.ever_connected_relays.insert(session.clone())
        });
        Self {
            core,
            relay,
            session,
            handle,
        }
    }

    fn atom(&self, author_byte: u8) -> ContextualAtom {
        ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1u16])),
                authors: Some(BTreeSet::from([format!("{author_byte:02x}").repeat(32)])),
                ..ConcreteFilter::default()
            },
            routing: ReadRouting::Explicit(vec![self.relay.clone()]),
            authenticate_as: None,
            routing_evidence: BTreeSet::new(),
        }
    }

    /// One recompile, in the order `CoreState::recompile` performs it: install
    /// `demand` as attribution's current logical demand, then compile the
    /// router from the same set. Each caller used to hand-write the
    /// attribution half — `observe_atom` for whatever arrived and
    /// `release_atom` for whatever left, twenty sites across five tests — so
    /// the fixture's "recompile" and the reducer's could drift apart with
    /// nothing to catch it (#1850).
    fn compile(&mut self, demand: BTreeSet<ContextualAtom>) -> Vec<Effect> {
        let outcome = self.core.white_box("recompile", |s| {
            s.attribution.set_active_demand(demand.iter());
            s.router
                .compile(&demand, &s.routing_facts, s.compile_budget())
        });
        let mut effects = Vec::new();
        self.core.white_box("apply_request_metadata_updates", |s| {
            s.apply_request_metadata_updates(&outcome.request_metadata_updates, &mut effects)
        });
        self.core.white_box("apply_router_plan_delta", |s| {
            s.apply_router_plan_delta(
                &outcome.replacements,
                outcome.wire,
                PlanDeltaMode::Full,
                &mut effects,
            )
        });
        effects
    }

    fn accept(&mut self, effects: &[Effect]) -> SubId {
        let (_, sub_id, _, attempt_id) = only_request(effects);
        self.core
            .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
                attempt_id,
                handle: self.handle,
            });
        sub_id
    }

    fn eose(&mut self, sub_id: &SubId) -> Vec<Effect> {
        self.core.white_box("on_relay_frame", |s| {
            s.on_relay_frame(
                self.handle,
                self.session.clone(),
                RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
                    nostr::SubscriptionId::new(wire_sub_id_string(sub_id)),
                ))),
            )
        })
    }

    fn establish_active_nip77(&mut self, atom: &ContextualAtom) -> (SubId, SubId) {
        let opened = self.compile(BTreeSet::from([atom.clone()]));
        let (_, plan_sub_id, filter, attempt_id) = only_request(&opened);
        self.core
            .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
                attempt_id,
                handle: self.handle,
            });
        self.core.white_box("prober.force_supported_for_test", |s| {
            s.prober.force_supported_for_test(self.relay.clone())
        });
        let probed = self.core.prober.probed(&self.relay).unwrap();
        let mut effects = Vec::new();
        self.core.white_box("begin_neg_handoff", |s| {
            s.begin_neg_handoff(
                probed,
                plan_sub_id.clone(),
                Some(plan_sub_id.clone()),
                filter,
                &mut effects,
            )
        });
        let child = self.accept(&effects);
        self.eose(&child);
        assert_eq!(self.core.nip77.live_for_plan(&plan_sub_id), Some(&child));
        (plan_sub_id, child)
    }
}

fn only_request(effects: &[Effect]) -> (RelaySessionKey, SubId, ConcreteFilter, RequestAttemptId) {
    effects
        .iter()
        .find_map(|effect| {
            let Effect::Wire(delta) = effect else {
                return None;
            };
            delta.ops.iter().find_map(|(session, ops)| {
                ops.iter().find_map(|op| {
                    let WireOp::Req(sub_id, filter) = op else {
                        return None;
                    };
                    Some((
                        session.clone(),
                        sub_id.clone(),
                        filter.clone(),
                        delta.attempt_id(session, sub_id, filter),
                    ))
                })
            })
        })
        .expect("one request effect")
}

fn wire_ops(effects: &[Effect]) -> Vec<&WireOp> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| delta.ops.iter().flat_map(|(_, ops)| ops))
        .collect()
}

#[test]
fn predecessor_candidate_eose_during_replacement_keeps_its_plan_metadata() {
    let mut fixture = Fixture::new("nip77-predecessor-candidate-eose");
    let first = fixture.atom(20);
    let second = fixture.atom(21);

    let opened = fixture.compile(BTreeSet::from([first.clone()]));
    let (_, first_plan, first_filter, first_attempt) = only_request(&opened);
    fixture
        .core
        .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id: first_attempt,
            handle: fixture.handle,
        });
    fixture
        .core
        .white_box("prober.force_supported_for_test", |s| {
            s.prober.force_supported_for_test(fixture.relay.clone())
        });
    let probed = fixture.core.prober.probed(&fixture.relay).unwrap();
    let mut candidate_effects = Vec::new();
    fixture.core.white_box("begin_neg_handoff", |s| {
        s.begin_neg_handoff(
            probed,
            first_plan.clone(),
            Some(first_plan.clone()),
            first_filter,
            &mut candidate_effects,
        )
    });
    let (_, first_candidate, _, candidate_attempt) = only_request(&candidate_effects);
    fixture
        .core
        .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id: candidate_attempt,
            handle: fixture.handle,
        });

    let replacement = fixture.compile(BTreeSet::from([second.clone()]));
    let (_, second_candidate, _, second_attempt) = only_request(&replacement);
    assert!(fixture.core.request_replacements.len() == 1);
    assert!(fixture
        .core
        .plan_execution_metadata
        .contains_key(&first_plan));

    let predecessor_eose = fixture.eose(&first_candidate);
    assert!(predecessor_eose
        .iter()
        .any(|effect| matches!(effect, Effect::NegOpen(..))));

    fixture
        .core
        .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id: second_attempt,
            handle: fixture.handle,
        });
    fixture.eose(&second_candidate);
    fixture.compile(BTreeSet::new());
    assert_eq!(
        fixture.core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn superseding_a_nip77_candidate_before_eose_cancels_it_and_late_eose_is_inert() {
    let mut fixture = Fixture::new("nip77-supersede-before-eose");
    let first = fixture.atom(1);
    let second = fixture.atom(2);
    let third = fixture.atom(3);
    let second_claim = coverage_key(&second);
    let (first_plan, old_child) = fixture.establish_active_nip77(&first);

    let second_effects = fixture.compile(BTreeSet::from([second.clone()]));
    let (_, second_candidate, _, second_attempt) = only_request(&second_effects);
    fixture
        .core
        .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id: second_attempt,
            handle: fixture.handle,
        });
    let second_plan = fixture.core.router.plan().reqs[&fixture.session][0]
        .sub_id
        .clone();
    assert_eq!(
        fixture.core.nip77.live_for_plan(&first_plan),
        Some(&old_child)
    );

    let third_effects = fixture.compile(BTreeSet::from([third.clone()]));
    let (_, third_candidate, _, third_attempt) = only_request(&third_effects);
    let third_plan = fixture.core.router.plan().reqs[&fixture.session][0]
        .sub_id
        .clone();
    assert_eq!(
        fixture.core.nip77.phase_of(&second_candidate),
        None,
        "a superseded candidate must leave every repair phase, not merely its handoff"
    );
    assert!(!fixture
        .core
        .pending_request_evidence
        .contains_key(&(fixture.session.clone(), second_candidate.clone(),)));
    assert!(!fixture.core.request_replacements.contains(&second_plan));
    assert_eq!(
        fixture
            .core
            .request_replacements
            .get(&third_plan)
            .unwrap()
            .prior_sub_id,
        first_plan
    );
    assert_eq!(
        fixture.core.nip77.live_for_plan(&first_plan),
        Some(&old_child)
    );

    let late = fixture.eose(&second_candidate);
    assert!(wire_ops(&late).is_empty());
    assert!(fixture
        .core
        .store
        .get_coverage(
            second_claim,
            &RelaySessionKey::unauthenticated(fixture.relay.clone())
        )
        .unwrap()
        .is_none());
    assert_eq!(
        fixture.core.nip77.live_for_plan(&first_plan),
        Some(&old_child)
    );

    let accepted = fixture
        .core
        .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id: third_attempt,
            handle: fixture.handle,
        });
    assert!(wire_ops(&accepted)
        .iter()
        .all(|op| !matches!(op, WireOp::Close(sub_id) if sub_id == &old_child)));
    let promoted = fixture.eose(&third_candidate);
    assert_eq!(
        wire_ops(&promoted)
            .iter()
            .filter(|op| matches!(op, WireOp::Close(sub_id) if sub_id == &old_child))
            .count(),
        1
    );

    fixture.compile(BTreeSet::new());
    assert_eq!(
        fixture.core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn withdrawing_an_accepted_nip77_candidate_before_eose_closes_candidate_and_predecessor_once() {
    let mut fixture = Fixture::new("nip77-accepted-successor-withdraw");
    let first = fixture.atom(6);
    let second = fixture.atom(7);
    let (_, old_child) = fixture.establish_active_nip77(&first);

    let replacement = fixture.compile(BTreeSet::from([second.clone()]));
    let (_, successor_child, _, successor_attempt) = only_request(&replacement);
    fixture
        .core
        .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id: successor_attempt,
            handle: fixture.handle,
        });
    assert_eq!(fixture.core.request_replacements.len(), 1);

    let withdrawn = fixture.compile(BTreeSet::new());
    for expected in [&successor_child, &old_child] {
        assert_eq!(
            wire_ops(&withdrawn)
                .iter()
                .filter(|op| matches!(op, WireOp::Close(sub_id) if sub_id == expected))
                .count(),
            1
        );
    }
    assert_eq!(
        fixture.core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn withdrawing_a_refused_nip77_successor_retires_its_predecessor_and_every_transition_owner() {
    let mut fixture = Fixture::new("nip77-refused-successor-withdraw");
    let first = fixture.atom(4);
    let second = fixture.atom(5);
    let (_, old_child) = fixture.establish_active_nip77(&first);

    let replacement = fixture.compile(BTreeSet::from([second.clone()]));
    let (_, successor_child, _, successor_attempt) = only_request(&replacement);
    fixture
        .core
        .on_wire_request_handoff(RequestHandoffOutcome::Refused {
            attempt_id: successor_attempt,
            cause: LocalSendRefusal::SessionUnavailable,
        });
    assert_eq!(fixture.core.attempts.counts().retry_jobs, 1);
    assert_eq!(fixture.core.request_replacements.len(), 1);
    assert!(!fixture
        .core
        .live_wire_requests
        .contains_key(&(fixture.session.clone(), successor_child,)));

    let withdrawn = fixture.compile(BTreeSet::new());
    assert_eq!(
        wire_ops(&withdrawn)
            .iter()
            .filter(|op| matches!(op, WireOp::Close(sub_id) if sub_id == &old_child))
            .count(),
        1
    );
    assert_eq!(
        fixture.core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn accepted_byte_changed_replacements_retain_only_one_current_request() {
    let mut fixture = Fixture::new("bounded-accepted-replacements");
    let opened = fixture.compile(BTreeSet::from([fixture.atom(10)]));
    let mut current_sub = fixture.accept(&opened);

    for author in 11..=42 {
        let next = fixture.atom(author);
        let replacement = fixture.compile(BTreeSet::from([next]));
        let (_, next_sub, _, next_attempt) = only_request(&replacement);
        assert_ne!(current_sub, next_sub);
        assert_eq!(
            fixture
                .core
                .bench_ownership_census()
                .request_replacement_jobs,
            1
        );
        fixture
            .core
            .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
                attempt_id: next_attempt,
                handle: fixture.handle,
            });
        let census = fixture.core.bench_ownership_census();
        assert_eq!(census.request_replacement_jobs, 0);
        assert_eq!(census.live_wire_owners, 1);
        assert_eq!(census.active_execution_owners, 1);
        current_sub = next_sub;
    }

    fixture.compile(BTreeSet::new());
    assert_eq!(
        fixture.core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}
