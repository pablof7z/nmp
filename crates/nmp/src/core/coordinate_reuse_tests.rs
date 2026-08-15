//! Falsifiers for coordinate reuse (#1630).
//!
//! Each test drives the real reducer — subscribe, wire admission, transport
//! handoff, EVENT frames, EOSE — and then asks the same question an edit
//! asks: does this relay's current value for one coordinate still need a REQ?
//!
//! [`EngineCore::coordinate_reuse_new_reqs`] counts only the verdicts that
//! cost a round trip, so every reuse path is asserted as a zero that a
//! regression turns into a one. The truncation and lost-count scenarios pin the
//! counter at one, so the number is known to move.

use std::borrow::Cow;

use nmp_grammar::{Binding, Demand, Filter};
use nmp_router::SubId;
use nmp_store::{RedbStore, RelayObserved};
use nostr::nips::nip01::Coordinate;
use nostr::{Event, EventBuilder, Keys, Kind, RelayMessage, SubscriptionId, Tag};

use super::*;

fn contact_list(author: &Keys, follows: &[&Keys], created_at: u64) -> Event {
    let mut builder = EventBuilder::new(Kind::ContactList, "");
    for follow in follows {
        builder = builder.tag(Tag::public_key(follow.public_key()));
    }
    builder
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("fixture contact list signs")
}

fn coordinate_of(author: &Keys) -> Coordinate {
    Coordinate {
        kind: Kind::ContactList,
        public_key: author.public_key(),
        identifier: String::new(),
    }
}

fn connected_core(relay: &RelayUrl) -> (EngineCore, TransportRelayHandle, RelaySessionKey) {
    connected_core_with_store(relay, RedbStore::temporary().expect("temporary Redb store"))
}

fn connected_core_with_store(
    relay: &RelayUrl,
    store: RedbStore,
) -> (EngineCore, TransportRelayHandle, RelaySessionKey) {
    let mut core = EngineCore::new(store, 20);
    let transport = TransportRelayHandle {
        slot: 7,
        generation: 1,
    };
    let session = RelaySessionKey::public(relay.clone());
    core.handle(EngineMsg::RelayConnected(transport, session.clone()));
    core.handle(EngineMsg::RelayInformationResolved(relay.clone(), None));
    core.handle(EngineMsg::Tick(Timestamp::from(100u64)));
    (core, transport, session)
}

/// The shape an app already has on screen: every contact list this relay
/// holds, with no author or window narrowing.
fn broad_contact_list_feed(
    relay: &RelayUrl,
    since: Option<u64>,
    limit: Option<usize>,
) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
                since,
                limit,
                ..Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            AccessContext::Public,
        )
        .expect("a pinned kind:3 feed is a nonempty demand"),
    )
}

/// The one-off an offline edit opens for one coordinate.
fn coordinate_query(relay: &RelayUrl, coordinate: &Coordinate) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([coordinate.kind.as_u16()])),
                authors: Some(Binding::Literal(BTreeSet::from([coordinate
                    .public_key
                    .to_hex()]))),
                ..Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            AccessContext::Public,
        )
        .expect("a pinned coordinate is a nonempty demand"),
    )
}

/// Acknowledge every REQ the reducer handed out, exactly as a live pool
/// would. Without this the engine owns a planned request but no accepted wire
/// owner, and nothing can have returned a frame.
fn accept_every_pending_request(core: &mut EngineCore, transport: TransportRelayHandle) {
    loop {
        let pending: Vec<_> = core
            .pending_request_evidence
            .values()
            .flatten()
            .map(|request| request.attempt_id)
            .collect();
        let Some(attempt_id) = pending.into_iter().next() else {
            return;
        };
        core.on_wire_request_handoff(RequestHandoffOutcome::Accepted {
            attempt_id,
            handle: transport,
        });
    }
}

/// The NIP-01 subscription id string one planned request speaks on the wire.
fn wire_of(sub_id: &SubId) -> String {
    sub_id.1.to_string()
}

