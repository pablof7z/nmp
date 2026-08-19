use super::*;

// ---- durable write delivery and recovery -------------------------------

/// An explicit route naming no relays is refused AT THE DOOR: "reject it
/// immediately". The CALL refuses -- nothing is taken into custody, so there
/// is no receipt and no fact at all, no sign request, no journal row, and
/// never a quiet degradation into `Auto`.
///
/// This is deliberately stricter than the route it replaces. That route
/// accepted an empty set and failed closed later, at resolution, so
/// emptiness was a sentence an app read off a receipt. The sentence now
/// lives where it can explain itself, and the empty route stops being
/// acceptable at all.
#[test]
fn an_explicit_route_with_no_relays_is_refused_before_acceptance() {
    let a = Keys::generate();
    // The directory is empty on purpose: there is no write relay anywhere
    // for a refusal-turned-fallback to leak into, so a passing assertion
    // here is about the door, not about luck.
    let dir = FixtureRoutingFacts::new();
    let mut core = new_core(dir);
    activate(&mut core, &a);

    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "nowhere")),
        routing: WriteRouting::Explicit(Vec::new()),
        identity: Identity::Active,
    }));

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::PublishFailed(PublishError::EmptyExplicitRoute)]
        ),
        "the refusal must be the first and only effect, and nothing may be \
         taken into custody: {effects:?}"
    );
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::RequestSign(..))),
        "no signer is asked for anything on a refused publish"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::PublishEvent(..) | Effect::EnsureWriteRelay(..))),
        "no relay is contacted, and no lane is opened, for a refused publish"
    );
}

/// The sibling of the refusal above, and the reason it is safe to be this
/// strict: emptiness is a property of the REQUEST, knowable at the door,
/// while reachability is a property of the world and is not. A write aimed
/// at a relay nobody can reach is accepted and routed verbatim to exactly
/// that relay -- it fails per relay afterwards, never at the door.
#[test]
fn an_unreachable_explicit_relay_is_accepted_because_the_door_cannot_know() {
    let a = Keys::generate();
    let nowhere = RelayUrl::parse("wss://non-existent.example").unwrap();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &a);

    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "hello")),
        routing: WriteRouting::Explicit(vec![nowhere.clone()]),
        identity: Identity::Active,
    }));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::WriteAccepted(..))),
        "acceptance cannot validate that a relay exists, so it does not try"
    );

    // Routing happened and named exactly the caller's relay: the lane the
    // engine opens is for that session and no other. Nothing reaches the
    // wire, because nothing can -- which is the point. Failing per relay is
    // what an unreachable target gets, not a refusal at the door.
    let (id, generation, u) = find_sign_request(&effects);
    let signed = u.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    let opened: Vec<RelaySessionKey> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::EnsureWriteRelay(session) => Some(session.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        opened,
        vec![signer_session(&nowhere, a.public_key())],
        "the write is routed verbatim to the relay the caller named"
    );
    assert!(
        !effects.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(_, WriteFact::Outcome(_)) | Effect::PublishFailed(_)
        )),
        "an unreachable relay is a per-relay outcome, never a whole-write terminal"
    );
}

/// Test 11 analog: `write_ack_per_relay`. A durable publish to two relays,
/// one OKs and one NACKs -- the receipt stream reaches `Acked(R_ok)` and
/// `Rejected(R_bad, reason)` independently; "is it sent?" is only readable
/// from the stream, never a single bool.
#[test]
fn one_attempt_start_failure_is_owned_nonterminal_and_never_hits_the_wire() {
    let author = Keys::generate();
    let good = RelayUrl::parse("wss://persisted.example").unwrap();
    let blocked = RelayUrl::parse("wss://blocked.example").unwrap();
    let store = RedbStore::temporary_with_failed_lane_starts([blocked.clone()])
        .expect("temporary Redb failure fixture");
    let mut core = EngineCore::new(store, 10);
    connect_signer(&mut core, 0, &good, author.public_key());
    connect_signer(&mut core, 1, &blocked, author.public_key());
    authenticate_signer(&mut core, 0, &good, &author);
    authenticate_signer(&mut core, 1, &blocked, &author);

    let (id, _signed, effects) =
        publish_explicit(&mut core, &author, [good.clone(), blocked.clone()]);
    assert!(effects.iter().any(
        |effect| matches!(effect, Effect::PublishEvent(session, event, _)
            if session == &signer_session(&good, event.pubkey))
    ));
    assert!(!effects.iter().any(
        |effect| matches!(effect, Effect::PublishEvent(session, event, _)
            if session == &signer_session(&blocked, event.pubkey))
    ));
    assert!(no_relay_fact_for(&receipt_statuses(&effects), &blocked));
    let replay = core.reattach_receipt(id);
    assert!(replay.is_attached());
    assert!(no_relay_fact_for(&replay.facts, &blocked));
}

// ---- issue #93: durable EVENT handoff -----------------------------------

/// `Sent` must never fire synchronously at enqueue time -- the moment this
/// call returns effects for a signed publish is not the same fact as
/// transport confirming the write. Only `EngineMsg::EventHandoff(_,
/// Written)` may ever produce it (asserted below by actually driving that
/// message and observing exactly one `Sent`).
#[test]
fn sent_never_fires_synchronously_and_only_written_handoff_produces_it() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
    let mut core = new_core(dir);
    connect_signer(&mut core, 0, &relay, author.public_key());
    authenticate_signer(&mut core, 0, &relay, &author);

    let (id, _signed, effects) = publish_explicit(&mut core, &author, [relay.clone()]);

    assert!(
        !effects.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(
                _,
                WriteFact::Relay {
                    state: RelayState::Sent { .. },
                    ..
                }
            )
        )),
        "Sent must never fire synchronously at enqueue time, got {effects:?}"
    );

    let correlation = effects
        .iter()
        .find_map(|e| match e {
            Effect::PublishEvent(r, event, c) if r == &signer_session(&relay, event.pubkey) => {
                Some(*c)
            }
            _ => None,
        })
        .expect("a PublishEvent effect must have been emitted for this relay");

    let reattached = core.reattach_receipt(id);
    assert!(reattached.is_attached());
    assert!(
        !reattached.facts.iter().any(|status| matches!(
            status,
            WriteFact::Relay {
                state: RelayState::Sent { .. },
                ..
            }
        )),
        "a persisted Started row is pre-wire and must not replay as Sent"
    );

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10)));
    let handoff_effects = core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    assert!(
        handoff_effects.iter().any(|e| matches!(
            e,
            Effect::EmitReceipt(
                receipt,
                WriteFact::Relay { relay: r, state: RelayState::Sent { attempt: 1, written_at }, .. }
            ) if *receipt == id && r == &relay && *written_at == Timestamp::from(10)
        )),
        "a Written handoff must emit exactly one Sent, got {handoff_effects:?}"
    );
    assert!(core
        .reattach_receipt(id)
        .facts
        .iter()
        .any(|s| matches!(s, WriteFact::Relay { relay: r, state: RelayState::Sent { .. }, .. } if r == &relay)));

    // The SAME correlation resolving a second time (a defensive duplicate
    // delivery, which transport itself never actually produces) must be a
    // complete no-op -- the correlation was already consumed above.
    let repeat = core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    assert!(
        repeat.is_empty(),
        "an already-resolved correlation must never re-fire Sent, got {repeat:?}"
    );
}

