//! Issue #779's residual: a refused, still-current REQ must get a bounded,
//! event-driven re-handoff instead of waiting for a reconnect that may never
//! come.
//!
//! `Pool::send` has always been able to refuse a frame; #1331 bounded what a
//! relay worker retains outbound, which is why it now routinely will. The
//! refusal already reaches the reducer through `on_wire_request_handoff` and
//! is already app-visible as `relay_refused`. What it does NOT do is give the
//! still-current requirement another owner: `diff_plans` compares plan to
//! plan, so a refused REQ that is still in the plan is never re-emitted.
//!
//! The fact this file establishes as the re-handoff trigger is the worker's
//! outbound envelope releasing room after it refused something --
//! `EngineMsg::RelayOutboundCapacityAvailable`. It is a fact, not a clock: it
//! can only be produced by a socket actually accepting bytes, and it is
//! edge-armed by a refusal, so a permanently stalled worker produces nothing
//! and a re-refusal simply re-arms and waits again.

use super::*;

fn relay() -> RelayUrl {
    RelayUrl::parse("wss://relay.example.com").unwrap()
}

fn handle(generation: u64) -> RelayHandle {
    RelayHandle {
        slot: 0,
        generation,
    }
}

/// A connected Public relay with exactly one planned REQ, whose handoff the
/// transport refused. Returns the core and the refused request's identity.
fn refused_request() -> (EngineCore<MemoryStore>, SubId, ConcreteFilter, Vec<Effect>) {
    let author = Keys::generate();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay()]);
    let mut core = new_core(dir);
    let _ = connect(&mut core, 0, &relay());
    let subscribed = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &author.public_key().to_hex(),
    )));
    let (sub_id, filter) = {
        let (sub_id, filter) = req_for_kind(&subscribed, &relay(), 1);
        (sub_id.clone(), filter.clone())
    };
    // Exactly what the runtime does when `Pool::send` returns `false`: the
    // worker's finite outbound envelope refused the frame at admission.
    let refused = core.on_wire_request_handoff(
        &public_session(&relay()),
        &sub_id,
        filter.hash(),
        Some(handle(1)),
        false,
        Some("outbound envelope full".to_string()),
    );
    (core, sub_id, filter, refused)
}

fn replayed_reqs(effects: &[Effect], sub_id: &SubId) -> usize {
    effects
        .iter()
        .map(|effect| match effect {
            Effect::Replay(_, reqs) => reqs.iter().filter(|req| &req.sub_id == sub_id).count(),
            Effect::Wire(delta) => delta
                .ops
                .iter()
                .flat_map(|(_, ops)| ops.iter())
                .filter(|op| matches!(op, WireOp::Req(candidate, _) if candidate == sub_id))
                .count(),
            _ => 0,
        })
        .sum()
}