fn only_request(core: &EngineCore, session: &RelaySessionKey) -> SubId {
    let reqs = core.router.plan().reqs[session].clone();
    assert_eq!(reqs.len(), 1, "the fixture plans exactly one request");
    reqs[0].sub_id.clone()
}

fn wire_reqs(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(&delta.ops),
            _ => None,
        })
        .flatten()
        .flat_map(|(_, ops)| ops)
        .filter(|op| matches!(op, WireOp::Req(_, _)))
        .count()
}

fn deliver(
    core: &mut EngineCore,
    transport: TransportRelayHandle,
    session: &RelaySessionKey,
    wire: &str,
    event: Event,
) {
    core.handle(EngineMsg::RelayFrame(
        transport,
        session.clone(),
        RelayFrame::from_message(RelayMessage::event(SubscriptionId::new(wire), event)),
    ));
}

/// The batched EVENT door the runtime actually feeds
/// ([`crate::runtime`] hands the pool's drained frames to
/// [`EngineMsg::RelayFrames`]). Counting has to hold on this path, not only
/// on the one-frame-at-a-time door headless tests find convenient.
fn deliver_batch(
    core: &mut EngineCore,
    transport: TransportRelayHandle,
    session: &RelaySessionKey,
    wire: &str,
    events: Vec<Event>,
) {
    core.handle(EngineMsg::RelayFrames(
        events
            .into_iter()
            .map(|event| {
                (
                    transport,
                    session.clone(),
                    RelayFrame::from_message(RelayMessage::event(SubscriptionId::new(wire), event)),
                )
            })
            .collect(),
    ));
}

fn eose(
    core: &mut EngineCore,
    transport: TransportRelayHandle,
    session: &RelaySessionKey,
    wire: &str,
) {
    core.clock = Timestamp::from(101u64);
    core.handle(EngineMsg::RelayFrame(
        transport,
        session.clone(),
        RelayFrame::from_message(RelayMessage::EndOfStoredEvents(Cow::Owned(
            SubscriptionId::new(wire),
        ))),
    ));
}

/// Open the broad feed, accept its REQ, and hand back its wire identity.
fn open_broad_feed(
    core: &mut EngineCore,
    transport: TransportRelayHandle,
    session: &RelaySessionKey,
    relay: &RelayUrl,
    since: Option<u64>,
    limit: Option<usize>,
) -> (ObservationId, SubId, String) {
    let opened = core.handle(EngineMsg::Subscribe(broad_contact_list_feed(
        relay, since, limit,
    )));
    let observation = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, _, _) => Some(*id),
            _ => None,
        })
        .expect("opening the feed emits its local frame");
    core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)));
    let sub_id = only_request(core, session);
    accept_every_pending_request(core, transport);
    let wire = wire_of(&sub_id);
    (observation, sub_id, wire)
}

#[test]
fn a_finished_feed_that_delivered_the_coordinate_needs_no_request() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-witnessed.example").unwrap();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let (mut core, transport, session) = connected_core(&relay);
    let (observation, _, wire) =
        open_broad_feed(&mut core, transport, &session, &relay, None, None);

    deliver(
        &mut core,
        transport,
        &session,
        &wire,
        contact_list(&alice, &[&bob], 10),
    );
    eose(&mut core, transport, &session, &wire);
    core.coordinate_reuse_new_reqs.set(0);

    let coverage = core.coordinate_coverage(&coordinate_of(&alice), &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 0);
    assert_eq!(coverage, CoordinateCoverage::Witnessed);

    core.handle(EngineMsg::Unsubscribe(observation));
}

#[test]
fn a_live_feed_that_already_delivered_the_coordinate_needs_no_request() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-live-witness.example").unwrap();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let (mut core, transport, session) = connected_core(&relay);
    let (observation, _, wire) =
        open_broad_feed(&mut core, transport, &session, &relay, None, None);

    // No EOSE: the feed is still streaming its stored events, and presence
    // does not wait on the end of the stream.
    deliver(
        &mut core,
        transport,
        &session,
        &wire,
        contact_list(&alice, &[&bob], 10),
    );
    core.coordinate_reuse_new_reqs.set(0);

    let coverage = core.coordinate_coverage(&coordinate_of(&alice), &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 0);
    assert_eq!(coverage, CoordinateCoverage::Witnessed);

    core.handle(EngineMsg::Unsubscribe(observation));
}