/// The exact handoff class is public receipt truth, and the two classes stay
/// distinct: `NotHandedOff` waits VISIBLY for the relay without claiming an
/// attempt is sent, while `Ambiguous` is not a fact about the write at all --
/// the lane waits for ack/timeout exactly as a proven write does, so it says
/// nothing. Neither is ever collapsed into `Sent`.
#[test]
fn not_handed_off_waits_visibly_and_ambiguous_says_nothing_and_neither_is_sent() {
    let author = Keys::generate();
    let relay_a = RelayUrl::parse("wss://relay-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://relay-b.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(author.public_key(), [relay_a.clone(), relay_b.clone()]);
    let mut core = new_core(dir);
    connect_signer(&mut core, 0, &relay_a, author.public_key());
    connect_signer(&mut core, 1, &relay_b, author.public_key());
    authenticate_signer(&mut core, 0, &relay_a, &author);
    authenticate_signer(&mut core, 1, &relay_b, &author);

    let (id, _signed, effects) =
        publish_explicit(&mut core, &author, [relay_a.clone(), relay_b.clone()]);
    let correlation_for = |relay: &RelayUrl| {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::PublishEvent(r, event, c) if r == &signer_session(relay, event.pubkey) => {
                    Some(*c)
                }
                _ => None,
            })
            .expect("a PublishEvent effect must have been emitted for this relay")
    };

    let not_handed_off = core.handle(EngineMsg::EventHandoff(
        correlation_for(&relay_a),
        HandoffResult::NotHandedOff,
    ));
    assert!(not_handed_off.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(
            receipt,
            WriteFact::Relay { relay, state: RelayState::Waiting(RelayWaiting::NotConnected), .. }
        ) if *receipt == id && relay == &relay_a
    )));
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10)));
    let ambiguous = core.handle(EngineMsg::EventHandoff(
        correlation_for(&relay_b),
        HandoffResult::Ambiguous,
    ));
    assert!(
        !ambiguous
            .iter()
            .any(|effect| matches!(effect, Effect::EmitReceipt(receipt, _) if *receipt == id)),
        "an ambiguous handoff proves nothing about the write, so it states nothing: {ambiguous:?}"
    );
    assert!(
        !not_handed_off
            .iter()
            .chain(&ambiguous)
            .any(|effect| matches!(
                effect,
                Effect::EmitReceipt(
                    _,
                    WriteFact::Relay {
                        state: RelayState::Sent { .. },
                        ..
                    }
                )
            )),
        "neither NotHandedOff nor Ambiguous may ever surface as Sent"
    );
}

/// An `EventHandoff` for a correlation this reducer never minted (unknown,
/// or belonging to a different process entirely) is a structural no-op --
/// never a panic, never a stray effect.
#[test]
fn event_handoff_for_an_unknown_correlation_is_inert() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
    let mut core = new_core(dir);
    let _ = publish_explicit(&mut core, &author, [relay]);

    let unknown = nmp_transport::AttemptCorrelation(u64::MAX);
    let effects = core.handle(EngineMsg::EventHandoff(unknown, HandoffResult::Written));
    assert!(effects.is_empty());
}

#[test]
fn all_attempt_start_failures_retain_every_lane_without_empty_terminal_sentinel() {
    let author = Keys::generate();
    let a = RelayUrl::parse("wss://blocked-a.example").unwrap();
    let b = RelayUrl::parse("wss://blocked-b.example").unwrap();
    let store = RedbStore::temporary_with_failed_lane_starts([a.clone(), b.clone()])
        .expect("temporary Redb failure fixture");
    let mut core = EngineCore::new(store, 10);
    connect_signer(&mut core, 0, &a, author.public_key());
    connect_signer(&mut core, 1, &b, author.public_key());
    authenticate_signer(&mut core, 0, &a, &author);
    authenticate_signer(&mut core, 1, &b, &author);

    let (id, _signed, effects) = publish_explicit(&mut core, &author, [a.clone(), b.clone()]);
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(..)))
            .count(),
        0
    );
    let statuses = receipt_statuses(&effects);
    assert!(no_relay_fact_for(&statuses, &a));
    assert!(no_relay_fact_for(&statuses, &b));
    let replay = core.reattach_receipt(id);
    assert!(replay.is_attached());
    let replayed = replay.facts;
    assert!(no_relay_fact_for(&replayed, &a));
    assert!(no_relay_fact_for(&replayed, &b));
}

#[test]
fn ack_of_persisted_lane_does_not_terminalize_mixed_blocked_obligation() {
    let author = Keys::generate();
    let good = RelayUrl::parse("wss://ack-persisted.example").unwrap();
    let blocked = RelayUrl::parse("wss://still-blocked.example").unwrap();
    let store = RedbStore::temporary_with_failed_lane_starts([blocked.clone()])
        .expect("temporary Redb failure fixture");
    let mut core = EngineCore::new(store, 10);
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&good, author.public_key()),
    ));
    connect_signer(&mut core, 1, &blocked, author.public_key());
    authenticate_signer(&mut core, 0, &good, &author);
    authenticate_signer(&mut core, 1, &blocked, &author);
    let (id, signed, scheduled) =
        publish_explicit(&mut core, &author, [good.clone(), blocked.clone()]);
    mark_written(&mut core, &scheduled, &good);
    let acked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&good, signed.pubkey),
        RelayFrame::from(RelayMessage::ok(signed.id, true, "")),
    ));
    assert!(acked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(receipt, WriteFact::Relay { relay, state: RelayState::Published, .. })
            if *receipt == id && relay == &good
    )));
    let replay = core.reattach_receipt(id);
    assert!(replay.is_attached());
    assert!(no_relay_fact_for(&replay.facts, &blocked));
}

