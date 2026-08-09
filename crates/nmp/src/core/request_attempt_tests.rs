//! Exact local REQ attempt, refusal, and retry ownership (#849).

use std::{borrow::Cow, collections::BTreeSet};

use nmp_grammar::{Binding, Demand, Filter, IndexedTagName};
use nmp_store::{coverage_key, MemoryStore};

use super::query::PlanDeltaMode;
use super::*;

fn live_query(relay: &RelayUrl) -> LiveQuery {
    let mut demand = Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        tags: BTreeMap::from([(
            IndexedTagName::new('p').expect("valid fixture tag"),
            Binding::Literal(BTreeSet::from(["owner".to_string()])),
        )]),
        ..Filter::default()
    });
    demand.source = SourceAuthority::Pinned(BTreeSet::from([relay.clone()]));
    demand.freshness = Freshness::Live;
    LiveQuery::single(demand)
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

fn statuses(effects: &[Effect]) -> Vec<SourceStatus> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitRows(_, _, evidence) => Some(evidence),
            _ => None,
        })
        .flat_map(|evidence| evidence.iter())
        .flat_map(|evidence| evidence.sources.iter())
        .map(|source| source.status)
        .collect()
}

fn observation_id(effects: &[Effect]) -> ObservationId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, _, _) => Some(*id),
            _ => None,
        })
        .expect("an observation open returns its immediate cache seed")
}

fn atom(relay: &RelayUrl, author: &str) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(BTreeSet::from([author.to_string()])),
            ..ConcreteFilter::default()
        },
        source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn apply_compile(
    core: &mut EngineCore<MemoryStore>,
    demand: BTreeSet<ContextualAtom>,
) -> Vec<Effect> {
    let outcome = core
        .router
        .compile(&demand, &core.routing_facts, core.compile_budget());
    let mut effects = Vec::new();
    core.apply_request_metadata_updates(&outcome.request_metadata_updates, &mut effects);
    core.apply_router_plan_delta(
        &outcome.replacements,
        outcome.wire,
        PlanDeltaMode::Full,
        &mut effects,
    );
    effects
}