#[test]
fn the_batched_event_door_counts_and_witnesses_the_same_way() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-batched.example").unwrap();
    let alice = Keys::generate();
    let carol = Keys::generate();
    let dave = Keys::generate();
    let (mut core, transport, session) = connected_core(&relay);
    let (observation, _, wire) =
        open_broad_feed(&mut core, transport, &session, &relay, None, None);

    deliver_batch(
        &mut core,
        transport,
        &session,
        &wire,
        vec![
            contact_list(&carol, &[&dave], 10),
            contact_list(&alice, &[&dave], 10),
        ],
    );
    eose(&mut core, transport, &session, &wire);
    core.coordinate_reuse_new_reqs.set(0);

    let witnessed = core.coordinate_coverage(&coordinate_of(&alice), &session);
    let absent = core.coordinate_coverage(&coordinate_of(&Keys::generate()), &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 0);
    assert_eq!(witnessed, CoordinateCoverage::Witnessed);
    assert_eq!(absent, CoordinateCoverage::ProvenEmpty);

    core.handle(EngineMsg::Unsubscribe(observation));
}

#[test]
fn a_finished_feed_below_the_relay_bound_proves_the_coordinate_absent() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-empty.example").unwrap();
    let alice = Keys::generate();
    let carol = Keys::generate();
    let dave = Keys::generate();
    let (mut core, transport, session) = connected_core(&relay);
    let (observation, _, wire) =
        open_broad_feed(&mut core, transport, &session, &relay, None, None);

    // The relay answered with everything it had for kind:3, and Alice's
    // contact list was not among it.
    deliver(
        &mut core,
        transport,
        &session,
        &wire,
        contact_list(&carol, &[&dave], 10),
    );
    eose(&mut core, transport, &session, &wire);
    core.coordinate_reuse_new_reqs.set(0);

    let coverage = core.coordinate_coverage(&coordinate_of(&alice), &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 0);
    assert_eq!(coverage, CoordinateCoverage::ProvenEmpty);

    core.handle(EngineMsg::Unsubscribe(observation));
}

#[test]
fn a_second_caller_joins_the_outstanding_coordinate_request() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-join.example").unwrap();
    let alice = Keys::generate();
    let coordinate = coordinate_of(&alice);
    let (mut core, transport, session) = connected_core(&relay);

    let opened = core.handle(EngineMsg::Subscribe(coordinate_query(&relay, &coordinate)));
    let observation = opened
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, _, _) => Some(*id),
            _ => None,
        })
        .expect("opening the one-off emits its local frame");
    let admitted = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)));
    assert_eq!(wire_reqs(&admitted), 1, "the first caller places one REQ");
    accept_every_pending_request(&mut core, transport);
    core.coordinate_reuse_new_reqs.set(0);

    // Still streaming: a second caller for the same coordinate attaches to
    // the request already outstanding instead of opening its own.
    let coverage = core.coordinate_coverage(&coordinate, &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 0);
    assert_eq!(coverage, CoordinateCoverage::JoinsOutstandingRequest);

    let second = core.handle(EngineMsg::Subscribe(coordinate_query(&relay, &coordinate)));
    let second_observation = second
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, _, _) => Some(*id),
            _ => None,
        })
        .expect("the second caller opens an observation");
    let readmitted = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)));
    assert_eq!(
        wire_reqs(&readmitted),
        0,
        "the second caller must add no REQ of its own"
    );

    core.handle(EngineMsg::Unsubscribe(second_observation));
    core.handle(EngineMsg::Unsubscribe(observation));
}