#[test]
fn restart_rediscovers_unstarted_lane_and_persists_it_before_recovery_publish() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://recover-blocked.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("start-failure.redb");
    let (receipt, _event_id) = {
        let store = RedbStore::open_with_failed_lane_starts(&path, [relay.clone()])
            .expect("open Redb lane-start failure fixture");
        let mut first = EngineCore::new(store, 10);
        connect_signer(&mut first, 0, &relay, author.public_key());
        authenticate_signer(&mut first, 0, &relay, &author);
        let (id, signed, effects) = publish_explicit(&mut first, &author, [relay.clone()]);
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        (id, signed.id)
    };

    let store = RedbStore::open_with_failed_lane_starts(&path, [relay.clone()])
        .expect("reopen Redb lane-start failure fixture");
    let mut still_blocked = EngineCore::new(store, 10);
    assert!(still_blocked
        .recover_on_boot()
        .iter()
        .any(|effect| matches!(effect, Effect::EnsureWriteRelay(r)
            if r == &signer_session(&relay, author.public_key()))));
    connect_signer(&mut still_blocked, 0, &relay, author.public_key());
    authenticate_signer(&mut still_blocked, 0, &relay, &author);
    let replay = still_blocked.reattach_receipt(receipt);
    assert!(replay.is_attached());
    assert!(no_relay_fact_for(&replay.facts, &relay));
    drop(still_blocked);

    let mut recovered = EngineCore::new(
        RedbStore::open(&path).expect("reopen healthy Redb store"),
        10,
    );
    let boot = recovered.recover_on_boot();
    assert!(boot
        .iter()
        .any(|effect| matches!(effect, Effect::EnsureWriteRelay(r)
            if r == &signer_session(&relay, author.public_key()))));
    connect_signer(&mut recovered, 0, &relay, author.public_key());
    let effects = release_author_probe(
        &mut recovered,
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        &relay,
        author.public_key(),
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
                if r == &signer_session(&relay, event.pubkey)))
            .count(),
        1
    );
    drop(recovered);
    let store = RedbStore::open(&path).expect("inspect recovered redb");
    let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
    let attempts = store.recover_attempts(intent).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].relay, relay);
    assert_eq!(attempts[0].outcome, PublishQueueAttemptOutcome::Started);
}

/// I1: `pending` must stay exactly mirrored by `intent_receipts` and
/// `event_to_receipts`. `remember_pending_indexes` (#1725) is the insertion
/// half of that mirror, called both at ordinary acceptance and -- this
/// test's own case -- at every `recover_on_boot` intent. Two co-owner
/// obligations accept the exact same bytes before a restart (the same
/// mechanism `duplicate_coowners_keep_independent_routes_and_terminal_receipts`
/// proves in-process); after the restart, `event_to_receipts` has ONLY the
/// boot-recovery insertion to rebuild it from -- `on_signed`'s own insertion
/// never runs again, because its guard (`pending.event_id.is_some()`) skips
/// an obligation that recovery already loaded as signed. A shared event's
/// ack must still fan out to both co-owners, not just whichever one owns the
/// lane the ack physically arrived on.
///
/// nmp:falsifier=An ack for an event two co-owners share reaches both
/// receipts after a restart, exactly as it does in-process.
#[test]
fn a_shared_events_ack_reaches_every_co_owner_after_a_restart() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://co-owner-restart.example").unwrap();
    let other = RelayUrl::parse("wss://co-owners-other-lane.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("co-owner-restart.redb");
    let template = draft(1, "same bytes, two obligations, survives a restart");

    let (id_a, id_b, event_id) = {
        let store = RedbStore::open(&path).expect("open fresh Redb store");
        let mut core = EngineCore::new(store, 10);
        activate(&mut core, &author);
        connect_signer(&mut core, 0, &relay, author.public_key());
        authenticate_signer(&mut core, 0, &relay, &author);

        let first = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(template.clone()),
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
        }));
        let (id_a, generation_a, to_sign) = find_sign_request(&first);
        let second = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(template.clone()),
            routing: WriteRouting::Explicit(vec![other.clone()]),
            identity: Identity::Active,
        }));
        let (id_b, _, _) = find_sign_request(&second);
        assert_ne!(
            id_a, id_b,
            "fixture sanity: two distinct obligations accept the same bytes"
        );

        let signed = to_sign.sign_with_keys(&author).expect("sign fixture event");
        let routed = core.handle(EngineMsg::SignerCompleted(
            id_a,
            generation_a,
            Ok(signed.clone()),
        ));
        // Fixture sanity, pre-restart: the store's co-owner promotion must
        // have actually advanced BOTH obligations from this one signer
        // completion, or nothing below is testing what this test claims to.
        assert!(
            routed.iter().any(|effect| matches!(
                effect,
                Effect::EmitReceipt(id, WriteFact::Signing(SigningState::Signed { event_id }))
                    if *id == id_a && *event_id == signed.id
            )),
            "fixture sanity: co-owner A must be signed before the restart, got {routed:?}"
        );
        assert!(
            routed.iter().any(|effect| matches!(
                effect,
                Effect::EmitReceipt(id, WriteFact::Signing(SigningState::Signed { event_id }))
                    if *id == id_b && *event_id == signed.id
            )),
            "fixture sanity: co-owner B must be signed by the SAME completion before the \
             restart, got {routed:?}"
        );
        mark_written(&mut core, &routed, &relay);
        (id_a, id_b, signed.id)
    };

    // The restart: a fresh process reopens the same durable store. Neither
    // obligation is re-signed here -- `on_signed` never runs again for
    // either one, so `event_to_receipts` has only `recover_on_boot`'s
    // insertion to rebuild it from.
    let store = RedbStore::open(&path).expect("reopen Redb store after restart");
    let mut recovered = EngineCore::new(store, 10);
    recovered.recover_on_boot();
    connect_signer(&mut recovered, 0, &relay, author.public_key());
    // The restart re-dispatches: neither the wire correlation nor the
    // in-flight lane state is durable, only the fact that the attempt
    // exists. The reconnect + AUTH readiness re-arms exactly one fresh
    // attempt for this relay, which must be marked written before an ack
    // for it can correlate.
    let authd = authenticate_signer(&mut recovered, 0, &relay, &author);
    mark_written(&mut recovered, &authd, &relay);

    // Fixture sanity, post-restart and BEFORE the ack: both co-owners must
    // have survived recovery as live, signed obligations on the shared
    // event. Asserting this first is what makes a red assertion below
    // evidence of I1's fan-out specifically -- a generation that never
    // reformed and a fan-out that silently degraded to one receipt would
    // otherwise both fail the same way.
    assert!(
        recovered.reattach_receipt(id_a).is_attached(),
        "co-owner A must survive the restart"
    );
    assert!(
        recovered.reattach_receipt(id_b).is_attached(),
        "co-owner B must survive the restart"
    );

    let acked = recovered.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, author.public_key()),
        RelayFrame::from(RelayMessage::ok(event_id, true, "")),
    ));
    assert!(
        acked.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                id,
                WriteFact::Relay { event_id: e, relay: r, state: RelayState::Published }
            ) if *id == id_a && *e == event_id && r == &relay
        )),
        "I1: co-owner A's ack must fan out after a restart, got {acked:?}"
    );
    assert!(
        acked.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                id,
                WriteFact::Relay { event_id: e, relay: r, state: RelayState::Published }
            ) if *id == id_b && *e == event_id && r == &relay
        )),
        "I1: co-owner B's ack must fan out after a restart even though B's own routing \
         never named this relay -- exactly as it does without a restart -- got {acked:?}"
    );
}