/// The refusal is app-visible. This is the difference from #775, where the
/// refusal reached nothing at all, and it is why #779's residual is a
/// scheduling question rather than an ownership one.
#[test]
fn a_refused_request_is_app_visible_as_a_refusal() {
    let (_core, _sub_id, _filter, refused) = refused_request();
    let facts: Vec<_> = refused
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitObservationEvidence(_, facts, ..) => Some(facts.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(
        facts
            .iter()
            .any(|evidence| matches!(evidence.fact, ObservationFact::RelayRefused { .. })),
        "a refused handoff must reach the app as a refusal: {refused:?}"
    );
}

/// Nothing that is not a fresh connection re-hands the refused request. Ticks
/// and inbound frames leave a planned read lane that never reached the wire.
#[test]
fn a_refused_request_is_never_re_handed_off_without_a_new_generation() {
    let (mut core, sub_id, _filter, _refused) = refused_request();

    let mut later = core.handle(EngineMsg::Tick(Timestamp::from(1_000u64)));
    later.extend(core.handle(EngineMsg::Tick(Timestamp::from(2_000u64))));
    later.extend(core.handle(EngineMsg::RelayInformationResolved(relay(), None)));
    assert_eq!(
        replayed_reqs(&later, &sub_id),
        0,
        "no clock and no unrelated fact may re-hand a refused request: {later:?}"
    );

    // Only a brand-new transport generation recovers it today.
    let reconnected = core.handle(EngineMsg::RelayConnected(
        handle(2),
        public_session(&relay()),
    ));
    assert_eq!(
        replayed_reqs(&reconnected, &sub_id),
        1,
        "a fresh generation is the ONLY recovery on current master: {reconnected:?}"
    );
}

/// The fact. A worker whose outbound envelope released room after refusing a
/// frame re-hands every still-planned request that is not already live on
/// that exact handle -- the same reconciliation `on_relay_connected` performs,
/// not a second replay mechanism.
#[test]
fn released_outbound_capacity_re_hands_the_still_current_request() {
    let (mut core, sub_id, _filter, _refused) = refused_request();

    let effects = core.handle(EngineMsg::RelayOutboundCapacityAvailable(
        handle(1),
        public_session(&relay()),
    ));

    assert_eq!(
        replayed_reqs(&effects, &sub_id),
        1,
        "released capacity is the fact on which a refused request is re-handed: {effects:?}"
    );
}

/// Bounded: the re-handoff carries no retry memory of its own. Once the
/// request is accepted, a further capacity release re-hands nothing, because
/// the reconciliation subtracts what is live on this exact handle.
#[test]
fn released_capacity_re_hands_nothing_once_the_request_is_live() {
    let (mut core, sub_id, filter, _refused) = refused_request();

    let first = core.handle(EngineMsg::RelayOutboundCapacityAvailable(
        handle(1),
        public_session(&relay()),
    ));
    assert_eq!(replayed_reqs(&first, &sub_id), 1);
    let _ = core.on_wire_request_handoff(
        &public_session(&relay()),
        &sub_id,
        filter.hash(),
        Some(handle(1)),
        true,
        None,
    );

    let second = core.handle(EngineMsg::RelayOutboundCapacityAvailable(
        handle(1),
        public_session(&relay()),
    ));
    assert_eq!(
        replayed_reqs(&second, &sub_id),
        0,
        "an accepted request is live; releasing capacity again must not resend it: {second:?}"
    );
}

/// Still-current. Demand withdrawn between the refusal and the capacity fact
/// cannot be resurrected: the reducer replays the CURRENT plan, and it holds
/// no deferred set that could outlive the plan entry.
#[test]
fn released_capacity_never_resurrects_a_withdrawn_request() {
    let author = Keys::generate();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay()]);
    let mut core = new_core(dir);
    let _ = connect(&mut core, 0, &relay());
    let subscribed = core.handle(EngineMsg::Subscribe(literal_query(
        &[1],
        &author.public_key().to_hex(),
    )));
    let observation = subscribed_handle(&subscribed);
    let (sub_id, filter) = {
        let (sub_id, filter) = req_for_kind(&subscribed, &relay(), 1);
        (sub_id.clone(), filter.clone())
    };
    let _ = core.on_wire_request_handoff(
        &public_session(&relay()),
        &sub_id,
        filter.hash(),
        Some(handle(1)),
        false,
        Some("outbound envelope full".to_string()),
    );
    let _ = core.handle(EngineMsg::Unsubscribe(observation));

    let effects = core.handle(EngineMsg::RelayOutboundCapacityAvailable(
        handle(1),
        public_session(&relay()),
    ));
    assert_eq!(
        replayed_reqs(&effects, &sub_id),
        0,
        "a withdrawn request has no current owner to re-hand: {effects:?}"
    );
}

/// A stale generation's capacity release is inert. The envelope belongs to a
/// worker; a release observed for a generation the reducer has moved past
/// cannot place frames on the connection that replaced it.
#[test]
fn a_stale_generations_capacity_release_re_hands_nothing() {
    let (mut core, sub_id, _filter, _refused) = refused_request();
    let _ = core.handle(EngineMsg::RelayDisconnected(
        handle(1),
        public_session(&relay()),
        DisconnectReason::Error,
    ));

    let effects = core.handle(EngineMsg::RelayOutboundCapacityAvailable(
        handle(1),
        public_session(&relay()),
    ));
    assert_eq!(
        replayed_reqs(&effects, &sub_id),
        0,
        "a superseded handle may not re-hand anything: {effects:?}"
    );
}

/// #8 isolation survives the new fact: a protected session's requests stay
/// parked on AUTH readiness, and released capacity is not an AUTH fact.
#[test]
fn released_capacity_never_bypasses_the_protected_auth_gate() {
    let signer = Keys::generate();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(signer.public_key(), [relay()]);
    let mut core = new_core(dir);
    let session = signer_session(&relay(), signer.public_key());
    let _ = core.handle(EngineMsg::RelayConnected(handle(1), session.clone()));
    let _ = core.handle(EngineMsg::Subscribe(protected_pinned_query(
        &relay(),
        signer.public_key(),
        1,
    )));

    let effects = core.handle(EngineMsg::RelayOutboundCapacityAvailable(
        handle(1),
        session.clone(),
    ));
    assert_no_protected_req(&effects, &session);
}