#[test]
fn a_feed_that_returned_the_relay_bound_cannot_prove_the_coordinate_absent() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-truncated.example").unwrap();
    let alice = Keys::generate();
    let follow = Keys::generate();
    let (mut core, transport, session) = connected_core(&relay);
    let (observation, _, wire) =
        open_broad_feed(&mut core, transport, &session, &relay, None, None);

    // Exactly the fixed bound, from authors that are not Alice. The relay may
    // have held more and stopped here, so Alice's absence is unproven.
    for _ in 0..crate::core::coordinate_reuse::RELAY_RESULT_BOUND {
        let other = Keys::generate();
        deliver(
            &mut core,
            transport,
            &session,
            &wire,
            contact_list(&other, &[&follow], 10),
        );
    }
    eose(&mut core, transport, &session, &wire);
    core.coordinate_reuse_new_reqs.set(0);

    let coverage = core.coordinate_coverage(&coordinate_of(&alice), &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 1);
    assert_eq!(coverage, CoordinateCoverage::RequiresRequest);

    // And the ordinary REQ really is the fallback, on the wire.
    core.handle(EngineMsg::Subscribe(coordinate_query(
        &relay,
        &coordinate_of(&alice),
    )));
    let admitted = core.handle(EngineMsg::FlushWireAdmission(Timestamp::from(0u64)));
    assert_eq!(wire_reqs(&admitted), 1);

    core.handle(EngineMsg::Unsubscribe(observation));
}

#[test]
fn a_feed_bounded_by_its_own_limit_cannot_prove_the_coordinate_absent() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-limited.example").unwrap();
    let alice = Keys::generate();
    let carol = Keys::generate();
    let dave = Keys::generate();
    let (mut core, transport, session) = connected_core(&relay);
    let (observation, _, wire) =
        open_broad_feed(&mut core, transport, &session, &relay, None, Some(1));

    deliver(
        &mut core,
        transport,
        &session,
        &wire,
        contact_list(&carol, &[&dave], 10),
    );
    eose(&mut core, transport, &session, &wire);
    core.coordinate_reuse_new_reqs.set(0);

    let coverage = core.coordinate_coverage(&coordinate_of(&alice), &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 1);
    assert_eq!(
        coverage,
        CoordinateCoverage::RequiresRequest,
        "a request that returned its own limit may have had more to give"
    );

    core.handle(EngineMsg::Unsubscribe(observation));
}

#[test]
fn an_unattributable_frame_leaves_absence_unknown_on_the_whole_session() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-lost-count.example").unwrap();
    let alice = Keys::generate();
    let carol = Keys::generate();
    let dave = Keys::generate();
    let (mut core, transport, session) = connected_core(&relay);
    let (observation, _, wire) =
        open_broad_feed(&mut core, transport, &session, &relay, None, None);

    deliver(
        &mut core,
        transport,
        &session,
        &wire,
        contact_list(&carol, &[&dave], 10),
    );
    // A frame this engine cannot attribute to any request it owns. The exact
    // raw returned count for the feed is no longer available, so 500 returned
    // and 499 counted can never be mistaken for an uncapped result.
    deliver(
        &mut core,
        transport,
        &session,
        "unattributable",
        contact_list(&Keys::generate(), &[&dave], 10),
    );
    eose(&mut core, transport, &session, &wire);
    core.coordinate_reuse_new_reqs.set(0);

    let coverage = core.coordinate_coverage(&coordinate_of(&alice), &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 1);
    assert_eq!(coverage, CoordinateCoverage::RequiresRequest);

    core.handle(EngineMsg::Unsubscribe(observation));
}

#[test]
fn public_coverage_never_satisfies_the_same_protected_question() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-access.example").unwrap();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let (mut core, transport, session) = connected_core(&relay);
    let (observation, _, wire) =
        open_broad_feed(&mut core, transport, &session, &relay, None, None);

    deliver(
        &mut core,
        transport,
        &session,
        &wire,
        contact_list(&alice, &[&bob], 10),
    );
    eose(&mut core, transport, &session, &wire);
    core.coordinate_reuse_new_reqs.set(0);

    let protected = RelaySessionKey::new(
        relay.clone(),
        AccessContext::Nip42(Keys::generate().public_key()),
    );
    let coverage = core.coordinate_coverage(&coordinate_of(&alice), &protected);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 1);
    assert_eq!(
        coverage,
        CoordinateCoverage::RequiresRequest,
        "the same relay under a different access context is a different session"
    );
    let coverage = core.coordinate_coverage(&coordinate_of(&alice), &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 1);
    assert_eq!(
        coverage,
        CoordinateCoverage::Witnessed,
        "and the public question the public request answered is still answered"
    );

    core.handle(EngineMsg::Unsubscribe(observation));
}