#[test]
fn author_outbox_failed_attempt_survives_restart_with_empty_directory() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://durable-author-route.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("author-route.redb");
    let receipt = {
        let directory =
            FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbStore::open_with_failed_lane_starts(&path, [relay.clone()])
                .expect("open Redb lane-start failure fixture"),
            directory,
            10,
        );
        connect_signer(&mut core, 0, &relay, author.public_key());
        authenticate_signer(&mut core, 0, &relay, &author);
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(86, "dynamic author route")),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
        }));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let _event_id = signed.id;
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        assert!(receipt_statuses(&effects)
            .iter()
            .all(|fact| !matches!(fact, WriteFact::Relay { relay: r, .. } if r == &relay)));
        id
    };

    {
        let store = RedbStore::open(&path).unwrap();
        let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
        let revisions = store.recover_route_revisions(intent).unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].relays, BTreeSet::from([relay.clone()]));
        assert!(store.recover_attempts(intent).unwrap().is_empty());
    }

    let mut recovered = EngineCore::new(
        RedbStore::open(&path).expect("reopen healthy Redb store"),
        10,
    );
    recovered.recover_on_boot();
    connect_signer(&mut recovered, 0, &relay, author.public_key());
    let effects = release_author_probe(
        &mut recovered,
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        &relay,
        author.public_key(),
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
                if r == &signer_session(&relay, event.pubkey)))
            .count(),
        1
    );
    assert!(recovered.reattach_receipt(receipt).is_attached());
}

#[test]
fn accepted_explicit_route_ignores_later_directory_fact_across_restart() {
    let author = Keys::generate();
    let chosen = RelayUrl::parse("wss://chosen-archive.example").unwrap();
    let learned = RelayUrl::parse("wss://later-outbox.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("explicit-route.redb");

    let receipt = {
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbStore::open(&path).unwrap(),
            FixtureRoutingFacts::new(),
            10,
        );
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(94, "for the archive")),
            routing: WriteRouting::Explicit(vec![chosen.clone()]),
            identity: Identity::Active,
        }));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        let ensured = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::EnsureWriteRelay(session) => Some(session.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            ensured,
            BTreeSet::from([signer_session(&chosen, author.public_key())]),
            "offline acceptance must mint exactly the explicit destination"
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::PublishEvent(..))),
            "the chosen destination is still offline"
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(
                receipt,
                WriteFact::Destinations {
                    relays,
                    complete: true,
                    awaiting_author_routes,
                }
            ) if *receipt == id
                && relays == &BTreeSet::from([chosen.clone()])
                && awaiting_author_routes.is_empty()
        )));
        id
    };

    {
        let store = RedbStore::open(&path).unwrap();
        let intents = store
            .recover_publish_queue()
            .expect("recover explicit write");
        assert_eq!(intents.len(), 1, "one publish owns one durable intent");
        let durable = store
            .recover_route_revisions(intents[0].intent_id)
            .unwrap()
            .into_iter()
            .flat_map(|revision| revision.relays)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            durable,
            BTreeSet::from([chosen.clone()]),
            "only the app-named destination may survive acceptance"
        );
    }

    // This is learned only after acceptance. An Explicit strategy has no
    // author-directory input, so recovery must not append it to the durable
    // route or ask the transport to open it.
    let changed =
        FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [learned.clone()]);
    let mut recovered =
        EngineCore::new_with_fixture_routing_facts(RedbStore::open(&path).unwrap(), changed, 10);
    let recovery = recovered.recover_on_boot();
    let ensured = recovery
        .iter()
        .filter_map(|effect| match effect {
            Effect::EnsureWriteRelay(session) => Some(session.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        ensured,
        BTreeSet::from([signer_session(&chosen, author.public_key())]),
        "recovery must retain the explicit route verbatim"
    );
    assert!(
        !recovery.iter().any(|effect| matches!(
            effect,
            Effect::EnsureWriteRelay(session) if session.relay == learned
        )),
        "a later author-directory fact must not become a destination"
    );

    let connected = connect_signer(&mut recovered, 0, &chosen, author.public_key());
    assert!(!connected.iter().any(|effect| matches!(
        effect,
        Effect::EnsureWriteRelay(session) | Effect::PublishEvent(session, ..)
            if session.relay == learned
    )));
    let effects = release_author_probe(
        &mut recovered,
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        &chosen,
        author.public_key(),
    );
    let event = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PublishEvent(session, event, _)
                if session == &signer_session(&chosen, event.pubkey) =>
            {
                Some(event.clone())
            }
            _ => None,
        })
        .expect("the recovered write publishes to its explicit destination");
    assert!(
        !effects.iter().any(|effect| matches!(
            effect,
            Effect::EnsureWriteRelay(session) | Effect::PublishEvent(session, ..)
                if session.relay == learned
        )),
        "delivery must never widen to the later author outbox"
    );

    mark_written(&mut recovered, &effects, &chosen);
    let acked = recovered.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&chosen, event.pubkey),
        RelayFrame::from(RelayMessage::ok(event.id, true, "")),
    ));
    assert!(acked.iter().any(
        |effect| matches!(effect, Effect::EmitReceipt(id, WriteFact::Relay { relay, state: RelayState::Published, .. })
            if *id == receipt && relay == &chosen)
    ));

    let replay = recovered.reattach_receipt(receipt);
    assert!(replay.is_attached());
    assert!(
        !replay.facts.iter().any(|status| match status {
            WriteFact::Destinations { relays, .. } => relays.contains(&learned),
            WriteFact::Relay { relay, .. } => relay == &learned,
            WriteFact::Signing(_) | WriteFact::Outcome(_) => false,
        }),
        "no durable or receipt fact may name the later directory relay"
    );
}