#[test]
fn repeated_local_refusals_keep_one_goal_increase_backoff_and_become_requesting_only_on_accept() {
    let relay = RelayUrl::parse("wss://request-attempt.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let handle = TransportRelayHandle {
        slot: 81,
        generation: 1,
    };
    let mut core = EngineCore::new(MemoryStore::new(), 8);
    core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let opened = core.handle(EngineMsg::Subscribe(live_query(&relay)));
    let observation = observation_id(&opened);
    let flushed = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)));
    let (_, sub_id, filter, first_attempt) = only_request(&flushed);
    let awaiting_index = flushed
        .iter()
        .position(|effect| {
            matches!(
                effect,
                Effect::EmitRows(_, _, evidence)
                    if evidence.iter().flat_map(|item| &item.sources).any(
                        |source| source.status == SourceStatus::AwaitingRequest
                    )
            )
        })
        .expect("pre-handoff evidence reports the reducer-owned send attempt");
    let wire_index = flushed
        .iter()
        .position(|effect| matches!(effect, Effect::Wire(_)))
        .expect("the admission flush emits its request");
    assert!(awaiting_index < wire_index);
    let claim = *core.router.plan().reqs[&session][0]
        .coverage_claims
        .iter()
        .next()
        .expect("fixture request owns one durable claim");
    assert!(statuses(&flushed).contains(&SourceStatus::AwaitingRequest));

    let refused = core.on_wire_request_handoff(RequestHandoffOutcome::Refused {
        attempt_id: first_attempt,
        cause: LocalSendRefusal::SessionUnavailable,
    });
    assert!(refused.iter().any(|effect| matches!(
        effect,
        Effect::EmitObservationEvidence(_, facts)
            if facts.iter().any(|fact| matches!(fact.fact, ObservationFact::RequestDeferred { .. }))
    )));
    let due_one = core.next_deadline().unwrap().expect("retry deadline");
    assert_eq!(due_one, Timestamp::from(RETRY_INITIAL_SECS));
    let census = core.bench_ownership_census();
    assert_eq!(census.request_retry_jobs, 1);
    assert_eq!(census.request_attempts, 0);
    assert_eq!(census.pending_execution_owners, 0);

    // A never-accepted generation cannot earn coverage from a stray EOSE.
    core.on_relay_frame(
        handle,
        session.clone(),
        RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
            nostr::SubscriptionId::new(wire_sub_id_string(&sub_id)),
        ))),
    );
    assert!(core
        .resolver
        .store()
        .get_coverage(claim, &relay)
        .unwrap()
        .is_none());

    let retry_one = core.handle(EngineMsg::Tick(due_one));
    let (_, _, _, second_attempt) = only_request(&retry_one);
    assert_ne!(first_attempt, second_attempt);
    assert!(core
        .on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id: first_attempt,
            handle,
        })
        .is_empty());
    core.on_wire_request_handoff(RequestHandoffOutcome::Refused {
        attempt_id: second_attempt,
        cause: LocalSendRefusal::WorkerAdmissionRefused { handle },
    });
    let due_two = core.next_deadline().unwrap().expect("second deadline");
    assert_eq!(due_two, due_one + RETRY_INITIAL_SECS * 2);
    assert_eq!(core.bench_ownership_census().request_retry_jobs, 1);

    let retry_two = core.handle(EngineMsg::Tick(due_two));
    let (_, retry_sub, retry_filter, mut current_attempt) = only_request(&retry_two);
    assert_eq!(retry_sub, sub_id);
    assert_eq!(retry_filter, filter);
    let mut prior_due = due_two;
    for failure in 3..=12 {
        core.on_wire_request_handoff(RequestHandoffOutcome::Refused {
            attempt_id: current_attempt,
            cause: LocalSendRefusal::SessionUnavailable,
        });
        let due = core
            .next_deadline()
            .unwrap()
            .expect("capped retry deadline");
        assert_eq!(due, prior_due + bootstrap_retry_delay_secs(failure));
        assert_eq!(core.bench_ownership_census().request_retry_jobs, 1);
        let emitted = core.handle(EngineMsg::Tick(due));
        current_attempt = only_request(&emitted).3;
        prior_due = due;
    }
    assert_eq!(bootstrap_retry_delay_secs(12), RETRY_MAX_SECS);
    let accepted = core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
        attempt_id: current_attempt,
        handle,
    });
    assert!(statuses(&accepted).contains(&SourceStatus::Requesting));
    let census = core.bench_ownership_census();
    assert_eq!(census.request_retry_jobs, 0);
    assert_eq!(census.request_retry_sub_keys, 0);
    assert_eq!(census.request_retry_session_keys, 0);
    assert_eq!(census.request_attempts, 0);
    assert_eq!(census.live_wire_owners, 1);

    let closed = core.handle(EngineMsg::Unsubscribe(observation));
    assert!(closed.iter().any(|effect| matches!(
        effect,
        Effect::Wire(delta)
            if delta.ops.iter().flat_map(|(_, ops)| ops).any(
                |op| matches!(op, WireOp::Close(closed) if closed == &sub_id)
            )
    )));
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn nip77_candidate_status_projects_its_role_id_to_the_live_plan_request() {
    let relay = RelayUrl::parse("wss://request-attempt-nip77.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let handle = TransportRelayHandle {
        slot: 97,
        generation: 1,
    };
    let mut core = EngineCore::new(MemoryStore::new(), 8);
    core.prober
        .states
        .insert(relay.clone(), crate::negentropy::ProbeState::Supported);
    core.handle(EngineMsg::RelayConnected(handle, session.clone()));
    let opened = core.handle(EngineMsg::Subscribe(live_query(&relay)));
    let observation = observation_id(&opened);

    let flushed = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)));
    let (_, candidate_sub_id, _, attempt_id) = only_request(&flushed);
    let plan_sub_id = core.router.plan().reqs[&session][0].sub_id.clone();
    assert_ne!(candidate_sub_id, plan_sub_id);
    assert!(statuses(&flushed).contains(&SourceStatus::AwaitingRequest));
    assert!(!statuses(&flushed).contains(&SourceStatus::Error));

    let accepted =
        core.on_wire_request_handoff(RequestHandoffOutcome::Accepted { attempt_id, handle });
    assert!(statuses(&accepted).contains(&SourceStatus::Requesting));
    assert!(!statuses(&accepted).contains(&SourceStatus::Error));

    core.handle(EngineMsg::Unsubscribe(observation));
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn dynamic_full_recompile_publishes_awaiting_request_before_wire_dispatch() {
    let relay = RelayUrl::parse("wss://request-recompile-order.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let handle = TransportRelayHandle {
        slot: 96,
        generation: 1,
    };
    let mut core = EngineCore::new(MemoryStore::new(), 8);
    core.handle(EngineMsg::RelayConnected(handle, session));
    let opened = core.handle(EngineMsg::Subscribe(live_query(&relay)));
    let observation = observation_id(&opened);

    let mut effects = Vec::new();
    core.recompile(&mut effects);
    let awaiting_index = effects
        .iter()
        .position(|effect| {
            matches!(
                effect,
                Effect::EmitRows(id, _, evidence)
                    if *id == observation
                        && evidence.iter().flat_map(|item| &item.sources).any(
                            |source| source.status == SourceStatus::AwaitingRequest
                        )
            )
        })
        .expect("the full recompile publishes its pre-handoff truth");
    let wire_index = effects
        .iter()
        .position(|effect| matches!(effect, Effect::Wire(_)))
        .expect("the full recompile emits its request");
    assert!(awaiting_index < wire_index);

    core.handle(EngineMsg::Unsubscribe(observation));
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[path = "request_attempt_tests/nip77_status.rs"]
mod nip77_status;
#[path = "request_attempt_tests/protected.rs"]
mod protected;
#[path = "request_attempt_tests/replacement.rs"]
mod replacement;