#[test]
fn a_feed_with_a_tighter_window_cannot_answer_the_coordinate() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-window.example").unwrap();
    let alice = Keys::generate();
    let carol = Keys::generate();
    let dave = Keys::generate();
    let (mut core, transport, session) = connected_core(&relay);
    // The feed asked only for contact lists newer than 50. Alice's current
    // one may be older, so neither its presence nor its absence follows.
    let (observation, _, wire) =
        open_broad_feed(&mut core, transport, &session, &relay, Some(50), None);

    deliver(
        &mut core,
        transport,
        &session,
        &wire,
        contact_list(&carol, &[&dave], 60),
    );
    eose(&mut core, transport, &session, &wire);
    core.coordinate_reuse_new_reqs.set(0);

    let coverage = core.coordinate_coverage(&coordinate_of(&alice), &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 1);
    assert_eq!(coverage, CoordinateCoverage::RequiresRequest);

    core.handle(EngineMsg::Unsubscribe(observation));
}

#[test]
fn a_restarted_engine_repeats_the_check_with_nothing_remembered() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-restart.example").unwrap();
    let alice = Keys::generate();
    let bob = Keys::generate();
    // Everything a durable store could possibly carry after the run that
    // witnessed the coordinate: the row itself, observed from this relay.
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    store
        .insert(
            contact_list(&alice, &[&bob], 10),
            RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
        )
        .expect("fixture row inserts");
    let (core, _, session) = connected_core_with_store(&relay, store);

    let coverage = core.coordinate_coverage(&coordinate_of(&alice), &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 1);
    assert_eq!(
        coverage,
        CoordinateCoverage::RequiresRequest,
        "no durable row may stand in for a request that is no longer open"
    );
}

#[test]
fn a_returned_frame_ledger_never_outlives_the_request_that_owns_it() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-teardown.example").unwrap();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let (mut core, transport, session) = connected_core(&relay);
    let (observation, sub_id, wire) =
        open_broad_feed(&mut core, transport, &session, &relay, None, None);

    deliver(
        &mut core,
        transport,
        &session,
        &wire,
        contact_list(&alice, &[&bob], 10),
    );
    assert_eq!(core.bench_ownership_census().returned_frame_ledgers, 1);

    core.abandon_sub(&sub_id);
    assert_eq!(core.bench_ownership_census().returned_frame_ledgers, 0);
    assert_eq!(
        core.coordinate_coverage(&coordinate_of(&alice), &session),
        CoordinateCoverage::RequiresRequest
    );

    core.handle(EngineMsg::Unsubscribe(observation));
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn a_dropped_session_takes_every_ledger_it_owned_with_it() {
    let relay = RelayUrl::parse("wss://coordinate-reuse-disconnect.example").unwrap();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let (mut core, transport, session) = connected_core(&relay);
    let (observation, _, wire) =
        open_broad_feed(&mut core, transport, &session, &relay, None, None);

    deliver(
        &mut core,
        transport,
        &session,
        &wire,
        contact_list(&alice, &[&bob], 10),
    );
    eose(&mut core, transport, &session, &wire);
    assert_eq!(core.bench_ownership_census().returned_frame_ledgers, 1);

    core.handle(EngineMsg::RelayDisconnected(
        transport,
        session.clone(),
        DisconnectReason::Error,
    ));

    assert_eq!(core.bench_ownership_census().returned_frame_ledgers, 0);
    let coverage = core.coordinate_coverage(&coordinate_of(&alice), &session);
    assert_eq!(core.coordinate_reuse_new_reqs.get(), 1);
    assert_eq!(
        coverage,
        CoordinateCoverage::RequiresRequest,
        "a replayed request on a fresh generation has proven nothing yet"
    );

    core.handle(EngineMsg::Unsubscribe(observation));
}