/// Fail-closed survived the removal of the route that used to spell it, and
/// the privacy WORDING did not survive with it.
///
/// The directory already names both of this author's write relays when the
/// publish arrives, and both are connected and authenticated -- so they are
/// demonstrably live destinations the engine could reach in one step. An
/// exact route that consulted the directory, or widened to it under any
/// pressure, would therefore have somewhere to widen TO: the one-destination
/// witness below is about the route being verbatim, not about the
/// alternatives being unreachable.
///
/// The second half is vocabulary. An exact route is a routing property, and
/// the relay an app names is routinely public -- a group host, an archive.
/// A fact that described this write as private would be lying about what the
/// app asked for, so no fact this publish emits may say the word.
#[test]
fn an_explicit_route_over_a_live_directory_is_verbatim_and_claims_no_privacy() {
    let author = Keys::generate();
    let chosen = RelayUrl::parse("wss://chosen-relay.example").unwrap();
    let known_a = RelayUrl::parse("wss://outbox-a.example").unwrap();
    let known_b = RelayUrl::parse("wss://outbox-b.example").unwrap();

    let mut core = new_core(
        FixtureRoutingFacts::new()
            .with_outbound_routes(author.public_key(), [known_a.clone(), known_b.clone()]),
    );
    activate(&mut core, &author);
    for (slot, relay) in [(0, &chosen), (1, &known_a), (2, &known_b)] {
        connect_signer(&mut core, slot, relay, author.public_key());
        authenticate_signer(&mut core, slot, relay, &author);
    }

    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(61, "narrow")),
        routing: WriteRouting::Explicit(vec![chosen.clone()]),
        identity: Identity::Active,
    }));
    let (receipt, generation, unsigned) = find_sign_request(&accepted);
    let signed = unsigned.sign_with_keys(&author).unwrap();
    let completed = core.handle(EngineMsg::SignerCompleted(
        receipt,
        generation,
        Ok(signed.clone()),
    ));

    let published: BTreeSet<RelayUrl> = accepted
        .iter()
        .chain(completed.iter())
        .filter_map(|effect| match effect {
            Effect::PublishEvent(session, event, _) if event.id == signed.id => {
                Some(session.relay.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        published,
        BTreeSet::from([chosen.clone()]),
        "an exact route executes verbatim: the two live relays the directory \
         knows are never offered the event"
    );
    let ensured: BTreeSet<RelayUrl> = accepted
        .iter()
        .chain(completed.iter())
        .filter_map(|effect| match effect {
            Effect::EnsureWriteRelay(session) => Some(session.relay.clone()),
            _ => None,
        })
        .collect();
    assert!(
        ensured.iter().all(|relay| relay == &chosen),
        "no lane may be opened outside the named relay: {ensured:?}"
    );
    assert!(
        accepted
            .iter()
            .chain(completed.iter())
            .any(|effect| matches!(
                effect,
                Effect::EmitReceipt(
                    id,
                    WriteFact::Destinations {
                        relays,
                        complete: true,
                        awaiting_author_routes,
                    }
                ) if *id == receipt
                    && relays == &BTreeSet::from([chosen.clone()])
                    && awaiting_author_routes.is_empty()
            )),
        "the receipt names exactly the app's destination and nothing it \
         could have discovered: {completed:?}"
    );

    mark_written(&mut core, &completed, &chosen);
    let acked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&chosen, signed.pubkey),
        RelayFrame::from(RelayMessage::ok(signed.id, true, "")),
    ));
    assert!(
        acked.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(id, WriteFact::Relay { relay, state: RelayState::Published, .. })
                if *id == receipt && relay == &chosen
        )),
        "the named relay is where the note actually lands: {acked:?}"
    );

    let facts = core.reattach_receipt(receipt).facts;
    assert!(
        !facts.iter().any(|fact| match fact {
            WriteFact::Destinations { relays, .. } =>
                relays.contains(&known_a) || relays.contains(&known_b),
            WriteFact::Relay { relay, .. } => relay == &known_a || relay == &known_b,
            WriteFact::Signing(_) | WriteFact::Outcome(_) => false,
        }),
        "no durable or replayed fact may name a relay the app did not: {facts:?}"
    );
    let privacy_claims: Vec<String> = facts
        .iter()
        .map(|fact| format!("{fact:?}"))
        .filter(|rendered| rendered.to_lowercase().contains("private"))
        .collect();
    assert!(
        privacy_claims.is_empty(),
        "an exact route is not a privacy claim, but a fact said: \
         {privacy_claims:?}"
    );
}

#[test]
fn author_route_removal_cannot_erase_durable_lane_and_new_revision_failure_is_volatile() {
    let author = Keys::generate();
    let old = RelayUrl::parse("wss://old-outbox.example").unwrap();
    let new = RelayUrl::parse("wss://new-outbox.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("author-route.redb");
    let receipt = {
        let directory =
            FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [old.clone()]);
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbStore::open_with_failed_lane_starts(&path, [old.clone()])
                .expect("open Redb lane-start failure fixture"),
            directory,
            10,
        );
        connect_signer(&mut core, 0, &old, author.public_key());
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(87, "dynamic author route")),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
        }));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        id
    };

    // Directory removal/replacement cannot subtract `old`. Failure to append
    // the newly resolved `new` revision blocks only that volatile lane; the
    // already-durable old obligation may still start and publish.
    {
        let changed =
            FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [new.clone()]);
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbStore::open_with_route_revision_write_failure(&path)
                .expect("open Redb route-revision failure fixture"),
            changed,
            10,
        );
        core.recover_on_boot();
        connect_signer(&mut core, 0, &old, author.public_key());
        let effects = release_author_probe(
            &mut core,
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            &old,
            author.public_key(),
        );
        let old_event = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::PublishEvent(session, event, _)
                    if session == &signer_session(&old, event.pubkey) =>
                {
                    Some(event.clone())
                }
                _ => None,
            })
            .expect("durable old lane publishes");
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
                if r == &signer_session(&old, event.pubkey))));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
                if r == &signer_session(&new, event.pubkey))));
        mark_written(&mut core, &effects, &old);
        let acked = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            signer_session(&old, old_event.pubkey),
            RelayFrame::from(RelayMessage::ok(old_event.id, true, "")),
        ));
        assert!(acked.iter().any(|effect| matches!(
            effect,
            Effect::EmitReceipt(_, WriteFact::Relay { relay: r, state: RelayState::Published, .. }) if r == &old
        )));
        let replay = core.reattach_receipt(receipt);
        assert!(replay.is_attached());
        assert!(no_relay_fact_for(&replay.facts, &new));
    }

    {
        let store = RedbStore::open(&path).unwrap();
        let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
        let durable = store
            .recover_route_revisions(intent)
            .unwrap()
            .into_iter()
            .flat_map(|revision| revision.relays)
            .collect::<BTreeSet<_>>();
        assert_eq!(durable, BTreeSet::from([old.clone()]));
    }

    // Once a later boot can persist the changed revision, `new` starts. The
    // old lane is retained in route history but is already terminal (Acked),
    // so it is correctly not published again.
    let changed =
        FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [new.clone()]);
    let mut core = EngineCore::new_with_fixture_routing_facts(
        RedbStore::open(&path).expect("reopen healthy Redb store"),
        changed,
        10,
    );
    core.recover_on_boot();
    connect_signer(&mut core, 0, &new, author.public_key());
    let effects = release_author_probe(
        &mut core,
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        &new,
        author.public_key(),
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
            if r == &signer_session(&old, event.pubkey))));
    assert!(effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(r, event, _)
            if r == &signer_session(&new, event.pubkey))));
}

#[test]
fn route_revision_failure_emits_no_attempt_or_wire_and_claims_no_crash_durable_url() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://volatile-route.example").unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("route-failure.redb");
    {
        let directory =
            FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [relay.clone()]);
        let mut core = EngineCore::new_with_fixture_routing_facts(
            RedbStore::open_with_route_revision_write_failure(&path)
                .expect("open Redb route-revision failure fixture"),
            directory,
            10,
        );
        activate(&mut core, &author);
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(88, "volatile route")),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
        }));
        let (id, generation, unsigned) = find_sign_request(&accepted);
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let _event_id = signed.id;
        let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        assert!(receipt_statuses(&effects)
            .iter()
            .all(|fact| !matches!(fact, WriteFact::Relay { relay: r, .. } if r == &relay)));
        assert!(
            !receipt_statuses(&effects)
                .iter()
                .any(|fact| matches!(fact, WriteFact::Outcome(WriteOutcome::NoDestination))),
            "a named relay whose route revision failed to persist is stalled, not absent"
        );
    }
    let store = RedbStore::open(&path).unwrap();
    let intent = store.recover_publish_queue().expect("recover delivery")[0].intent_id;
    assert!(store.recover_route_revisions(intent).unwrap().is_empty());
    assert!(store.recover_attempts(intent).unwrap().is_empty());
    drop(store);

    let mut recovered = EngineCore::new(
        RedbStore::open(&path).expect("reopen healthy Redb store"),
        10,
    );
    let effects = recovered.recover_on_boot();
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::AuthorRouteNeedsChanged(needs)]
                if needs == &BTreeSet::from([author.public_key()])
        ),
        "the failed volatile route must produce exactly its one provider-need effect: {effects:?}"
    );
    assert!(
        !effects.iter().any(|effect| matches!(
            effect,
            Effect::PublishEvent(..) | Effect::EnsureWriteRelay(..)
        )),
        "redeclaring discovery need cannot claim a crash-durable route, attempt, or wire send"
    );
}

#[test]
fn write_ack_per_relay() {
    let a = Keys::generate();
    let relay_ok = RelayUrl::parse("wss://relay-ok.example.com").unwrap();
    let relay_bad = RelayUrl::parse("wss://relay-bad.example.com").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [relay_ok.clone(), relay_bad.clone()]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    connect_signer(&mut core, 0, &relay_ok, a.public_key());
    connect_signer(&mut core, 1, &relay_bad, a.public_key());
    authenticate_signer(&mut core, 0, &relay_ok, &a);
    authenticate_signer(&mut core, 1, &relay_bad, &a);

    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "durable ack test")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
    }));
    let (id, generation, u) = find_sign_request(&effects);
    let signed = u.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Ok(signed.clone()),
    ));
    assert_eq!(
        effects
            .iter()
            .filter(|e| matches!(e, Effect::PublishEvent(..)))
            .count(),
        2,
        "a durable Auto-routed write reaches both of the author's write relays"
    );
    mark_written(&mut core, &effects, &relay_ok);
    mark_written(&mut core, &effects, &relay_bad);

    let ok_frame = RelayFrame::from(RelayMessage::ok(signed.id, true, ""));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay_ok, signed.pubkey),
        ok_frame,
    ));
    assert!(effects.iter().any(
        |e| matches!(e, Effect::EmitReceipt(rid, WriteFact::Relay { relay: r, state: RelayState::Published, .. }) if *rid == id && r == &relay_ok)
    ));

    let nack_frame = RelayFrame::from(RelayMessage::ok(signed.id, false, "blocked: spam"));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        signer_session(&relay_bad, signed.pubkey),
        nack_frame,
    ));
    assert!(effects.iter().any(
        |e| matches!(e, Effect::EmitReceipt(rid, WriteFact::Relay { relay: r, state: RelayState::Rejected { reason: msg }, .. }) if *rid == id && r == &relay_bad && msg.contains("blocked"))
    ));

    let statuses = core.reattach_receipt(id).facts;
    assert!(statuses
        .iter()
        .any(|s| matches!(s, WriteFact::Relay { relay: r, state: RelayState::Published, .. } if r == &relay_ok)));
    assert!(statuses
        .iter()
        .any(|s| matches!(s, WriteFact::Relay { relay: r, state: RelayState::Rejected { reason: _ }, .. } if r == &relay_bad)));
}

/// A relay named by two outbox sources is ONE destination, not two.
///
/// Bob reads from a relay the author also writes to, which is extremely
/// ordinary. The `(intent_id, relay)` lane key must make the recipient's
/// inbox collide with the lane the author's own outbox already minted, so
/// the event is offered to that host once — not published twice to the same
/// relay because two sources happened to name it.
#[test]
fn a_relay_two_outbox_sources_name_is_offered_the_event_exactly_once() {
    let author = Keys::generate();
    let bob = Keys::generate();
    let shared = RelayUrl::parse("wss://shared.example").unwrap();
    let author_only = RelayUrl::parse("wss://author-only.example").unwrap();
    // Bob's inbox is not a subset of the author's outbox, so a resolver that
    // ignored p-tagged recipients entirely would fail this test rather than
    // pass it by coincidence.
    let bob_only = RelayUrl::parse("wss://bob-only.example").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(author.public_key(), [shared.clone(), author_only.clone()])
        .with_inbound_routes(bob.public_key(), [shared.clone(), bob_only.clone()]);
    let mut core = new_core(dir);
    activate(&mut core, &author);

    // Nothing is connected yet, so every obligation this route mints announces
    // itself as one `EnsureWriteRelay` -- the countable witness that the
    // shared host became ONE delivery obligation rather than one per source.
    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(
            draft(11, "we share a relay").tag(nostr::Tag::public_key(bob.public_key())),
        ),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
    }));
    let (id, generation, unsigned) = find_sign_request(&accepted);
    let signed = unsigned.sign_with_keys(&author).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Ok(signed.clone()),
    ));

    let shared_session = signer_session(&shared, signed.pubkey);
    assert_eq!(
        effects
            .iter()
            .filter(
                |effect| matches!(effect, Effect::EnsureWriteRelay(session) if session == &shared_session)
            )
            .count(),
        1,
        "a host two sources name must cost ONE obligation, not one per source: {effects:?}"
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, Effect::EnsureWriteRelay(_)))
            .count(),
        3,
        "the union of the author's two write relays with Bob's two-relay inbox is three \
         obligations, and the shared host is one of them: {effects:?}"
    );

    // The same claim at the wire: connected, the note is offered to the shared
    // host once, not once per source that named it.
    let mut delivery: Vec<Effect> = Vec::new();
    delivery.extend(connect_signer(&mut core, 0, &shared, author.public_key()));
    delivery.extend(connect_signer(
        &mut core,
        1,
        &author_only,
        author.public_key(),
    ));
    delivery.extend(connect_signer(&mut core, 2, &bob_only, author.public_key()));
    delivery.extend(authenticate_signer(&mut core, 0, &shared, &author));
    delivery.extend(authenticate_signer(&mut core, 1, &author_only, &author));
    delivery.extend(authenticate_signer(&mut core, 2, &bob_only, &author));
    let offered_to_shared = |seen: &[Effect]| {
        seen.iter()
            .filter(
                |effect| matches!(effect, Effect::PublishEvent(session, ..) if session == &shared_session)
            )
            .count()
    };
    assert_eq!(
        offered_to_shared(&effects) + offered_to_shared(&delivery),
        1,
        "the shared host is offered the note exactly once across the whole delivery: {delivery:?}"
    );

    let destinations = effects
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::EmitReceipt(receipt, WriteFact::Destinations { relays, .. })
                if *receipt == id =>
            {
                Some(relays.clone())
            }
            _ => None,
        })
        .expect("an accepted Auto write reports its destinations");
    assert_eq!(
        destinations,
        BTreeSet::from([shared.clone(), author_only.clone(), bob_only.clone()]),
        "the route is a set union, never a concatenation: {effects:?}"
    );
}

#[test]
fn uncommitted_attempt_terminal_emits_no_receipt_and_keeps_lane_live() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://finish-failure.example").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay.clone()]);
    let mut core = EngineCore::new_with_fixture_routing_facts(
        RedbStore::temporary_with_failed_lane_attempt_finish()
            .expect("temporary Redb lane-finish failure fixture"),
        dir,
        10,
    );
    activate(&mut core, &a);
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, a.public_key()),
    ));
    authenticate_signer(&mut core, 0, &relay, &a);
    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(2, "finish persistence")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
    }));
    let (id, generation, unsigned) = find_sign_request(&effects);
    let signed = unsigned.sign_with_keys(&a).unwrap();
    let scheduled = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Ok(signed.clone()),
    ));
    mark_written(&mut core, &scheduled, &relay);
    let frame = || RelayFrame::from(RelayMessage::ok(signed.id, true, ""));
    let failed = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, signed.pubkey),
        frame(),
    ));
    assert!(!failed.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(
            _,
            WriteFact::Relay {
                relay: _,
                state: RelayState::Published,
                ..
            }
        )
    )));
    let retried = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        signer_session(&relay, signed.pubkey),
        frame(),
    ));
    assert!(retried.iter().any(
        |effect| matches!(effect, Effect::EmitReceipt(receipt, WriteFact::Relay { relay: r, state: RelayState::Published, .. }) if *receipt == id && r == &relay)
    ));
}

/// A refused publish never reaches a receipt at all: the refusal is the
/// return value, so there is no id to be distinct, no fact to carry it and
/// nothing for a later store-issued receipt to collide with.
#[test]
fn a_refused_publish_mints_no_receipt_and_no_fact() {
    let mut core = new_core(FixtureRoutingFacts::new());
    let refuse = |core: &mut EngineCore, seq| {
        core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(seq, "unaccepted")),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
        }))
    };
    for seq in [200, 201] {
        let effects = refuse(&mut core, seq);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::PublishFailed(PublishError::NoCurrentAccount)]
            ),
            "the refusal is the whole answer: {effects:?}"
        );
    }
}

/// Read one receipt's projected entry out of the enumeration door, which is
/// the surface an app actually reads.
fn queue_entry(core: &EngineCore, id: ReceiptId) -> PublishQueueEntry {
    core.publish_queue_entries(None, u8::MAX)
        .expect("enumerate publish queue")
        .into_iter()
        .find(|entry| entry.receipt_id == id)
        .unwrap_or_else(|| panic!("receipt {id:?} is missing from the publish queue"))
}

fn state_at(states: &[(RelayUrl, RelayState)], relay: &RelayUrl) -> RelayState {
    states
        .iter()
        .find(|(url, _)| url == relay)
        .map(|(_, state)| state.clone())
        .unwrap_or_else(|| panic!("no projected state for {relay}: {states:?}"))
}

/// A lane that is queued, or that has an attempt on the wire, is not a relay
/// an app should tell a person it cannot reach.
///
/// Three durable lane states used to collapse into the single sentence "the
/// connection is unavailable", and for every one of them that sentence was
/// false:
///
/// - `Eligible` with a live session: routed and scheduled, waiting only for
///   the relay's one attempt slot. Nothing is wrong with the connection.
/// - `InFlight`/`AwaitingHandoff`: an attempt ordinal is SPENT and the bytes
///   are with transport. There is still no proof they reached the socket, so
///   it is not `Sent` either -- but it is emphatically not "not connected".
/// - `InFlight`/`AwaitingAck` after a `Written` handoff: transport PROVED
///   socket write and flush, and the relay simply has not answered yet.
///
/// The one case that legitimately reads as not-connected is kept: an
/// `Eligible` lane whose session is gone stays durably `Eligible` on purpose
/// (`schedule_ready` will not spend an fsync per pass to say so), and the
/// projection asks `connected_relays` the same question the scheduler does.
#[test]
fn a_queued_or_in_flight_lane_is_never_reported_as_a_missing_connection() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://one-slot.example").unwrap();
    let dir = FixtureRoutingFacts::new().with_outbound_routes(a.public_key(), [relay.clone()]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    connect_signer(&mut core, 0, &relay, a.public_key());
    authenticate_signer(&mut core, 0, &relay, &a);

    // First write: the relay's one attempt slot is taken, so this lane is in
    // flight with transport holding the bytes.
    let first = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "first in the slot")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
    }));
    let (first_id, generation, unsigned) = find_sign_request(&first);
    let signed = unsigned.sign_with_keys(&a).unwrap();
    let first_effects = core.handle(EngineMsg::SignerCompleted(
        first_id,
        generation,
        Ok(signed.clone()),
    ));

    let state = state_at(&queue_entry(&core, first_id).relay_states, &relay);
    assert!(
        matches!(state, RelayState::Attempting { attempt: 1, .. }),
        "an attempt whose bytes are with transport is attempting -- not \
         unreachable, and not yet proved sent: {state:?}"
    );

    // Second write to the SAME relay: the slot is busy, so this lane stays
    // durably `Eligible` -- over a perfectly live, authenticated session.
    let second = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(2, "waiting for the slot")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
    }));
    let (second_id, generation, unsigned) = find_sign_request(&second);
    core.handle(EngineMsg::SignerCompleted(
        second_id,
        generation,
        Ok(unsigned.sign_with_keys(&a).unwrap()),
    ));

    let state = state_at(&queue_entry(&core, second_id).relay_states, &relay);
    assert!(
        matches!(state, RelayState::Waiting(RelayWaiting::Eligible { .. })),
        "a queued lane on a connected relay is eligible, not disconnected: \
         {state:?}"
    );

    // Transport proves the bytes reached the socket. Now, and only now, the
    // first lane may claim `Sent`.
    mark_written(&mut core, &first_effects, &relay);
    let state = state_at(&queue_entry(&core, first_id).relay_states, &relay);
    assert!(
        matches!(state, RelayState::Sent { attempt: 1, .. }),
        "a proved handoff is sent: {state:?}"
    );
}

/// A write that finished still knows where it went.
///
/// Settlement DELETES the in-memory pending row -- that deletion is precisely
/// what `outcome: Settled` is derived from. The destination set and the
/// per-relay picture used to be read off that same row, so the instant a
/// write became wholly successful it began reporting that it had reached
/// nobody. An app showing "published to 2 of 3" went blank on becoming 3 of
/// 3, which is the worst possible moment to lose the answer.
///
/// The durable rows outlive settlement: `close_terminal_intent` removes the
/// intent row and its deadlines and explicitly retains every route revision,
/// lane, attempt and detail, which only `remove_publish_queue_entry`
/// reclaims. The finished picture is still on disk, and this door must read
/// it from there.
#[test]
fn a_settled_write_still_reports_every_relay_it_published_to() {
    let a = Keys::generate();
    let one = RelayUrl::parse("wss://settled-one.example").unwrap();
    let two = RelayUrl::parse("wss://settled-two.example").unwrap();
    let three = RelayUrl::parse("wss://settled-three.example").unwrap();
    let dir = FixtureRoutingFacts::new()
        .with_outbound_routes(a.public_key(), [one.clone(), two.clone(), three.clone()]);
    let mut core = new_core(dir);
    activate(&mut core, &a);
    for (slot, relay) in [(0, &one), (1, &two), (2, &three)] {
        connect_signer(&mut core, slot, relay, a.public_key());
        authenticate_signer(&mut core, slot, relay, &a);
    }

    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "three for three")),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
    }));
    let (id, generation, unsigned) = find_sign_request(&accepted);
    let signed = unsigned.sign_with_keys(&a).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(
        id,
        generation,
        Ok(signed.clone()),
    ));

    for relay in [&one, &two, &three] {
        mark_written(&mut core, &effects, relay);
    }
    for (slot, relay) in [(0, &one), (1, &two), (2, &three)] {
        core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot,
                generation: 1,
            },
            signer_session(relay, signed.pubkey),
            RelayFrame::from(RelayMessage::ok(signed.id, true, "")),
        ));
    }

    let entry = queue_entry(&core, id);
    assert_eq!(
        entry.outcome,
        Some(WriteOutcome::Settled),
        "every destination acked, so the write is settled"
    );
    assert_eq!(
        entry.relays,
        BTreeSet::from([one.clone(), two.clone(), three.clone()]),
        "a settled write still names the three relays it was routed to"
    );
    assert_eq!(
        entry.relay_states.len(),
        3,
        "one projected state per destination, after settlement as before it: \
         {:?}",
        entry.relay_states
    );
    for relay in [&one, &two, &three] {
        assert_eq!(
            state_at(&entry.relay_states, relay),
            RelayState::Published,
            "{relay} acked, and settlement must not erase that"
        );
    }
}
